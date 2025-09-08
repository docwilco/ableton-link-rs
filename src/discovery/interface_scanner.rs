use std::{
    collections::HashSet,
    net::{IpAddr, Ipv6Addr},
    sync::Arc,
    time::Duration,
};

use tokio::{sync::Notify, time::interval};
use tracing::{debug, info};

/// Network interface scanner that monitors available network interfaces
/// and notifies when interfaces change
pub struct InterfaceScanner {
    scan_period: Duration,
    cancel_notify: Arc<Notify>,
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
            cancel_notify: Arc::new(Notify::new()),
            callback: Arc::new(callback),
        }
    }

    /// Enable or disable the interface scanner
    pub async fn enable(&self, enable: bool) {
        if enable {
            self.start_scanning().await;
        } else {
            self.cancel_notify.notify_one();
        }
    }

    /// Perform a single scan of network interfaces
    pub async fn scan_once(&self) {
        let interfaces = scan_network_interfaces().await;
        (self.callback)(interfaces);
    }

    /// Start continuous scanning
    async fn start_scanning(&self) {
        let mut interval = interval(self.scan_period);
        let cancel_notify = self.cancel_notify.clone();
        let callback = self.callback.clone();

        tokio::spawn(async move {
            let mut last_interfaces: Option<HashSet<IpAddr>> = None;

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        debug!("Scanning network interfaces");
                        let interfaces = scan_network_interfaces().await;
                        let interface_set: HashSet<IpAddr> = interfaces.iter().copied().collect();

                        // Only call callback if interfaces have changed
                        if last_interfaces.as_ref() != Some(&interface_set) {
                            info!("Network interfaces changed: {:?}", interfaces);
                            callback(interfaces);
                            last_interfaces = Some(interface_set);
                        }
                    }
                    _ = cancel_notify.notified() => {
                        debug!("Interface scanner cancelled");
                        break;
                    }
                }
            }
        });
    }
}

/// Scan for available network interfaces that can be used for Link discovery
pub async fn scan_network_interfaces() -> Vec<IpAddr> {
    // Use platform-optimized implementation
    crate::platform::network::scan_network_interfaces().await
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn test_interface_scanning() {
        let _ = tracing_subscriber::fmt::try_init();

        let interfaces = Arc::new(Mutex::new(Vec::new()));
        let interfaces_clone = interfaces.clone();

        let scanner = InterfaceScanner::new(Duration::from_millis(100), move |addrs| {
            *interfaces_clone.lock().unwrap() = addrs;
        });

        // Test single scan
        scanner.scan_once().await;

        let scanned_interfaces = interfaces.lock().unwrap().clone();
        info!(
            "Found {} interfaces: {:?}",
            scanned_interfaces.len(),
            scanned_interfaces
        );
    }

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
