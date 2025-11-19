use std::{
    net::SocketAddrV4,
    sync::{
        mpsc::{Receiver, Sender},
        Arc, Mutex,
    },
    thread,
    vec,
};

use tracing::{debug, info};

use crate::link::{
    controller::SessionPeerCounter,
    node::{NodeId, NodeState},
    sessions::SessionId,
    state::StartStopState,
    timeline::Timeline,
};

#[derive(Clone)]
pub struct PeerStateMessageType {
    pub node_state: NodeState,
    pub ttl: u8,
    pub measurement_endpoint: Option<SocketAddrV4>,
}

pub enum PeerEvent {
    SawPeer(PeerState),
    PeerLeft(NodeId),
    PeerTimedOut(NodeId),
}

pub struct GatewayObserver {
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
}

impl GatewayObserver {
    pub fn new(
        on_peer_event: Receiver<PeerEvent>,
        peer_state: Arc<Mutex<PeerState>>,
        session_peer_counter: Arc<Mutex<SessionPeerCounter>>,
        tx_peer_state_change: Sender<Vec<PeerStateChange>>,
        peers: Arc<Mutex<Vec<ControllerPeer>>>,
    ) -> Self {
        let gwo = GatewayObserver { peers };

        let peers = gwo.peers.clone();
        let peer_state_loop = peer_state.clone();

        thread::Builder::new()
            .stack_size(8192)
            .spawn(move || {
                loop {
                    match on_peer_event.recv() {
                        Ok(PeerEvent::SawPeer(peer_state)) => {
                            saw_peer(
                                peer_state,
                                peers.clone(),
                                peer_state_loop.clone(),
                                session_peer_counter.clone(),
                                tx_peer_state_change.clone(),
                            )
                        }
                        Ok(PeerEvent::PeerLeft(node_id)) => {
                            peer_left(node_id, peers.clone(), tx_peer_state_change.clone())
                        }
                        Ok(PeerEvent::PeerTimedOut(node_id)) => {
                            peer_left(node_id, peers.clone(), tx_peer_state_change.clone())
                        }
                        Err(_) => {
                            debug!("Peer event channel closed");
                            break;
                        }
                    }
                }
            })
            .expect("Failed to spawn gateway observer thread");

        gwo
    }

    pub fn set_session_timeline(&mut self, session_id: SessionId, timeline: Timeline) {
        if let Ok(mut peers) = self.peers.lock() {
            peers.iter_mut().for_each(|peer| {
                if peer.peer_state.session_id() == session_id {
                    peer.peer_state.node_state.timeline = timeline;
                }
            });
        } else {
            debug!("Could not acquire peers lock in set_session_timeline");
        }
    }

    pub fn forget_session(&mut self, session_id: Arc<Mutex<SessionId>>) {
        let session_id_val = match session_id.lock() {
            Ok(guard) => *guard,
            Err(_) => {
                debug!("Could not acquire session_id lock in forget_session");
                return;
            }
        };

        if let Ok(mut peers) = self.peers.lock() {
            peers.retain(|peer| peer.peer_state.session_id() != session_id_val);
        } else {
            debug!("Could not acquire peers lock in forget_session");
        }
    }

    pub fn reset_peers(&self) {
        if let Ok(mut peers) = self.peers.lock() {
            peers.clear();
        } else {
            debug!("Could not acquire peers lock in reset_peers");
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PeerState {
    pub node_state: NodeState,
    pub measurement_endpoint: Option<SocketAddrV4>,
}

impl PeerState {
    pub fn ident(&self) -> NodeId {
        self.node_state.ident()
    }

    pub fn session_id(&self) -> SessionId {
        self.node_state.session_id
    }

    pub fn timeline(&self) -> Timeline {
        self.node_state.timeline
    }

    pub fn start_stop_state(&self) -> StartStopState {
        self.node_state.start_stop_state
    }
}

pub fn unique_session_peer_count(
    session_id: SessionId,
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    self_node_id: NodeId,
) -> usize {
    let all_peers = match peers.lock() {
        Ok(guard) => guard,
        Err(_) => {
            debug!("Could not acquire peers lock in unique_session_peer_count, returning 0");
            return 0;
        }
    };
    debug!(
        "unique_session_peer_count: looking for session_id={}, self_node_id={}",
        session_id, self_node_id
    );
    debug!(
        "unique_session_peer_count: total peers in list: {}",
        all_peers.len()
    );

    for (i, peer) in all_peers.iter().enumerate() {
        debug!(
            "  peer[{}]: node_id={}, session_id={}",
            i,
            peer.peer_state.ident(),
            peer.peer_state.session_id()
        );
    }

    let mut peers = all_peers
        .iter()
        .filter(|p| {
            let matches_session = p.peer_state.session_id() == session_id;
            let is_not_self = p.peer_state.ident() != self_node_id;
            debug!(
                "  filtering peer {}: matches_session={}, is_not_self={}",
                p.peer_state.ident(),
                matches_session,
                is_not_self
            );
            matches_session && is_not_self
        })
        .cloned()
        .collect::<Vec<_>>();

    debug!(
        "unique_session_peer_count: before dedup, filtered peers count: {}",
        peers.len()
    );
    peers.dedup_by(|a, b| {
        let is_duplicate = a.peer_state.ident() == b.peer_state.ident();
        if is_duplicate {
            debug!("  removing duplicate peer: {}", a.peer_state.ident());
        }
        is_duplicate
    });

    debug!(
        "unique_session_peer_count: after dedup, final peers count: {}",
        peers.len()
    );
    peers.len()
}

#[derive(Clone)]
pub enum PeerStateChange {
    SessionMembership,
    SessionTimeline(SessionId, Timeline),
    SessionStartStopState(SessionId, StartStopState),
    PeerLeft,
}

fn saw_peer(
    peer_seen_peer_state: PeerState,
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    self_peer_state: Arc<Mutex<PeerState>>,
    _session_peer_counter: Arc<Mutex<SessionPeerCounter>>,
    tx_peer_state_change: Sender<Vec<PeerStateChange>>,
) {
    // Additional safety check: Don't add ourselves to our own peer list
    let self_ident = match self_peer_state.lock() {
        Ok(guard) => guard.ident(),
        Err(_) => {
            debug!("Could not acquire self_peer_state lock in saw_peer, skipping self-check");
            // If we can't get the lock, continue anyway - the duplicate check later will catch self-peers
            peer_seen_peer_state.ident() // This will not match unless it's actually self, which is unlikely
        }
    };

    if peer_seen_peer_state.ident() == self_ident {
        debug!(
            "ignoring peer state from self (node {})",
            peer_seen_peer_state.ident()
        );
        return;
    }

    let ps = PeerState {
        node_state: peer_seen_peer_state.node_state,
        measurement_endpoint: peer_seen_peer_state.measurement_endpoint,
    };

    let peer_session = ps.session_id();
    let peer_timeline = ps.timeline();
    let peer_start_stop_state = ps.start_stop_state();

    let is_new_session_timeline = match peers.lock() {
        Ok(guard) => !guard.iter().any(|p| {
            p.peer_state.session_id() == peer_session && p.peer_state.timeline() == peer_timeline
        }),
        Err(_) => {
            debug!("Could not acquire peers lock to check session timeline, assuming new");
            true // Assume it's new if we can't check
        }
    };

    let is_new_session_start_stop_state = match peers.lock() {
        Ok(guard) => !guard
            .iter()
            .any(|p| p.peer_state.start_stop_state() == peer_start_stop_state),
        Err(_) => {
            debug!("Could not acquire peers lock to check start/stop state, assuming new");
            true // Assume it's new if we can't check
        }
    };

    let peer = ControllerPeer { peer_state: ps };

    let existing_peer_index = match peers.lock() {
        Ok(guard) => guard
            .iter()
            .position(|p| p.peer_state.ident() == peer.peer_state.ident()),
        Err(_) => {
            debug!("Could not acquire peers lock to find existing peer, assuming new");
            None // Assume it's new if we can't check
        }
    };

    let did_session_membership_change = if let Some(index) = existing_peer_index {
        // Update existing peer with new state
        match peers.lock() {
            Ok(mut guard) => {
                let old_session_id = guard[index].peer_state.session_id();
                guard[index] = peer.clone();
                // Session membership changed if the session ID changed
                old_session_id != peer_session
            }
            Err(_) => {
                debug!("Could not acquire peers lock to update existing peer");
                false // No change if we can't update
            }
        }
    } else {
        // Add new peer
        match peers.lock() {
            Ok(mut guard) => {
                guard.push(peer.clone());
                true
            }
            Err(_) => {
                debug!("Could not acquire peers lock to add new peer");
                false // No change if we can't add
            }
        }
    };

    let mut peer_state_changes = vec![];

    if is_new_session_timeline {
        debug!("session timeline changed");
        peer_state_changes.push(PeerStateChange::SessionTimeline(
            peer_session,
            peer_timeline,
        ));
    }

    if is_new_session_start_stop_state {
        debug!("session start stop changed");
        peer_state_changes.push(PeerStateChange::SessionStartStopState(
            peer_session,
            peer_start_stop_state,
        ));
    }

    if did_session_membership_change {
        debug!("session membership changed");
        peer_state_changes.push(PeerStateChange::SessionMembership);
    }

    if !peer_state_changes.is_empty() {
        debug!("sending peer state changes to controller");
        if let Err(e) = tx_peer_state_change.send(peer_state_changes) {
            debug!("Failed to send peer state changes: {}", e);
        }
    }
}

fn peer_left(
    node_id: NodeId,
    peers: Arc<Mutex<Vec<ControllerPeer>>>,
    tx_peer_state_change: Sender<Vec<PeerStateChange>>,
) {
    info!("Processing peer_left for node {}", node_id);

    let mut did_session_membership_change = false;
    if let Ok(mut peers_guard) = peers.lock() {
        let initial_count = peers_guard.len();
        peers_guard.retain(|peer| {
            if peer.peer_state.ident() == node_id {
                info!("Removing peer {} from peers list", node_id);
                did_session_membership_change = true;
                false
            } else {
                true
            }
        });
        let final_count = peers_guard.len();
        info!(
            "Peer count changed from {} to {} after removing peer {}",
            initial_count, final_count, node_id
        );
    } else {
        debug!(
            "Could not acquire peers lock in peer_left for node {}",
            node_id
        );
        return;
    }

    if did_session_membership_change {
        info!(
            "Session membership changed due to peer {} leaving, sending PeerLeft event",
            node_id
        );
        if let Err(e) = tx_peer_state_change.send(vec![PeerStateChange::PeerLeft]) {
            debug!("Failed to send peer left event: {}", e);
        } else {
            info!("Successfully sent PeerLeft event for peer {}", node_id);
        }
    } else {
        info!(
            "No session membership change for peer {} (peer not found in list)",
            node_id
        );
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ControllerPeer {
    pub peer_state: PeerState,
}

// Tests removed - they depend on tokio runtime
