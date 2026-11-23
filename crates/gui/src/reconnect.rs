use std::time::{Duration, Instant};

use client::{ERR_AUTH_REJECTED, ERR_PORT_IN_USE};

/// Backoff between consecutive reconnect attempts: 1, 2, 4, 8, 16, then 30s.
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Give up after this many consecutive failed attempts (~4.5 minutes of trying).
const MAX_ATTEMPTS: u32 = 12;
/// A session that lasted at least this long counts as stable: the next drop starts over at 1s.
const STABLE_AFTER: Duration = Duration::from_secs(30);

#[derive(Debug, Default)]
pub(crate) struct ReconnectPolicy {
    /// 1-based number of the most recently scheduled attempt; 0 when not in a reconnect streak.
    attempt: u32,
    next_at: Option<Instant>,
    connected_since: Option<Instant>,
}

impl ReconnectPolicy {
    fn backoff(attempt: u32) -> Duration {
        Duration::from_secs(1u64 << (attempt - 1).min(5)).min(MAX_BACKOFF)
    }

    /// The tunnel is up. The failure count is only cleared once this session proves
    /// stable (see `dropped`), so a flapping server is not hammered at 1s intervals.
    pub(crate) fn connected(&mut self, now: Instant) {
        self.next_at = None;
        self.connected_since = Some(now);
    }

    /// The established tunnel dropped, or a reconnect attempt failed.
    /// Returns the wait before the next attempt, or `None` when giving up.
    pub(crate) fn dropped(&mut self, now: Instant) -> Option<Duration> {
        if self
            .connected_since
            .is_some_and(|t| now.duration_since(t) >= STABLE_AFTER)
        {
            self.attempt = 0;
        }
        self.connected_since = None;
        if self.attempt >= MAX_ATTEMPTS {
            self.next_at = None;
            return None;
        }
        self.attempt += 1;
        let delay = Self::backoff(self.attempt);
        self.next_at = Some(now + delay);
        Some(delay)
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn in_progress(&self) -> bool {
        self.attempt > 0
    }

    /// 1-based number of the pending or in-flight attempt.
    pub(crate) fn attempt(&self) -> u32 {
        self.attempt
    }

    pub(crate) fn is_waiting(&self) -> bool {
        self.next_at.is_some()
    }

    /// Seconds until the pending attempt fires (0 when an attempt is already in flight).
    pub(crate) fn countdown(&self, now: Instant) -> u64 {
        self.next_at
            .map(|t| t.saturating_duration_since(now).as_secs())
            .unwrap_or(0)
    }

    /// Takes the pending attempt if its backoff has elapsed.
    pub(crate) fn take_due(&mut self, now: Instant) -> bool {
        match self.next_at {
            Some(t) if now >= t => {
                self.next_at = None;
                true
            }
            _ => false,
        }
    }
}

/// Only failures that can resolve on their own (server restart, network switch, packet
/// loss) are worth retrying; a rejected token or an occupied local port never will.
pub(crate) fn is_retryable_connect_error(err: &str) -> bool {
    !err.contains(ERR_AUTH_REJECTED) && !err.contains(ERR_PORT_IN_USE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    #[test]
    fn backoff_grows_then_caps() {
        let base = Instant::now();
        let mut p = ReconnectPolicy::default();
        p.connected(base);
        let mut delays = Vec::new();
        let mut now = base;
        for _ in 0..6 {
            let d = p.dropped(now).unwrap();
            delays.push(d);
            now += d;
        }
        assert_eq!(
            delays,
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
                Duration::from_secs(16),
                Duration::from_secs(30),
            ]
        );
    }

    #[test]
    fn gives_up_after_max_attempts() {
        let base = Instant::now();
        let mut p = ReconnectPolicy::default();
        p.connected(base);
        let mut now = base;
        for _ in 0..MAX_ATTEMPTS {
            assert!(p.dropped(now).is_some());
            now += Duration::from_secs(60);
        }
        assert!(p.dropped(now).is_none());
        assert!(!p.is_waiting());
    }

    #[test]
    fn stable_session_resets_the_streak() {
        let base = Instant::now();
        let mut p = ReconnectPolicy::default();
        p.connected(base);
        for _ in 0..5 {
            p.dropped(at(base, 60)).unwrap();
        }
        // A long-lived session clears the failure history
        p.connected(at(base, 100));
        let d = p.dropped(at(base, 200)).unwrap();
        assert_eq!(d, Duration::from_secs(1), "streak must restart at 1s");
    }

    #[test]
    fn short_session_keeps_counting() {
        let base = Instant::now();
        let mut p = ReconnectPolicy::default();
        p.connected(base);
        p.dropped(at(base, 10)).unwrap();
        p.connected(at(base, 12));
        // came back but dropped again within the stability window
        let d = p.dropped(at(base, 20)).unwrap();
        assert_eq!(d, Duration::from_secs(2));
    }

    #[test]
    fn due_and_countdown() {
        let base = Instant::now();
        let mut p = ReconnectPolicy::default();
        assert!(!p.in_progress());
        p.connected(base);
        assert!(!p.in_progress());
        assert!(p.dropped(base).is_some());
        assert!(p.in_progress() && p.is_waiting());
        assert!(!p.take_due(at(base, 0)));
        assert_eq!(p.countdown(at(base, 0)), 1);
        assert_eq!(p.countdown(at(base, 1)), 0);
        assert!(p.take_due(at(base, 1)));
        assert!(!p.is_waiting());
        assert!(p.in_progress(), "attempt in flight is still part of the streak");
        p.reset();
        assert!(!p.in_progress());
    }

    #[test]
    fn only_transient_errors_are_retried() {
        assert!(is_retryable_connect_error(
            "QUIC 握手或鉴权超时（请检查网络、UDP 端口）"
        ));
        assert!(!is_retryable_connect_error(
            "服务端拒绝鉴权 (code=1)"
        ));
        assert!(!is_retryable_connect_error(
            "本地端口 127.0.0.1:7890 被占用，无法监听聚合代理"
        ));
    }
}
