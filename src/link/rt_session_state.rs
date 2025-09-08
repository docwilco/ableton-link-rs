use chrono::Duration;
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use triple_buffer::{Input, Output, TripleBuffer};

use super::{
    state::{ClientStartStopState, ClientState},
    timeline::Timeline,
};
use crate::link::IncomingClientState;

/// Real-time client state for lock-free access from audio threads
#[derive(Clone, Debug, Default)]
pub struct RtClientState {
    pub timeline: Timeline,
    pub start_stop_state: ClientStartStopState,
    pub timeline_timestamp: Duration,
    pub start_stop_state_timestamp: Duration,
}

impl From<ClientState> for RtClientState {
    fn from(client_state: ClientState) -> Self {
        Self {
            timeline: client_state.timeline,
            start_stop_state: client_state.start_stop_state,
            timeline_timestamp: Duration::zero(),
            start_stop_state_timestamp: Duration::zero(),
        }
    }
}

impl From<RtClientState> for ClientState {
    fn from(rt_state: RtClientState) -> Self {
        Self {
            timeline: rt_state.timeline,
            start_stop_state: rt_state.start_stop_state,
        }
    }
}

/// Real-time safe session state handler
/// This provides lock-free access to session state for real-time audio threads
/// while allowing safe updates from other threads.
///
/// Based on the C++ Link implementation's real-time safety mechanisms using
/// triple buffers and atomic operations to ensure no blocking in audio threads.
pub struct RtSessionStateHandler {
    /// Triple buffer for sharing client state between RT and non-RT threads
    client_state_input: UnsafeCell<Input<ClientState>>,
    client_state_output: UnsafeCell<Output<ClientState>>,

    /// Cached RT client state for immediate access
    cached_rt_state: UnsafeCell<RtClientState>,

    /// Atomic timestamps for grace period management (in microseconds)
    timeline_timestamp: AtomicU64,
    start_stop_timestamp: AtomicU64,

    /// Flag indicating pending updates
    has_pending_updates: AtomicBool,

    /// Grace period for local modifications (in microseconds)
    local_mod_grace_period_micros: u64,
}

unsafe impl Send for RtSessionStateHandler {}
unsafe impl Sync for RtSessionStateHandler {}

impl RtSessionStateHandler {
    pub fn new(initial_state: ClientState, grace_period: Duration) -> Self {
        let grace_period_micros = grace_period.num_microseconds().unwrap_or(1000000) as u64; // Default 1 second

        // Create triple buffer for client state
        let (input, output) = TripleBuffer::new(&initial_state).split();

        Self {
            client_state_input: UnsafeCell::new(input),
            client_state_output: UnsafeCell::new(output),
            cached_rt_state: UnsafeCell::new(initial_state.into()),
            timeline_timestamp: AtomicU64::new(0),
            start_stop_timestamp: AtomicU64::new(0),
            has_pending_updates: AtomicBool::new(false),
            local_mod_grace_period_micros: grace_period_micros,
        }
    }

    /// Get the current real-time client state
    /// This is lock-free and safe to call from audio threads
    pub fn get_rt_client_state(&self, current_time: Duration) -> ClientState {
        let current_time_micros = current_time.num_microseconds().unwrap_or(0) as u64;

        // Check if grace period has expired for timeline
        let timeline_grace_expired = current_time_micros
            .saturating_sub(self.timeline_timestamp.load(Ordering::Acquire))
            > self.local_mod_grace_period_micros;

        // Check if grace period has expired for start/stop state
        let start_stop_grace_expired = current_time_micros
            .saturating_sub(self.start_stop_timestamp.load(Ordering::Acquire))
            > self.local_mod_grace_period_micros;

        // Try to get new state if grace periods have expired and there are updates
        if timeline_grace_expired || start_stop_grace_expired {
            // Check for new data from the triple buffer (RT-safe read)
            let output = unsafe { &mut *self.client_state_output.get() };

            // Check if there's new data available
            if output.updated() {
                let new_state = output.read().clone();

                // Update the cached state
                let mut updated_rt_state = unsafe { (*self.cached_rt_state.get()).clone() };

                if timeline_grace_expired && new_state.timeline != updated_rt_state.timeline {
                    updated_rt_state.timeline = new_state.timeline.clone();
                }

                if start_stop_grace_expired
                    && new_state.start_stop_state != updated_rt_state.start_stop_state
                {
                    updated_rt_state.start_stop_state = new_state.start_stop_state.clone();
                }

                unsafe { *self.cached_rt_state.get() = updated_rt_state };

                return new_state;
            }
        }

        // Return current cached state
        unsafe { (*self.cached_rt_state.get()).clone().into() }
    }

    /// Update the real-time client state with new data
    /// This should only be called from the real-time thread
    pub fn update_rt_client_state(
        &self,
        incoming_state: IncomingClientState,
        current_time: Duration,
        is_enabled: bool,
    ) {
        if incoming_state.timeline.is_none() && incoming_state.start_stop_state.is_none() {
            return;
        }

        let current_time_micros = current_time.num_microseconds().unwrap_or(0) as u64;
        let timestamp_to_store = if is_enabled { current_time_micros } else { 0 };

        // Update cached state
        let mut updated_rt_state = unsafe { (*self.cached_rt_state.get()).clone() };

        // Update timeline if provided
        if let Some(timeline) = incoming_state.timeline {
            updated_rt_state.timeline = timeline;
            updated_rt_state.timeline_timestamp = current_time;
            self.timeline_timestamp
                .store(timestamp_to_store, Ordering::Release);
        }

        // Update start/stop state if provided
        if let Some(start_stop_state) = incoming_state.start_stop_state {
            updated_rt_state.start_stop_state = start_stop_state;
            updated_rt_state.start_stop_state_timestamp = current_time;
            self.start_stop_timestamp
                .store(timestamp_to_store, Ordering::Release);
        }

        // Update cached state
        unsafe { *self.cached_rt_state.get() = updated_rt_state.clone() };

        // Send updated state to non-RT thread via triple buffer
        let input = unsafe { &mut *self.client_state_input.get() };
        input.write(updated_rt_state.into());

        // Mark that we have pending updates
        self.has_pending_updates.store(true, Ordering::Release);
    }

    /// Process any pending updates from the triple buffers
    /// This should be called periodically from a non-realtime thread
    pub fn process_pending_updates(&self) -> Option<IncomingClientState> {
        if !self.has_pending_updates.load(Ordering::Acquire) {
            return None;
        }

        // Check for new state data from triple buffer
        let output = unsafe { &mut *self.client_state_output.get() };

        if output.updated() {
            let new_state = output.read().clone();

            let result = IncomingClientState {
                timeline: Some(new_state.timeline.clone()),
                start_stop_state: Some(new_state.start_stop_state.clone()),
                timeline_timestamp: Duration::microseconds(
                    self.timeline_timestamp.load(Ordering::Acquire) as i64,
                ),
            };

            // Clear the pending flag
            self.has_pending_updates.store(false, Ordering::Release);
            Some(result)
        } else {
            None
        }
    }

    /// Check if there are pending updates waiting to be processed
    pub fn has_pending_updates(&self) -> bool {
        self.has_pending_updates.load(Ordering::Acquire)
    }

    /// Get the current grace period
    pub fn grace_period(&self) -> Duration {
        Duration::microseconds(self.local_mod_grace_period_micros as i64)
    }

    /// Set a new grace period for local modifications
    pub fn set_grace_period(&mut self, grace_period: Duration) {
        self.local_mod_grace_period_micros =
            grace_period.num_microseconds().unwrap_or(1000000) as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::{beats::Beats, tempo::Tempo};

    fn create_test_client_state() -> ClientState {
        ClientState {
            timeline: Timeline {
                tempo: Tempo::new(120.0),
                beat_origin: Beats::new(0.0),
                time_origin: Duration::zero(),
            },
            start_stop_state: ClientStartStopState {
                is_playing: false,
                time: Duration::zero(),
                timestamp: Duration::zero(),
            },
        }
    }

    #[test]
    fn test_rt_session_state_handler_creation() {
        let client_state = create_test_client_state();
        let handler = RtSessionStateHandler::new(client_state, Duration::milliseconds(1000));

        assert_eq!(handler.grace_period(), Duration::milliseconds(1000));
        assert!(!handler.has_pending_updates());
    }

    #[test]
    fn test_rt_session_state_handler_get_state() {
        let client_state = create_test_client_state();
        let handler = RtSessionStateHandler::new(client_state, Duration::milliseconds(1000));

        let current_time = Duration::milliseconds(500);
        let retrieved_state = handler.get_rt_client_state(current_time);

        assert_eq!(
            retrieved_state.timeline.tempo.bpm(),
            client_state.timeline.tempo.bpm()
        );
        assert_eq!(
            retrieved_state.start_stop_state.is_playing,
            client_state.start_stop_state.is_playing
        );
    }

    #[test]
    fn test_triple_buffer_basic_ops() {
        use triple_buffer::TripleBuffer;

        let (mut input, mut output) = TripleBuffer::new(&42i32).split();

        // Initial read should return the initial value since data is always available
        assert_eq!(*output.read(), 42);
        assert!(!output.updated()); // No new data has been written yet

        // Write a new value
        input.write(100);

        // Should indicate that new data is available
        assert!(output.updated());

        // Read should return the new value
        assert_eq!(*output.read(), 100);

        // After reading, updated should still be true until the next write
        // (Note: this depends on the specific implementation behavior)
    }

    #[test]
    fn test_rt_session_state_handler_updates() {
        let client_state = create_test_client_state();
        let handler = RtSessionStateHandler::new(client_state, Duration::milliseconds(1000));

        // Create an incoming state update
        let new_timeline = Timeline {
            tempo: Tempo::new(140.0),
            beat_origin: Beats::new(1.0),
            time_origin: Duration::milliseconds(1000),
        };

        let incoming_state = IncomingClientState {
            timeline: Some(new_timeline),
            start_stop_state: None,
            timeline_timestamp: Duration::milliseconds(2000),
        };

        // Update the real-time state
        handler.update_rt_client_state(incoming_state, Duration::milliseconds(3000), true);

        // Should have pending updates
        assert!(handler.has_pending_updates());

        // Process pending updates
        let processed = handler.process_pending_updates();
        assert!(processed.is_some());

        let processed_state = processed.unwrap();
        assert!(processed_state.timeline.is_some());
        assert_eq!(processed_state.timeline.unwrap().tempo.bpm(), 140.0);
    }

    #[test]
    fn test_rt_session_state_handler_grace_period() {
        let client_state = create_test_client_state();
        let mut handler = RtSessionStateHandler::new(client_state, Duration::milliseconds(1000));

        // Change grace period
        handler.set_grace_period(Duration::milliseconds(2000));
        assert_eq!(handler.grace_period(), Duration::milliseconds(2000));
    }
}
