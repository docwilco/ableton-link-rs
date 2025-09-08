// Platform-specific optimizations and implementations
// Based on the C++ implementation's platform abstractions
// Now using 100% safe Rust implementations!

pub mod clock;
pub mod network;
pub mod thread;

// Re-export the safe OptimizedClock for all platforms
pub use clock::OptimizedClock as PlatformClock;

// Also export the safe clock for direct usage
pub use clock::SafeClock;

// Thread factory
pub use thread::ThreadFactory;

// Network interface scanner
pub use network::scan_network_interfaces;
