//! Process-wide DNS cache + bounded multi-address TCP dialer.
//!
//! glibc `getaddrinfo` does not cache, so without this every proxied TCP connection
//! and every domain-targeted UDP packet paid a full DNS round trip (often 10–50 ms,
//! more on a cold resolver) before the target was even dialed.

use dashmap::DashMap;
use protocol::frame::TargetAddr;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;

/// Positive cache lifetime. `getaddrinfo` exposes no TTL; 2 minutes is short enough
/// to follow CDN/DNS changes and long enough to cover a page load and its subresources.
const CACHE_TTL: Duration = Duration::from_secs(120);
const CACHE_MAX_ENTRIES: usize = 8192;
/// Minimum per-address budget when a host has several addresses, so one black-holed
/// IP cannot consume the whole connect timeout before healthy ones are tried.
const MIN_PER_ADDR_BUDGET: Duration = Duration::from_secs(2);

static CACHE: LazyLock<DashMap<String, (Arc<[IpAddr]>, Instant)>> = LazyLock::new(DashMap::new);

/// Resolve `host:port`, serving fresh answers from the cache.
pub async fn resolve(host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
    let host = host.trim().trim_end_matches('.');
    if let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let key = host.to_ascii_lowercase();
    if let Some(hit) = CACHE.get(&key) {
        if hit.1.elapsed() < CACHE_TTL {
            return Ok(hit.0.iter().map(|ip| SocketAddr::new(*ip, port)).collect());
        }
    }

    let mut ips: Vec<IpAddr> = Vec::new();
    for addr in tokio::net::lookup_host((key.as_str(), 0)).await? {
        if !ips.contains(&addr.ip()) {
            ips.push(addr.ip());
        }
    }
    if ips.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no addresses for {key}"),
        ));
    }

    if CACHE.len() >= CACHE_MAX_ENTRIES {
        CACHE.retain(|_, v| v.1.elapsed() < CACHE_TTL);
        if CACHE.len() >= CACHE_MAX_ENTRIES {
            CACHE.clear();
        }
    }
    let out = ips.iter().map(|ip| SocketAddr::new(*ip, port)).collect();
    CACHE.insert(key, (ips.into(), Instant::now()));
    Ok(out)
}

/// Drop a cached answer (e.g. every resolved address refused the connection).
pub fn invalidate(host: &str) {
    CACHE.remove(&host.trim().trim_end_matches('.').to_ascii_lowercase());
}

/// Dial `target` within `total` (DNS included), trying every resolved address in
/// order with a fair share of the remaining budget each.
pub async fn connect_tcp(target: &TargetAddr, total: Duration) -> io::Result<TcpStream> {
    let deadline = Instant::now() + total;
    let addrs = match target {
        TargetAddr::Ip(addr) => vec![*addr],
        TargetAddr::Domain(host, port) => tokio::time::timeout(total, resolve(host, *port))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "dns resolve timed out"))??,
    };

    let mut last_err = None;
    for (i, addr) in addrs.iter().enumerate() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let left = (addrs.len() - i) as u32;
        let budget = if left > 1 {
            (remaining / left).max(MIN_PER_ADDR_BUDGET).min(remaining)
        } else {
            remaining
        };
        match tokio::time::timeout(budget, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(e)) => last_err = Some(e),
            Err(_) => {
                last_err = Some(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("connect {addr} timed out"),
                ))
            }
        }
    }

    if let TargetAddr::Domain(host, _) = target {
        invalidate(host);
    }
    Err(last_err.unwrap_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "connect timed out")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ip_literals_bypass_dns_and_localhost_is_cached() {
        let v = resolve("127.0.0.1", 80).await.unwrap();
        assert_eq!(v, vec!["127.0.0.1:80".parse::<SocketAddr>().unwrap()]);
        let v6 = resolve("[::1]", 443).await.unwrap();
        assert_eq!(v6, vec!["[::1]:443".parse::<SocketAddr>().unwrap()]);

        let first = resolve("localhost", 8080).await.unwrap();
        assert!(!first.is_empty() && first.iter().all(|a| a.port() == 8080));
        assert!(CACHE.contains_key("localhost"));
        // Cached entry is port-independent
        let second = resolve("LOCALHOST.", 9090).await.unwrap();
        assert!(second.iter().all(|a| a.port() == 9090));
        assert_eq!(first.len(), second.len());
    }

    #[tokio::test]
    async fn connect_falls_through_to_next_address_and_times_out() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ok_addr = listener.local_addr().unwrap();
        // Unbound loopback port refuses immediately
        let refused = TargetAddr::Ip("127.0.0.1:1".parse().unwrap());
        assert!(connect_tcp(&refused, Duration::from_secs(2)).await.is_err());
        let ok = TargetAddr::Ip(ok_addr);
        assert!(connect_tcp(&ok, Duration::from_secs(2)).await.is_ok());
    }
}
