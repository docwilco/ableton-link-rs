// Platform-specific optimizations and implementations
// Based on the C++ implementation's platform abstractions

pub mod clock;
pub mod network;
pub mod thread;

// Re-export platform-specific types based on current platform
#[cfg(target_os = "macos")]
pub use clock::DarwinClock as PlatformClock;

#[cfg(target_os = "linux")]
pub use clock::LinuxClock as PlatformClock;

#[cfg(target_os = "windows")]
pub use clock::WindowsClock as PlatformClock;

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub use clock::GenericClock as PlatformClock;

// Thread factory
pub use thread::ThreadFactory;

// Network interface scanner
pub use network::scan_network_interfaces;
