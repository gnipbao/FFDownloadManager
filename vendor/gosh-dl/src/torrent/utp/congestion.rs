//! LEDBAT Congestion Control (BEP 29)
//!
//! This module implements the Low Extra Delay Background Transport (LEDBAT)
//! congestion control algorithm used by uTP. LEDBAT is designed to yield
//! to other traffic by maintaining low queuing delay.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Target delay in microseconds (100ms as per BEP 29)
pub const TARGET_DELAY_US: u32 = 100_000;

/// Maximum congestion window size (1MB)
pub const MAX_CWND: u32 = 1_000_000;

/// Minimum congestion window size (150 bytes - one minimum packet)
pub const MIN_CWND: u32 = 150;

/// Initial congestion window size (2 * MSS)
pub const INITIAL_CWND: u32 = 3000;

/// Maximum Segment Size (approximate)
pub const MSS: u32 = 1400;

/// Base delay history window duration (2 minutes)
const BASE_DELAY_HISTORY_DURATION: Duration = Duration::from_secs(120);

/// Number of base delay samples to keep
const BASE_DELAY_HISTORY_SIZE: usize = 13;

/// LEDBAT gain parameter (1/TARGET_DELAY per BEP 29)
const GAIN: f64 = 1.0;

/// Window used to track delay samples
#[derive(Debug, Clone)]
struct DelayHistory {
    /// Circular buffer of (timestamp, delay) samples
    samples: VecDeque<(Instant, u32)>,
    /// Maximum size
    max_size: usize,
    /// Duration to keep samples
    window_duration: Duration,
}

impl DelayHistory {
    fn new(max_size: usize, window_duration: Duration) -> Self {
        Self {
            samples: VecDeque::with_capacity(max_size),
            max_size,
            window_duration,
        }
    }

    fn add_sample(&mut self, now: Instant, delay_us: u32) {
        // Remove old samples
        let cutoff = now - self.window_duration;
        while let Some(&(ts, _)) = self.samples.front() {
            if ts < cutoff {
                self.samples.pop_front();
            } else {
                break;
            }
        }

        // Add new sample
        if self.samples.len() >= self.max_size {
            self.samples.pop_front();
        }
        self.samples.push_back((now, delay_us));
    }

    fn min(&self) -> Option<u32> {
        self.samples.iter().map(|(_, d)| *d).min()
    }
}

/// LEDBAT congestion controller state
#[derive(Debug)]
pub struct LedbatController {
    /// Current congestion window (bytes)
    cwnd: u32,

    /// Slow start threshold
    ssthresh: u32,

    /// Base delay history (minimum delays over time windows)
    base_delay_history: DelayHistory,

    /// Current delay filter (most recent delay samples)
    current_delay_filter: VecDeque<u32>,

    /// Number of bytes in flight (sent but not acknowledged)
    bytes_in_flight: u32,

    /// RTT estimate (microseconds)
    rtt_us: u32,

    /// RTT variance (microseconds)
    rtt_var_us: u32,

    /// Retransmission timeout (microseconds)
    rto_us: u32,

    /// Whether in slow start phase
    in_slow_start: bool,

    /// Last time we got an ACK
    last_ack_time: Option<Instant>,

    /// Configured window ceiling (defaults to MAX_CWND)
    max_cwnd: u32,

    /// Configured LEDBAT target delay (defaults to TARGET_DELAY_US)
    target_delay_us: u32,
}

impl Default for LedbatController {
    fn default() -> Self {
        Self::new()
    }
}

impl LedbatController {
    /// Create a new LEDBAT controller
    pub fn new() -> Self {
        Self {
            cwnd: INITIAL_CWND,
            ssthresh: MAX_CWND,
            base_delay_history: DelayHistory::new(
                BASE_DELAY_HISTORY_SIZE,
                BASE_DELAY_HISTORY_DURATION,
            ),
            current_delay_filter: VecDeque::with_capacity(8),
            bytes_in_flight: 0,
            rtt_us: 1_000_000, // Initial 1 second
            rtt_var_us: 0,
            rto_us: 3_000_000, // Initial 3 seconds
            in_slow_start: true,
            last_ack_time: None,
            max_cwnd: MAX_CWND,
            target_delay_us: TARGET_DELAY_US,
        }
    }

    /// Configure the window ceiling and LEDBAT target delay (from UtpConfig)
    pub fn set_params(&mut self, max_cwnd: u32, target_delay_us: u32) {
        self.max_cwnd = max_cwnd.max(MIN_CWND);
        self.target_delay_us = target_delay_us.max(1_000);
        self.cwnd = self.cwnd.min(self.max_cwnd);
    }

    /// Get current congestion window
    pub fn cwnd(&self) -> u32 {
        self.cwnd
    }

    /// Get bytes in flight
    pub fn bytes_in_flight(&self) -> u32 {
        self.bytes_in_flight
    }

    /// Get available window (how many bytes we can send)
    pub fn available_window(&self) -> u32 {
        self.cwnd.saturating_sub(self.bytes_in_flight)
    }

    /// Check if we can send more data
    pub fn can_send(&self) -> bool {
        self.bytes_in_flight < self.cwnd
    }

    /// Get RTT estimate in microseconds
    pub fn rtt_us(&self) -> u32 {
        self.rtt_us
    }

    /// Get retransmission timeout in microseconds
    pub fn rto_us(&self) -> u32 {
        self.rto_us
    }

    /// Get RTO as Duration
    pub fn rto(&self) -> Duration {
        Duration::from_micros(self.rto_us as u64)
    }

    /// Record that bytes were sent
    pub fn on_send(&mut self, bytes: u32) {
        self.bytes_in_flight += bytes;
    }

    /// Process an acknowledgment
    ///
    /// # Arguments
    /// * `bytes_acked` - Number of bytes acknowledged
    /// * `delay_us` - One-way delay sample in microseconds (from timestamp_diff)
    /// * `rtt_us` - Round-trip time sample in microseconds
    pub fn on_ack(&mut self, bytes_acked: u32, delay_us: u32, rtt_us: Option<u32>) {
        let now = Instant::now();
        self.last_ack_time = Some(now);
        self.bytes_in_flight = self.bytes_in_flight.saturating_sub(bytes_acked);

        // Update RTT estimate
        if let Some(sample_rtt) = rtt_us {
            self.update_rtt(sample_rtt);
        }

        // Update delay estimates
        self.base_delay_history.add_sample(now, delay_us);

        // Current delay filter - keep last few samples
        if self.current_delay_filter.len() >= 8 {
            self.current_delay_filter.pop_front();
        }
        self.current_delay_filter.push_back(delay_us);

        // Calculate base delay (minimum observed over long window)
        let base_delay = self.base_delay_history.min().unwrap_or(delay_us);

        // Current delay is the most recent sample
        // (could also use exponential moving average)
        let current_delay = delay_us;

        // Calculate queuing delay (how much delay is due to queuing vs propagation)
        let queuing_delay = current_delay.saturating_sub(base_delay);

        // LEDBAT window adjustment
        self.adjust_window(bytes_acked, queuing_delay);
    }

    /// Adjust congestion window based on LEDBAT algorithm
    fn adjust_window(&mut self, bytes_acked: u32, queuing_delay_us: u32) {
        if self.in_slow_start {
            // Slow start: exponential increase
            if queuing_delay_us < self.target_delay_us {
                self.cwnd += bytes_acked;
            } else {
                // Exit slow start
                self.in_slow_start = false;
                self.ssthresh = self.cwnd;
            }
        } else {
            // LEDBAT congestion avoidance
            // off_target = (TARGET_DELAY - queuing_delay) / TARGET_DELAY
            // cwnd += GAIN * off_target * bytes_acked * MSS / cwnd

            let target = self.target_delay_us;
            let off_target = if queuing_delay_us < target {
                (target - queuing_delay_us) as f64 / target as f64
            } else {
                -((queuing_delay_us - target) as f64 / target as f64)
            };

            let cwnd_delta =
                (GAIN * off_target * bytes_acked as f64 * MSS as f64 / self.cwnd as f64) as i32;

            if cwnd_delta >= 0 {
                self.cwnd = self.cwnd.saturating_add(cwnd_delta as u32);
            } else {
                self.cwnd = self.cwnd.saturating_sub((-cwnd_delta) as u32);
            }
        }

        // Clamp window
        self.cwnd = self.cwnd.clamp(MIN_CWND, self.max_cwnd);
    }

    /// Update RTT estimate using Jacobson/Karels algorithm
    fn update_rtt(&mut self, sample_rtt_us: u32) {
        if self.rtt_us == 1_000_000 {
            // First sample
            self.rtt_us = sample_rtt_us;
            self.rtt_var_us = sample_rtt_us / 2;
        } else {
            // SRTT = (1 - alpha) * SRTT + alpha * sample
            // RTTVAR = (1 - beta) * RTTVAR + beta * |SRTT - sample|
            // alpha = 1/8, beta = 1/4
            let diff = sample_rtt_us.abs_diff(self.rtt_us);

            self.rtt_var_us = self.rtt_var_us * 3 / 4 + diff / 4;
            self.rtt_us = self.rtt_us * 7 / 8 + sample_rtt_us / 8;
        }

        // RTO = SRTT + 4 * RTTVAR (minimum 500ms)
        self.rto_us = (self.rtt_us + 4 * self.rtt_var_us).max(500_000);
    }

    /// Handle packet loss (timeout or triple duplicate ACK)
    pub fn on_loss(&mut self) {
        // Multiplicative decrease
        self.ssthresh = (self.cwnd / 2).max(MIN_CWND);
        self.cwnd = MIN_CWND;
        self.in_slow_start = false;
    }

    /// Handle retransmission timeout
    pub fn on_timeout(&mut self) {
        self.on_loss();
        // Double RTO on timeout (exponential backoff)
        self.rto_us = (self.rto_us * 2).min(60_000_000); // Max 60 seconds
    }

    /// Reset for new connection
    pub fn reset(&mut self) {
        *self = Self::new();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_initial_state() {
        let ctrl = LedbatController::new();
        assert_eq!(ctrl.cwnd(), INITIAL_CWND);
        assert!(ctrl.can_send());
        assert!(ctrl.in_slow_start);
    }

    #[test]
    fn test_slow_start() {
        let mut ctrl = LedbatController::new();

        // Low delay - should increase window
        ctrl.on_ack(1000, 10_000, Some(50_000)); // 10ms delay, 50ms RTT
        assert!(ctrl.cwnd() > INITIAL_CWND);
        assert!(ctrl.in_slow_start);
    }

    #[test]
    fn test_exit_slow_start() {
        let mut ctrl = LedbatController::new();

        // First establish a base delay with low-delay sample
        ctrl.on_ack(1000, 10_000, Some(50_000)); // 10ms delay
        assert!(ctrl.in_slow_start); // Should still be in slow start

        // Now send high delay - queuing delay = 110ms - 10ms = 100ms = TARGET_DELAY
        // Plus a bit more to exceed threshold
        ctrl.on_ack(1000, TARGET_DELAY_US + 20_000, Some(200_000)); // 120ms delay
        assert!(!ctrl.in_slow_start);
    }

    #[test]
    fn test_on_loss() {
        let mut ctrl = LedbatController::new();
        ctrl.cwnd = 100_000;

        ctrl.on_loss();

        assert_eq!(ctrl.ssthresh, 50_000);
        assert_eq!(ctrl.cwnd, MIN_CWND);
        assert!(!ctrl.in_slow_start);
    }

    #[test]
    fn test_window_bounds() {
        let mut ctrl = LedbatController::new();
        ctrl.cwnd = MAX_CWND;

        // Try to increase beyond max
        ctrl.on_ack(10_000, 1000, Some(10_000));
        assert!(ctrl.cwnd() <= MAX_CWND);

        // Loss should not go below min
        ctrl.on_loss();
        assert!(ctrl.cwnd() >= MIN_CWND);
    }

    #[test]
    fn test_rtt_update() {
        let mut ctrl = LedbatController::new();

        ctrl.update_rtt(100_000); // First sample 100ms
        assert_eq!(ctrl.rtt_us, 100_000);

        ctrl.update_rtt(120_000); // Second sample 120ms
                                  // Should be somewhere between 100 and 120ms
        assert!(ctrl.rtt_us > 100_000 && ctrl.rtt_us < 120_000);
    }
}
