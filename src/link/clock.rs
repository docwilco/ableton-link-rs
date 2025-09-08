use chrono::Duration;
use crate::platform::clock::OptimizedClock;

#[derive(Copy, Clone, Debug)]
pub struct Clock {
    inner: OptimizedClock,
}

impl Default for Clock {
    fn default() -> Self {
        Self {
            inner: OptimizedClock::new(),
        }
    }
}

impl Clock {
    /// Create a new clock with platform-specific optimizations
    pub fn new() -> Self {
        Self::default()
    }

    /// Get current time in microseconds since clock initialization
    /// Uses platform-specific high-resolution timers for maximum precision
    pub fn micros(&self) -> Duration {
        self.inner.micros()
    }

    /// Get raw platform-specific ticks
    pub fn ticks(&self) -> u64 {
        self.inner.ticks()
    }

    /// Convert ticks to microseconds using platform-specific conversion
    pub fn ticks_to_micros(&self, ticks: u64) -> Duration {
        self.inner.ticks_to_micros(ticks)
    }

    /// Convert microseconds to ticks using platform-specific conversion
    pub fn micros_to_ticks(&self, micros: Duration) -> u64 {
        self.inner.micros_to_ticks(micros)
    }
}
