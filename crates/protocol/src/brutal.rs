//! Loss-tolerant, queue-guarded Brutal congestion controller for `quinn` (`ControllerFactory`).
//!
//! Designed specifically for the bulk TCP QUIC lane on high-RTT / policy-loss cross-border links:
//! - Maintains a 5-second sliding ACK/loss window to compensate for non-congestive policy packet loss
//!   without collapsing throughput like standard BBR/CUBIC.
//! - Uses a configured **target rate** (`target_mbps`, clamped to `1..=1000` Mbps) and a 2.5x
//!   loss-compensation ceiling (`MIN_ACK_RATE = 0.40`). Note: `quinn`'s `Controller` controls `cwnd`
//!   (with 1.5x BDP headroom on clean paths for ACK aggregation), not a strict hard packet pacer.
//! - Includes a **queue-protection guard**: when smoothed RTT rises sharply above recent `min_rtt`
//!   (`rtt > min_rtt * 2 + 40ms`, indicating real bottleneck queuing rather than random loss),
//!   computes `cwnd` against `min_rtt` at `0.5x` target rate (1.0x BDP) so `cwnd` genuinely shrinks
//!   to drain the queue and protect co-resident game UDP latency. While queuing is active, `min_rtt`
//!   is never overwritten upward by the 30s refresh timer.
//! - Holds minimum `cwnd` briefly across a persistent-congestion backoff window so a single ACK
//!   cannot immediately undo persistent-congestion recovery.

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
const PERSISTENT_BACKOFF_MIN: Duration = Duration::from_millis(200);
const PERSISTENT_BACKOFF_MAX: Duration = Duration::from_secs(1);

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
    /// Create a Brutal config targeting `mbps` megabits/sec (clamped to `1..=1000` Mbps).
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
    initial_cwnd: u64,
    started_at: Instant,
    slots: [PktSlot; WINDOW_SLOTS],
    min_rtt: Option<Duration>,
    min_rtt_at: Instant,
    smoothed_rtt: Duration,
    backoff_until: Option<Instant>,
    cwnd: u64,
}

impl BrutalController {
    fn new(target_bps: u64, now: Instant, current_mtu: u16) -> Self {
        let mtu = (current_mtu as u64).max(1200);
        let init_rtt = Duration::from_millis(100);
        let init_cwnd = Self::compute_cwnd(target_bps, 1.0, false, init_rtt, init_rtt, mtu);
        Self {
            target_bps,
            mtu,
            initial_cwnd: init_cwnd,
            started_at: now,
            slots: [PktSlot::default(); WINDOW_SLOTS],
            min_rtt: None,
            min_rtt_at: now,
            smoothed_rtt: init_rtt,
            backoff_until: None,
            cwnd: init_cwnd,
        }
    }

    fn min_cwnd(&self) -> u64 {
        self.mtu.saturating_mul(MIN_CWND_PACKETS)
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

    fn record_rtt_sample(&mut self, now: Instant, sample: Duration) {
        self.smoothed_rtt = sample;
        match self.min_rtt {
            None => {
                self.min_rtt = Some(sample);
                self.min_rtt_at = now;
            }
            Some(m) if sample <= m => {
                self.min_rtt = Some(sample);
                self.min_rtt_at = now;
            }
            Some(_) => {
                // Only refresh min_rtt upward when the path is NOT actively queuing,
                // so sustained bufferbloat cannot raise the baseline and disable queue protection.
                if !self.is_queuing()
                    && now.saturating_duration_since(self.min_rtt_at) > MIN_RTT_REFRESH_INTERVAL
                {
                    self.min_rtt = Some(sample);
                    self.min_rtt_at = now;
                }
            }
        }
    }

    fn compute_cwnd(
        target_bps: u64,
        ack_rate: f64,
        queuing: bool,
        smoothed_rtt: Duration,
        min_rtt: Duration,
        mtu: u64,
    ) -> u64 {
        let (effective_bps, base_rtt, bdp_gain) = if queuing {
            // Drain the queue: throttle against propagation min_rtt (not inflated queuing RTT)
            // with 1.0x BDP so cwnd genuinely drops below the clean-path window.
            (
                (target_bps as f64) * QUEUE_THROTTLE_FACTOR,
                min_rtt,
                1.0,
            )
        } else {
            // 1.5x BDP keeps the pipe full under QUIC ACK aggregation while bounding queue depth
            (
                (target_bps as f64) / ack_rate.clamp(MIN_ACK_RATE, 1.0),
                smoothed_rtt,
                1.5,
            )
        };
        let rtt_secs = base_rtt.as_secs_f64().clamp(0.005, 2.0);
        let raw = (effective_bps * rtt_secs * bdp_gain) as u64;
        let min_cwnd = mtu.saturating_mul(MIN_CWND_PACKETS);
        raw.clamp(min_cwnd, MAX_CWND_BYTES)
    }

    fn refresh_cwnd(&mut self, now: Instant) {
        if let Some(until) = self.backoff_until {
            if now < until {
                self.cwnd = self.min_cwnd();
                return;
            }
            self.backoff_until = None;
        }
        let ack_rate = self.ack_rate(now);
        let queuing = self.is_queuing();
        let min_rtt = self.min_rtt.unwrap_or(self.smoothed_rtt);
        self.cwnd = Self::compute_cwnd(
            self.target_bps,
            ack_rate,
            queuing,
            self.smoothed_rtt,
            min_rtt,
            self.mtu,
        );
    }
    fn on_ack_sample(&mut self, now: Instant, bytes: u64, rtt_sample: Duration) {
        self.record_rtt_sample(now, rtt_sample);
        let slot = self.slot_mut(now);
        slot.acked_bytes = slot.acked_bytes.saturating_add(bytes);
        self.refresh_cwnd(now);
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
        self.on_ack_sample(now, bytes, rtt.get());
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
            // Persistent congestion (multiple PTOs): hold minimum safe cwnd for ~2 RTTs
            // (clamped to 200ms..1s) so the next ACK cannot immediately undo backoff.
            let hold = self
                .smoothed_rtt
                .saturating_mul(2)
                .clamp(PERSISTENT_BACKOFF_MIN, PERSISTENT_BACKOFF_MAX);
            self.backoff_until = Some(now + hold);
            self.cwnd = self.min_cwnd();
        } else {
            self.refresh_cwnd(now);
        }
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.mtu = (new_mtu as u64).max(1200);
        let min_cwnd = self.min_cwnd();
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
        self.initial_cwnd
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brutal_compensates_policy_loss_and_drains_on_queuing() {
        let t0 = Instant::now();
        // 80 Mbps = 10 MB/s
        let cfg = BrutalConfig::new(80);
        assert_eq!(cfg.target_bps(), 10_000_000);
        let mut ctrl = BrutalController::new(cfg.target_bps(), t0, 1200);
        let init_win = ctrl.initial_window();
        ctrl.record_rtt_sample(t0, Duration::from_millis(50));

        // Clean path: 10 MB/s * 0.05s * 1.5 = 750_000 bytes
        ctrl.slot_mut(t0).acked_bytes = 100_000;
        ctrl.refresh_cwnd(t0);
        let clean_cwnd = ctrl.window();
        assert_eq!(clean_cwnd, 750_000);
        assert_eq!(ctrl.initial_window(), init_win);

        // 20% non-congestive loss (RTT still 50ms): ack_rate = 0.8 -> cwnd rises by 1.25x
        ctrl.slot_mut(t0).lost_bytes = 25_000;
        ctrl.refresh_cwnd(t0);
        let loss_cwnd = ctrl.window();
        assert!(
            loss_cwnd > clean_cwnd,
            "expected loss compensation: {loss_cwnd} <= {clean_cwnd}"
        );

        // Persistent bottleneck queuing (RTT jumps from 50ms -> 200ms > 2*50 + 40 = 140ms):
        // queue guard uses min_rtt (50ms) * 0.5x * 1.0 BDP = 250_000 bytes, genuinely < clean_cwnd
        ctrl.record_rtt_sample(t0 + Duration::from_secs(1), Duration::from_millis(200));
        ctrl.refresh_cwnd(t0 + Duration::from_secs(1));
        let queued_cwnd = ctrl.window();
        assert_eq!(queued_cwnd, 250_000);
        assert!(
            queued_cwnd < clean_cwnd,
            "queue guard must shrink cwnd below clean path: {queued_cwnd} >= {clean_cwnd}"
        );

        // Even after 35s of sustained queuing (> 30s refresh interval), min_rtt stays 50ms
        // and queue protection remains active instead of resetting baseline to 200ms.
        let t_later = t0 + Duration::from_secs(35);
        ctrl.record_rtt_sample(t_later, Duration::from_millis(200));
        assert_eq!(ctrl.min_rtt, Some(Duration::from_millis(50)));
        assert!(ctrl.is_queuing());

        // Once queue drains to a moderate new route RTT (80ms <= 2*50 + 40 = 140ms),
        // the expired 30s baseline can safely refresh to 80ms.
        let t_drained = t_later + Duration::from_secs(1);
        ctrl.record_rtt_sample(t_drained, Duration::from_millis(80));
        assert!(!ctrl.is_queuing());
        assert_eq!(ctrl.min_rtt, Some(Duration::from_millis(80)));

        // 100% loss blackout: ack_rate clamped to MIN_ACK_RATE (never NaN / unbounded)
        ctrl.slot_mut(t_drained).acked_bytes = 0;
        ctrl.slot_mut(t_drained).lost_bytes = 100_000;
        ctrl.refresh_cwnd(t_drained);
        assert!(ctrl.window() <= MAX_CWND_BYTES);
        assert!(ctrl.window() >= 1200 * MIN_CWND_PACKETS);
    }

    #[test]
    fn brutal_persistent_congestion_holds_backoff_across_acks() {
        let t0 = Instant::now();
        let cfg = BrutalConfig::new(80);
        let mut ctrl = BrutalController::new(cfg.target_bps(), t0, 1200);
        let sample_rtt = Duration::from_millis(50);

        ctrl.on_ack_sample(t0, 20_000, sample_rtt);
        let normal_cwnd = ctrl.window();
        assert!(normal_cwnd > ctrl.min_cwnd());

        // Trigger persistent congestion: cwnd drops to min_cwnd and stays there on immediate ACKs
        ctrl.on_congestion_event(t0 + Duration::from_millis(10), t0, true, 12_000);
        assert_eq!(ctrl.window(), ctrl.min_cwnd());

        ctrl.on_ack_sample(t0 + Duration::from_millis(50), 20_000, sample_rtt);
        assert_eq!(
            ctrl.window(),
            ctrl.min_cwnd(),
            "backoff should hold across early ACKs"
        );

        // After backoff window expires (> 200ms), cwnd recovers
        ctrl.on_ack_sample(t0 + Duration::from_millis(300), 20_000, sample_rtt);
        assert!(ctrl.window() > ctrl.min_cwnd());
    }
}
