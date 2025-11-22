// Platform-specific network interface scanning
// For ESP32: Returns the configured ethernet IP address
// For other platforms: Would use platform-specific network enumeration

use std::net::IpAddr;
use log::debug;

/// Scan for available network interfaces
/// On ESP32, this will be called after ethernet initialization
/// and should return the configured W5500 IP address
pub fn scan_network_interfaces() -> Vec<IpAddr> {
    // On ESP32, this will be implemented to return the ethernet IP
    // For now, return empty vec - will be populated by platform layer
    debug!("Scanning network interfaces (ESP32 stub)");
    Vec::new()
}
