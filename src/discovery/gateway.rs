use std::{
    net::{SocketAddr, SocketAddrV4, UdpSocket},
    sync::{
        mpsc::{self, Receiver, Sender},
        Arc, Mutex,
    },
    time::Instant,
    thread,
};

use log::{debug, info};

use crate::{
    discovery::{
        messages::{encode_message, BYEBYE},
        LINK_PORT, MULTICAST_ADDR,
    },
    link::{
        clock::Clock,
        controller::SessionPeerCounter,
        ghostxform::GhostXForm,
        measurement::{MeasurePeerEvent, MeasurementService},
        node::{NodeId, NodeState},
        payload::Payload,
        state::SessionState,
    },
};

use super::{
    messenger::{send_byebye, Messenger},
    peers::{
        ControllerPeer, GatewayObserver, PeerEvent, PeerState, PeerStateChange,
        PeerStateMessageType,
    },
};

pub struct PeerGateway {
    pub observer: GatewayObserver,
    pub peer_state: Arc<Mutex<PeerState>>,
    pub session_peer_counter: Arc<Mutex<SessionPeerCounter>>,
    pub measurement_service: MeasurementService,
    epoch: Instant,
    messenger: Messenger,
    tx_peer_event: Sender<PeerEvent>,
    peer_timeouts: Arc<Mutex<Vec<(Instant, NodeId)>>>,
    stop_flag: Arc<Mutex<bool>>,
}

#[derive(Clone)]
pub enum OnEvent {
    PeerState(PeerStateMessageType),
    Byebye(NodeId),
}

impl PeerGateway {
    pub fn new(
        peer_state: Arc<Mutex<PeerState>>,
        session_state: Arc<Mutex<SessionState>>,
        clock: Clock,
        session_peer_counter: Arc<Mutex<SessionPeerCounter>>,
        tx_peer_state_change: Sender<Vec<PeerStateChange>>,
        tx_event: Sender<OnEvent>,
        tx_measure_peer_result: Sender<MeasurePeerEvent>,
        peers: Arc<Mutex<Vec<ControllerPeer>>>,
        stop_flag: Arc<Mutex<bool>>,
        rx_measure_peer_state: Receiver<MeasurePeerEvent>,
        ping_responder_unicast_socket: Arc<UdpSocket>,
        enabled: Arc<Mutex<bool>>,
    ) -> Self {
        let (tx_peer_event, rx_peer_event) = mpsc::channel::<PeerEvent>();
        let epoch = Instant::now();
        
        if let Ok(mut state) = peer_state.lock() {
            state.measurement_endpoint = if let SocketAddr::V4(socket_addr) =
                ping_responder_unicast_socket.local_addr().unwrap()
            {
                Some(socket_addr)
            } else {
                None
            };
        }

        let messenger = Messenger::new(
            peer_state.clone(),
            tx_event.clone(),
            epoch,
            stop_flag.clone(),
            enabled.clone(),
        );

        PeerGateway {
            epoch,
            observer: GatewayObserver::new(
                rx_peer_event,
                peer_state.clone(),
                session_peer_counter.clone(),
                tx_peer_state_change,
                peers,
            ),
            messenger,
            measurement_service: MeasurementService::new(
                ping_responder_unicast_socket,
                peer_state.clone(),
                session_state,
                clock,
                tx_measure_peer_result,
                stop_flag.clone(),
                rx_measure_peer_state,
            ),
            peer_state,
            tx_peer_event,
            peer_timeouts: Arc::new(Mutex::new(vec![])),
            session_peer_counter,
            stop_flag,
        }
    }

    pub fn update_node_state(
        &self,
        node_state: NodeState,
        _measurement_endpoint: Option<SocketAddrV4>,
        ghost_xform: GhostXForm,
    ) {
        self.measurement_service
            .update_node_state(node_state.session_id, ghost_xform);

        if let Ok(mut state) = self.peer_state.lock() {
            state.node_state = node_state;
        }
    }

    pub fn listen(&self, rx_event: Receiver<OnEvent>) {
        let node_id = self.peer_state.lock()
            .map(|state| state.node_state.node_id)
            .unwrap_or_default();
        
        info!(
            "initializing peer gateway {:?} on interface {}",
            node_id,
            self.messenger
                .interface
                .as_ref()
                .unwrap()
                .local_addr()
                .unwrap()
        );

        let ctrl_socket = self.messenger.interface.as_ref().unwrap().clone();
        let peer_state = self.peer_state.clone();

        // Get self node ID for filtering self-messages
        let self_node_id = peer_state.lock()
            .map(|state| state.node_state.node_id)
            .unwrap_or_default();

        let peer_timeouts = self.peer_timeouts.clone();
        let tx_peer_event = self.tx_peer_event.clone();
        let epoch = self.epoch;
        let stop_flag = self.stop_flag.clone();

        self.measurement_service
            .ping_responder
            .listen();

        // Spawn event handler thread with 8KB stack
        thread::Builder::new()
            .stack_size(8192)
            .name("link-event-handler".to_string())
            .spawn(move || {
                loop {
                    // Check stop flag
                    if let Ok(should_stop) = stop_flag.lock() {
                        if *should_stop {
                            // Send BYEBYE message before shutting down
                            ctrl_socket.set_broadcast(true).ok();
                            ctrl_socket.set_multicast_ttl_v4(2).ok();

                            let peer_ident = peer_state.lock()
                                .map(|state| state.ident())
                                .unwrap_or_default();

                            if let Ok(message) = encode_message(
                                peer_ident,
                                0,
                                BYEBYE,
                                &Payload::default(),
                            ) {
                                let _ = ctrl_socket.send_to(&message, (MULTICAST_ADDR, LINK_PORT));
                            }
                            break;
                        }
                    }

                    // Process events
                    match rx_event.recv_timeout(std::time::Duration::from_millis(100)) {
                        Ok(val) => {
                            match val {
                                OnEvent::PeerState(msg) => {
                                    on_peer_state(
                                        msg,
                                        peer_timeouts.clone(),
                                        tx_peer_event.clone(),
                                        epoch,
                                        self_node_id,
                                    );
                                }
                                OnEvent::Byebye(node_id) => {
                                    on_byebye(node_id, peer_timeouts.clone(), tx_peer_event.clone());
                                }
                            }
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            // Timeout - check for expired peers inline (no thread spawn)
                            if let Ok(peer_timeouts_locked) = peer_timeouts.lock() {
                                if !peer_timeouts_locked.is_empty() {
                                    drop(peer_timeouts_locked); // Release lock before pruning
                                    let expired_peers = prune_expired_peers(peer_timeouts.clone(), epoch);
                                    for peer in expired_peers.iter() {
                                        info!("pruning peer {}", peer.1);
                                        if let Err(e) = tx_peer_event.send(PeerEvent::PeerTimedOut(peer.1)) {
                                            debug!("Failed to send PeerTimedOut event: {:?}", e);
                                        }
                                    }
                                }
                            }
                            continue;
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                            debug!("Event receiver disconnected");
                            break;
                        }
                    }
                }
            })
            .expect("Failed to spawn event handler thread");

        self.messenger.listen();
    }
}

pub fn on_peer_state(
    msg: PeerStateMessageType,
    peer_timeouts: Arc<Mutex<Vec<(Instant, NodeId)>>>,
    tx_peer_event: Sender<PeerEvent>,
    epoch: Instant,
    _self_node_id: NodeId,
) {
    debug!("received peer state from messenger");

    let peer_id = msg.node_state.ident();
    peer_timeouts
        .lock()
        .unwrap()
        .retain(|(_, id)| id != &peer_id);

    let new_to = (
        Instant::now() + std::time::Duration::from_secs(std::cmp::min(msg.ttl as u64, 3)), // Cap timeout at 3 seconds max
        peer_id,
    );

    debug!("updating peer timeout status for peer {} with TTL {} seconds (capped at 3)", peer_id, msg.ttl);

    if let Ok(mut timeouts) = peer_timeouts.lock() {
        let i = timeouts.iter().position(|(timeout, _)| timeout >= &new_to.0);
        if let Some(i) = i {
            timeouts.insert(i, new_to);
        } else {
            timeouts.push(new_to);
        }
    } else {
        debug!("Could not acquire peer_timeouts lock to update timeout for peer {}", peer_id);
    }

    debug!("sending peer state to observer");
    if let Err(e) = tx_peer_event
        .send(PeerEvent::SawPeer(PeerState {
            node_state: msg.node_state,
            measurement_endpoint: msg.measurement_endpoint,
        }))
    {
        debug!("Failed to send SawPeer event: {:?}", e);
        return;
    }

    schedule_next_pruning(peer_timeouts.clone(), epoch, tx_peer_event.clone());
}

pub fn on_byebye(
    peer_id: NodeId,
    peer_timeouts: Arc<Mutex<Vec<(Instant, NodeId)>>>,
    tx_peer_event: Sender<PeerEvent>,
) {
    info!("Processing BYEBYE from peer {}", peer_id);
    
    if find_peer(&peer_id, peer_timeouts.clone()) {
        info!("Found peer {} in timeout list, sending PeerLeft event", peer_id);
        if let Err(e) = tx_peer_event.send(PeerEvent::PeerLeft(peer_id)) {
            debug!("Failed to send PeerLeft event: {:?}", e);
        } else {
            info!("Successfully sent PeerLeft event for peer {}", peer_id);
        }

        if let Ok(mut timeouts) = peer_timeouts.lock() {
            timeouts.retain(|(_, id)| id != &peer_id);
            info!("Removed peer {} from timeout list", peer_id);
        } else {
            debug!("Could not acquire peer_timeouts lock to remove peer {}", peer_id);
        }
    } else {
        info!("Peer {} not found in timeout list (may have already been removed)", peer_id);
    }
}

pub fn find_peer(
    peer_id: &NodeId,
    peer_timeouts: Arc<Mutex<Vec<(Instant, NodeId)>>>,
) -> bool {
    match peer_timeouts.lock() {
        Ok(timeouts) => {
            let found = timeouts.iter().any(|(_, id)| id == peer_id);
            debug!("find_peer: Looking for peer {}, found: {}", peer_id, found);
            found
        },
        Err(_) => {
            debug!("Could not acquire peer_timeouts lock in find_peer for {}", peer_id);
            false
        }
    }
}

pub fn schedule_next_pruning(
    _peer_timeouts: Arc<Mutex<Vec<(Instant, NodeId)>>>,
    _epoch: Instant,
    _tx_peer_event: Sender<PeerEvent>,
) {
    // NOOP: Pruning is now handled by periodic checks in the event handler loop
    // This function is kept for API compatibility but does nothing
    // The event handler thread checks for expired peers every 100ms
}

fn prune_expired_peers(
    peer_timeouts: Arc<Mutex<Vec<(Instant, NodeId)>>>,
    epoch: Instant,
) -> Vec<(Instant, NodeId)> {
    info!(
        "pruning peers @ {}ms since gateway initialization epoch",
        Instant::now().duration_since(epoch).as_millis(),
    );

    // find the first element in pt whose timeout value is not less than Instant::now()
    // NOTE: peer_timeouts will be empty if byebye message is received prior to pruning

    let i = peer_timeouts
        .lock()
        .ok()
        .and_then(|timeouts| timeouts.iter().position(|(timeout, _)| timeout >= &Instant::now()));

    if let Some(i) = i {
        peer_timeouts
            .lock()
            .map(|mut timeouts| timeouts.drain(0..i).collect::<Vec<_>>())
            .unwrap_or_default()
    } else {
        peer_timeouts
            .lock()
            .map(|mut timeouts| timeouts.drain(..).collect::<Vec<_>>())
            .unwrap_or_default()
    }
}

impl Drop for PeerGateway {
    fn drop(&mut self) {
        // Set stop flag
        if let Ok(mut flag) = self.stop_flag.lock() {
            *flag = true;
        }
        
        if let Ok(state) = self.messenger.peer_state.lock() {
            send_byebye(state.ident());
        }
    }
}

// Tests removed - they require tokio runtime
// Tests removed - they depend on tokio runtime
