//! Pure link telemetry calculators: RFC 3550 jitter estimator and bounded throughput series.

use crate::relay::ClientStats;
use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const DEFAULT_HISTORY_CAP: usize = 60;

/// Stateful RFC 3550 smoothed network jitter estimator (`J = J + (|D| - J) / 16`).
#[derive(Debug, Default, Clone)]
pub struct JitterEstimator {
    prev_rtt_ms: Option<u64>,
    jitter_ms: f64,
}

impl JitterEstimator {
    /// Observes a new RTT sample (in ms) and returns smoothed jitter in tenths of a millisecond
    /// (`18` represents `±1.8 ms`).
    pub fn observe_rtt_ms(&mut self, rtt_ms: u64) -> u32 {
        if let Some(prev) = self.prev_rtt_ms {
            let delta = rtt_ms.abs_diff(prev) as f64;
            if self.jitter_ms == 0.0 {
                self.jitter_ms = delta;
            } else {
                self.jitter_ms += (delta - self.jitter_ms) / 16.0;
            }
        }
        self.prev_rtt_ms = Some(rtt_ms);
        (self.jitter_ms * 10.0).round().clamp(0.0, 99_990.0) as u32
    }
}

/// Bounded 60-second throughput & RTT ring buffer decouples byte-delta sampling math from UI views.
#[derive(Debug)]
pub struct TelemetrySeries {
    last_sample_at: Instant,
    last_tx_bytes: u64,
    last_rx_bytes: u64,
    pub tx_speed_bps: f64,
    pub rx_speed_bps: f64,
    pub rtt_history: VecDeque<u64>,
    /// Optional domestic (secondary) node RTT history in `Dual` mode (`None` when offline/inactive).
    pub secondary_rtt_history: VecDeque<Option<u64>>,
    pub speed_history: VecDeque<(f64, f64)>,
}

impl Default for TelemetrySeries {
    fn default() -> Self {
        Self::new()
    }
}

impl TelemetrySeries {
    pub fn new() -> Self {
        Self {
            last_sample_at: Instant::now(),
            last_tx_bytes: 0,
            last_rx_bytes: 0,
            tx_speed_bps: 0.0,
            rx_speed_bps: 0.0,
            rtt_history: VecDeque::with_capacity(DEFAULT_HISTORY_CAP),
            secondary_rtt_history: VecDeque::with_capacity(DEFAULT_HISTORY_CAP),
            speed_history: VecDeque::with_capacity(DEFAULT_HISTORY_CAP),
        }
    }

    pub fn reset(&mut self) {
        self.last_sample_at = Instant::now();
        self.last_tx_bytes = 0;
        self.last_rx_bytes = 0;
        self.tx_speed_bps = 0.0;
        self.rx_speed_bps = 0.0;
        self.rtt_history.clear();
        self.secondary_rtt_history.clear();
        self.speed_history.clear();
    }

    pub fn clear_instant_rates(&mut self) {
        self.tx_speed_bps = 0.0;
        self.rx_speed_bps = 0.0;
    }

    /// Samples `stats` if at least 1 second has elapsed since the previous sample.
    pub fn tick_if_due(&mut self, stats: &ClientStats, is_connected: bool) {
        let elapsed = self.last_sample_at.elapsed();
        if elapsed < Duration::from_secs(1) {
            return;
        }
        let secs = elapsed.as_secs_f64();
        let cur_tx = stats.tx_bytes.load(Ordering::Relaxed);
        let cur_rx = stats.rx_bytes.load(Ordering::Relaxed);
        if is_connected {
            self.tx_speed_bps = cur_tx.saturating_sub(self.last_tx_bytes) as f64 / secs;
            self.rx_speed_bps = cur_rx.saturating_sub(self.last_rx_bytes) as f64 / secs;
            let rtt = stats.rtt_ms.load(Ordering::Relaxed);
            if self.rtt_history.len() >= DEFAULT_HISTORY_CAP {
                self.rtt_history.pop_front();
            }
            self.rtt_history.push_back(rtt);
            let sec_rtt = stats
                .secondary_connected
                .load(Ordering::Relaxed)
                .then(|| stats.secondary_rtt_ms.load(Ordering::Relaxed))
                .filter(|&v| v > 0);
            if self.secondary_rtt_history.len() >= DEFAULT_HISTORY_CAP {
                self.secondary_rtt_history.pop_front();
            }
            self.secondary_rtt_history.push_back(sec_rtt);
            if self.speed_history.len() >= DEFAULT_HISTORY_CAP {
                self.speed_history.pop_front();
            }
            self.speed_history
                .push_back((self.rx_speed_bps, self.tx_speed_bps));
        } else {
            self.tx_speed_bps = 0.0;
            self.rx_speed_bps = 0.0;
        }
        self.last_tx_bytes = cur_tx;
        self.last_rx_bytes = cur_rx;
        self.last_sample_at = Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_estimator_tracks_rfc3550_smoothing() {
        let mut est = JitterEstimator::default();
        assert_eq!(est.observe_rtt_ms(30), 0);
        // First delta = |34 - 30| = 4.0 ms => 40 tenths
        assert_eq!(est.observe_rtt_ms(34), 40);
        // Steady RTT decays jitter smoothly toward zero
        let decayed = est.observe_rtt_ms(34);
        assert!(decayed < 40 && decayed > 30);
    }
}
