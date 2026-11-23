use bytes::Bytes;
use quinn::{Connection, SendDatagramError};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{debug, warn};

const LOSS_SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
const ADAPTIVE_HOLD_DURATION: Duration = Duration::from_secs(15);
const MIN_LOST_PACKETS_TO_TRIGGER: u64 = 2;
const LOSS_RATIO_THRESHOLD: f64 = 0.005; // 0.5%
const PACER_QUEUE_CAP: usize = 2048;

/// Deterministic loss-adaptive dual-send controller.
///
/// Keeps UDP dual-send OFF on clean links (zero redundant bandwidth and zero fixed
/// duplicate cadence). When QUIC detects >= 0.5% loss (and >= 2 lost packets) in a
/// sampling window, engages protective dual-send and holds it active for 15s of
/// clean hysteresis before turning back off.
#[derive(Debug)]
struct AdaptiveDupPolicy {
    prev_sent: u64,
    prev_lost: u64,
    hold_until: Option<Instant>,
}

impl AdaptiveDupPolicy {
    fn new() -> Self {
        Self {
            prev_sent: 0,
            prev_lost: 0,
            hold_until: None,
        }
    }

    fn update(&mut self, sent_packets: u64, lost_packets: u64, now: Instant) -> bool {
        let delta_sent = sent_packets.saturating_sub(self.prev_sent);
        let delta_lost = lost_packets.saturating_sub(self.prev_lost);
        self.prev_sent = sent_packets;
        self.prev_lost = lost_packets;

        let loss_ratio = delta_lost as f64 / delta_sent.max(1) as f64;
        if delta_lost >= MIN_LOST_PACKETS_TO_TRIGGER && loss_ratio >= LOSS_RATIO_THRESHOLD {
            self.hold_until = Some(now + ADAPTIVE_HOLD_DURATION);
            return true;
        }

        if let Some(until) = self.hold_until {
            if now < until {
                return true;
            }
            self.hold_until = None;
        }
        false
    }
}

/// Connection-scoped, loss-adaptive zero-copy UDP duplicate pacer.
///
/// - Replaces per-packet `tokio::spawn` with a single background pacing task per QUIC connection.
/// - Monitors live QUIC path loss every 2s via `conn.stats().path` and only emits duplicates
///   when packet loss is actually detected (off on healthy links).
/// - Applies slight 2–5ms jittered pacing delay when active to avoid rigid periodic timing.
#[derive(Clone)]
pub struct UdpDupPacer {
    tx: mpsc::Sender<(Instant, Bytes)>,
    dup_active: Arc<AtomicBool>,
    jitter_seq: Arc<AtomicU64>,
}

impl UdpDupPacer {
    pub fn spawn(conn: Connection) -> Self {
        let (tx, mut rx) = mpsc::channel::<(Instant, Bytes)>(PACER_QUEUE_CAP);
        let dup_active = Arc::new(AtomicBool::new(false));
        let dup_flag = dup_active.clone();

        tokio::spawn(async move {
            let mut loss_tick = tokio::time::interval(LOSS_SAMPLE_INTERVAL);
            loss_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut policy = AdaptiveDupPolicy::new();

            loop {
                tokio::select! {
                    _ = loss_tick.tick() => {
                        if conn.close_reason().is_some() {
                            break;
                        }
                        let st = conn.stats().path;
                        let active = policy.update(st.sent_packets, st.lost_packets, Instant::now());
                        let prev = dup_flag.swap(active, Ordering::Relaxed);
                        if active != prev {
                            debug!(
                                "adaptive UDP dual-send {} (sent={}, lost={})",
                                if active { "ENGAGED" } else { "DISENGAGED" },
                                st.sent_packets,
                                st.lost_packets
                            );
                        }
                    }
                    item = rx.recv() => {
                        let Some((due, pkt)) = item else {
                            break;
                        };
                        if conn.close_reason().is_some() {
                            break;
                        }
                        let now = Instant::now();
                        if due > now {
                            tokio::time::sleep_until(due).await;
                        }
                        match conn.send_datagram(pkt) {
                            Ok(()) => {}
                            Err(SendDatagramError::ConnectionLost(_)) => break,
                            Err(e) => debug!("dup datagram skipped: {e}"),
                        }
                    }
                }
            }
        });

        Self {
            tx,
            dup_active,
            jitter_seq: Arc::new(AtomicU64::new(0x9e37_79b9_7f4a_7c15)),
        }
    }

    /// Returns whether loss-adaptive UDP dual-send is currently engaged on this connection.
    #[allow(dead_code)]
    pub fn is_dup_active(&self) -> bool {
        self.dup_active.load(Ordering::Relaxed)
    }

    /// Schedule a zero-copy `Bytes` duplicate ONLY when QUIC path loss has engaged adaptive
    /// dual-send. On clean paths this is a single relaxed atomic load and immediate return.
    pub fn schedule_dup(&self, pkt: Bytes) {
        if !self.dup_active.load(Ordering::Relaxed) {
            return;
        }
        // Lightweight splitmix-style jitter in [2, 5] ms (non-rigid pacing, well inside game tick)
        let n = self.jitter_seq.fetch_add(1, Ordering::Relaxed);
        let mixed = n.wrapping_mul(0x517c_c1b7_2722_0a95);
        let delay_ms = 2 + ((mixed >> 32) % 4);
        let _ = self
            .tx
            .try_send((Instant::now() + Duration::from_millis(delay_ms), pkt));
    }
}

/// Send a QUIC datagram safely: returns `false` ONLY when the underlying QUIC connection
/// is lost; single oversized packets (`TooLarge`) are logged and dropped without tearing
/// down the UDP session.
pub fn send_datagram_checked(conn: &Connection, pkt: Bytes) -> bool {
    match conn.send_datagram(pkt) {
        Ok(()) => true,
        Err(SendDatagramError::ConnectionLost(_)) => false,
        Err(SendDatagramError::TooLarge) => {
            warn!("dropped oversized UDP datagram (> QUIC max_datagram_size)");
            true
        }
        Err(e) => {
            debug!("datagram send ignored: {e}");
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adaptive_policy_stays_off_on_clean_and_isolated_loss() {
        let mut p = AdaptiveDupPolicy::new();
        let t0 = Instant::now();

        // Clean path: 0 lost out of 500
        assert!(!p.update(500, 0, t0));
        // Isolated single lost packet (< MIN_LOST_PACKETS_TO_TRIGGER)
        assert!(!p.update(1000, 1, t0 + Duration::from_secs(2)));
        // Tiny ratio: 2 lost out of 2000 (0.1% < 0.5%)
        assert!(!p.update(3000, 3, t0 + Duration::from_secs(4)));
    }

    #[test]
    fn adaptive_policy_engages_on_loss_and_holds_hysteresis() {
        let mut p = AdaptiveDupPolicy::new();
        let t0 = Instant::now();

        assert!(!p.update(100, 0, t0));

        // 3 lost out of 200 (1.5% >= 0.5%) -> engages immediately
        let t1 = t0 + Duration::from_secs(2);
        assert!(p.update(300, 3, t1));

        // Next window is completely clean, still inside 15s hold window -> stays active
        assert!(p.update(500, 3, t1 + Duration::from_secs(10)));

        // Once 15s clean window elapses -> disengages automatically
        assert!(!p.update(700, 3, t1 + Duration::from_secs(16)));
    }
}
