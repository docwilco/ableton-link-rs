use std::{
    fmt::{self, Display},
    mem,
    sync::{
        mpsc::{Receiver, Sender},
        Arc, Mutex,
    },
    thread,
    time::Duration as StdDuration,
};

use bincode::{Decode, Encode};
use chrono::Duration;
use tracing::{debug, info};

use crate::discovery::{peers::ControllerPeer, ENCODING_CONFIG};

use super::{
    clock::Clock, ghostxform::GhostXForm, measurement::MeasurePeerEvent, node::NodeId,
    payload::PayloadEntryHeader, timeline::Timeline, Result,
};

pub const SESSION_MEMBERSHIP_HEADER_KEY: u32 = u32::from_be_bytes(*b"sess");
pub const SESSION_MEMBERSHIP_SIZE: u32 = mem::size_of::<SessionId>() as u32;
pub const SESSION_MEMBERSHIP_HEADER: PayloadEntryHeader = PayloadEntryHeader {
    key: SESSION_MEMBERSHIP_HEADER_KEY,
    size: SESSION_MEMBERSHIP_SIZE,
};

#[derive(Clone, Copy, Debug, Encode, Decode, Default, PartialEq, Eq, PartialOrd)]
pub struct SessionId(pub NodeId);

impl Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone, Copy, Debug, Default, Encode, Decode)]
pub struct SessionMembership {
    pub session_id: SessionId,
}

impl From<SessionId> for SessionMembership {
    fn from(session_id: SessionId) -> Self {
        SessionMembership { session_id }
    }
}

impl SessionMembership {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoded = SESSION_MEMBERSHIP_HEADER.encode()?;
        encoded.append(&mut bincode::encode_to_vec(
            self.session_id,
            ENCODING_CONFIG,
        )?);
        Ok(encoded)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SessionMeasurement {
    pub x_form: GhostXForm,
    pub timestamp: Duration,
}

impl Default for SessionMeasurement {
    fn default() -> Self {
        Self {
            x_form: GhostXForm::default(),
            timestamp: Duration::zero(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Session {
    pub session_id: SessionId,
    pub timeline: Timeline,
    pub measurement: SessionMeasurement,
}

#[derive(Clone)]
pub struct Sessions {
    pub other_sessions: Arc<Mutex<Vec<Session>>>,
    pub current: Arc<Mutex<Session>>,
    pub is_founding: Arc<Mutex<bool>>,
    pub tx_measure_peer_state: Sender<MeasurePeerEvent>,
    pub peers: Arc<Mutex<Vec<ControllerPeer>>>,
    pub clock: Clock,
    pub has_joined: Arc<Mutex<bool>>,
    stop_flag: Arc<Mutex<bool>>,
}

impl Sessions {
    pub fn new(
        init: Session,
        tx_measure_peer_state: Sender<MeasurePeerEvent>,
        peers: Arc<Mutex<Vec<ControllerPeer>>>,
        clock: Clock,
        tx_join_session: Sender<Session>,
        mut rx_measure_peer_result: Receiver<MeasurePeerEvent>,
    ) -> Self {
        let other_sessions = Arc::new(Mutex::new(vec![init.clone()]));
        let current = Arc::new(Mutex::new(init));
        let stop_flag = Arc::new(Mutex::new(false));

        let other_sessions_loop = other_sessions.clone();
        let current_loop = current.clone();
        let tx_join_session_loop = tx_join_session.clone();
        let peers_loop = peers.clone();
        let tx_measure_peer_state_loop = tx_measure_peer_state.clone();
        let stop_flag_loop = stop_flag.clone();

        thread::Builder::new()
            .stack_size(8192)
            .spawn(move || {
            loop {
                // Check stop flag
                if let Ok(flag) = stop_flag_loop.lock() {
                    if *flag {
                        break;
                    }
                }

                match rx_measure_peer_result.recv_timeout(StdDuration::from_millis(100)) {
                    Ok(MeasurePeerEvent::XForm(session_id, x_form)) => {
                        if x_form == GhostXForm::default() {
                            handle_failed_measurement(
                                session_id,
                                other_sessions_loop.clone(),
                                current_loop.clone(),
                                peers_loop.clone(),
                                tx_measure_peer_state_loop.clone(),
                            );
                        } else {
                            handle_successful_measurement(
                                session_id,
                                x_form,
                                other_sessions_loop.clone(),
                                current_loop.clone(),
                                clock,
                                tx_join_session_loop.clone(),
                                peers_loop.clone(),
                                tx_measure_peer_state_loop.clone(),
                            );
                        }
                    }
                    Ok(_) => {} // Other event types
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        // Timeout is normal, continue loop
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        info!("measure peer event channel closed");
                        break;
                    }
                }
            }
        })
        .expect("Failed to spawn sessions measurement thread");

        Self {
            other_sessions,
            current,
            tx_measure_peer_state,
            peers,
            clock,
            is_founding: Arc::new(Mutex::new(false)),
            has_joined: Arc::new(Mutex::new(false)),
            stop_flag,
        }
    }

    pub fn reset_session(&mut self, session: Session) {
        *self.current.try_lock().unwrap() = session;
        self.other_sessions.try_lock().unwrap().clear()
    }

    pub fn reset_timeline(&self, timeline: Timeline) {
        if let Some(session) = self
            .other_sessions
            .try_lock()
            .unwrap()
            .iter_mut()
            .find(|s| s.session_id == self.current.try_lock().unwrap().session_id)
        {
            session.timeline = timeline;
        }
    }

    pub fn saw_session_timeline(
        &self,
        session_id: SessionId,
        timeline: Timeline,
    ) -> Timeline {
        debug!(
            "saw session timeline {:?} for session {}",
            timeline, session_id,
        );

        if self.current.try_lock().unwrap().session_id == session_id {
            let session = self.update_timeline(self.current.try_lock().unwrap().clone(), timeline);
            self.current.try_lock().unwrap().timeline = session.timeline;
            if !*self.has_joined.try_lock().unwrap() {
                debug!(
                    "updating current session {} with timeline {:?}",
                    session_id, session.timeline
                );

                *self.has_joined.try_lock().unwrap() = true;
            }
        } else {
            let session = Session {
                session_id,
                timeline,
                measurement: SessionMeasurement {
                    x_form: GhostXForm::default(),
                    timestamp: Duration::zero(),
                },
            };

            let s = self
                .other_sessions
                .try_lock()
                .unwrap()
                .iter()
                .cloned()
                .enumerate()
                .find(|(_, s)| s.session_id == session_id);

            if let Some((idx, s)) = s {
                let session = self.update_timeline(s, timeline);
                info!(
                    "updating already seen session {} with timeline {:?}",
                    session_id, session.timeline
                );
                self.other_sessions.try_lock().unwrap()[idx].timeline = session.timeline;
            } else {
                info!("adding session {} to other sessions", session_id);
                self.other_sessions
                    .try_lock()
                    .unwrap()
                    .push(session.clone());

                launch_session_measurement(
                    self.peers.clone(),
                    self.tx_measure_peer_state.clone(),
                    session,
                );
            }
        }

        self.current.try_lock().unwrap().timeline
    }

    pub fn update_timeline(&self, mut session: Session, timeline: Timeline) -> Session {
        if timeline.beat_origin > session.timeline.beat_origin {
            info!(
                "[adopting] updating peer timeline for session {} (bpm: {}, beat origin: {}, time: origin: {})",
                session.session_id,
                timeline.tempo.bpm().round(),
                timeline.beat_origin.floating(),
                timeline.time_origin,
            );
            session.timeline = timeline;
        } else {
            debug!(
                "[rejecting] updating peer timeline with beat origin: {}. current timeline beat origin: {}",
                timeline.beat_origin.floating(),
                session.timeline.beat_origin.floating()
            );
        }

        session
    }
}

impl Drop for Sessions {
    fn drop(&mut self) {
        if let Ok(mut flag) = self.stop_flag.lock() {
            *flag = true;
        }
    }
}

pub fn launch_session_measurement(
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    tx_measure_peer_state: Sender<MeasurePeerEvent>,
    mut session: Session,
) {
    info!(
        "launching session measurement for session {}",
        session.session_id
    );

    let peers = session_peers(peers.clone(), session.session_id);

    if let Some(p) = peers
        .iter()
        .find(|p| p.peer_state.ident() == session.session_id.0)
    {
        session.measurement.timestamp = Duration::zero();
        let _ = tx_measure_peer_state.send(MeasurePeerEvent::PeerState(
            session.session_id,
            p.peer_state.clone(),
        ));
    } else if let Some(p) = peers.first() {
        session.measurement.timestamp = Duration::zero();
        let _ = tx_measure_peer_state.send(MeasurePeerEvent::PeerState(
            session.session_id,
            p.peer_state.clone(),
        ));
    }
}

pub fn handle_successful_measurement(
    session_id: SessionId,
    x_form: GhostXForm,
    other_sessions: Arc<Mutex<Vec<Session>>>,
    current: Arc<Mutex<Session>>,
    clock: Clock,
    tx_join_session: std::sync::mpsc::Sender<Session>,
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    tx_measure_peer_state: std::sync::mpsc::Sender<MeasurePeerEvent>,
) {
    info!(
        "session {} measurement completed with result ({}, {})",
        session_id,
        x_form.slope,
        x_form.intercept.num_microseconds().unwrap(),
    );

    let measurement = SessionMeasurement {
        x_form,
        timestamp: clock.micros(),
    };

    let current_session_id = current.try_lock().unwrap().session_id;
    debug!("Current session: {}, measured session: {}", current_session_id, session_id);

    if current_session_id == session_id {
        current.try_lock().unwrap().measurement = measurement;
        let session = current.try_lock().unwrap().clone();
        if let Err(e) = tx_join_session.send(session) {
            debug!("Failed to send session join event: {}", e);
        }
    } else {
        let s = other_sessions
            .try_lock()
            .unwrap()
            .iter()
            .cloned()
            .enumerate()
            .find(|(_, s)| s.session_id == session_id);

        if let Some((idx, mut s)) = s {
            const SESSION_EPS: Duration = Duration::microseconds(500000);

            let host_time = clock.micros();
            let cur_ghost = current
                .try_lock()
                .unwrap()
                .measurement
                .x_form
                .host_to_ghost(host_time);
            let new_ghost = measurement.x_form.host_to_ghost(host_time);

            s.measurement = measurement;
            other_sessions.try_lock().unwrap()[idx] = s.clone();

            let ghost_diff = new_ghost - cur_ghost;
            debug!("Ghost time comparison: current={} us, new={} us, diff={} us, eps={} us", 
                   cur_ghost.num_microseconds().unwrap(), 
                   new_ghost.num_microseconds().unwrap(),
                   ghost_diff.num_microseconds().unwrap(),
                   SESSION_EPS.num_microseconds().unwrap());

            // Session switching logic: be selective about when to join other sessions
            // 1. Always join if we have significantly better timing (>500ms)
            // 2. Join if times are similar and we prefer older session IDs
            // 3. Join if we just started up and have no peers (prefer any established session)
            let current_session_has_no_peers = session_peers(peers.clone(), current.try_lock().unwrap().session_id).is_empty();
            let just_started = current_session_has_no_peers && measurement.timestamp < Duration::seconds(5);
            let current_session_id = current.try_lock().unwrap().session_id;
            
            let should_switch = 
                // Significant timing advantage
                ghost_diff > SESSION_EPS
                // Similar timing, prefer older session
                || (ghost_diff.num_microseconds().unwrap().abs() < SESSION_EPS.num_microseconds().unwrap()
                    && session_id < current_session_id)
                // Just started, prefer any established session over isolation
                || just_started;

            if should_switch {
                info!("Session {} wins over current session (ghost_diff={} us, just_started={}, tempo={}), switching!", 
                      session_id, 
                      ghost_diff.num_microseconds().unwrap(),
                      just_started,
                      s.timeline.tempo.bpm());
                let c = current.try_lock().unwrap().clone();

                *current.try_lock().unwrap() = s.clone();
                other_sessions.try_lock().unwrap().remove(idx);
                other_sessions.try_lock().unwrap().insert(idx, c);

                if let Err(e) = tx_join_session.send(s.clone()) {
                    debug!("Failed to send session join event: {}", e);
                }

                schedule_remeasurement(peers.clone(), tx_measure_peer_state.clone(), s);
            } else {
                debug!("Session {} does not win over current session (ghost_diff={} us, just_started={}), staying with current", 
                       session_id, 
                       ghost_diff.num_microseconds().unwrap(),
                       just_started);
            }
        }
    }
}

pub fn handle_failed_measurement(
    session_id: SessionId,
    other_sessions: Arc<Mutex<Vec<Session>>>,
    current: Arc<Mutex<Session>>,
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    tx_measure_peer: Sender<MeasurePeerEvent>,
) {
    info!("session {} measurement failed", session_id);

    if current.try_lock().unwrap().session_id == session_id {
        let current = current.try_lock().unwrap().clone();
        schedule_remeasurement(peers, tx_measure_peer, current);
    } else {
        let s = other_sessions
            .try_lock()
            .unwrap()
            .iter()
            .cloned()
            .enumerate()
            .find(|(_, s)| s.session_id != session_id);

        if let Some((idx, _)) = s {
            other_sessions.try_lock().unwrap().remove(idx);

            let p = peers
                .try_lock()
                .unwrap()
                .iter()
                .cloned()
                .enumerate()
                .filter(|(_, p)| p.peer_state.session_id() == session_id)
                .collect::<Vec<_>>();

            for (idx, _) in p {
                peers.try_lock().unwrap().remove(idx);
            }
        }
    }
}

pub fn schedule_remeasurement(
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    tx_measure_peer: Sender<MeasurePeerEvent>,
    session: Session,
) {
    thread::Builder::new()
        .stack_size(8192)
        .spawn(move || {
        loop {
            thread::sleep(StdDuration::from_secs(30));
            launch_session_measurement(peers.clone(), tx_measure_peer.clone(), session.clone());
        }
    })
    .expect("Failed to spawn remeasurement thread");
}

pub fn session_peers(
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    session_id: SessionId,
) -> Vec<ControllerPeer> {
    let mut peers = peers
        .try_lock()
        .unwrap()
        .iter()
        .filter(|p| p.peer_state.session_id() == session_id)
        .cloned()
        .collect::<Vec<_>>();
    peers.sort_by(|a, b| a.peer_state.ident().cmp(&b.peer_state.ident()));

    peers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key() {
        assert_eq!(SESSION_MEMBERSHIP_HEADER_KEY, 0x73657373);
    }
}
