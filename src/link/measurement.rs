use std::{
    collections::HashMap,
    io, mem,
    net::{Ipv4Addr, SocketAddrV4, UdpSocket},
    sync::{
        Arc, Mutex,
        mpsc::{self, Sender},
    },
    thread,
    time::Duration as StdDuration,
};

use chrono::Duration;
use tracing::{debug, info};

use crate::{
    discovery::{
        messages::parse_payload, messenger::new_udp_reuseport, peers::PeerState, ENCODING_CONFIG,
    },
    link::{
        payload::PrevGhostTime,
        pingresponder::{parse_message_header, MAX_MESSAGE_SIZE, PONG},
        Result,
    },
};

use super::{
    clock::Clock,
    ghostxform::GhostXForm,
    node::NodeId,
    payload::{HostTime, Payload, PayloadEntry, PayloadEntryHeader},
    pingresponder::{encode_message, PingResponder, PING},
    sessions::SessionId,
    state::SessionState,
};

pub const MEASUREMENT_ENDPOINT_V4_HEADER_KEY: u32 = u32::from_be_bytes(*b"mep4");
pub const MEASUREMENT_ENDPOINT_V4_SIZE: u32 =
    (mem::size_of::<Ipv4Addr>() + mem::size_of::<u16>()) as u32;
pub const MEASUREMENT_ENDPOINT_V4_HEADER: PayloadEntryHeader = PayloadEntryHeader {
    key: MEASUREMENT_ENDPOINT_V4_HEADER_KEY,
    size: MEASUREMENT_ENDPOINT_V4_SIZE,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeasurementEndpointV4 {
    pub endpoint: Option<SocketAddrV4>,
}

impl bincode::Encode for MeasurementEndpointV4 {
    fn encode<E: bincode::enc::Encoder>(
        &self,
        encoder: &mut E,
    ) -> std::result::Result<(), bincode::error::EncodeError> {
        bincode::Encode::encode(
            &(
                u32::from(*self.endpoint.unwrap().ip()),
                self.endpoint.unwrap().port(),
            ),
            encoder,
        )
    }
}

impl bincode::Decode<()> for MeasurementEndpointV4 {
    fn decode<D: bincode::de::Decoder>(
        decoder: &mut D,
    ) -> std::result::Result<Self, bincode::error::DecodeError> {
        let (ip, port) = bincode::Decode::decode(decoder)?;
        Ok(Self {
            endpoint: Some(SocketAddrV4::new(ip, port)),
        })
    }
}

impl MeasurementEndpointV4 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoded = MEASUREMENT_ENDPOINT_V4_HEADER.encode()?;
        encoded.append(&mut bincode::encode_to_vec(self, ENCODING_CONFIG)?);
        Ok(encoded)
    }
}

#[derive(Clone, Debug)]
pub enum MeasurePeerEvent {
    PeerState(SessionId, PeerState),
    XForm(SessionId, GhostXForm),
}

#[derive(Debug, Clone)]
pub struct MeasurementService {
    pub measurement_map: Arc<Mutex<HashMap<NodeId, Measurement>>>,
    pub clock: Clock,
    pub ping_responder: PingResponder,
    pub tx_measure_peer: std::sync::mpsc::Sender<MeasurePeerEvent>,
}

impl MeasurementService {
    pub fn new(
        ping_responder_unicast_socket: Arc<UdpSocket>,
        peer_state: Arc<Mutex<PeerState>>,
        session_state: Arc<Mutex<SessionState>>,
        clock: Clock,
        tx_measure_peer_result: std::sync::mpsc::Sender<MeasurePeerEvent>,
        stop_flag: Arc<Mutex<bool>>,
        rx_measure_peer_state: std::sync::mpsc::Receiver<MeasurePeerEvent>,
    ) -> MeasurementService {
        let measurement_map = Arc::new(Mutex::new(HashMap::new()));

        let m_map = measurement_map.clone();
        let t_peer = tx_measure_peer_result.clone();

        thread::Builder::new()
            .stack_size(8192)
            .spawn(move || {
                loop {
                    // Check stop flag
                    if let Ok(flag) = stop_flag.lock() {
                        if *flag {
                            break;
                        }
                    }

                    match rx_measure_peer_state.recv_timeout(StdDuration::from_millis(100)) {
                        Ok(MeasurePeerEvent::PeerState(session_id, peer)) => {
                            measure_peer(
                                clock,
                                m_map.clone(),
                                t_peer.clone(),
                                session_id,
                                peer,
                                stop_flag.clone(),
                            );
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            // Normal timeout, continue
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                            break;
                        }
                        _ => {}
                    }
                }
            })
            .expect("Failed to spawn measurement service thread");

        MeasurementService {
            measurement_map,
            clock,
            ping_responder: PingResponder::new(
                ping_responder_unicast_socket,
                peer_state.try_lock().unwrap().session_id(),
                session_state.try_lock().unwrap().ghost_x_form,
                clock,
            ),
            tx_measure_peer: tx_measure_peer_result,
        }
    }

    pub fn update_node_state(&self, session_id: SessionId, x_form: GhostXForm) {
        self.ping_responder
            .update_node_state(session_id, x_form);
    }
}

pub fn measure_peer(
    clock: Clock,
    measurement_map: Arc<Mutex<HashMap<NodeId, Measurement>>>,
    tx_measure_peer_result: std::sync::mpsc::Sender<MeasurePeerEvent>,
    session_id: SessionId,
    state: PeerState,
    stop_flag: Arc<Mutex<bool>>,
) {
    info!(
        "measuring peer {} at {} for session {}",
        state.node_state.node_id,
        state.measurement_endpoint.unwrap(),
        session_id
    );

    let node_id = state.node_state.node_id;

    let (tx_measurement, rx_measurement) = mpsc::channel();

    let measurement = Measurement::new(state, clock, tx_measurement, stop_flag.clone());
    measurement_map
        .try_lock()
        .unwrap()
        .insert(node_id, measurement);

    let tx_measure_peer_result_loop = tx_measure_peer_result.clone();
    let measurement_map_loop = measurement_map.clone();

    thread::Builder::new()
        .stack_size(8192)
        .spawn(move || {
            loop {
                // Check stop flag
                if let Ok(flag) = stop_flag.lock() {
                    if *flag {
                        break;
                    }
                }

                match rx_measurement.recv_timeout(StdDuration::from_millis(100)) {
                    Ok(data) => {
                        if data.is_empty() {
                            let _ = tx_measure_peer_result_loop
                                .send(MeasurePeerEvent::XForm(session_id, GhostXForm::default()));
                        } else {
                            let _ = tx_measure_peer_result_loop
                                .send(MeasurePeerEvent::XForm(
                                    session_id,
                                    GhostXForm {
                                        slope: 1.0,
                                        intercept: Duration::microseconds(median(data).round() as i64),
                                    },
                                ));
                        }

                        measurement_map_loop.try_lock().unwrap().remove(&node_id);
                        break;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        // Continue waiting
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        break;
                    }
                }
            }
        })
        .expect("Failed to spawn measurement result handler thread");
}

pub const NUMBER_DATA_POINTS: usize = 20;
pub const NUMBER_MEASUREMENTS: usize = 5;

#[derive(Debug)]
pub struct Measurement {
    pub unicast_socket: Option<Arc<UdpSocket>>,
    pub session_id: SessionId,
    pub measurement_endpoint: Option<SocketAddrV4>,
    pub data: Arc<Mutex<Vec<f64>>>,
    pub clock: Clock,
    pub measurements_started: Arc<Mutex<usize>>,
    pub success: Arc<Mutex<bool>>,
    pub init_bytes_sent: usize,
    tx_timer: Sender<()>,
}

impl Measurement {
    pub fn new(
        state: PeerState,
        clock: Clock,
        tx_measurement: Sender<Vec<f64>>,
        stop_flag: Arc<Mutex<bool>>,
    ) -> Self {
        let (tx_timer, rx_timer) = mpsc::channel();

        // TODO: Get actual IP from platform/network configuration
        // For ESP32, this will be injected by W5500 driver
        let ip = Ipv4Addr::new(0, 0, 0, 0);

        let unicast_socket = Arc::new(new_udp_reuseport(SocketAddrV4::new(ip, 0).into()).unwrap());
        info!(
            "initiating new unicast socket {} for measurement_endpoint {:?}",
            unicast_socket.local_addr().unwrap(),
            state.measurement_endpoint
        );

        let success = Arc::new(Mutex::new(false));
        let data = Arc::new(Mutex::new(vec![]));

        let mut measurement = Measurement {
            unicast_socket: Some(unicast_socket.clone()),
            session_id: state.node_state.session_id,
            measurement_endpoint: state.measurement_endpoint,
            data: data.clone(),
            clock,
            measurements_started: Arc::new(Mutex::new(0)),
            success: success.clone(),
            tx_timer: tx_timer.clone(),
            init_bytes_sent: 0,
        };

        let ht = HostTime::new(clock.micros());

        let s = success.clone();
        let d = data.clone();
        let t = tx_measurement.clone();
        let stop_flag_loop = stop_flag.clone();
        let endpoint = state.measurement_endpoint.unwrap();

        // Spawn timer/finish handler thread
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

                    match rx_timer.recv_timeout(StdDuration::from_millis(100)) {
                        Ok(_) => {
                            finish(
                                s.clone(),
                                endpoint,
                                d.clone(),
                                t.clone(),
                            );
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            // Continue
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                            break;
                        }
                    }
                }
            })
            .expect("Failed to spawn measurement timer thread");

        measurement.listen();

        info!("sending initial host time ping {:?}", ht);

        let init_bytes_sent = send_ping(
            unicast_socket.clone(),
            *state.measurement_endpoint.as_ref().unwrap(),
            &Payload {
                entries: vec![PayloadEntry::HostTime(ht)],
            },
        )
        .unwrap();

        measurement.init_bytes_sent = init_bytes_sent;

        reset_timer(
            measurement.measurements_started.clone(),
            clock,
            Some(unicast_socket.clone()),
            state.measurement_endpoint.unwrap(),
            data.clone(),
            tx_measurement.clone(),
            tx_timer.clone(),
        );

        measurement
    }

    pub fn listen(&mut self) {
        let socket = self.unicast_socket.as_ref().unwrap().clone();
        let endpoint = *self.measurement_endpoint.as_ref().unwrap();

        // Set socket timeouts for blocking operations
        if let Err(e) = socket.set_read_timeout(Some(StdDuration::from_millis(100))) {
            debug!("Failed to set socket timeout: {}", e);
            return;
        }

        let clock = self.clock;
        let s_id = self.session_id;
        let data = self.data.clone();
        let tx_timer = self.tx_timer.clone();

        info!(
            "listening for pong messages on {}",
            socket.local_addr().unwrap()
        );

        thread::Builder::new()
            .stack_size(8192)
            .spawn(move || {
                let mut pong_received = false;
                loop {
                    let mut buf = [0; MAX_MESSAGE_SIZE];

                    // Handle receive failure gracefully - peer may have disconnected
                    let (amt, src) = match socket.recv_from(&mut buf) {
                        Ok(result) => result,
                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {
                            continue;
                        }
                        Err(e) => {
                            debug!("Failed to receive from measurement socket: {}", e);
                            break;
                        }
                    };

                let (header, header_len) = parse_message_header(&buf[..amt]).unwrap();
                if header.message_type == PONG {
                    if !pong_received {
                        info!("received pong message from {}", src);
                        pong_received = true;
                    }

                    let payload = parse_payload(&buf[header_len..amt]).unwrap();

                    let mut session_id = SessionId::default();
                    let mut ghost_time = Duration::zero();
                    let mut prev_ghost_time = Duration::zero();
                    let mut prev_host_time = Duration::zero();

                    for entry in payload.entries.iter() {
                        match entry {
                            PayloadEntry::SessionMembership(id) => session_id = id.session_id,
                            PayloadEntry::GhostTime(gt) => ghost_time = gt.time,
                            PayloadEntry::PrevGhostTime(gt) => prev_ghost_time = gt.time,
                            PayloadEntry::HostTime(ht) => prev_host_time = ht.time,
                            _ => continue,
                        }
                    }

                    if s_id == session_id {
                        let host_time = clock.micros();

                        let payload = Payload {
                            entries: vec![
                                PayloadEntry::HostTime(HostTime { time: host_time }),
                                PayloadEntry::PrevGhostTime(PrevGhostTime {
                                    time: prev_ghost_time,
                                }),
                            ],
                        };

                        let _ = send_ping(socket.clone(), endpoint, &payload).unwrap();

                        if ghost_time != Duration::microseconds(0)
                            && prev_host_time != Duration::microseconds(0)
                        {
                            data.try_lock().unwrap().push(
                                ghost_time.num_microseconds().unwrap() as f64
                                    - ((host_time + prev_host_time).num_microseconds().unwrap()
                                        as f64
                                        * 0.5),
                            );

                            if prev_ghost_time != Duration::microseconds(0) {
                                data.try_lock().unwrap().push(
                                    ((ghost_time + prev_ghost_time).num_microseconds().unwrap()
                                        as f64
                                        * 0.5)
                                        - prev_host_time.num_microseconds().unwrap() as f64,
                                );
                            }
                        }

                        if data.try_lock().unwrap().len() > NUMBER_DATA_POINTS {
                            let _ = tx_timer.send(());
                            break;
                        }
                    }
                    }
                }
            })
            .expect("Failed to spawn measurement listener thread");
    }
}

fn reset_timer(
    measurements_started: Arc<Mutex<usize>>,
    clock: Clock,
    unicast_socket: Option<Arc<UdpSocket>>,
    measurement_endpoint: SocketAddrV4,
    data: Arc<Mutex<Vec<f64>>>,
    tx_measurement: Sender<Vec<f64>>,
    tx_timer: Sender<()>,
) {
    thread::Builder::new()
        .stack_size(8192)
        .spawn(move || {
            loop {
                thread::sleep(Duration::milliseconds(50).to_std().unwrap());
                info!(
                    "measurements_start {}",
                    measurements_started.try_lock().unwrap()
                );
                if *measurements_started.try_lock().unwrap() < NUMBER_MEASUREMENTS {
                    let ht = HostTime {
                        time: clock.micros(),
                    };

                    if let Err(e) = send_ping(
                        unicast_socket.as_ref().unwrap().clone(),
                        measurement_endpoint,
                        &Payload {
                            entries: vec![PayloadEntry::HostTime(ht)],
                        },
                    ) {
                        debug!("Failed to send ping to {}: {}", measurement_endpoint, e);
                        break;
                    }

                    thread::sleep(Duration::seconds(1).to_std().unwrap());

                    *measurements_started.try_lock().unwrap() += 1;
                } else {
                    data.try_lock().unwrap().clear();
                    info!("measuring {} failed", measurement_endpoint);

                    let data = data.try_lock().unwrap().clone();
                    let _ = tx_measurement.send(data);
                    break;
                }

                // Trigger timer callback
                let _ = tx_timer.send(());
            }
        })
        .expect("Failed to spawn reset_timer thread");
}

fn finish(
    success: Arc<Mutex<bool>>,
    measurement_endpoint: SocketAddrV4,
    data: Arc<Mutex<Vec<f64>>>,
    tx_measurement: Sender<Vec<f64>>,
) {
    *success.try_lock().unwrap() = true;
    debug!("measuring {} done", measurement_endpoint);

    let d = data.try_lock().unwrap().clone();
    let _ = tx_measurement.send(d);
    data.try_lock().unwrap().clear();
}

pub fn send_ping(
    socket: Arc<UdpSocket>,
    measurement_endpoint: SocketAddrV4,
    payload: &Payload,
) -> io::Result<usize> {
    let message = encode_message(PING, payload).unwrap();
    debug!(
        "sending ping message to measurement endpoint {}",
        measurement_endpoint
    );

    socket.send_to(&message, measurement_endpoint)
}

pub fn median(mut numbers: Vec<f64>) -> f64 {
    numbers.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let length = numbers.len();

    assert!(length > 2);
    if length % 2 == 0 {
        let mid = length / 2;
        (numbers[mid - 1] + numbers[mid]) / 2.0
    } else {
        numbers[length / 2]
    }
}


