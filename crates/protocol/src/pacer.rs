use bytes::Bytes;
use quinn::{Connection, SendDatagramError};
use std::cmp::{Ordering as CmpOrdering, Reverse};
use std::collections::BinaryHeap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{debug, warn};

const LOSS_SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
const ADAPTIVE_HOLD_DURATION: Duration = Duration::from_secs(15);
const MIN_LOST_PACKETS_TO_TRIGGER: u64 = 2;
const LOSS_RATIO_THRESHOLD: f64 = 0.005; // 0.5%
const PACER_QUEUE_CAP: usize = 2048;

struct ScheduledDup {
    due: Instant,
    seq: u64,
    pkt: Bytes,
}

impl PartialEq for ScheduledDup {
    fn eq(&self, other: &Self) -> bool {
        self.due == other.due && self.seq == other.seq
    }
}
impl Eq for ScheduledDup {}

impl PartialOrd for ScheduledDup {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScheduledDup {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        self.due.cmp(&other.due).then_with(|| self.seq.cmp(&other.seq))
    }
}

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

    fn update(&mut self, sent_packets: u64, lost_packets: u64, now: Instant) -> (bool, u32) {
        let delta_sent = sent_packets.saturating_sub(self.prev_sent);
        let delta_lost = lost_packets.saturating_sub(self.prev_lost);
        self.prev_sent = sent_packets;
        self.prev_lost = lost_packets;

        let loss_ratio = if delta_sent > 0 {
            (delta_lost as f64 / delta_sent as f64).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let loss_bps = (loss_ratio * 10_000.0).round() as u32;

        if delta_lost >= MIN_LOST_PACKETS_TO_TRIGGER && loss_ratio >= LOSS_RATIO_THRESHOLD {
            self.hold_until = Some(now + ADAPTIVE_HOLD_DURATION);
            return (true, loss_bps);
        }

        if let Some(until) = self.hold_until {
            if now < until {
                return (true, loss_bps);
            }
            self.hold_until = None;
        }
        (false, loss_bps)
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
    loss_bps: Arc<AtomicU32>,
    jitter_seq: Arc<AtomicU64>,
}

impl UdpDupPacer {
    pub fn spawn(conn: Connection) -> Self {
        let (tx, mut rx) = mpsc::channel::<(Instant, Bytes)>(PACER_QUEUE_CAP);
        let dup_active = Arc::new(AtomicBool::new(false));
        let loss_bps = Arc::new(AtomicU32::new(0));
        let dup_flag = dup_active.clone();
        let loss_slot = loss_bps.clone();

        tokio::spawn(async move {
            let mut loss_tick = tokio::time::interval(LOSS_SAMPLE_INTERVAL);
            loss_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut policy = AdaptiveDupPolicy::new();
            let mut heap: BinaryHeap<Reverse<ScheduledDup>> = BinaryHeap::new();
            let mut order_seq: u64 = 0;

            loop {
                let next_due = heap.peek().map(|r| r.0.due);
                tokio::select! {
                    _ = loss_tick.tick() => {
                        if conn.close_reason().is_some() {
                            break;
                        }
                        let st = conn.stats().path;
                        let (active, bps) = policy.update(st.sent_packets, st.lost_packets, Instant::now());
                        loss_slot.store(bps, Ordering::Relaxed);
                        let prev = dup_flag.swap(active, Ordering::Relaxed);
                        if active != prev {
                            debug!(
                                "adaptive UDP dual-send {} (sent={}, lost={}, bps={})",
                                if active { "ENGAGED" } else { "DISENGAGED" },
                                st.sent_packets,
                                st.lost_packets,
                                bps
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
                        if heap.len() < PACER_QUEUE_CAP {
                            order_seq = order_seq.wrapping_add(1);
                            heap.push(Reverse(ScheduledDup { due, seq: order_seq, pkt }));
                        }
                    }
                    _ = async {
                        match next_due {
                            Some(due) => tokio::time::sleep_until(due).await,
                            None => std::future::pending::<()>().await,
                        }
                    } => {
                        let now = Instant::now();
                        while let Some(top) = heap.peek() {
                            if top.0.due > now {
                                break;
                            }
                            let item = heap.pop().expect("peeked").0;
                            match conn.send_datagram(item.pkt) {
                                Ok(()) => {}
                                Err(SendDatagramError::ConnectionLost(_)) => return,
                                Err(e) => debug!("dup datagram skipped: {e}"),
                            }
                        }
                    }
                }
            }
        });

        Self {
            tx,
            dup_active,
            loss_bps,
            jitter_seq: Arc::new(AtomicU64::new(0x9e37_79b9_7f4a_7c15)),
        }
    }

    /// Returns whether loss-adaptive UDP dual-send is currently engaged on this connection.
    pub fn is_dup_active(&self) -> bool {
        self.dup_active.load(Ordering::Relaxed)
    }

    /// Returns the most recently sampled packet loss ratio in basis points (`100` = `1.0%`).
    pub fn loss_bps(&self) -> u32 {
        self.loss_bps.load(Ordering::Relaxed)
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
            // Rate-limited: a game sending oversized packets would otherwise flood the log.
            static DROPPED: AtomicU64 = AtomicU64::new(0);
            static LAST_WARN_SECS: AtomicU64 = AtomicU64::new(0);
            let dropped = DROPPED.fetch_add(1, Ordering::Relaxed) + 1;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let last = LAST_WARN_SECS.load(Ordering::Relaxed);
            if now >= last + 10
                && LAST_WARN_SECS
                    .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
            {
                warn!(
                    "dropped oversized UDP datagram (> QUIC max_datagram_size {:?}); {dropped} dropped so far",
                    conn.max_datagram_size()
                );
            }
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
        assert_eq!(p.update(500, 0, t0), (false, 0));
        // Isolated single lost packet (< MIN_LOST_PACKETS_TO_TRIGGER)
        assert!(!p.update(1000, 1, t0 + Duration::from_secs(2)).0);
        // Tiny ratio: 2 lost out of 2000 (0.1% < 0.5%)
        assert_eq!(p.update(3000, 3, t0 + Duration::from_secs(4)), (false, 10));
    }

    #[test]
    fn adaptive_policy_engages_on_loss_and_holds_hysteresis() {
        let mut p = AdaptiveDupPolicy::new();
        let t0 = Instant::now();

        assert_eq!(p.update(100, 0, t0), (false, 0));

        // 3 lost out of 200 (1.5% >= 0.5%) -> engages immediately
        let t1 = t0 + Duration::from_secs(2);
        assert_eq!(p.update(300, 3, t1), (true, 150));

        // Next window is completely clean, still inside 15s hold window -> stays active
        assert_eq!(p.update(500, 3, t1 + Duration::from_secs(10)), (true, 0));

        // Once 15s clean window elapses -> disengages automatically
        assert_eq!(p.update(700, 3, t1 + Duration::from_secs(16)), (false, 0));
    }

    #[test]
    fn pacer_heap_orders_earliest_due_first() {
        let t0 = Instant::now();
        let mut heap: BinaryHeap<Reverse<ScheduledDup>> = BinaryHeap::new();
        heap.push(Reverse(ScheduledDup {
            due: t0 + Duration::from_millis(5),
            seq: 1,
            pkt: Bytes::from_static(b"late"),
        }));
        heap.push(Reverse(ScheduledDup {
            due: t0 + Duration::from_millis(2),
            seq: 2,
            pkt: Bytes::from_static(b"early"),
        }));
        assert_eq!(heap.pop().unwrap().0.pkt, Bytes::from_static(b"early"));
        assert_eq!(heap.pop().unwrap().0.pkt, Bytes::from_static(b"late"));
    }
}
