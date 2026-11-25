//! Loss-tolerant, queue-guarded Brutal congestion controller for `quinn` (`ControllerFactory`).
//!
//! Designed specifically for the bulk TCP QUIC lane on high-RTT / policy-loss cross-border links:
//! - Maintains a 5-second sliding ACK/loss window to compensate for non-congestive policy packet loss
//!   without collapsing throughput like standard BBR/CUBIC.
//! - Applies a hard **rate cap** (`target_mbps`, clamped to `1..=1000` Mbps) and a 2.5x loss-compensation
//!   ceiling so it never floods the local router.
//! - Includes a **queue-protection guard**: when smoothed RTT rises sharply above recent `min_rtt`
//!   (`rtt > min_rtt * 2 + 40ms`, indicating real bottleneck queuing rather than random loss),
//!   throttles the effective rate down to `0.5x` so the co-resident game UDP lane's latency is protected.

use quinn::congestion::{Controller, ControllerFactory};
use quinn_proto::RttEstimator;
use std::any::Any;
use std::sync::Arc;
use std::time::{Duration, Instant};

const WINDOW_SLOTS: usize = 5;
const MIN_ACK_RATE: f64 = 0.40; // Caps loss compensation at 1 / 0.40 = 2.5x target rate
const MIN_CWND_PACKETS: u64 = 10;
const MAX_CWND_BYTES: u64 = 32 * 1024 * 1024;
const MIN_RTT_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const QUEUE_RTT_ABS_MARGIN: Duration = Duration::from_millis(40);
const QUEUE_THROTTLE_FACTOR: f64 = 0.50;

#[derive(Debug, Clone, Copy, Default)]
struct PktSlot {
    sec: u64,
    acked_bytes: u64,
    lost_bytes: u64,
}

/// Factory for creating per-connection Brutal congestion controllers.
#[derive(Debug, Clone)]
pub struct BrutalConfig {
    target_bps: u64,
}

impl BrutalConfig {
    /// Create a Brutal config capped at `mbps` megabits/sec (clamped to `1..=1000` Mbps).
    pub fn new(mbps: u32) -> Self {
        let clamped = mbps.clamp(1, 1000) as u64;
        Self {
            target_bps: clamped * 1_000_000 / 8,
        }
    }

    pub fn target_bps(&self) -> u64 {
        self.target_bps
    }
}

impl ControllerFactory for BrutalConfig {
    fn build(self: Arc<Self>, now: Instant, current_mtu: u16) -> Box<dyn Controller> {
        Box::new(BrutalController::new(self.target_bps, now, current_mtu))
    }
}

#[derive(Debug, Clone)]
struct BrutalController {
    target_bps: u64,
    mtu: u64,
    started_at: Instant,
    slots: [PktSlot; WINDOW_SLOTS],
    min_rtt: Option<Duration>,
    min_rtt_at: Instant,
    smoothed_rtt: Duration,
    cwnd: u64,
}

impl BrutalController {
    fn new(target_bps: u64, now: Instant, current_mtu: u16) -> Self {
        let mtu = (current_mtu as u64).max(1200);
        let init_rtt = Duration::from_millis(100);
        let init_cwnd = Self::compute_cwnd(target_bps, 1.0, false, init_rtt, mtu);
        Self {
            target_bps,
            mtu,
            started_at: now,
            slots: [PktSlot::default(); WINDOW_SLOTS],
            min_rtt: None,
            min_rtt_at: now,
            smoothed_rtt: init_rtt,
            cwnd: init_cwnd,
        }
    }

    fn slot_mut(&mut self, now: Instant) -> &mut PktSlot {
        let sec = now.saturating_duration_since(self.started_at).as_secs();
        let idx = (sec as usize) % WINDOW_SLOTS;
        let slot = &mut self.slots[idx];
        if slot.sec != sec {
            *slot = PktSlot {
                sec,
                acked_bytes: 0,
                lost_bytes: 0,
            };
        }
        slot
    }

    fn ack_rate(&self, now: Instant) -> f64 {
        let cur_sec = now.saturating_duration_since(self.started_at).as_secs();
        let mut acked: u64 = 0;
        let mut lost: u64 = 0;
        for slot in &self.slots {
            if slot.sec <= cur_sec && cur_sec.saturating_sub(slot.sec) < WINDOW_SLOTS as u64 {
                acked = acked.saturating_add(slot.acked_bytes);
                lost = lost.saturating_add(slot.lost_bytes);
            }
        }
        let total = acked.saturating_add(lost);
        if total < self.mtu * 4 {
            return 1.0;
        }
        (acked as f64 / total as f64).clamp(MIN_ACK_RATE, 1.0)
    }

    fn is_queuing(&self) -> bool {
        let Some(min_rtt) = self.min_rtt else {
            return false;
        };
        let threshold = min_rtt.saturating_mul(2).saturating_add(QUEUE_RTT_ABS_MARGIN);
        self.smoothed_rtt > threshold
    }

    fn compute_cwnd(
        target_bps: u64,
        ack_rate: f64,
        queuing: bool,
        rtt: Duration,
        mtu: u64,
    ) -> u64 {
        let mut effective_bps = (target_bps as f64) / ack_rate.clamp(MIN_ACK_RATE, 1.0);
        if queuing {
            effective_bps = (target_bps as f64) * QUEUE_THROTTLE_FACTOR;
        }
        let rtt_secs = rtt.as_secs_f64().clamp(0.005, 2.0);
        // 1.5x BDP keeps the pipe full under QUIC ACK aggregation while bounding queue depth
        let raw = (effective_bps * rtt_secs * 1.5) as u64;
        let min_cwnd = mtu.saturating_mul(MIN_CWND_PACKETS);
        raw.clamp(min_cwnd, MAX_CWND_BYTES)
    }

    fn refresh_cwnd(&mut self, now: Instant) {
        let ack_rate = self.ack_rate(now);
        let queuing = self.is_queuing();
        self.cwnd = Self::compute_cwnd(
            self.target_bps,
            ack_rate,
            queuing,
            self.smoothed_rtt,
            self.mtu,
        );
    }
}

impl Controller for BrutalController {
    fn on_ack(
        &mut self,
        now: Instant,
        _sent: Instant,
        bytes: u64,
        _app_limited: bool,
        rtt: &RttEstimator,
    ) {
        let sample = rtt.get();
        self.smoothed_rtt = sample;
        if self.min_rtt.is_none_or(|m| sample < m)
            || now.saturating_duration_since(self.min_rtt_at) > MIN_RTT_REFRESH_INTERVAL
        {
            self.min_rtt = Some(sample);
            self.min_rtt_at = now;
        }
        let slot = self.slot_mut(now);
        slot.acked_bytes = slot.acked_bytes.saturating_add(bytes);
        self.refresh_cwnd(now);
    }

    fn on_congestion_event(
        &mut self,
        now: Instant,
        _sent: Instant,
        is_persistent_congestion: bool,
        lost_bytes: u64,
    ) {
        let slot = self.slot_mut(now);
        slot.lost_bytes = slot.lost_bytes.saturating_add(lost_bytes);
        if is_persistent_congestion {
            // Persistent congestion (multiple PTOs): back off to minimum safe cwnd briefly
            self.cwnd = self.mtu.saturating_mul(MIN_CWND_PACKETS);
        } else {
            self.refresh_cwnd(now);
        }
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.mtu = (new_mtu as u64).max(1200);
        let min_cwnd = self.mtu.saturating_mul(MIN_CWND_PACKETS);
        if self.cwnd < min_cwnd {
            self.cwnd = min_cwnd;
        }
    }

    fn window(&self) -> u64 {
        self.cwnd
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }

    fn initial_window(&self) -> u64 {
        self.cwnd
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brutal_compensates_policy_loss_and_throttles_on_queuing() {
        let t0 = Instant::now();
        // 80 Mbps = 10 MB/s
        let cfg = BrutalConfig::new(80);
        assert_eq!(cfg.target_bps(), 10_000_000);
        let mut ctrl = BrutalController::new(cfg.target_bps(), t0, 1200);
        ctrl.min_rtt = Some(Duration::from_millis(50));
        ctrl.smoothed_rtt = Duration::from_millis(50);

        // Clean path: 10 MB/s * 0.05s * 1.5 = 750_000 bytes
        ctrl.slot_mut(t0).acked_bytes = 100_000;
        ctrl.refresh_cwnd(t0);
        let clean_cwnd = ctrl.window();
        assert_eq!(clean_cwnd, 750_000);

        // 20% non-congestive loss (RTT still 50ms): ack_rate = 0.8 -> cwnd rises by 1.25x
        ctrl.slot_mut(t0).lost_bytes = 25_000;
        ctrl.refresh_cwnd(t0);
        let loss_cwnd = ctrl.window();
        assert!(loss_cwnd > clean_cwnd, "expected loss compensation: {loss_cwnd} <= {clean_cwnd}");

        // Persistent bottleneck queuing (RTT jumps from 50ms -> 200ms > 2*50 + 40 = 140ms):
        // queue guard throttles effective rate to 0.5x target_bps
        ctrl.smoothed_rtt = Duration::from_millis(200);
        ctrl.refresh_cwnd(t0);
        let queued_cwnd = ctrl.window();
        let expected_queued = ((10_000_000.0 * QUEUE_THROTTLE_FACTOR) * 0.2 * 1.5) as u64;
        assert_eq!(queued_cwnd, expected_queued);

        // 100% loss blackout: ack_rate clamped to MIN_ACK_RATE (never NaN / unbounded)
        ctrl.smoothed_rtt = Duration::from_millis(50);
        ctrl.slot_mut(t0).acked_bytes = 0;
        ctrl.slot_mut(t0).lost_bytes = 100_000;
        ctrl.refresh_cwnd(t0);
        assert!(ctrl.window() <= MAX_CWND_BYTES);
        assert!(ctrl.window() >= 1200 * MIN_CWND_PACKETS);
    }
}
