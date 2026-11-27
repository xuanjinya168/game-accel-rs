//! Split routing (分流): decide per connection whether a target goes DIRECT from this
//! machine, through the primary (foreign) tunnel, or through the domestic tunnel:
//!
//! ```text
//! private / LAN address            -> DIRECT
//! DOMAIN in proxy-list             -> PRIMARY (known foreign sites, skips local DNS)
//! DOMAIN in direct-list            -> domestic egress (geosite:cn — ~110k Chinese domains)
//! IP in geoip:cn                   -> domestic egress
//! unknown DOMAIN -> local DNS -> CN IP ? domestic egress : PRIMARY
//! everything else                  -> PRIMARY
//! ```
//!
//! "Domestic egress" is DIRECT unless a domestic node is configured and connected, in which
//! case domestic traffic is tunnelled through it. Rule data (Loyalsoldier v2ray-rules-dat /
//! geoip) is embedded in the binary; files with the same names in the optional `rules_dir`
//! override the embedded copies.

use crate::direct::DirectDialer;
use dashmap::DashMap;
use protocol::frame::TargetAddr;
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

const EMBEDDED_DIRECT: &str = include_str!("../data/direct-list.txt");
const EMBEDDED_PROXY: &str = include_str!("../data/proxy-list.txt");
const EMBEDDED_GEOIP_CN: &str = include_str!("../data/geoip-cn.txt");

/// Local DNS lookup budget for the GEOIP fallback; on timeout the target is proxied.
const GEOIP_RESOLVE_TIMEOUT: Duration = Duration::from_millis(800);
const DNS_CACHE_TTL: Duration = Duration::from_secs(120);
const DNS_CACHE_MAX: usize = 8192;

/// User-facing routing mode (大陆直连 / 双节点 / 全局 / 直连).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RouteMode {
    /// Split routing: domestic direct, foreign via primary node (default).
    #[default]
    Rule,
    /// Dual-node split routing: domestic via domestic node, foreign via primary node.
    Dual,
    /// Everything through the primary tunnel (except LAN/localhost).
    Global,
    /// Everything direct (tunnel effectively bypassed; handy for A/B comparison).
    Direct,
}

impl RouteMode {
    pub fn label(self) -> &'static str {
        match self {
            RouteMode::Rule => "大陆直连",
            RouteMode::Dual => "双节点分流",
            RouteMode::Global => "全局代理",
            RouteMode::Direct => "纯直连",
        }
    }

    pub fn is_split(self) -> bool {
        matches!(self, RouteMode::Rule | RouteMode::Dual)
    }
}

/// Live, lock-free routing mode shared between the UI and the running session, so
/// switching mode applies to the next connection without reconnecting.
#[derive(Debug)]
pub struct RouteModeCell(AtomicU8);

impl RouteModeCell {
    pub fn new(mode: RouteMode) -> Arc<Self> {
        Arc::new(Self(AtomicU8::new(mode as u8)))
    }
    pub fn get(&self) -> RouteMode {
        match self.0.load(Ordering::Relaxed) {
            1 => RouteMode::Dual,
            2 => RouteMode::Global,
            3 => RouteMode::Direct,
            _ => RouteMode::Rule,
        }
    }
    pub fn set(&self, mode: RouteMode) {
        self.0.store(mode as u8, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Route {
    Direct,
    /// Through the primary (foreign) tunnel.
    Primary,
    /// Through the domestic tunnel; only produced in `Dual` mode when secondary is up.
    Secondary,
}

/// Where the domestic (CN) bucket goes when `RouteMode::Dual` is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DomesticEgress {
    #[default]
    Direct,
    /// Route through the secondary (domestic) tunnel.
    Secondary,
    /// Reuse the primary tunnel (when the user chose the same node for both roles).
    Primary,
}

impl DomesticEgress {
    pub fn label(self) -> &'static str {
        match self {
            DomesticEgress::Direct => "直连",
            DomesticEgress::Secondary => "国内节点",
            DomesticEgress::Primary => "复用主节点",
        }
    }
}

/// Live domestic-egress policy shared between the session and its watchdog: if the domestic
/// tunnel dies, the watchdog flips this to `Direct` so new connections fall back gracefully.
#[derive(Debug)]
pub struct DomesticEgressCell(AtomicU8);

impl DomesticEgressCell {
    pub fn new(egress: DomesticEgress) -> Arc<Self> {
        Arc::new(Self(AtomicU8::new(egress as u8)))
    }
    pub fn get(&self) -> DomesticEgress {
        match self.0.load(Ordering::Relaxed) {
            1 => DomesticEgress::Secondary,
            2 => DomesticEgress::Primary,
            _ => DomesticEgress::Direct,
        }
    }
    pub fn set(&self, egress: DomesticEgress) {
        self.0.store(egress as u8, Ordering::Relaxed);
    }
}

/// Explicit exit action for a user-defined highest-priority split-routing rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CustomRuleAction {
    /// Always bypass tunnels and dial directly from this machine (even in `Dual` mode).
    #[default]
    Direct,
    /// Route through the domestic (secondary) node in `Dual` mode; falls back to Direct otherwise.
    Domestic,
    /// Always route through the primary (foreign) node.
    Primary,
}

impl CustomRuleAction {
    pub fn label(self) -> &'static str {
        match self {
            CustomRuleAction::Direct => "直连",
            CustomRuleAction::Domestic => "国内节点",
            CustomRuleAction::Primary => "主节点",
        }
    }

    pub fn token(self) -> &'static str {
        match self {
            CustomRuleAction::Direct => "direct",
            CustomRuleAction::Domestic => "domestic",
            CustomRuleAction::Primary => "primary",
        }
    }

    pub fn next(self) -> Self {
        match self {
            CustomRuleAction::Direct => CustomRuleAction::Domestic,
            CustomRuleAction::Domestic => CustomRuleAction::Primary,
            CustomRuleAction::Primary => CustomRuleAction::Direct,
        }
    }

    pub fn parse_token(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "direct" | "直连" => Some(Self::Direct),
            "domestic" | "cn" | "secondary" | "国内" | "国内节点" => Some(Self::Domestic),
            "primary" | "proxy" | "foreign" | "主节点" | "代理" => Some(Self::Primary),
            _ => None,
        }
    }
}

fn default_rule_enabled() -> bool {
    true
}

/// Serializable user-defined custom split-routing rule (persisted in GUI `config.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomRule {
    /// Domain suffix (`steampowered.com`), exact (`full:dl.google.com`), IP (`45.67.89.10/32`),
    /// or CIDR (`104.16.0.0/16` / `2606:4700::/32`).
    pub pattern: String,
    #[serde(default)]
    pub action: CustomRuleAction,
    #[serde(default = "default_rule_enabled")]
    pub enabled: bool,
}

fn is_valid_domain_host(h: &str) -> bool {
    if h.is_empty() || !h.contains('.') || h.len() > 253 {
        return false;
    }
    let mut last_label = "";
    for label in h.split('.') {
        if label.is_empty()
            || label.len() > 63
            || label.starts_with('-')
            || label.ends_with('-')
            || !label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return false;
        }
        last_label = label;
    }
    // Reject all-numeric pseudo-domains like `123.456.789.0`
    !last_label.bytes().all(|b| b.is_ascii_digit())
}

impl CustomRule {
    /// Normalize user input:
    /// - supports `full:` (exact domain), `domain:` / `*.` (domain suffix), bare IP, `IP:port`,
    ///   CIDR (`ip/prefix`), and accidental `http(s)://.../path` URLs.
    /// - Returns `None` if the input is empty or structurally invalid.
    pub fn normalize_pattern(raw: &str) -> Option<String> {
        let mut s = raw.trim().to_ascii_lowercase();
        if s.is_empty() || s.contains(char::is_whitespace) {
            return None;
        }
        let mut exact = false;
        if let Some(rest) = s.strip_prefix("full:") {
            exact = true;
            s = rest.trim().to_string();
        } else if let Some(rest) = s.strip_prefix("domain:") {
            s = rest.trim().to_string();
        }
        if let Some(rest) = s
            .strip_prefix("https://")
            .or_else(|| s.strip_prefix("http://"))
        {
            s = rest.split('/').next().unwrap_or("").to_string();
        }

        if !exact {
            // CIDR (`ip/prefix`)
            if let Some((ip_s, len_s)) = s.split_once('/') {
                let ip: IpAddr = ip_s.trim_matches(['[', ']']).parse().ok()?;
                let len: u32 = len_s.parse().ok()?;
                return match ip {
                    IpAddr::V4(v4) if len <= 32 => Some(format!("{v4}/{len}")),
                    IpAddr::V6(v6) if len <= 128 => Some(format!("{v6}/{len}")),
                    _ => None,
                };
            }
            // Bare IP or `IP:port` / `[IPv6]:port`
            let bare_ip = s
                .trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .ok()
                .or_else(|| s.parse::<std::net::SocketAddr>().ok().map(|sa| sa.ip()));
            if let Some(ip) = bare_ip {
                return Some(match ip {
                    IpAddr::V4(v4) => format!("{v4}/32"),
                    IpAddr::V6(v6) => format!("{v6}/128"),
                });
            }
        }

        // Strip optional `:port`, wildcard `*.`, leading/trailing dots
        let no_path = s.split('/').next().unwrap_or("");
        let domain_only = no_path.split(':').next().unwrap_or("");
        let stripped = domain_only
            .strip_prefix("*.")
            .unwrap_or(domain_only)
            .trim_start_matches('.');
        let h = normalize_host(stripped);
        if !is_valid_domain_host(&h) {
            return None;
        }
        if exact { Some(format!("full:{h}")) } else { Some(h) }
    }

    /// Human-readable classification badge for UI display.
    pub fn kind_label(raw: &str) -> &'static str {
        let Some(norm) = Self::normalize_pattern(raw) else {
            return if raw.trim().is_empty() {
                "自动识别"
            } else {
                "格式无效"
            };
        };
        if norm.starts_with("full:") {
            "精确域名"
        } else if let Some((_, len)) = norm.split_once('/') {
            if len == "32" || len == "128" {
                "IP"
            } else {
                "CIDR"
            }
        } else {
            "域名后缀"
        }
    }
}

/// Strictly parse bulk text rules, returning a line-numbered error on malformed entries.
/// - `# ...` lines are comments; inline `# ...` comments are stripped.
/// - Leading `!` marks a paused (`enabled = false`) rule so paused rules survive bulk round-trips.
pub fn parse_bulk_custom_rules_checked(text: &str) -> Result<Vec<CustomRule>, String> {
    let mut out = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with("//") {
            continue;
        }
        let content = trimmed.split('#').next().unwrap_or("").trim();
        if content.is_empty() {
            continue;
        }
        let (enabled, body) = match content.strip_prefix('!') {
            Some(rest) => (false, rest.trim()),
            None => (true, content),
        };
        let mut parts = body
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|p| !p.is_empty());
        let Some(raw_pat) = parts.next() else {
            continue;
        };
        let Some(pattern) = CustomRule::normalize_pattern(raw_pat) else {
            return Err(format!("第 {line_no} 行规则格式无效：「{raw_pat}」"));
        };
        let action = match parts.next() {
            Some(tok) => CustomRuleAction::parse_token(tok).ok_or_else(|| {
                format!("第 {line_no} 行出口动作无效：「{tok}」（支持 direct / domestic / primary）")
            })?,
            None => CustomRuleAction::Direct,
        };
        out.push(CustomRule {
            pattern,
            action,
            enabled,
        });
    }
    Ok(out)
}

/// Lenient bulk parser (skips invalid lines); kept for non-interactive callers.
pub fn parse_bulk_custom_rules(text: &str) -> Vec<CustomRule> {
    let mut out = Vec::new();
    for line in text.lines() {
        if let Ok(mut parsed) = parse_bulk_custom_rules_checked(line) {
            out.append(&mut parsed);
        }
    }
    out
}

/// Format rules into editable bulk text (paused rules are prefixed with `!` to preserve state).
pub fn format_bulk_custom_rules(rules: &[CustomRule]) -> String {
    let mut lines = Vec::with_capacity(rules.len() + 2);
    lines.push("# 格式: 域名或IP/CIDR, direct | domestic | primary（行首加 ! 表示暂停）".to_string());
    lines.push("# 示例: steampowered.com, direct   或   45.67.89.10/32, domestic".to_string());
    for r in rules {
        let prefix = if r.enabled { "" } else { "!" };
        lines.push(format!("{prefix}{}, {}", r.pattern, r.action.token()));
    }
    lines.join("\n")
}

#[derive(Debug, Clone)]
enum CompiledMatcher {
    DomainFull(String),
    DomainSuffix(String),
    Ipv4Range(u32, u32),
    Ipv6Range(u128, u128),
}

#[derive(Debug, Clone)]
struct CompiledCustomEntry {
    pattern: String,
    matcher: CompiledMatcher,
    action: CustomRuleAction,
}

#[derive(Debug, Default)]
pub struct CompiledCustomRules {
    entries: Vec<CompiledCustomEntry>,
}

impl CompiledCustomRules {
    pub fn compile(rules: &[CustomRule]) -> Self {
        let mut entries = Vec::new();
        for rule in rules {
            if !rule.enabled {
                continue;
            }
            let Some(pattern) = CustomRule::normalize_pattern(&rule.pattern) else {
                continue;
            };
            let matcher = if let Some(exact) = pattern.strip_prefix("full:") {
                CompiledMatcher::DomainFull(exact.to_string())
            } else if let Some((ip_s, len_s)) = pattern.split_once('/') {
                let Ok(ip) = ip_s.parse::<IpAddr>() else {
                    continue;
                };
                let Ok(len) = len_s.parse::<u32>() else {
                    continue;
                };
                match ip {
                    IpAddr::V4(a) if len <= 32 => {
                        let start = u32::from(a) & mask32(len);
                        CompiledMatcher::Ipv4Range(start, start | !mask32(len))
                    }
                    IpAddr::V6(a) if len <= 128 => {
                        let start = u128::from(a) & mask128(len);
                        CompiledMatcher::Ipv6Range(start, start | !mask128(len))
                    }
                    _ => continue,
                }
            } else {
                CompiledMatcher::DomainSuffix(pattern.clone())
            };
            entries.push(CompiledCustomEntry {
                pattern,
                matcher,
                action: rule.action,
            });
        }
        Self { entries }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// First matching enabled custom domain rule (top-to-bottom precedence).
    pub fn match_domain(&self, host: &str) -> Option<(CustomRuleAction, &str)> {
        for e in &self.entries {
            let matched = match &e.matcher {
                CompiledMatcher::DomainFull(exact) => host == exact,
                CompiledMatcher::DomainSuffix(suf) => {
                    host == suf
                        || (host.len() > suf.len()
                            && host.ends_with(suf.as_str())
                            && host.as_bytes()[host.len() - suf.len() - 1] == b'.')
                }
                _ => false,
            };
            if matched {
                return Some((e.action, e.pattern.as_str()));
            }
        }
        None
    }

    /// First matching enabled custom IP / CIDR rule (top-to-bottom precedence).
    pub fn match_ip(&self, ip: IpAddr) -> Option<(CustomRuleAction, &str)> {
        for e in &self.entries {
            let matched = match (&e.matcher, ip) {
                (CompiledMatcher::Ipv4Range(s, end), IpAddr::V4(v4)) => {
                    let v = u32::from(v4);
                    v >= *s && v <= *end
                }
                (CompiledMatcher::Ipv4Range(s, end), IpAddr::V6(v6)) => v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| {
                        let v = u32::from(v4);
                        v >= *s && v <= *end
                    }),
                (CompiledMatcher::Ipv6Range(s, end), IpAddr::V6(v6)) => {
                    let v = u128::from(v6);
                    v >= *s && v <= *end
                }
                _ => false,
            };
            if matched {
                return Some((e.action, e.pattern.as_str()));
            }
        }
        None
    }
}

/// Live, hot-reloadable custom split-routing rules shared between the UI and running `Router`.
#[derive(Debug)]
pub struct CustomRulesCell(RwLock<Arc<CompiledCustomRules>>);

impl CustomRulesCell {
    pub fn new(rules: &[CustomRule]) -> Arc<Self> {
        Arc::new(Self(RwLock::new(Arc::new(CompiledCustomRules::compile(
            rules,
        )))))
    }

    pub fn empty() -> Arc<Self> {
        Arc::new(Self(RwLock::new(Arc::new(CompiledCustomRules::default()))))
    }

    pub fn replace(&self, rules: &[CustomRule]) {
        let compiled = Arc::new(CompiledCustomRules::compile(rules));
        if let Ok(mut guard) = self.0.write() {
            *guard = compiled;
        }
    }

    pub fn snapshot(&self) -> Arc<CompiledCustomRules> {
        self.0
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|_| Arc::new(CompiledCustomRules::default()))
    }
}

/// Result of a synchronous route preview for the GUI live hit-tester.
#[derive(Debug, Clone)]
pub struct RoutePreview {
    /// Human-readable description of which rule matched (e.g. `自定义 steampowered.com` or `内置代理表`).
    pub source: String,
    pub action: CustomRuleAction,
    /// True when action is `Domestic` while current route mode is not `Dual` (falls back to Direct).
    pub domestic_fallback_to_direct: bool,
}

/// Synchronous preview of how `raw_input` (domain or IP) will be routed under `mode` and `rules`.
pub fn preview_route_match(
    raw_input: &str,
    mode: RouteMode,
    rules: &[CustomRule],
    data: &RuleData,
) -> Option<RoutePreview> {
    let mut cleaned = raw_input.trim().to_ascii_lowercase();
    if cleaned.is_empty() {
        return None;
    }
    if let Some(rest) = cleaned
        .strip_prefix("https://")
        .or_else(|| cleaned.strip_prefix("http://"))
    {
        cleaned = rest.split('/').next().unwrap_or("").to_string();
    }
    let host_only = cleaned.split('/').next().unwrap_or("");
    let ip_opt = host_only
        .trim_matches(['[', ']'])
        .parse::<IpAddr>()
        .ok()
        .or_else(|| host_only.parse::<std::net::SocketAddr>().ok().map(|s| s.ip()));

    let mk = |source: String, action: CustomRuleAction| RoutePreview {
        source,
        action,
        domestic_fallback_to_direct: action == CustomRuleAction::Domestic && mode != RouteMode::Dual,
    };

    if mode == RouteMode::Direct {
        return Some(mk("纯直连模式".to_string(), CustomRuleAction::Direct));
    }

    if let Some(ip) = ip_opt {
        if is_local_ip(ip) {
            return Some(mk("局域网 / 本机地址".to_string(), CustomRuleAction::Direct));
        }
        if mode == RouteMode::Global {
            return Some(mk("全局代理模式".to_string(), CustomRuleAction::Primary));
        }
        let compiled = CompiledCustomRules::compile(rules);
        if let Some((act, pat)) = compiled.match_ip(ip) {
            return Some(mk(format!("命中自定义 {pat}"), act));
        }
        if data.cn_ip.contains(ip) {
            return Some(mk(
                "内置规则（国内 IP 段）".to_string(),
                if mode == RouteMode::Dual {
                    CustomRuleAction::Domestic
                } else {
                    CustomRuleAction::Direct
                },
            ));
        }
        return Some(mk("内置规则（海外默认）".to_string(), CustomRuleAction::Primary));
    }

    let host = normalize_host(host_only.split(':').next().unwrap_or(""));
    if host.is_empty() {
        return None;
    }
    if host == "localhost" {
        return Some(mk("本机地址".to_string(), CustomRuleAction::Direct));
    }
    if host.ends_with(".local") {
        return Some(mk("隧道内域名 (.local)".to_string(), CustomRuleAction::Primary));
    }
    if mode == RouteMode::Global {
        return Some(mk("全局代理模式".to_string(), CustomRuleAction::Primary));
    }

    let compiled = CompiledCustomRules::compile(rules);
    if let Some((act, pat)) = compiled.match_domain(&host) {
        return Some(mk(format!("命中自定义 {pat}"), act));
    }
    if data.proxy.matches(&host) {
        return Some(mk("内置规则（海外代理表）".to_string(), CustomRuleAction::Primary));
    }
    if data.direct.matches(&host) {
        return Some(mk(
            "内置规则（国内域名表）".to_string(),
            if mode == RouteMode::Dual {
                CustomRuleAction::Domestic
            } else {
                CustomRuleAction::Direct
            },
        ));
    }
    Some(mk(
        "未知域名（将按本地解析 IP 归属或默认主节点）".to_string(),
        CustomRuleAction::Primary,
    ))
}

/// Sorted domain list supporting exact (`full:`) and suffix matches via binary search.
#[derive(Default)]
struct DomainSet {
    suffix: Vec<&'static str>,
    full: Vec<&'static str>,
}

impl DomainSet {
    fn parse(text: &'static str) -> Self {
        let mut set = DomainSet::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(d) = line.strip_prefix("full:") {
                set.full.push(d);
            } else if let Some(d) = line.strip_prefix("domain:") {
                set.suffix.push(d);
            } else if line.contains(':') {
                // regexp:/keyword: entries are rare in these lists; skipped on purpose.
                continue;
            } else {
                set.suffix.push(line);
            }
        }
        set.suffix.sort_unstable();
        set.suffix.dedup();
        set.full.sort_unstable();
        set.full.dedup();
        set
    }

    fn len(&self) -> usize {
        self.suffix.len() + self.full.len()
    }

    /// `host` must already be lowercase without a trailing dot.
    fn matches(&self, host: &str) -> bool {
        if self.full.binary_search(&host).is_ok() {
            return true;
        }
        let mut rest = host;
        loop {
            if self.suffix.binary_search(&rest).is_ok() {
                return true;
            }
            match rest.find('.') {
                Some(i) => rest = &rest[i + 1..],
                None => return false,
            }
        }
    }
}

/// Sorted, merged, non-overlapping inclusive ranges.
#[derive(Default)]
struct IpRanges {
    v4: Vec<(u32, u32)>,
    v6: Vec<(u128, u128)>,
}

impl IpRanges {
    fn parse(text: &str) -> Self {
        let mut v4 = Vec::new();
        let mut v6 = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            let Some((ip, len)) = line.split_once('/') else {
                continue;
            };
            let Ok(len) = len.parse::<u32>() else {
                continue;
            };
            match ip.parse::<IpAddr>() {
                Ok(IpAddr::V4(a)) if len <= 32 => {
                    let start = u32::from(a) & mask32(len);
                    v4.push((start, start | !mask32(len)));
                }
                Ok(IpAddr::V6(a)) if len <= 128 => {
                    let start = u128::from(a) & mask128(len);
                    v6.push((start, start | !mask128(len)));
                }
                _ => {}
            }
        }
        Self {
            v4: merge(v4),
            v6: merge(v6),
        }
    }

    fn contains(&self, ip: IpAddr) -> bool {
        match ip {
            IpAddr::V4(a) => in_ranges(&self.v4, u32::from(a)),
            IpAddr::V6(a) => match a.to_ipv4_mapped() {
                Some(v4) => in_ranges(&self.v4, u32::from(v4)),
                None => in_ranges(&self.v6, u128::from(a)),
            },
        }
    }
}

fn mask32(len: u32) -> u32 {
    if len == 0 { 0 } else { u32::MAX << (32 - len) }
}

fn mask128(len: u32) -> u128 {
    if len == 0 {
        0
    } else {
        u128::MAX << (128 - len)
    }
}

fn merge<T: Ord + Copy + num_succ::Succ>(mut v: Vec<(T, T)>) -> Vec<(T, T)> {
    v.sort_unstable();
    let mut out: Vec<(T, T)> = Vec::with_capacity(v.len());
    for (s, e) in v {
        if let Some(last) = out.last_mut()
            && s <= last.1.succ_saturating()
        {
            if e > last.1 {
                last.1 = e;
            }
            continue;
        }
        out.push((s, e));
    }
    out
}

mod num_succ {
    pub trait Succ {
        fn succ_saturating(self) -> Self;
    }
    impl Succ for u32 {
        fn succ_saturating(self) -> Self {
            self.saturating_add(1)
        }
    }
    impl Succ for u128 {
        fn succ_saturating(self) -> Self {
            self.saturating_add(1)
        }
    }
}

fn in_ranges<T: Ord + Copy>(ranges: &[(T, T)], x: T) -> bool {
    let idx = ranges.partition_point(|&(start, _)| start <= x);
    idx > 0 && x <= ranges[idx - 1].1
}

/// LAN / loopback / link-local / CGNAT / multicast: never worth tunnelling.
pub fn is_local_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => is_local_v4(a),
        IpAddr::V6(a) => {
            if let Some(v4) = a.to_ipv4_mapped() {
                return is_local_v4(v4);
            }
            let seg0 = a.segments()[0];
            a.is_loopback()
                || a.is_unspecified()
                || a.is_multicast()
                || (seg0 & 0xfe00) == 0xfc00 // fc00::/7 unique local
                || (seg0 & 0xffc0) == 0xfe80 // fe80::/10 link local
        }
    }
}

fn is_local_v4(a: Ipv4Addr) -> bool {
    let o = a.octets();
    a.is_private()
        || a.is_loopback()
        || a.is_link_local()
        || a.is_unspecified()
        || a.is_broadcast()
        || a.is_multicast()
        || (o[0] == 100 && (o[1] & 0xc0) == 64) // 100.64.0.0/10 CGNAT
}

/// Immutable rule tables, built once per process (parsing ~140k lines takes tens of ms).
pub struct RuleData {
    direct: DomainSet,
    proxy: DomainSet,
    cn_ip: IpRanges,
}

impl RuleData {
    fn load(rules_dir: Option<&Path>) -> Self {
        let read = |name: &str, embedded: &'static str| -> &'static str {
            rules_dir
                .map(|d| d.join(name))
                .and_then(|p| std::fs::read_to_string(p).ok())
                .filter(|s| !s.trim().is_empty())
                // Leaked once per process: tables are cached for the process lifetime.
                .map(|s| &*Box::leak(s.into_boxed_str()))
                .unwrap_or(embedded)
        };
        Self {
            direct: DomainSet::parse(read("direct-list.txt", EMBEDDED_DIRECT)),
            proxy: DomainSet::parse(read("proxy-list.txt", EMBEDDED_PROXY)),
            cn_ip: IpRanges::parse(read("geoip-cn.txt", EMBEDDED_GEOIP_CN)),
        }
    }

    /// Process-wide shared tables. `rules_dir` is only consulted on the first call;
    /// edited override files take effect after restarting the app.
    pub fn shared(rules_dir: Option<&Path>) -> Arc<RuleData> {
        static DATA: OnceLock<Arc<RuleData>> = OnceLock::new();
        DATA.get_or_init(|| Arc::new(RuleData::load(rules_dir)))
            .clone()
    }

    pub fn summary(&self) -> String {
        format!(
            "直连域名 {} 条 / 代理域名 {} 条 / 国内 IP 段 {} 条",
            self.direct.len(),
            self.proxy.len(),
            self.cn_ip.v4.len() + self.cn_ip.v6.len()
        )
    }
}

pub struct Router {
    mode: Arc<RouteModeCell>,
    domestic: Arc<DomesticEgressCell>,
    custom: Arc<CustomRulesCell>,
    data: Arc<RuleData>,
    /// host -> first resolved IP (None = resolution failed), for the GEOIP fallback.
    dns_cache: DashMap<String, (Option<IpAddr>, Instant)>,
    /// IP -> domain learned from DNS answers seen in TUN mode, so IP-only TUN
    /// connections can still be matched against the domain lists.
    dns_hints: DashMap<IpAddr, Arc<str>>,
    /// Physical-NIC-pinned sockets for DIRECT traffic.
    pub dialer: DirectDialer,
}

/// Where a DNS query seen on the TUN should go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnsRoute {
    /// Domestic domain: rewrite to the domestic resolver and send direct, so the answer
    /// points at the nearest in-China CDN node.
    Domestic,
    /// Send as-is directly (Direct mode).
    Direct,
    /// Through the tunnel to the original resolver (unpolluted, foreign-optimised).
    Proxy,
}

/// Domestic resolver used for split DNS (AliDNS).
pub const DOMESTIC_DNS: std::net::SocketAddr =
    std::net::SocketAddr::V4(std::net::SocketAddrV4::new(Ipv4Addr::new(223, 5, 5, 5), 53));
const DNS_HINTS_MAX: usize = 65536;

/// Domain-list bucket before the egress policy is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DomainBucket {
    /// Localhost / machine-local names: strictly DIRECT even in `Dual` or `Global` mode.
    Local,
    Domestic,
    Foreign,
}

impl Router {
    pub fn new(
        mode: Arc<RouteModeCell>,
        domestic: Arc<DomesticEgressCell>,
        custom: Arc<CustomRulesCell>,
        data: Arc<RuleData>,
    ) -> Self {
        Self {
            mode,
            domestic,
            custom,
            data,
            dns_cache: DashMap::new(),
            dns_hints: DashMap::new(),
            dialer: DirectDialer::default(),
        }
    }

    /// Where the domestic bucket currently goes (only uses secondary/primary in `Dual` mode).
    fn domestic_route(&self) -> Route {
        if self.mode.get() != RouteMode::Dual {
            return Route::Direct;
        }
        match self.domestic.get() {
            DomesticEgress::Direct => Route::Direct,
            DomesticEgress::Secondary => Route::Secondary,
            DomesticEgress::Primary => Route::Primary,
        }
    }

    fn resolve_custom_action(&self, action: CustomRuleAction) -> Route {
        match action {
            CustomRuleAction::Direct => Route::Direct,
            CustomRuleAction::Domestic => self.domestic_route(),
            CustomRuleAction::Primary => Route::Primary,
        }
    }

    /// Route for a DNS query payload (TUN UDP to port 53).
    pub fn decide_dns_query(&self, query: &[u8]) -> DnsRoute {
        match self.mode.get() {
            RouteMode::Global => DnsRoute::Proxy,
            RouteMode::Direct => DnsRoute::Direct,
            RouteMode::Rule | RouteMode::Dual => {
                let Some(name) = crate::dns_msg::question_name(query) else {
                    return DnsRoute::Proxy;
                };
                let norm = normalize_host(&name);
                if norm == "localhost" {
                    return DnsRoute::Domestic;
                }
                if norm.ends_with(".local") {
                    return DnsRoute::Proxy;
                }
                let custom = self.custom.snapshot();
                if let Some((act, _)) = custom.match_domain(&norm) {
                    return match act {
                        CustomRuleAction::Direct | CustomRuleAction::Domestic => DnsRoute::Domestic,
                        CustomRuleAction::Primary => DnsRoute::Proxy,
                    };
                }
                match self.domain_bucket(&norm) {
                    Some(DomainBucket::Domestic | DomainBucket::Local) => DnsRoute::Domestic,
                    _ => DnsRoute::Proxy,
                }
            }
        }
    }

    pub fn has_dns_hint(&self, ip: IpAddr) -> bool {
        self.dns_hints.contains_key(&ip)
    }

    /// Learn IP -> domain hints from a DNS response payload.
    pub fn record_dns_answer(&self, response: &[u8]) {
        let Some((name, ips)) = crate::dns_msg::answer_addresses(response) else {
            return;
        };
        if ips.is_empty() || name.is_empty() {
            return;
        }
        if self.dns_hints.len() >= DNS_HINTS_MAX {
            self.dns_hints.clear();
        }
        let name: Arc<str> = name.into();
        for ip in ips {
            self.dns_hints.insert(ip, name.clone());
        }
    }

    pub fn mode(&self) -> RouteMode {
        self.mode.get()
    }

    /// Decide where `target` goes. Only unknown domains in `Rule` mode may await a
    /// (cached, time-bounded) local DNS lookup; everything else is synchronous.
    pub async fn decide(&self, target: &TargetAddr) -> Route {
        let route = self.decide_inner(target).await;
        tracing::debug!("route {} -> {:?}", target.to_host_port(), route);
        route
    }

    async fn decide_inner(&self, target: &TargetAddr) -> Route {
        match self.mode.get() {
            RouteMode::Direct => return Route::Direct,
            RouteMode::Global => {
                return match target {
                    TargetAddr::Ip(addr) if is_local_ip(addr.ip()) => Route::Direct,
                    TargetAddr::Domain(host, _) => {
                        let h = normalize_host(host);
                        if h == "localhost" || h.parse::<IpAddr>().is_ok_and(is_local_ip) {
                            Route::Direct
                        } else {
                            Route::Primary
                        }
                    }
                    _ => Route::Primary,
                };
            }
            RouteMode::Rule | RouteMode::Dual => {}
        }
        match target {
            TargetAddr::Ip(addr) => self.decide_ip(addr.ip()),
            TargetAddr::Domain(host, _) => {
                let host = normalize_host(host);
                if let Ok(ip) = host.parse::<IpAddr>() {
                    return self.decide_ip(ip);
                }
                if let Some(route) = self.decide_domain_static(&host) {
                    return route;
                }
                match self.resolve_cached(&host).await {
                    Some(ip) if is_local_ip(ip) => Route::Direct,
                    Some(ip) => {
                        if let Some((act, _)) = self.custom.snapshot().match_ip(ip) {
                            return self.resolve_custom_action(act);
                        }
                        if self.data.cn_ip.contains(ip) {
                            self.domestic_route()
                        } else {
                            Route::Primary
                        }
                    }
                    None => Route::Primary,
                }
            }
        }
    }

    /// Synchronous fast-path routing verdict for an already-resolved IP target
    /// (avoids async state-machine overhead on the UDP game packet hot path).
    pub fn decide_ip_sync(&self, ip: IpAddr) -> Route {
        match self.mode.get() {
            RouteMode::Global => {
                if is_local_ip(ip) {
                    Route::Direct
                } else {
                    Route::Primary
                }
            }
            RouteMode::Direct => Route::Direct,
            RouteMode::Rule | RouteMode::Dual => self.decide_ip(ip),
        }
    }

    fn decide_ip(&self, ip: IpAddr) -> Route {
        if is_local_ip(ip) {
            return Route::Direct;
        }
        let custom = self.custom.snapshot();
        let hint = self.dns_hints.get(&ip).map(|h| h.clone());
        // Highest priority: user custom domain rules (via DNS hint), then user custom IP/CIDR rules.
        if let Some(ref name) = hint {
            let norm = normalize_host(name);
            if let Some((act, _)) = custom.match_domain(&norm) {
                return self.resolve_custom_action(act);
            }
        }
        if let Some((act, _)) = custom.match_ip(ip) {
            return self.resolve_custom_action(act);
        }
        // Built-in domain lists (via DNS hint) beat GeoIP.
        if let Some(ref name) = hint
            && let Some(route) = self.decide_domain_builtin(&normalize_host(name))
        {
            return route;
        }
        if self.data.cn_ip.contains(ip) {
            self.domestic_route()
        } else {
            Route::Primary
        }
    }

    /// List-based verdict without any DNS; `None` means "unknown domain".
    fn decide_domain_static(&self, host: &str) -> Option<Route> {
        if host == "localhost" {
            return Some(Route::Direct);
        }
        if host.ends_with(".local") {
            return Some(Route::Primary);
        }
        if let Some((act, _)) = self.custom.snapshot().match_domain(host) {
            return Some(self.resolve_custom_action(act));
        }
        self.decide_domain_builtin(host)
    }

    fn decide_domain_builtin(&self, host: &str) -> Option<Route> {
        Some(match self.domain_bucket(host)? {
            DomainBucket::Local => Route::Direct,
            DomainBucket::Foreign => Route::Primary,
            DomainBucket::Domestic => self.domestic_route(),
        })
    }

    /// Domain-list classification, independent of the current egress policy (so DNS
    /// splitting keeps working when domestic traffic is routed through the domestic node).
    fn domain_bucket(&self, host: &str) -> Option<DomainBucket> {
        // Localhost stays strictly local; tunnel-only `.local` aliases (server panel) go primary.
        if host == "localhost" {
            return Some(DomainBucket::Local);
        }
        if host.ends_with(".local") {
            return Some(DomainBucket::Foreign);
        }
        if self.data.proxy.matches(host) {
            return Some(DomainBucket::Foreign);
        }
        if self.data.direct.matches(host) {
            return Some(DomainBucket::Domestic);
        }
        None
    }

    /// Resolve `host` with the local (ISP) resolver, cached; used for GEOIP fallback and
    /// for addressing direct UDP to domain targets.
    pub async fn resolve_cached(&self, host: &str) -> Option<IpAddr> {
        if let Some(hit) = self.dns_cache.get(host)
            && hit.1.elapsed() < DNS_CACHE_TTL
        {
            return hit.0;
        }
        let ip = tokio::time::timeout(GEOIP_RESOLVE_TIMEOUT, tokio::net::lookup_host((host, 0)))
            .await
            .ok()
            .and_then(|r| r.ok())
            .and_then(|mut it| it.next())
            .map(|a| a.ip());
        if self.dns_cache.len() >= DNS_CACHE_MAX {
            self.dns_cache.clear();
        }
        self.dns_cache
            .insert(host.to_string(), (ip, Instant::now()));
        ip
    }
}

fn normalize_host(host: &str) -> String {
    host.trim()
        .trim_end_matches('.')
        .trim_matches(['[', ']'])
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn router(mode: RouteMode) -> Router {
        router_with(mode, DomesticEgress::Direct)
    }

    fn router_with(mode: RouteMode, domestic: DomesticEgress) -> Router {
        Router::new(
            RouteModeCell::new(mode),
            DomesticEgressCell::new(domestic),
            CustomRulesCell::empty(),
            RuleData::shared(None),
        )
    }

    fn dom(h: &str) -> TargetAddr {
        TargetAddr::Domain(h.to_string(), 443)
    }

    fn ip(s: &str) -> TargetAddr {
        TargetAddr::Ip(s.parse::<SocketAddr>().unwrap())
    }

    #[tokio::test]
    async fn chinese_sites_go_direct_foreign_sites_go_proxy() {
        let r = router(RouteMode::Rule);
        for d in [
            "www.baidu.com",
            "baidu.com",
            "WWW.QQ.COM.",
            "api.bilibili.com",
            "gov.cn",
            "a.b.edu.cn",
        ] {
            assert_eq!(r.decide(&dom(d)).await, Route::Direct, "{d}");
        }
        for d in ["www.google.com", "youtube.com", "i.ytimg.com", "github.com"] {
            assert_eq!(r.decide(&dom(d)).await, Route::Primary, "{d}");
        }
        // Tunnel-only panel alias must never be resolved locally or sent direct
        assert_eq!(r.decide(&dom("panel.gacl.local")).await, Route::Primary);
    }

    #[tokio::test]
    async fn domestic_bucket_follows_egress_policy() {
        let r = router_with(RouteMode::Dual, DomesticEgress::Secondary);
        for d in ["www.baidu.com", "api.bilibili.com", "gov.cn"] {
            assert_eq!(r.decide(&dom(d)).await, Route::Secondary, "{d}");
        }
        assert_eq!(r.decide(&dom("www.google.com")).await, Route::Primary);
        assert_eq!(r.decide(&ip("223.5.5.5:53")).await, Route::Secondary);
        assert_eq!(r.decide(&ip("8.8.8.8:53")).await, Route::Primary);
        // LAN and localhost MUST always stay direct even in Dual mode!
        assert_eq!(r.decide(&ip("192.168.1.1:80")).await, Route::Direct);
        assert_eq!(r.decide(&ip("127.0.0.1:8080")).await, Route::Direct);
        assert_eq!(r.decide(&dom("localhost")).await, Route::Direct);

        // Rule mode ignores Secondary domestic egress and keeps CN direct
        let r_rule = router_with(RouteMode::Rule, DomesticEgress::Secondary);
        assert_eq!(r_rule.decide(&dom("www.baidu.com")).await, Route::Direct);
        assert_eq!(r_rule.decide(&dom("www.google.com")).await, Route::Primary);

        // DNS splitting classifies by the domain bucket, not the egress
        use crate::dns_msg::tests::build_query;
        assert_eq!(
            r.decide_dns_query(&build_query(1, "www.baidu.com")),
            DnsRoute::Domestic
        );
        assert_eq!(
            r.decide_dns_query(&build_query(2, "www.google.com")),
            DnsRoute::Proxy
        );
    }

    #[tokio::test]
    async fn domestic_egress_downgrades_live() {
        let cell = DomesticEgressCell::new(DomesticEgress::Secondary);
        let r = Router::new(
            RouteModeCell::new(RouteMode::Dual),
            cell.clone(),
            CustomRulesCell::empty(),
            RuleData::shared(None),
        );
        assert_eq!(r.decide(&dom("www.baidu.com")).await, Route::Secondary);
        cell.set(DomesticEgress::Direct);
        assert_eq!(r.decide(&dom("www.baidu.com")).await, Route::Direct);
        assert_eq!(
            r.decide_ip_sync("223.5.5.5".parse().unwrap()),
            Route::Direct
        );
    }

    #[tokio::test]
    async fn ip_targets_use_geoip_and_lan() {
        let r = router(RouteMode::Rule);
        assert_eq!(r.decide(&ip("114.114.114.114:53")).await, Route::Direct); // CN DNS
        assert_eq!(r.decide(&ip("223.5.5.5:53")).await, Route::Direct); // AliDNS
        assert_eq!(r.decide(&ip("8.8.8.8:53")).await, Route::Primary);
        assert_eq!(r.decide(&ip("1.1.1.1:443")).await, Route::Primary);
        assert_eq!(r.decide(&ip("192.168.1.1:80")).await, Route::Direct);
        assert_eq!(r.decide(&ip("127.0.0.1:80")).await, Route::Direct);
        assert_eq!(r.decide(&ip("[::1]:80")).await, Route::Direct);
        assert_eq!(
            r.decide(&ip("[2001:4860:4860::8888]:53")).await,
            Route::Primary
        );
        // Tunnel-only panel TUN alias (198.18.0.1) is not local -> must be tunnelled
        assert_eq!(r.decide(&ip("198.18.0.1:80")).await, Route::Primary);
    }

    #[tokio::test]
    async fn modes_override_rules_and_switch_live() {
        let cell = RouteModeCell::new(RouteMode::Global);
        let r = Router::new(
            cell.clone(),
            DomesticEgressCell::new(DomesticEgress::Direct),
            CustomRulesCell::empty(),
            RuleData::shared(None),
        );
        assert_eq!(r.decide(&dom("www.baidu.com")).await, Route::Primary);
        cell.set(RouteMode::Direct);
        assert_eq!(r.decide(&dom("www.google.com")).await, Route::Direct);
        cell.set(RouteMode::Rule);
        assert_eq!(r.decide(&dom("www.baidu.com")).await, Route::Direct);
    }

    #[test]
    fn dns_split_and_hints() {
        use crate::dns_msg::tests::build_query;
        let r = router(RouteMode::Rule);
        assert_eq!(
            r.decide_dns_query(&build_query(1, "www.baidu.com")),
            DnsRoute::Domestic
        );
        assert_eq!(
            r.decide_dns_query(&build_query(2, "www.google.com")),
            DnsRoute::Proxy
        );
        assert_eq!(
            r.decide_dns_query(&build_query(3, "unknown-zzz.example")),
            DnsRoute::Proxy
        );

        // A foreign IP learned as belonging to a domestic domain goes direct (and vice versa).
        let foreign_ip: IpAddr = "8.8.4.4".parse().unwrap();
        assert_eq!(r.decide_ip(foreign_ip), Route::Primary);
        let mut resp = build_query(4, "www.baidu.com");
        resp[2] = 0x81;
        resp[7] = 1;
        resp.extend_from_slice(&[0xC0, 0x0C, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 8, 8, 4, 4]);
        r.record_dns_answer(&resp);
        assert_eq!(r.decide_ip(foreign_ip), Route::Direct);
    }

    #[test]
    fn domain_set_suffix_semantics() {
        let set = DomainSet::parse("example.com\nfull:exact.org\nregexp:.*\n");
        assert!(set.matches("example.com"));
        assert!(set.matches("a.b.example.com"));
        assert!(!set.matches("notexample.com"));
        assert!(set.matches("exact.org"));
        assert!(!set.matches("sub.exact.org"));
    }

    #[test]
    fn ip_ranges_merge_and_lookup() {
        let r = IpRanges::parse("10.0.0.0/8\n10.1.0.0/16\n11.0.0.0/8\n2400:da00::/32\n");
        assert_eq!(r.v4.len(), 1, "adjacent/overlapping ranges merge");
        assert!(r.contains("10.200.1.1".parse().unwrap()));
        assert!(r.contains("11.255.255.255".parse().unwrap()));
        assert!(!r.contains("12.0.0.0".parse().unwrap()));
        assert!(r.contains("2400:da00::1".parse().unwrap()));
        assert!(r.contains("::ffff:10.0.0.1".parse().unwrap()));
    }

    #[tokio::test]
    async fn custom_rules_override_builtin_lists_and_hot_reload() {
        use crate::dns_msg::tests::build_query;
        let mode_cell = RouteModeCell::new(RouteMode::Dual);
        let custom_cell = CustomRulesCell::new(&[
            CustomRule {
                pattern: "github.com".to_string(),
                action: CustomRuleAction::Direct,
                enabled: true,
            },
            CustomRule {
                pattern: "45.67.89.10".to_string(),
                action: CustomRuleAction::Domestic,
                enabled: true,
            },
            CustomRule {
                pattern: "full:www.baidu.com".to_string(),
                action: CustomRuleAction::Primary,
                enabled: true,
            },
        ]);
        let r = Router::new(
            mode_cell.clone(),
            DomesticEgressCell::new(DomesticEgress::Secondary),
            custom_cell.clone(),
            RuleData::shared(None),
        );

        // Foreign proxy-list domain overridden to Direct stays Direct even in Dual mode
        assert_eq!(r.decide(&dom("api.github.com")).await, Route::Direct);
        assert_eq!(
            r.decide_dns_query(&build_query(1, "api.github.com")),
            DnsRoute::Domestic
        );
        // Foreign IP overridden to Domestic uses Secondary in Dual, falls back to Direct in Rule
        assert_eq!(r.decide(&ip("45.67.89.10:443")).await, Route::Secondary);
        // Exact CN domain overridden to Primary beats direct-list & uses foreign DNS
        assert_eq!(r.decide(&dom("www.baidu.com")).await, Route::Primary);
        assert_eq!(r.decide(&dom("tieba.baidu.com")).await, Route::Secondary);
        assert_eq!(
            r.decide_dns_query(&build_query(2, "www.baidu.com")),
            DnsRoute::Proxy
        );

        // Switching to Rule mode makes Domestic custom rule fall back to Direct
        mode_cell.set(RouteMode::Rule);
        assert_eq!(r.decide(&ip("45.67.89.10:443")).await, Route::Direct);

        // Hot-reload custom rules live without recreating Router
        custom_cell.replace(&[]);
        assert_eq!(r.decide(&dom("api.github.com")).await, Route::Primary);
        assert_eq!(r.decide(&ip("45.67.89.10:443")).await, Route::Primary);
    }

    #[test]
    fn custom_rule_normalize_and_bulk_roundtrip() {
        assert_eq!(
            CustomRule::normalize_pattern("https://cdn.steampowered.com/foo").as_deref(),
            Some("cdn.steampowered.com")
        );
        assert_eq!(
            CustomRule::normalize_pattern("*.steampowered.com").as_deref(),
            Some("steampowered.com")
        );
        assert_eq!(
            CustomRule::normalize_pattern("full:https://dl.google.com/a").as_deref(),
            Some("full:dl.google.com")
        );
        assert_eq!(
            CustomRule::normalize_pattern("45.67.89.10").as_deref(),
            Some("45.67.89.10/32")
        );
        assert_eq!(
            CustomRule::normalize_pattern("45.67.89.10:443").as_deref(),
            Some("45.67.89.10/32")
        );
        assert_eq!(
            CustomRule::normalize_pattern("104.16.0.0/16").as_deref(),
            Some("104.16.0.0/16")
        );
        assert_eq!(CustomRule::normalize_pattern("104.16.0.0/99"), None);
        assert_eq!(CustomRule::normalize_pattern("123.456.789.0"), None);

        let parsed = parse_bulk_custom_rules_checked(
            "# comment\nsteampowered.com, direct\n!45.67.89.10 domestic # paused\nfull:a.b.com, primary\n",
        )
        .unwrap();
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].action, CustomRuleAction::Direct);
        assert!(parsed[0].enabled);
        assert_eq!(parsed[1].pattern, "45.67.89.10/32");
        assert_eq!(parsed[1].action, CustomRuleAction::Domestic);
        assert!(!parsed[1].enabled);
        assert_eq!(parsed[2].pattern, "full:a.b.com");
        assert_eq!(parsed[2].action, CustomRuleAction::Primary);

        // Paused rules survive format -> parse round-trip intact
        let roundtrip =
            parse_bulk_custom_rules_checked(&format_bulk_custom_rules(&parsed)).unwrap();
        assert_eq!(roundtrip, parsed);

        // Malformed lines return a clear line-numbered error
        assert!(parse_bulk_custom_rules_checked("steampowered.com, bogus").is_err());
        assert!(parse_bulk_custom_rules_checked("1.2.3.4/99, direct").is_err());
    }
}
