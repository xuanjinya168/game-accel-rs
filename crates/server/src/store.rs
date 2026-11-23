use anyhow::{Context, Result, bail};
use chrono::Local;
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use protocol::share::{NodeProfile, SubscriptionPayload};
use rand::RngCore;
use rand::rngs::OsRng;
use ring::{digest, pbkdf2};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Write;
use std::net::IpAddr;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tracing::info;

const MAX_MEM_ACCESS_LOGS: usize = 500;
const MAX_ACCESS_LOG_FILE_BYTES: u64 = 10 * 1024 * 1024; // 10 MB rotation

const TOKEN_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
const SESSION_TTL: Duration = Duration::from_secs(24 * 3600);
const PBKDF2_ITERS: u32 = 100_000;
const LOGIN_MAX_FAILURES: u32 = 10;
const LOGIN_WINDOW: Duration = Duration::from_secs(5 * 60);
const LOGIN_FAILURES_CAP: usize = 4096;
const PBKDF2_PREFIX: &str = "pbkdf2$";

pub fn generate_random_str(len: usize) -> String {
    let mut bytes = vec![0u8; len];
    OsRng.fill_bytes(&mut bytes);
    bytes
        .into_iter()
        .map(|b| TOKEN_ALPHABET[(b as usize) % TOKEN_ALPHABET.len()] as char)
        .collect()
}

/// Length-hiding constant-time equality: hashes both inputs to fixed 32-byte SHA-256 digests
/// and folds all 32 bytes without short-circuiting so neither content nor length leaks via timing.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let da = digest::digest(&digest::SHA256, a.as_bytes());
    let db = digest::digest(&digest::SHA256, b.as_bytes());
    let mut diff = 0u8;
    for (&x, &y) in da.as_ref().iter().zip(db.as_ref().iter()) {
        diff |= std::hint::black_box(x ^ y);
    }
    std::hint::black_box(diff) == 0
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
    }
    Some(out)
}

/// Hash a panel password with random 16-byte salt and PBKDF2-HMAC-SHA256.
/// Format: `pbkdf2$<iters>$<salt_hex>$<dk_hex>`.
pub fn hash_password(plain: &str) -> String {
    let mut salt = [0u8; 16];
    OsRng.fill_bytes(&mut salt);
    let mut dk = [0u8; 32];
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        NonZeroU32::new(PBKDF2_ITERS).expect("non-zero iterations"),
        &salt,
        plain.as_bytes(),
        &mut dk,
    );
    format!(
        "{PBKDF2_PREFIX}{PBKDF2_ITERS}${}${}",
        hex_encode(&salt),
        hex_encode(&dk)
    )
}

/// Verify a candidate password against a stored PBKDF2 hash (or legacy plaintext fallback).
pub fn verify_password(candidate: &str, stored: &str) -> bool {
    if let Some(rest) = stored.strip_prefix(PBKDF2_PREFIX) {
        let mut parts = rest.split('$');
        let Some(iters_str) = parts.next() else {
            return false;
        };
        let Some(salt_hex) = parts.next() else {
            return false;
        };
        let Some(dk_hex) = parts.next() else {
            return false;
        };
        if parts.next().is_some() {
            return false;
        }
        let Ok(iters) = iters_str.parse::<u32>() else {
            return false;
        };
        let Some(iters_nz) = NonZeroU32::new(iters) else {
            return false;
        };
        let (Some(salt), Some(dk)) = (hex_decode(salt_hex), hex_decode(dk_hex)) else {
            return false;
        };
        pbkdf2::verify(
            pbkdf2::PBKDF2_HMAC_SHA256,
            iters_nz,
            &salt,
            candidate.as_bytes(),
            &dk,
        )
        .is_ok()
    } else {
        constant_time_eq(candidate, stored)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkRecord {
    pub id: String,
    pub name: String,
    pub token: String,
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub game_dup: bool,
    #[serde(default)]
    pub traffic_limit_gb: Option<f64>,
    #[serde(default)]
    pub expire_at: Option<String>, // YYYY-MM-DD
    #[serde(default)]
    pub tx_bytes: u64,
    #[serde(default)]
    pub rx_bytes: u64,
    pub created_at: String,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelConfig {
    pub admin_user: String,
    pub admin_pass: String,
    pub public_host: String,
    pub udp_port: u16,
    pub sni: String,
    pub insecure: bool,
    #[serde(default = "default_true")]
    pub access_log_enabled: bool,
    pub links: Vec<LinkRecord>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccessLogEntry {
    pub id: u64,
    pub time: String,
    pub link_id: String,
    pub link_name: String,
    pub client_ip: String,
    pub proto: String,
    pub root_domain: String,
    pub target: String,
    pub tx_bytes: u64,
    pub rx_bytes: u64,
    pub duration_ms: u64,
    pub status: String,
}

#[derive(Debug, Serialize)]
pub struct AccessLogsResponse {
    pub enabled: bool,
    pub total_buffered: usize,
    pub logs: Vec<AccessLogEntry>,
}

#[derive(Debug, Default)]
pub struct LiveCounters {
    pub tx_bytes: AtomicU64,
    pub rx_bytes: AtomicU64,
    pub online_conns: AtomicU64,
    pub active_conns: DashMap<u64, quinn::Connection>,
}

impl LiveCounters {
    pub fn from_persisted(tx: u64, rx: u64) -> Self {
        Self {
            tx_bytes: AtomicU64::new(tx),
            rx_bytes: AtomicU64::new(rx),
            online_conns: AtomicU64::new(0),
            active_conns: DashMap::new(),
        }
    }

    pub fn kick_all(&self) {
        let keys: Vec<u64> = self.active_conns.iter().map(|e| *e.key()).collect();
        for k in keys {
            if let Some((_, conn)) = self.active_conns.remove(&k) {
                conn.close(1u32.into(), b"link_disabled_or_reset");
            }
        }
    }
}

pub struct AuthTicket {
    pub link_id: String,
    pub link_name: String,
    pub counters: Arc<LiveCounters>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LinkView {
    pub id: String,
    pub name: String,
    pub token: String,
    pub enabled: bool,
    pub game_dup: bool,
    pub traffic_limit_gb: Option<f64>,
    pub expire_at: Option<String>,
    pub tx_bytes: u64,
    pub rx_bytes: u64,
    pub online_conns: u64,
    pub created_at: String,
    pub share_uri: String,
    pub status: String, // "active" | "disabled" | "expired" | "over_limit"
}

#[derive(Debug, Clone, Serialize)]
pub struct OverviewView {
    pub uptime_secs: u64,
    pub public_host: String,
    pub udp_port: u16,
    pub sni: String,
    pub insecure: bool,
    pub total_links: usize,
    pub active_links: usize,
    pub total_online_conns: u64,
    pub total_tx_bytes: u64,
    pub total_rx_bytes: u64,
}

#[derive(Debug, Deserialize)]
pub struct CreateLinkInput {
    pub name: String,
    #[serde(default = "default_true")]
    pub game_dup: bool,
    pub traffic_limit_gb: Option<f64>,
    pub expire_at: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateLinkInput {
    pub name: Option<String>,
    pub enabled: Option<bool>,
    pub game_dup: Option<bool>,
    // Double Option: absent key => None (keep), explicit null => Some(None) (clear),
    // value => Some(Some(v)). Plain serde would map null to the outer None.
    #[serde(default, deserialize_with = "deserialize_some")]
    pub traffic_limit_gb: Option<Option<f64>>,
    #[serde(default, deserialize_with = "deserialize_some")]
    pub expire_at: Option<Option<String>>,
}

fn deserialize_some<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[derive(Debug, Deserialize)]
pub struct UpdateNodeSettingsInput {
    pub public_host: String,
    pub udp_port: u16,
    pub sni: String,
    pub insecure: bool,
}

pub struct AppState {
    data_path: PathBuf,
    access_log_path: PathBuf,
    pub config: RwLock<PanelConfig>,
    pub live: DashMap<String, Arc<LiveCounters>>,
    sessions: DashMap<String, Instant>,
    login_failures: DashMap<IpAddr, (u32, Instant)>,
    next_conn_id: AtomicU64,
    next_log_id: AtomicU64,
    access_log_enabled: AtomicBool,
    access_logs: Mutex<VecDeque<AccessLogEntry>>,
    log_file_lock: Mutex<()>,
    started_at: Instant,
}

pub struct InitParams {
    pub data_path: PathBuf,
    pub seed_token: String,
    pub public_host: String,
    pub udp_port: u16,
    pub sni: String,
    pub insecure: bool,
    pub initial_panel_pass: Option<String>,
}

impl AppState {
    pub fn load_or_init(params: InitParams) -> Result<(Arc<Self>, Option<String>)> {
        let (cfg, newly_generated_pass) = if params.data_path.exists() {
            let text = std::fs::read_to_string(&params.data_path)
                .with_context(|| format!("read {:?}", params.data_path))?;
            let mut cfg: PanelConfig = serde_json::from_str(&text)
                .with_context(|| format!("parse {:?}", params.data_path))?;
            // Sync runtime CLI udp_port & insecure mode if changed
            cfg.udp_port = params.udp_port;
            cfg.insecure = params.insecure;
            if let Some(pass) = params.initial_panel_pass {
                if !pass.is_empty() {
                    cfg.admin_pass = hash_password(&pass);
                }
            }
            // Transparently migrate legacy plaintext admin_pass to salted PBKDF2-HMAC-SHA256
            if !cfg.admin_pass.starts_with(PBKDF2_PREFIX) {
                cfg.admin_pass = hash_password(&cfg.admin_pass);
            }
            (cfg, None)
        } else {
            let admin_pass = params
                .initial_panel_pass
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| generate_random_str(12));
            let now = Local::now().format("%Y-%m-%d %H:%M").to_string();
            let default_link = LinkRecord {
                id: format!("lnk-{}", generate_random_str(8).to_lowercase()),
                name: format!("默认主力节点 ({})", params.sni),
                token: params.seed_token,
                enabled: true,
                game_dup: true,
                traffic_limit_gb: None,
                expire_at: None,
                tx_bytes: 0,
                rx_bytes: 0,
                created_at: now,
            };
            let cfg = PanelConfig {
                admin_user: "admin".to_string(),
                admin_pass: hash_password(&admin_pass),
                public_host: params.public_host,
                udp_port: params.udp_port,
                sni: params.sni,
                insecure: params.insecure,
                access_log_enabled: true,
                links: vec![default_link],
            };
            (cfg, Some(admin_pass))
        };

        let live = DashMap::new();
        for link in &cfg.links {
            live.insert(
                link.id.clone(),
                Arc::new(LiveCounters::from_persisted(link.tx_bytes, link.rx_bytes)),
            );
        }

        let access_log_enabled = cfg.access_log_enabled;
        let access_log_path = params
            .data_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.join("access.log"))
            .unwrap_or_else(|| PathBuf::from("access.log"));

        let state = Arc::new(Self {
            data_path: params.data_path,
            access_log_path,
            config: RwLock::new(cfg),
            live,
            sessions: DashMap::new(),
            login_failures: DashMap::new(),
            next_conn_id: AtomicU64::new(1),
            next_log_id: AtomicU64::new(1),
            access_log_enabled: AtomicBool::new(access_log_enabled),
            access_logs: Mutex::new(VecDeque::with_capacity(MAX_MEM_ACCESS_LOGS)),
            log_file_lock: Mutex::new(()),
            started_at: Instant::now(),
        });
        state.sync_and_save()?;
        Ok((state, newly_generated_pass))
    }

    pub fn sync_and_save(&self) -> Result<()> {
        let snapshot = {
            let mut cfg = self.config.write().expect("config write lock");
            for link in &mut cfg.links {
                if let Some(c) = self.live.get(&link.id) {
                    link.tx_bytes = c.tx_bytes.load(Ordering::Relaxed);
                    link.rx_bytes = c.rx_bytes.load(Ordering::Relaxed);
                }
            }
            cfg.clone()
        };

        if let Some(parent) = self.data_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let tmp = self.data_path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(&snapshot)?;
        std::fs::write(&tmp, text)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, &self.data_path)?;
        Ok(())
    }

    pub fn data_path(&self) -> &Path {
        &self.data_path
    }

    // ---------- Access Logging (Ring Buffer + 10MB Rotating File) ----------

    pub fn is_access_log_enabled(&self) -> bool {
        self.access_log_enabled.load(Ordering::Relaxed)
    }

    pub fn set_access_log_enabled(&self, enabled: bool) -> Result<bool> {
        self.access_log_enabled.store(enabled, Ordering::Relaxed);
        {
            let mut cfg = self.config.write().expect("config write lock");
            cfg.access_log_enabled = enabled;
        }
        self.sync_and_save()?;
        Ok(enabled)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_access_log(
        &self,
        link_id: &str,
        link_name: &str,
        client_ip: &str,
        proto: &str,
        root_domain: String,
        target: String,
        tx_bytes: u64,
        rx_bytes: u64,
        duration_ms: u64,
        status: &str,
    ) {
        if !self.is_access_log_enabled() {
            return;
        }
        let id = self.next_log_id.fetch_add(1, Ordering::Relaxed);
        let time = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let entry = AccessLogEntry {
            id,
            time: time.clone(),
            link_id: link_id.to_string(),
            link_name: link_name.to_string(),
            client_ip: client_ip.to_string(),
            proto: proto.to_string(),
            root_domain: root_domain.clone(),
            target: target.clone(),
            tx_bytes,
            rx_bytes,
            duration_ms,
            status: status.to_string(),
        };

        {
            let mut ring = self.access_logs.lock().unwrap_or_else(|e| e.into_inner());
            if ring.len() >= MAX_MEM_ACCESS_LOGS {
                ring.pop_front();
            }
            ring.push_back(entry);
        }

        // Persist compact line to rotating access.log (10 MB cap)
        if let Ok(_guard) = self.log_file_lock.lock() {
            if let Ok(meta) = std::fs::metadata(&self.access_log_path) {
                if meta.len() >= MAX_ACCESS_LOG_FILE_BYTES {
                    let old = self.access_log_path.with_extension("log.old");
                    let _ = std::fs::rename(&self.access_log_path, old);
                }
            }
            if let Ok(mut f) = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.access_log_path)
            {
                let _ = writeln!(
                    f,
                    "[{time}] [{status}] [{proto}] user=\"{link_name}\" ip={client_ip} root={root_domain} target={target} tx={tx_bytes} rx={rx_bytes} dur={duration_ms}ms"
                );
            }
        }
    }

    pub fn query_access_logs(
        &self,
        link_id: Option<&str>,
        proto: Option<&str>,
        keyword: Option<&str>,
    ) -> AccessLogsResponse {
        let ring = self.access_logs.lock().unwrap_or_else(|e| e.into_inner());
        let total_buffered = ring.len();
        let kw = keyword.map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty());
        let lid = link_id.map(|s| s.trim()).filter(|s| !s.is_empty());
        let pr = proto.map(|s| s.trim().to_ascii_uppercase()).filter(|s| !s.is_empty());

        let logs: Vec<AccessLogEntry> = ring
            .iter()
            .rev()
            .filter(|e| {
                if let Some(id) = lid {
                    if e.link_id != id {
                        return false;
                    }
                }
                if let Some(p) = &pr {
                    if e.proto != *p {
                        return false;
                    }
                }
                if let Some(q) = &kw {
                    let hit = e.root_domain.to_ascii_lowercase().contains(q)
                        || e.target.to_ascii_lowercase().contains(q)
                        || e.link_name.to_ascii_lowercase().contains(q)
                        || e.client_ip.to_ascii_lowercase().contains(q);
                    if !hit {
                        return false;
                    }
                }
                true
            })
            .take(200)
            .cloned()
            .collect();

        AccessLogsResponse {
            enabled: self.is_access_log_enabled(),
            total_buffered,
            logs,
        }
    }

    pub fn clear_access_logs(&self) {
        if let Ok(mut ring) = self.access_logs.lock() {
            ring.clear();
        }
        if let Ok(_guard) = self.log_file_lock.lock() {
            let _ = std::fs::write(&self.access_log_path, b"");
        }
    }

    // ---------- Auth & Sessions (Web Panel) ----------

    pub fn verify_login(&self, user: &str, pass: &str) -> Option<String> {
        let cfg = self.config.read().expect("config read lock");
        if constant_time_eq(user.trim(), &cfg.admin_user) && verify_password(pass, &cfg.admin_pass)
        {
            let sid = generate_random_str(32);
            self.sessions.retain(|_, ts| ts.elapsed() < SESSION_TTL);
            self.sessions.insert(sid.clone(), Instant::now());
            Some(sid)
        } else {
            None
        }
    }

    pub fn verify_session(&self, sid: &str) -> bool {
        if let Some(ts) = self.sessions.get(sid) {
            if ts.elapsed() < SESSION_TTL {
                return true;
            }
        }
        self.sessions.remove(sid);
        false
    }

    /// Err(remaining lock time) once an IP has too many failed logins inside the window.
    pub fn check_login_throttle(&self, ip: IpAddr) -> std::result::Result<(), Duration> {
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

    pub fn record_login_result(&self, ip: IpAddr, success: bool) {
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

    /// Current (sni, public_host), read live so panel setting changes apply without restart.
    pub fn panel_aliases(&self) -> (String, String) {
        let cfg = self.config.read().expect("config read lock");
        (cfg.sni.clone(), cfg.public_host.clone())
    }

    pub fn revoke_session(&self, sid: &str) {
        self.sessions.remove(sid);
    }

    pub fn update_admin_password(&self, old_pass: &str, new_pass: &str) -> Result<()> {
        if new_pass.len() < 6 {
            bail!("新密码长度至少 6 位");
        }
        {
            let mut cfg = self.config.write().expect("config write lock");
            if !verify_password(old_pass, &cfg.admin_pass) {
                bail!("原密码不正确");
            }
            cfg.admin_pass = hash_password(new_pass);
        }
        self.sessions.clear();
        self.sync_and_save()
    }

    // ---------- QUIC Data-Plane Token Auth & Live Tracking ----------

    pub fn authenticate_token(&self, token: &str) -> Result<AuthTicket> {
        let cfg = self.config.read().expect("config read lock");
        let link = cfg
            .links
            .iter()
            .find(|l| constant_time_eq(&l.token, token))
            .context("token not found")?;

        let counters = self
            .live
            .entry(link.id.clone())
            .or_insert_with(|| Arc::new(LiveCounters::from_persisted(link.tx_bytes, link.rx_bytes)))
            .clone();

        let status = evaluate_link_status(link, &counters);
        match status.as_str() {
            "active" => Ok(AuthTicket {
                link_id: link.id.clone(),
                link_name: link.name.clone(),
                counters,
            }),
            "disabled" => bail!("link '{}' is disabled", link.name),
            "expired" => bail!("link '{}' is expired", link.name),
            "over_limit" => bail!("link '{}' exceeded traffic limit", link.name),
            other => bail!("link '{}' unavailable ({other})", link.name),
        }
    }

    pub fn register_conn(&self, ticket: &AuthTicket, conn: quinn::Connection) -> u64 {
        let cid = self.next_conn_id.fetch_add(1, Ordering::Relaxed);
        ticket.counters.active_conns.insert(cid, conn);
        ticket.counters.online_conns.fetch_add(1, Ordering::Relaxed);
        cid
    }

    pub fn unregister_conn(&self, ticket: &AuthTicket, cid: u64) {
        ticket.counters.active_conns.remove(&cid);
        ticket
            .counters
            .online_conns
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            })
            .ok();
    }

    // ---------- Panel Queries & CRUD ----------

    pub fn overview(&self) -> OverviewView {
        let cfg = self.config.read().expect("config read lock");
        let mut active_links = 0usize;
        let mut total_online = 0u64;
        let mut total_tx = 0u64;
        let mut total_rx = 0u64;

        for link in &cfg.links {
            let counters = self
                .live
                .get(&link.id)
                .map(|c| c.clone())
                .unwrap_or_else(|| Arc::new(LiveCounters::from_persisted(link.tx_bytes, link.rx_bytes)));
            if evaluate_link_status(link, &counters) == "active" {
                active_links += 1;
            }
            total_online += counters.online_conns.load(Ordering::Relaxed);
            total_tx += counters.tx_bytes.load(Ordering::Relaxed);
            total_rx += counters.rx_bytes.load(Ordering::Relaxed);
        }

        OverviewView {
            uptime_secs: self.started_at.elapsed().as_secs(),
            public_host: cfg.public_host.clone(),
            udp_port: cfg.udp_port,
            sni: cfg.sni.clone(),
            insecure: cfg.insecure,
            total_links: cfg.links.len(),
            active_links,
            total_online_conns: total_online,
            total_tx_bytes: total_tx,
            total_rx_bytes: total_rx,
        }
    }

    pub fn list_links(&self) -> Vec<LinkView> {
        let cfg = self.config.read().expect("config read lock");
        cfg.links
            .iter()
            .map(|link| self.to_link_view(&cfg, link))
            .collect()
    }

    pub fn create_link(&self, input: CreateLinkInput) -> Result<LinkView> {
        let name = input.name.trim();
        if name.is_empty() {
            bail!("链接备注名不能为空");
        }
        let expire_at = input
            .expire_at
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let traffic_limit_gb = input.traffic_limit_gb.filter(|&v| v > 0.0);

        let record = LinkRecord {
            id: format!("lnk-{}", generate_random_str(8).to_lowercase()),
            name: name.to_string(),
            token: generate_random_str(24),
            enabled: true,
            game_dup: input.game_dup,
            traffic_limit_gb,
            expire_at,
            tx_bytes: 0,
            rx_bytes: 0,
            created_at: Local::now().format("%Y-%m-%d %H:%M").to_string(),
        };

        self.live
            .insert(record.id.clone(), Arc::new(LiveCounters::default()));

        let view = {
            let mut cfg = self.config.write().expect("config write lock");
            cfg.links.push(record.clone());
            self.to_link_view(&cfg, &record)
        };
        self.sync_and_save()?;
        info!("panel created link '{}' (id={})", view.name, view.id);
        Ok(view)
    }

    pub fn update_link(&self, id: &str, input: UpdateLinkInput) -> Result<LinkView> {
        let mut kick_needed = false;
        let view = {
            let mut cfg = self.config.write().expect("config write lock");
            let idx = cfg
                .links
                .iter()
                .position(|l| l.id == id)
                .context("link not found")?;
            let link = &mut cfg.links[idx];
            if let Some(name) = input.name {
                let n = name.trim();
                if !n.is_empty() {
                    link.name = n.to_string();
                }
            }
            if let Some(enabled) = input.enabled {
                if link.enabled && !enabled {
                    kick_needed = true;
                }
                link.enabled = enabled;
            }
            if let Some(dup) = input.game_dup {
                link.game_dup = dup;
            }
            if let Some(limit) = input.traffic_limit_gb {
                link.traffic_limit_gb = limit.filter(|&v| v > 0.0);
            }
            if let Some(exp) = input.expire_at {
                link.expire_at = exp.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
            }
            let cloned = link.clone();
            self.to_link_view(&cfg, &cloned)
        };

        if kick_needed {
            if let Some(c) = self.live.get(id) {
                c.kick_all();
            }
        }
        self.sync_and_save()?;
        Ok(view)
    }

    pub fn reset_link_token(&self, id: &str) -> Result<LinkView> {
        let view = {
            let mut cfg = self.config.write().expect("config write lock");
            let idx = cfg
                .links
                .iter()
                .position(|l| l.id == id)
                .context("link not found")?;
            cfg.links[idx].token = generate_random_str(24);
            let cloned = cfg.links[idx].clone();
            self.to_link_view(&cfg, &cloned)
        };
        if let Some(c) = self.live.get(id) {
            c.kick_all();
        }
        self.sync_and_save()?;
        Ok(view)
    }

    pub fn reset_link_traffic(&self, id: &str) -> Result<LinkView> {
        if let Some(c) = self.live.get(id) {
            c.tx_bytes.store(0, Ordering::Relaxed);
            c.rx_bytes.store(0, Ordering::Relaxed);
        }
        let view = {
            let mut cfg = self.config.write().expect("config write lock");
            let idx = cfg
                .links
                .iter()
                .position(|l| l.id == id)
                .context("link not found")?;
            cfg.links[idx].tx_bytes = 0;
            cfg.links[idx].rx_bytes = 0;
            let cloned = cfg.links[idx].clone();
            self.to_link_view(&cfg, &cloned)
        };
        self.sync_and_save()?;
        Ok(view)
    }

    pub fn delete_link(&self, id: &str) -> Result<()> {
        if let Some((_, c)) = self.live.remove(id) {
            c.kick_all();
        }
        {
            let mut cfg = self.config.write().expect("config write lock");
            let before = cfg.links.len();
            cfg.links.retain(|l| l.id != id);
            if cfg.links.len() == before {
                bail!("link not found");
            }
        }
        self.sync_and_save()
    }

    pub fn update_node_settings(&self, input: UpdateNodeSettingsInput) -> Result<OverviewView> {
        if input.public_host.trim().is_empty() || input.udp_port == 0 {
            bail!("公网地址和端口不能为空");
        }
        {
            let mut cfg = self.config.write().expect("config write lock");
            cfg.public_host = input.public_host.trim().to_string();
            cfg.udp_port = input.udp_port;
            cfg.sni = input.sni.trim().to_string();
            cfg.insecure = input.insecure;
        }
        self.sync_and_save()?;
        Ok(self.overview())
    }

    /// Build a standard `SubscriptionPayload` JSON for our Windows client's "订阅 URL 同步".
    pub fn build_subscription(&self, token: &str) -> Result<(SubscriptionPayload, u64, u64, Option<u64>)> {
        let cfg = self.config.read().expect("config read lock");
        let link = cfg
            .links
            .iter()
            .find(|l| constant_time_eq(&l.token, token))
            .context("invalid subscription token")?;

        let counters = self
            .live
            .get(&link.id)
            .map(|c| c.clone())
            .unwrap_or_else(|| Arc::new(LiveCounters::from_persisted(link.tx_bytes, link.rx_bytes)));

        let status = evaluate_link_status(link, &counters);
        if status != "active" {
            bail!("subscription unavailable: {status}");
        }

        let tx = counters.tx_bytes.load(Ordering::Relaxed);
        let rx = counters.rx_bytes.load(Ordering::Relaxed);
        let used_gb = (tx + rx) as f64 / (1024.0 * 1024.0 * 1024.0);
        let total_limit_bytes = link
            .traffic_limit_gb
            .map(|gb| (gb * 1024.0 * 1024.0 * 1024.0) as u64);

        let node = build_node_profile(&cfg, link);
        let payload = SubscriptionPayload {
            name: format!("GameAccel · {}", link.name),
            expire_at: link.expire_at.clone(),
            traffic_used_gb: Some((used_gb * 100.0).round() / 100.0),
            traffic_limit_gb: link.traffic_limit_gb,
            nodes: vec![node],
        };
        Ok((payload, tx, rx, total_limit_bytes))
    }

    fn to_link_view(&self, cfg: &PanelConfig, link: &LinkRecord) -> LinkView {
        let counters = self
            .live
            .get(&link.id)
            .map(|c| c.clone())
            .unwrap_or_else(|| Arc::new(LiveCounters::from_persisted(link.tx_bytes, link.rx_bytes)));
        let tx = counters.tx_bytes.load(Ordering::Relaxed);
        let rx = counters.rx_bytes.load(Ordering::Relaxed);
        let online = counters.online_conns.load(Ordering::Relaxed);
        let status = evaluate_link_status(link, &counters);
        let share_uri = build_node_profile(cfg, link).to_share_uri();

        LinkView {
            id: link.id.clone(),
            name: link.name.clone(),
            token: link.token.clone(),
            enabled: link.enabled,
            game_dup: link.game_dup,
            traffic_limit_gb: link.traffic_limit_gb,
            expire_at: link.expire_at.clone(),
            tx_bytes: tx,
            rx_bytes: rx,
            online_conns: online,
            created_at: link.created_at.clone(),
            share_uri,
            status,
        }
    }
}

fn build_node_profile(cfg: &PanelConfig, link: &LinkRecord) -> NodeProfile {
    NodeProfile {
        id: link.id.clone(),
        name: link.name.clone(),
        server: format!("{}:{}", cfg.public_host, cfg.udp_port),
        sni: cfg.sni.clone(),
        token: link.token.clone(),
        game_dup: link.game_dup,
        insecure: cfg.insecure,
        source: None,
    }
}

fn evaluate_link_status(link: &LinkRecord, counters: &LiveCounters) -> String {
    if !link.enabled {
        return "disabled".to_string();
    }
    if let Some(exp) = &link.expire_at {
        let today = Local::now().format("%Y-%m-%d").to_string();
        if today.as_str() > exp.as_str() {
            return "expired".to_string();
        }
    }
    if let Some(limit_gb) = link.traffic_limit_gb {
        let limit_bytes = (limit_gb * 1024.0 * 1024.0 * 1024.0) as u64;
        let used = counters.tx_bytes.load(Ordering::Relaxed)
            + counters.rx_bytes.load(Ordering::Relaxed);
        if used >= limit_bytes {
            return "over_limit".to_string();
        }
    }
    "active".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_data_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "game-accel-store-test-{tag}-{}.json",
            generate_random_str(8)
        ))
    }

    #[test]
    fn constant_time_eq_handles_diff_lengths() {
        assert!(constant_time_eq("secret-token-123", "secret-token-123"));
        assert!(!constant_time_eq("secret-token-123", "secret-token-124"));
        assert!(!constant_time_eq("short", "much-longer-string"));
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn pbkdf2_hash_verify_and_migration() {
        let hashed = hash_password("MyStrongPass!99");
        assert!(hashed.starts_with(PBKDF2_PREFIX));
        assert!(verify_password("MyStrongPass!99", &hashed));
        assert!(!verify_password("wrong-pass", &hashed));

        // Legacy plaintext config auto-migrates to PBKDF2 on load
        let path = temp_data_path("migrate");
        let legacy = PanelConfig {
            admin_user: "admin".to_string(),
            admin_pass: "legacyPlain123".to_string(),
            public_host: "1.2.3.4".to_string(),
            udp_port: 8443,
            sni: "example.com".to_string(),
            insecure: false,
            access_log_enabled: true,
            links: vec![LinkRecord {
                id: "lnk-1".to_string(),
                name: "Test".to_string(),
                token: "tok-active".to_string(),
                enabled: true,
                game_dup: true,
                traffic_limit_gb: Some(1.0),
                expire_at: Some("2099-12-31".to_string()),
                tx_bytes: 0,
                rx_bytes: 0,
                created_at: "2026-01-01 00:00".to_string(),
            }],
        };
        std::fs::write(&path, serde_json::to_string_pretty(&legacy).unwrap()).unwrap();

        let (state, gen_pass) = AppState::load_or_init(InitParams {
            data_path: path.clone(),
            public_host: "1.2.3.4".to_string(),
            udp_port: 8443,
            sni: "example.com".to_string(),
            insecure: false,
            seed_token: "unused".to_string(),
            initial_panel_pass: None,
        })
        .unwrap();
        assert!(gen_pass.is_none());

        // Persisted file must now contain pbkdf2$ hash, not plaintext
        let disk_text = std::fs::read_to_string(&path).unwrap();
        assert!(disk_text.contains("pbkdf2$"));
        assert!(!disk_text.contains("legacyPlain123"));

        // Login with old plaintext succeeds
        let sid = state.verify_login("admin", "legacyPlain123");
        assert!(sid.is_some());
        assert!(state.verify_session(sid.as_deref().unwrap()));
        assert!(state.verify_login("admin", "badpass").is_none());

        // Change password invalidates existing session
        state
            .update_admin_password("legacyPlain123", "newPass456")
            .unwrap();
        assert!(!state.verify_session(sid.as_deref().unwrap()));
        assert!(state.verify_login("admin", "newPass456").is_some());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn token_auth_enforces_enabled_expiry_and_quota() {
        let path = temp_data_path("auth");
        let (state, _) = AppState::load_or_init(InitParams {
            data_path: path.clone(),
            public_host: "1.2.3.4".to_string(),
            udp_port: 8443,
            sni: "example.com".to_string(),
            insecure: false,
            seed_token: "seed-tok-123456".to_string(),
            initial_panel_pass: Some("initpass123".to_string()),
        })
        .unwrap();

        let ticket = state.authenticate_token("seed-tok-123456").unwrap();
        assert!(state.authenticate_token("wrong-token").is_err());

        // Disable link
        state
            .update_link(
                &ticket.link_id,
                UpdateLinkInput {
                    name: None,
                    enabled: Some(false),
                    game_dup: None,
                    traffic_limit_gb: None,
                    expire_at: None,
                },
            )
            .unwrap();
        assert!(state.authenticate_token("seed-tok-123456").is_err());

        // Re-enable with expired date
        state
            .update_link(
                &ticket.link_id,
                UpdateLinkInput {
                    name: None,
                    enabled: Some(true),
                    game_dup: None,
                    traffic_limit_gb: None,
                    expire_at: Some(Some("2000-01-01".to_string())),
                },
            )
            .unwrap();
        assert!(state.authenticate_token("seed-tok-123456").is_err());

        // Clear expiry, set 1 GB quota and exceed it
        state
            .update_link(
                &ticket.link_id,
                UpdateLinkInput {
                    name: None,
                    enabled: Some(true),
                    game_dup: None,
                    traffic_limit_gb: Some(Some(1.0)),
                    expire_at: Some(None),
                },
            )
            .unwrap();
        assert!(state.authenticate_token("seed-tok-123456").is_ok());
        ticket
            .counters
            .rx_bytes
            .store(2 * 1024 * 1024 * 1024, Ordering::Relaxed);
        assert!(state.authenticate_token("seed-tok-123456").is_err());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn update_link_input_distinguishes_null_from_absent() {
        let cleared: UpdateLinkInput =
            serde_json::from_str(r#"{"traffic_limit_gb":null,"expire_at":null}"#).unwrap();
        assert_eq!(cleared.traffic_limit_gb, Some(None));
        assert_eq!(cleared.expire_at, Some(None));

        let untouched: UpdateLinkInput = serde_json::from_str("{}").unwrap();
        assert_eq!(untouched.traffic_limit_gb, None);
        assert_eq!(untouched.expire_at, None);
        assert!(untouched.name.is_none() && untouched.enabled.is_none());

        let set: UpdateLinkInput =
            serde_json::from_str(r#"{"traffic_limit_gb":5.5,"expire_at":"2099-01-01"}"#).unwrap();
        assert_eq!(set.traffic_limit_gb, Some(Some(5.5)));
        assert_eq!(set.expire_at, Some(Some("2099-01-01".to_string())));
    }

    #[test]
    fn login_throttle_locks_after_failures_and_clears_on_success() {
        let path = temp_data_path("throttle");
        let (state, _) = AppState::load_or_init(InitParams {
            data_path: path.clone(),
            public_host: "1.2.3.4".to_string(),
            udp_port: 8443,
            sni: "example.com".to_string(),
            insecure: false,
            seed_token: "seed-tok-throttle".to_string(),
            initial_panel_pass: None,
        })
        .unwrap();
        let ip: IpAddr = "203.0.113.9".parse().unwrap();
        let other: IpAddr = "203.0.113.10".parse().unwrap();

        for _ in 0..LOGIN_MAX_FAILURES - 1 {
            state.record_login_result(ip, false);
            assert!(state.check_login_throttle(ip).is_ok());
        }
        state.record_login_result(ip, false);
        let remaining = state.check_login_throttle(ip).unwrap_err();
        assert!(remaining > Duration::ZERO && remaining <= LOGIN_WINDOW);
        assert!(
            state.check_login_throttle(other).is_ok(),
            "other IPs must not be locked"
        );

        state.record_login_result(ip, true);
        assert!(state.check_login_throttle(ip).is_ok());

        let _ = std::fs::remove_file(&path);
    }
}
