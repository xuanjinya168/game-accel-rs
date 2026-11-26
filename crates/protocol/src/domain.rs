//! Domain value objects and shared ubiquitous language across protocol, client, server, and GUI.
//!
//! Follows *Domain-Driven Design* (Evans) and *Zero To Production in Rust* (Palmieri):
//! "Parse, don't validate" — invalid endpoints or stringly-typed link states are rejected
//! at the boundary rather than re-checked throughout business logic.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::net::IpAddr;

/// Validated `host:port` or `[IPv6]:port` server endpoint value object.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ServerEndpoint {
    raw: String,
    host: String,
    port: u16,
}

impl ServerEndpoint {
    pub fn parse(input: &str) -> Result<Self> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            bail!("服务器地址不能为空");
        }

        let (host_str, port_str) = if let Some(rest) = trimmed.strip_prefix('[') {
            let (ipv6, after) = rest
                .split_once(']')
                .context("IPv6 地址缺少右括号 ']'（示例: [2001:db8::1]:8443）")?;
            let port_part = after
                .strip_prefix(':')
                .context("IPv6 地址缺少端口号（示例: [2001:db8::1]:8443）")?;
            if ipv6.parse::<IpAddr>().is_err() {
                bail!("无效的 IPv6 地址: {ipv6}");
            }
            (ipv6.to_string(), port_part)
        } else {
            let (h, p) = trimmed
                .rsplit_once(':')
                .context("地址缺少端口号（格式: IP:端口 或 域名:端口）")?;
            if h.contains(':') {
                bail!("IPv6 地址请使用方括号包裹（示例: [2001:db8::1]:8443）");
            }
            let h = h.trim();
            if h.is_empty() || h.chars().any(char::is_whitespace) {
                bail!("服务器主机名或 IP 无效: {h}");
            }
            (h.to_string(), p)
        };

        let port: u16 = port_str
            .parse()
            .with_context(|| format!("端口号无效: {port_str}"))?;
        if port == 0 {
            bail!("端口号不能为 0");
        }

        let normalized = if host_str.contains(':') {
            format!("[{host_str}]:{port}")
        } else {
            format!("{host_str}:{port}")
        };

        Ok(Self {
            raw: normalized,
            host: host_str,
            port,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

impl fmt::Display for ServerEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl TryFrom<String> for ServerEndpoint {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self> {
        Self::parse(&value)
    }
}

impl From<ServerEndpoint> for String {
    fn from(ep: ServerEndpoint) -> Self {
        ep.raw
    }
}

/// Strongly-typed share link lifecycle status (wire-compatible with `"active"`, `"disabled"`,
/// `"expired"`, and `"over_limit"` JSON representations).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkStatus {
    Active,
    Disabled,
    Expired,
    OverLimit,
}

impl LinkStatus {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Active)
    }

    pub fn ui_label(self) -> &'static str {
        match self {
            Self::Active => "启用中",
            Self::Disabled => "已禁用",
            Self::Expired => "已过期",
            Self::OverLimit => "流量超额",
        }
    }
}

/// Outcome classification for server access audit records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccessOutcome {
    Ok,
    Denied,
    Error,
}

impl AccessOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Denied => "denied",
            Self::Error => "error",
        }
    }
}

/// Cohesive value object replacing 10-argument positional access log calls.
#[derive(Debug, Clone)]
pub struct AccessLogRecord {
    pub link_id: String,
    pub link_name: String,
    pub client_ip: String,
    pub proto: String,
    pub root_domain: String,
    pub target: String,
    pub tx_bytes: u64,
    pub rx_bytes: u64,
    pub duration_ms: u64,
    pub outcome: AccessOutcome,
}

/// Real-time game link quality telemetry sample.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LinkQuality {
    pub rtt_ms: u64,
    /// Smoothed RFC 3550 jitter in tenths of a millisecond (`18` => `±1.8 ms`).
    pub jitter_tenths_ms: u32,
    /// Recent packet loss ratio in basis points (`100` => `1.0%`, `0` => `0.0%`).
    pub loss_bps: u32,
    /// Whether adaptive `< 200B` game packet dual-send is currently engaged.
    pub dup_active: bool,
}

impl LinkQuality {
    pub fn jitter_ms(self) -> f32 {
        self.jitter_tenths_ms as f32 * 0.1
    }

    pub fn loss_percent(self) -> f32 {
        self.loss_bps as f32 * 0.01
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_endpoint_parses_v4_v6_and_domain() {
        let v4 = ServerEndpoint::parse(" 203.0.113.10:8443 ").unwrap();
        assert_eq!(v4.host(), "203.0.113.10");
        assert_eq!(v4.port(), 8443);
        assert_eq!(v4.as_str(), "203.0.113.10:8443");

        let v6 = ServerEndpoint::parse("[2001:db8::1]:443").unwrap();
        assert_eq!(v6.host(), "2001:db8::1");
        assert_eq!(v6.port(), 443);
        assert_eq!(v6.as_str(), "[2001:db8::1]:443");

        let dom = ServerEndpoint::parse("hk-01.goldpeak.io:8443").unwrap();
        assert_eq!(dom.host(), "hk-01.goldpeak.io");
        assert_eq!(dom.port(), 8443);

        assert!(ServerEndpoint::parse("missing-port.io").is_err());
        assert!(ServerEndpoint::parse("hk.io:0").is_err());
        assert!(ServerEndpoint::parse("2001:db8::1:8443").is_err());
    }

    #[test]
    fn link_status_wire_serialization_matches_legacy_strings() {
        assert_eq!(serde_json::to_string(&LinkStatus::Active).unwrap(), "\"active\"");
        assert_eq!(serde_json::to_string(&LinkStatus::OverLimit).unwrap(), "\"over_limit\"");
        let parsed: LinkStatus = serde_json::from_str("\"expired\"").unwrap();
        assert_eq!(parsed, LinkStatus::Expired);
    }
}
