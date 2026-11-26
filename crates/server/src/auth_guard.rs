//! Web Admin Panel session management and per-IP brute-force login throttling.

use crate::crypto::generate_random_str;
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use std::net::IpAddr;
use std::time::{Duration, Instant};

const SESSION_TTL: Duration = Duration::from_secs(24 * 3600);
pub(crate) const LOGIN_MAX_FAILURES: u32 = 10;
pub(crate) const LOGIN_WINDOW: Duration = Duration::from_secs(5 * 60);
const LOGIN_FAILURES_CAP: usize = 4096;

#[derive(Default)]
pub struct PanelSessionGuard {
    sessions: DashMap<String, Instant>,
    login_failures: DashMap<IpAddr, (u32, Instant)>,
}

impl PanelSessionGuard {
    pub fn issue_session(&self) -> String {
        let sid = generate_random_str(32);
        self.sessions.retain(|_, ts| ts.elapsed() < SESSION_TTL);
        self.sessions.insert(sid.clone(), Instant::now());
        sid
    }

    pub fn verify_session(&self, sid: &str) -> bool {
        if let Some(ts) = self.sessions.get(sid)
            && ts.elapsed() < SESSION_TTL
        {
            return true;
        }
        self.sessions.remove(sid);
        false
    }

    pub fn revoke_session(&self, sid: &str) {
        self.sessions.remove(sid);
    }

    pub fn revoke_all(&self) {
        self.sessions.clear();
    }

    /// Returns `Err(remaining_lock)` when `ip` has exceeded failed login attempts within window.
    pub fn check_throttle(&self, ip: IpAddr) -> Result<(), Duration> {
        let snapshot = self.login_failures.get(&ip).map(|e| *e.value());
        if let Some((count, first)) = snapshot {
            let elapsed = first.elapsed();
            if elapsed >= LOGIN_WINDOW {
                self.login_failures.remove(&ip);
            } else if count >= LOGIN_MAX_FAILURES {
                return Err(LOGIN_WINDOW - elapsed);
            }
        }
        Ok(())
    }

    pub fn record_login_attempt(&self, ip: IpAddr, success: bool) {
        if success {
            self.login_failures.remove(&ip);
            return;
        }
        match self.login_failures.entry(ip) {
            Entry::Occupied(mut o) => {
                let (count, first) = o.get_mut();
                if first.elapsed() >= LOGIN_WINDOW {
                    *count = 1;
                    *first = Instant::now();
                } else {
                    *count += 1;
                }
            }
            Entry::Vacant(v) => {
                v.insert((1, Instant::now()));
            }
        }
        if self.login_failures.len() > LOGIN_FAILURES_CAP {
            self.login_failures
                .retain(|_, (_, first)| first.elapsed() < LOGIN_WINDOW);
        }
    }
}
