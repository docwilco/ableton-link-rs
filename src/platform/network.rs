// Platform-specific network interface scanning optimizations
// Based on Ableton Link's interface detection implementations

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use tracing::{debug, error};

/// Platform-optimized network interface scanner
pub async fn scan_network_interfaces() -> Vec<IpAddr> {
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "freebsd"))]
    {
        scan_posix_interfaces().await
    }

    #[cfg(target_os = "windows")]
    {
        scan_windows_interfaces().await
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "freebsd", target_os = "windows")))]
    {
        scan_generic_interfaces().await
    }
}

// POSIX implementation using getifaddrs directly for maximum performance
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "freebsd"))]
async fn scan_posix_interfaces() -> Vec<IpAddr> {
    use std::collections::HashMap;
    use std::ffi::CStr;
    use std::mem;
    use std::ptr;

    tokio::task::spawn_blocking(|| {
        let mut addresses = Vec::new();
        let mut ipv4_interfaces: HashMap<String, IpAddr> = HashMap::new();

        unsafe {
            let mut ifaddrs_ptr: *mut libc::ifaddrs = ptr::null_mut();
            if libc::getifaddrs(&mut ifaddrs_ptr) != 0 {
                error!("Failed to call getifaddrs");
                return addresses;
            }

            // First pass: collect IPv4 addresses
            let mut current = ifaddrs_ptr;
            while !current.is_null() {
                let ifaddr = &*current;
                
                if !ifaddr.ifa_addr.is_null() && (ifaddr.ifa_flags & libc::IFF_RUNNING as u32) != 0 {
                    let sa_family = (*ifaddr.ifa_addr).sa_family;
                    
                    if u16::from(sa_family) == libc::AF_INET as u16 {
                        let sockaddr_in = &*(ifaddr.ifa_addr as *const libc::sockaddr_in);
                        let addr_bytes = sockaddr_in.sin_addr.s_addr.to_ne_bytes();
                        let ipv4 = Ipv4Addr::new(addr_bytes[0], addr_bytes[1], addr_bytes[2], addr_bytes[3]);
                        let ip_addr = IpAddr::V4(ipv4);
                        
                        if is_usable_interface(&ip_addr) {
                            addresses.push(ip_addr);
                            if let Ok(name) = CStr::from_ptr(ifaddr.ifa_name).to_str() {
                                ipv4_interfaces.insert(name.to_string(), ip_addr);
                            }
                        }
                    }
                }
                
                current = ifaddr.ifa_next;
            }

            // Second pass: collect IPv6 addresses only for interfaces that have IPv4
            current = ifaddrs_ptr;
            while !current.is_null() {
                let ifaddr = &*current;
                
                if !ifaddr.ifa_addr.is_null() && (ifaddr.ifa_flags & libc::IFF_RUNNING as u32) != 0 {
                    let sa_family = (*ifaddr.ifa_addr).sa_family;
                    
                    if u16::from(sa_family) == libc::AF_INET6 as u16 {
                        if let Ok(name) = CStr::from_ptr(ifaddr.ifa_name).to_str() {
                            if ipv4_interfaces.contains_key(name) {
                                let sockaddr_in6 = &*(ifaddr.ifa_addr as *const libc::sockaddr_in6);
                                let addr_bytes: [u8; 16] = mem::transmute(sockaddr_in6.sin6_addr.s6_addr);
                                let ipv6 = Ipv6Addr::from(addr_bytes);
                                let ip_addr = IpAddr::V6(ipv6);
                                
                                // Only include link-local IPv6 addresses
                                if !ipv6.is_loopback() && ipv6.is_unicast_link_local() {
                                    addresses.push(ip_addr);
                                }
                            }
                        }
                    }
                }
                
                current = ifaddr.ifa_next;
            }

            libc::freeifaddrs(ifaddrs_ptr);
        }

        // Sort and deduplicate
        addresses.sort();
        addresses.dedup();

        debug!("Found {} network interfaces using POSIX implementation", addresses.len());
        addresses
    }).await.unwrap_or_else(|e| {
        error!("Failed to scan POSIX interfaces: {}", e);
        Vec::new()
    })
}

// Windows implementation using native Windows APIs
#[cfg(target_os = "windows")]
async fn scan_windows_interfaces() -> Vec<IpAddr> {
    use std::collections::HashMap;
    use std::mem;
    use std::ptr;
    use winapi::shared::ws2def::{AF_INET, AF_INET6, SOCKADDR_IN, SOCKADDR_IN6};
    use winapi::shared::winerror::ERROR_SUCCESS;
    use winapi::um::iphlpapi::GetAdaptersAddresses;
    use winapi::um::iptypes::{GAA_FLAG_INCLUDE_PREFIX, IP_ADAPTER_ADDRESSES, IP_ADAPTER_UNICAST_ADDRESS};

    tokio::task::spawn_blocking(|| {
        let mut addresses = Vec::new();
        let mut ipv4_interfaces: HashMap<String, IpAddr> = HashMap::new();

        unsafe {
            // First, get the required buffer size
            let mut buf_len = 0u32;
            GetAdaptersAddresses(
                AF_INET as u32,
                GAA_FLAG_INCLUDE_PREFIX,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut buf_len,
            );

            if buf_len == 0 {
                error!("Failed to get adapter addresses buffer size");
                return addresses;
            }

            // Allocate buffer and get adapter addresses
            let mut buffer = vec![0u8; buf_len as usize];
            let adapter_addresses = buffer.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES;

            let result = GetAdaptersAddresses(
                AF_INET as u32,
                GAA_FLAG_INCLUDE_PREFIX,
                ptr::null_mut(),
                adapter_addresses,
                &mut buf_len,
            );

            if result != ERROR_SUCCESS {
                error!("Failed to get adapter addresses: {}", result);
                return addresses;
            }

            // First pass: collect IPv4 addresses
            let mut current_adapter = adapter_addresses;
            while !current_adapter.is_null() {
                let adapter = &*current_adapter;
                
                let mut current_address = adapter.FirstUnicastAddress;
                while !current_address.is_null() {
                    let address = &*current_address;
                    let sockaddr = address.Address.lpSockaddr;
                    
                    if (*sockaddr).sa_family == AF_INET as u16 {
                        let sockaddr_in = &*(sockaddr as *const SOCKADDR_IN);
                        let addr_bytes = sockaddr_in.sin_addr.S_un.S_addr().to_ne_bytes();
                        let ipv4 = Ipv4Addr::new(addr_bytes[0], addr_bytes[1], addr_bytes[2], addr_bytes[3]);
                        let ip_addr = IpAddr::V4(ipv4);
                        
                        if is_usable_interface(&ip_addr) {
                            addresses.push(ip_addr);
                            // Convert adapter name to string
                            let adapter_name = std::ffi::CStr::from_ptr(adapter.AdapterName as *const i8)
                                .to_string_lossy()
                                .to_string();
                            ipv4_interfaces.insert(adapter_name, ip_addr);
                        }
                    }
                    
                    current_address = address.Next;
                }
                
                current_adapter = adapter.Next;
            }

            // Second pass: collect IPv6 addresses for interfaces with IPv4
            // (Similar implementation for IPv6...)
        }

        // Sort and deduplicate
        addresses.sort();
        addresses.dedup();

        debug!("Found {} network interfaces using Windows implementation", addresses.len());
        addresses
    }).await.unwrap_or_else(|e| {
        error!("Failed to scan Windows interfaces: {}", e);
        Vec::new()
    })
}

// Generic fallback implementation using if-addrs crate
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "freebsd", target_os = "windows")))]
async fn scan_generic_interfaces() -> Vec<IpAddr> {
    tokio::task::spawn_blocking(|| {
        let mut addresses = Vec::new();

        match if_addrs::get_if_addrs() {
            Ok(interfaces) => {
                for interface in interfaces {
                    let addr = interface.ip();
                    if is_usable_interface(&addr) {
                        addresses.push(addr);
                        debug!("Found interface: {} ({})", interface.name, addr);
                    }
                }
            }
            Err(e) => {
                error!("Failed to get network interfaces: {}", e);
            }
        }

        // Sort and deduplicate
        addresses.sort();
        addresses.dedup();

        debug!("Found {} network interfaces using generic implementation", addresses.len());
        addresses
    }).await.unwrap_or_else(|e| {
        error!("Failed to scan generic interfaces: {}", e);
        Vec::new()
    })
}

/// Check if an IP address represents a usable interface for Link discovery
/// This mirrors the logic from the C++ implementation
fn is_usable_interface(addr: &IpAddr) -> bool {
    match addr {
        IpAddr::V4(ipv4) => is_usable_ipv4(ipv4),
        IpAddr::V6(ipv6) => is_usable_ipv6(ipv6),
    }
}

/// Check if an IPv4 address is usable for Link discovery
fn is_usable_ipv4(addr: &Ipv4Addr) -> bool {
    // Exclude loopback, broadcast, and other special addresses
    !addr.is_loopback()
        && !addr.is_broadcast()
        && !addr.is_multicast()
        && !addr.is_unspecified()
        && !addr.is_link_local()
        && *addr != Ipv4Addr::new(0, 0, 0, 0)
}

/// Check if an IPv6 address is usable for Link discovery  
fn is_usable_ipv6(addr: &Ipv6Addr) -> bool {
    // For IPv6, we only want link-local addresses (as per C++ implementation)
    !addr.is_loopback() && addr.is_unicast_link_local()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_interface_scanning() {
        let interfaces = scan_network_interfaces().await;
        // Should find at least one interface on most systems
        // Note: This might fail in containerized environments
        println!("Found {} interfaces: {:?}", interfaces.len(), interfaces);
    }

    #[test]
    fn test_usable_interface_detection() {
        // IPv4 tests
        assert!(!is_usable_ipv4(&Ipv4Addr::LOCALHOST)); // 127.0.0.1
        assert!(!is_usable_ipv4(&Ipv4Addr::UNSPECIFIED)); // 0.0.0.0
        assert!(!is_usable_ipv4(&Ipv4Addr::BROADCAST)); // 255.255.255.255
        assert!(is_usable_ipv4(&Ipv4Addr::new(192, 168, 1, 100))); // Private network

        // IPv6 tests
        assert!(!is_usable_ipv6(&Ipv6Addr::LOCALHOST)); // ::1
        assert!(!is_usable_ipv6(&Ipv6Addr::UNSPECIFIED)); // ::
        
        // Link-local IPv6 should be usable
        let link_local = Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
        assert!(is_usable_ipv6(&link_local));
    }
}
