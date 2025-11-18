use std::{
    collections::HashSet,
    net::{IpAddr, Ipv6Addr},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use tracing::{debug, info};

/// Network interface scanner that monitors available network interfaces
/// and notifies when interfaces change
pub struct InterfaceScanner {
    scan_period: Duration,
    stop_flag: Arc<Mutex<bool>>,
    callback: Arc<dyn Fn(Vec<IpAddr>) + Send + Sync>,
}

impl InterfaceScanner {
    /// Create a new interface scanner with the specified scan period and callback
    pub fn new<F>(scan_period: Duration, callback: F) -> Self
    where
        F: Fn(Vec<IpAddr>) + Send + Sync + 'static,
    {
        Self {
            scan_period,
            stop_flag: Arc::new(Mutex::new(false)),
            callback: Arc::new(callback),
        }
    }

    /// Enable or disable the interface scanner
    pub fn enable(&self, enable: bool) {
        if enable {
            self.start_scanning();
        } else {
            if let Ok(mut flag) = self.stop_flag.lock() {
                *flag = true;
            }
        }
    }

    /// Perform a single scan of network interfaces
    pub fn scan_once(&self) {
        let interfaces = scan_network_interfaces();
        (self.callback)(interfaces);
    }

    /// Start continuous scanning
    fn start_scanning(&self) {
        let scan_period = self.scan_period;
        let stop_flag = self.stop_flag.clone();
        let callback = self.callback.clone();

        thread::Builder::new()
            .stack_size(8192)
            .spawn(move || {
                let mut last_interfaces: Option<HashSet<IpAddr>> = None;
                let mut next_scan = Instant::now();

                loop {
                    // Check stop flag
                    if let Ok(flag) = stop_flag.lock() {
                        if *flag {
                            debug!("Interface scanner cancelled");
                            break;
                        }
                    }

                    if Instant::now() >= next_scan {
                        debug!("Scanning network interfaces");
                        let interfaces = scan_network_interfaces();
                        let interface_set: HashSet<IpAddr> = interfaces.iter().copied().collect();

                        // Only call callback if interfaces have changed
                        if last_interfaces.as_ref() != Some(&interface_set) {
                            info!("Network interfaces changed: {:?}", interfaces);
                            callback(interfaces);
                            last_interfaces = Some(interface_set);
                        }

                        next_scan = Instant::now() + scan_period;
                    }

                    thread::sleep(Duration::from_millis(100));
                }
            })
            .expect("Failed to spawn interface scanner thread");
    }
}

/// Scan for available network interfaces that can be used for Link discovery
pub fn scan_network_interfaces() -> Vec<IpAddr> {
    // Use platform-optimized implementation
    crate::platform::network::scan_network_interfaces()
}

/// Get the scope ID for an IPv6 address (needed for link-local addresses)
pub fn get_ipv6_scope_id(addr: &Ipv6Addr) -> Option<u32> {
    if addr.is_unicast_link_local() {
        // For link-local addresses, we need to determine the scope ID
        // This is a simplified implementation - in practice, you'd want to
        // get the actual interface index from the system
        Some(0) // Default scope
    } else {
        None
    }
}

// Tests removed - they depend on tokio runtime

    #[test]
    fn test_ipv6_scope_id() {
        // Link-local IPv6 should have scope ID
        let link_local = Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
        assert!(get_ipv6_scope_id(&link_local).is_some());

        // Global IPv6 should not have scope ID
        let global = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);
        assert!(get_ipv6_scope_id(&global).is_none());
    }
}
