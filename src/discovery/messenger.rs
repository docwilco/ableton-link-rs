use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket},
    sync::{
        mpsc::Sender,
        Arc, Mutex,
    },
    time::{Duration, Instant},
    thread,
};

use tracing::{debug, info};

use crate::{
    discovery::{messages::MESSAGE_TYPES, peers::PeerStateMessageType},
    link::{
        node::{NodeId, NodeState},
        payload::{Payload, PayloadEntry},
    },
};

use super::{
    gateway::OnEvent,
    messages::{
        encode_message, parse_message_header, parse_payload, MessageHeader, MessageType, ALIVE,
        BYEBYE, MAX_MESSAGE_SIZE, RESPONSE,
    },
    peers::PeerState,
    LINK_PORT, MULTICAST_ADDR, MULTICAST_IP_ANY,
};

// Safe UDP socket creation using socket2 and safe options
pub fn new_udp_reuseport(addr: SocketAddr) -> Result<UdpSocket, std::io::Error> {
    let domain = if addr.is_ipv4() {
        socket2::Domain::IPV4
    } else {
        socket2::Domain::IPV6
    };
    
    let udp_sock = socket2::Socket::new(domain, socket2::Type::DGRAM, None)?;

    // Set socket options using safe socket2 APIs
    udp_sock.set_reuse_address(true)?;

    // Set SO_REUSEPORT on Unix systems for better multicast support
    // Note: socket2 doesn't provide set_reuse_port, so we'll skip this optimization
    // The socket will still work fine without SO_REUSEPORT
    #[cfg(unix)]
    {
        // We could use socket2's raw methods here, but for maximum safety
        // we'll skip the SO_REUSEPORT optimization. The socket will still work.
        tracing::debug!("Note: SO_REUSEPORT not set (using safe implementation)");
    }

    udp_sock.bind(&socket2::SockAddr::from(addr))?;
    
    // Convert to std::net::UdpSocket
    let std_socket: std::net::UdpSocket = udp_sock.into();
    
    // Set read timeout to prevent blocking forever
    std_socket.set_read_timeout(Some(Duration::from_millis(100)))?;
    std_socket.set_write_timeout(Some(Duration::from_millis(100)))?;
    
    Ok(std_socket)
}

pub struct Messenger {
    pub interface: Option<Arc<UdpSocket>>,
    pub peer_state: Arc<Mutex<PeerState>>,
    pub ttl: u8,
    pub ttl_ratio: u8,
    pub last_broadcast_time: Arc<Mutex<Instant>>,
    pub tx_event: Sender<OnEvent>,
    pub stop_flag: Arc<Mutex<bool>>,
    pub enabled: Arc<Mutex<bool>>,
}

impl Messenger {
    pub fn new(
        peer_state: Arc<Mutex<PeerState>>,
        tx_event: Sender<OnEvent>,
        epoch: Instant,
        stop_flag: Arc<Mutex<bool>>,
        enabled: Arc<Mutex<bool>>,
    ) -> Self {
        let socket = new_udp_reuseport(MULTICAST_IP_ANY.into()).unwrap();
        socket
            .join_multicast_v4(&MULTICAST_ADDR, &Ipv4Addr::new(0, 0, 0, 0))
            .unwrap();
        socket.set_multicast_loop_v4(true).unwrap();

        Messenger {
            interface: Some(Arc::new(socket)),
            peer_state,
            ttl: 2, // Reduced from 5 to 2 seconds for faster peer timeout detection
            ttl_ratio: 20,
            last_broadcast_time: Arc::new(Mutex::new(epoch)),
            tx_event,
            stop_flag,
            enabled,
        }
    }

    pub fn listen(&self) {
        let socket = self.interface.as_ref().unwrap().clone();
        let peer_state = self.peer_state.clone();
        let ttl = self.ttl;
        let tx_event = self.tx_event.clone();
        let last_broadcast_time = self.last_broadcast_time.clone();
        let enabled = self.enabled.clone();
        let stop_flag = self.stop_flag.clone();

        // Spawn listener thread with 8KB stack
        thread::Builder::new()
            .stack_size(8192)
            .name("link-listener".to_string())
            .spawn(move || {
                let mut buf = [0; MAX_MESSAGE_SIZE];

                loop {
                    // Check stop flag
                    if let Ok(should_stop) = stop_flag.lock() {
                        if *should_stop {
                            break;
                        }
                    }

                    // recv_from with timeout (100ms set in socket creation)
                    let recv_result = socket.recv_from(&mut buf);
                    
                    let (amt, src) = match recv_result {
                        Ok(result) => result,
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock 
                                   || e.kind() == std::io::ErrorKind::TimedOut => {
                            // Timeout, continue loop to check stop flag
                            continue;
                        }
                        Err(e) => {
                            debug!("Error receiving message: {:?}", e);
                            continue;
                        }
                    };

                    let parse_result = parse_message_header(&buf[..amt]);
                    let (header, header_len) = match parse_result {
                        Ok(result) => result,
                        Err(e) => {
                            debug!("Error parsing message header: {:?}", e);
                            continue;
                        }
                    };

                    // TODO figure out how to encode group ID
                    let should_ignore = match peer_state.lock() {
                        Ok(guard) => header.ident == guard.ident() && header.group_id == 0,
                        Err(_) => false, // If we can't get the lock, don't ignore
                    };

                    if should_ignore {
                        debug!("ignoring messages from self (peer {})", header.ident);
                        continue;
                    } else {
                        debug!(
                            "received message type {} from peer {} at {}",
                            MESSAGE_TYPES[header.message_type as usize], header.ident, src
                        );
                    }

                    // Check if Link is enabled before processing ALIVE and RESPONSE messages
                    // BYEBYE messages should still be processed even when disabled to properly clean up peers
                    let is_enabled = if let Ok(enabled_guard) = enabled.lock() {
                        *enabled_guard
                    } else {
                        false
                    };

                    if let SocketAddr::V4(src) = src {
                        debug!(
                            "Received message type {} from peer {}",
                            header.message_type, header.ident
                        );
                        match header.message_type {
                            ALIVE => {
                                if !is_enabled {
                                    debug!(
                                        "ignoring ALIVE message from peer {} because Link is disabled",
                                        header.ident
                                    );
                                    continue;
                                }

                                send_response(
                                    socket.clone(),
                                    peer_state.clone(),
                                    ttl,
                                    src,
                                    last_broadcast_time.clone(),
                                );

                                receive_peer_state(tx_event.clone(), header, &buf[header_len..amt]);
                            }
                            RESPONSE => {
                                if !is_enabled {
                                    debug!("ignoring RESPONSE message from peer {} because Link is disabled", header.ident);
                                    continue;
                                }

                                receive_peer_state(tx_event.clone(), header, &buf[header_len..amt]);
                            }
                            BYEBYE => {
                                info!("Received BYEBYE message from peer {}", header.ident);
                                receive_bye_bye(tx_event.clone(), header.ident);
                            }
                            _ => {
                                debug!("Unknown message type: {}", header.message_type);
                            }
                        }
                    }
                }
            })
            .expect("Failed to spawn listener thread");

        // Spawn broadcaster thread with 8KB stack
        broadcast_state(
            self.ttl,
            self.ttl_ratio,
            self.last_broadcast_time.clone(),
            self.interface.as_ref().unwrap().clone(),
            self.peer_state.clone(),
            SocketAddrV4::new(MULTICAST_ADDR, LINK_PORT),
            self.stop_flag.clone(),
            self.enabled.clone(),
        );
    }
}

pub fn broadcast_state(
    ttl: u8,
    ttl_ratio: u8,
    last_broadcast_time: Arc<Mutex<Instant>>,
    socket: Arc<UdpSocket>,
    peer_state: Arc<Mutex<PeerState>>,
    to: SocketAddrV4,
    stop_flag: Arc<Mutex<bool>>,
    enabled: Arc<Mutex<bool>>,
) {
    let lbt = last_broadcast_time.clone();
    let s = socket.clone();

    // Spawn broadcaster thread with 8KB stack
    thread::Builder::new()
        .stack_size(8192)
        .name("link-broadcaster".to_string())
        .spawn(move || {
            let mut sleep_time = Duration::default();

            loop {
                // Check stop flag
                if let Ok(should_stop) = stop_flag.lock() {
                    if *should_stop {
                        break;
                    }
                }

                // Sleep
                if sleep_time > Duration::from_millis(0) {
                    thread::sleep(sleep_time);
                }

                let min_broadcast_period = Duration::from_millis(50);
                let nominal_broadcast_period =
                    Duration::from_millis(ttl as u64 * 1000 / ttl_ratio as u64);

                let lbt_clone = lbt.clone();

                let time_since_last_broadcast = match lbt_clone.lock() {
                    Ok(last_time) => {
                        if *last_time > Instant::now() {
                            0
                        } else {
                            Instant::now()
                                .duration_since(*last_time)
                                .as_millis()
                        }
                    }
                    Err(_) => {
                        // If we can't get the lock, use a conservative value
                        0
                    }
                };

                let tslb = Duration::from_millis(time_since_last_broadcast as u64);
                let delay = if tslb > min_broadcast_period {
                    Duration::default()
                } else {
                    min_broadcast_period - tslb
                };

                sleep_time = if delay > Duration::from_millis(0) {
                    delay
                } else {
                    nominal_broadcast_period
                };

                if delay < Duration::from_millis(1) {
                    // Only broadcast if Link is enabled
                    let should_broadcast = if let Ok(enabled_guard) = enabled.lock() {
                        *enabled_guard
                    } else {
                        false
                    };

                    if should_broadcast {
                        send_peer_state(s.clone(), peer_state.clone(), ttl, ALIVE, to, lbt_clone);
                    }
                }
            }
        })
        .expect("Failed to spawn broadcaster thread");
}

pub fn send_response(
    socket: Arc<UdpSocket>,
    peer_state: Arc<Mutex<PeerState>>,
    ttl: u8,
    to: SocketAddrV4,
    last_broadcast_time: Arc<Mutex<Instant>>,
) {
    send_peer_state(socket, peer_state, ttl, RESPONSE, to, last_broadcast_time)
}

pub fn send_message(
    socket: Arc<UdpSocket>,
    from: NodeId,
    ttl: u8,
    message_type: MessageType,
    payload: &Payload,
    to: SocketAddrV4,
) {
    socket.set_broadcast(true).unwrap();
    socket.set_multicast_ttl_v4(2).unwrap();
    socket.set_multicast_loop_v4(true).unwrap();

    let message = encode_message(from, ttl, message_type, payload).unwrap();

    let _ = socket.send_to(&message, to);
}

pub fn send_peer_state(
    socket: Arc<UdpSocket>,
    peer_state: Arc<Mutex<PeerState>>,
    ttl: u8,
    message_type: MessageType,
    to: SocketAddrV4,
    last_broadcast_time: Arc<Mutex<Instant>>,
) {
    let (ident, peer_state_clone) = match peer_state.lock() {
        Ok(guard) => (guard.ident(), guard.clone()),
        Err(_) => {
            // If we can't get the lock, skip this broadcast
            return;
        }
    };

    send_message(
        socket,
        ident,
        ttl,
        message_type,
        &peer_state_clone.into(),
        to,
    );

    if let Ok(mut last_time) = last_broadcast_time.lock() {
        *last_time = Instant::now();
    }
}

pub fn receive_peer_state(tx: Sender<OnEvent>, header: MessageHeader, buf: &[u8]) {
    let payload = parse_payload(buf).unwrap();
    let measurement_endpoint = payload.entries.iter().find_map(|e| {
        if let PayloadEntry::MeasurementEndpointV4(me) = e {
            me.endpoint
        } else {
            None
        }
    });

    let node_state: NodeState = NodeState::from_payload(header.ident, &payload);

    debug!("sending peer state to gateway {}", node_state.ident());
    let _ = tx.send(OnEvent::PeerState(PeerStateMessageType {
        node_state,
        ttl: header.ttl,
        measurement_endpoint,
    }));
}

pub fn receive_bye_bye(tx: Sender<OnEvent>, node_id: NodeId) {
    info!("Received BYEBYE message from peer {}", node_id);
    if let Err(e) = tx.send(OnEvent::Byebye(node_id)) {
        debug!("Failed to send BYEBYE event: {:?}", e);
    } else {
        info!("Successfully forwarded BYEBYE event for peer {}", node_id);
    }
}

pub fn send_byebye(node_state: NodeId) {
    info!("sending bye bye");

    let socket = new_udp_reuseport(MULTICAST_IP_ANY.into()).unwrap();
    socket.set_broadcast(true).unwrap();
    socket.set_multicast_ttl_v4(2).unwrap();

    let message = encode_message(node_state, 0, BYEBYE, &Payload::default()).unwrap();

    let _ = socket.send_to(&message, (MULTICAST_ADDR, LINK_PORT));
}
