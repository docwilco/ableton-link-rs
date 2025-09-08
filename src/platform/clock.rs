// Platform-specific high-resolution clock implementations
// Based on Ableton Link's clock optimizations

use chrono::Duration;

pub trait ClockTrait {
    /// Get current time in microseconds since some reference point
    fn micros(&self) -> Duration;
    
    /// Get raw ticks (platform-specific)
    fn ticks(&self) -> u64;
    
    /// Convert ticks to microseconds
    fn ticks_to_micros(&self, ticks: u64) -> Duration;
    
    /// Convert microseconds to ticks
    fn micros_to_ticks(&self, micros: Duration) -> u64;
}

// macOS/iOS optimized implementation using mach_absolute_time
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug)]
pub struct DarwinClock {
    ticks_to_micros: f64,
}

#[cfg(target_os = "macos")]
impl DarwinClock {
    pub fn new() -> Self {
        unsafe {
            let mut time_info = std::mem::zeroed::<mach2::mach_time::mach_timebase_info_data_t>();
            mach2::mach_time::mach_timebase_info(&mut time_info);
            // numer / denom gives nanoseconds, we want microseconds
            let ticks_to_micros = (time_info.numer as f64) / (time_info.denom as f64 * 1000.0);
            Self { ticks_to_micros }
        }
    }
}

#[cfg(target_os = "macos")]
impl Default for DarwinClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "macos")]
impl ClockTrait for DarwinClock {
    fn micros(&self) -> Duration {
        self.ticks_to_micros(self.ticks())
    }

    fn ticks(&self) -> u64 {
        unsafe { mach2::mach_time::mach_absolute_time() }
    }

    fn ticks_to_micros(&self, ticks: u64) -> Duration {
        let micros = (self.ticks_to_micros * ticks as f64).round() as i64;
        Duration::microseconds(micros)
    }

    fn micros_to_ticks(&self, micros: Duration) -> u64 {
        let ticks = (micros.num_microseconds().unwrap_or(0) as f64) / self.ticks_to_micros;
        ticks as u64
    }
}

// Linux optimized implementation using clock_gettime with CLOCK_MONOTONIC_RAW
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug)]
pub struct LinuxClock;

#[cfg(target_os = "linux")]
impl LinuxClock {
    pub fn new() -> Self {
        Self
    }
}

#[cfg(target_os = "linux")]
impl Default for LinuxClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "linux")]
impl ClockTrait for LinuxClock {
    fn micros(&self) -> Duration {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        
        unsafe {
            // Use CLOCK_MONOTONIC_RAW for best performance, fallback to CLOCK_MONOTONIC
            let clock_id = if cfg!(target_os = "linux") {
                libc::CLOCK_MONOTONIC_RAW
            } else {
                libc::CLOCK_MONOTONIC
            };
            
            libc::clock_gettime(clock_id, &mut ts);
        }
        
        let total_ns = ts.tv_sec as u64 * 1_000_000_000u64 + ts.tv_nsec as u64;
        let micros = total_ns / 1000;
        Duration::microseconds(micros as i64)
    }

    fn ticks(&self) -> u64 {
        self.micros().num_microseconds().unwrap_or(0) as u64
    }

    fn ticks_to_micros(&self, ticks: u64) -> Duration {
        Duration::microseconds(ticks as i64)
    }

    fn micros_to_ticks(&self, micros: Duration) -> u64 {
        micros.num_microseconds().unwrap_or(0) as u64
    }
}

// Windows optimized implementation using QueryPerformanceCounter
#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug)]
pub struct WindowsClock {
    ticks_to_micros: f64,
}

#[cfg(target_os = "windows")]
impl WindowsClock {
    pub fn new() -> Self {
        unsafe {
            let mut frequency = std::mem::zeroed::<winapi::shared::ntdef::LARGE_INTEGER>();
            winapi::um::profileapi::QueryPerformanceFrequency(&mut frequency);
            let ticks_to_micros = 1_000_000.0 / (*frequency.QuadPart()) as f64;
            Self { ticks_to_micros }
        }
    }
}

#[cfg(target_os = "windows")]
impl Default for WindowsClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "windows")]
impl ClockTrait for WindowsClock {
    fn micros(&self) -> Duration {
        self.ticks_to_micros(self.ticks())
    }

    fn ticks(&self) -> u64 {
        unsafe {
            let mut counter = std::mem::zeroed::<winapi::shared::ntdef::LARGE_INTEGER>();
            winapi::um::profileapi::QueryPerformanceCounter(&mut counter);
            *counter.QuadPart() as u64
        }
    }

    fn ticks_to_micros(&self, ticks: u64) -> Duration {
        let micros = (self.ticks_to_micros * ticks as f64).round() as i64;
        Duration::microseconds(micros)
    }

    fn micros_to_ticks(&self, micros: Duration) -> u64 {
        let ticks = (micros.num_microseconds().unwrap_or(0) as f64) / self.ticks_to_micros;
        ticks as u64
    }
}

// Generic fallback implementation for unsupported platforms
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
#[derive(Clone, Copy, Debug)]
pub struct GenericClock {
    // Store start time as timestamp to make it Copy
    start_micros: i64,
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
impl GenericClock {
    pub fn new() -> Self {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now();
        let start_micros = now.duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as i64;
        
        Self { start_micros }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
impl Default for GenericClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
impl ClockTrait for GenericClock {
    fn micros(&self) -> Duration {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now();
        let current_micros = now.duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as i64;
        
        let elapsed_micros = current_micros - self.start_micros;
        Duration::microseconds(elapsed_micros.max(0))
    }

    fn ticks(&self) -> u64 {
        self.micros().num_microseconds().unwrap_or(0) as u64
    }

    fn ticks_to_micros(&self, ticks: u64) -> Duration {
        Duration::microseconds(ticks as i64)
    }

    fn micros_to_ticks(&self, micros: Duration) -> u64 {
        micros.num_microseconds().unwrap_or(0) as u64
    }
}

// High-level Clock type that automatically selects the best implementation
#[derive(Clone, Copy, Debug)]
pub struct OptimizedClock {
    #[cfg(target_os = "macos")]
    inner: DarwinClock,
    #[cfg(target_os = "linux")]
    inner: LinuxClock,
    #[cfg(target_os = "windows")]
    inner: WindowsClock,
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    inner: GenericClock,
}

impl OptimizedClock {
    pub fn new() -> Self {
        Self {
            #[cfg(target_os = "macos")]
            inner: DarwinClock::new(),
            #[cfg(target_os = "linux")]
            inner: LinuxClock::new(),
            #[cfg(target_os = "windows")]
            inner: WindowsClock::new(),
            #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
            inner: GenericClock::new(),
        }
    }

    pub fn micros(&self) -> Duration {
        self.inner.micros()
    }

    pub fn ticks(&self) -> u64 {
        self.inner.ticks()
    }

    pub fn ticks_to_micros(&self, ticks: u64) -> Duration {
        self.inner.ticks_to_micros(ticks)
    }

    pub fn micros_to_ticks(&self, micros: Duration) -> u64 {
        self.inner.micros_to_ticks(micros)
    }
}

impl Default for OptimizedClock {
    fn default() -> Self {
        Self::new()
    }
}
