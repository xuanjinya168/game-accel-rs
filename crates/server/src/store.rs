pub use crate::audit_log::{AccessLogEntry, AccessLogService, AccessLogsResponse};
use crate::auth_guard::PanelSessionGuard;
pub use crate::crypto::{generate_random_str, hash_password, verify_password};
use crate::crypto::{PBKDF2_PREFIX, constant_time_eq};
use anyhow::{Context, Result, bail};
use chrono::Local;
use dashmap::DashMap;
use protocol::domain::{AccessLogRecord, AccessOutcome, LinkStatus};
use protocol::share::{NodeProfile, SubscriptionPayload};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tracing::info;

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
    pub(crate) token: String,
    pub counters: Arc<LiveCounters>,
}

/// RAII guard for a registered QUIC connection; guarantees unregistration on any exit path.
pub struct ConnGuard {
    counters: Arc<LiveCounters>,
    cid: u64,
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.counters.active_conns.remove(&self.cid);
        self.counters
            .online_conns
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            })
            .ok();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub hy2_uri: String,
    pub status: LinkStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateLinkInput {
    pub name: String,
    #[serde(default = "default_true")]
    pub game_dup: bool,
    pub traffic_limit_gb: Option<f64>,
    pub expire_at: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UpdateLinkInput {
    pub name: Option<String>,
    pub enabled: Option<bool>,
    pub game_dup: Option<bool>,
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

#[derive(Debug, Serialize, Deserialize)]
pub struct UpdateNodeSettingsInput {
    pub public_host: String,
    pub udp_port: u16,
    pub sni: String,
    pub insecure: bool,
}

pub struct AppState {
    data_path: PathBuf,
    pub config: RwLock<PanelConfig>,
    pub live: DashMap<String, Arc<LiveCounters>>,
    panel_auth: PanelSessionGuard,
    audit: AccessLogService,
    next_conn_id: AtomicU64,
    save_lock: Mutex<()>,
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
            cfg.udp_port = params.udp_port;
            cfg.insecure = params.insecure;
            if let Some(pass) = params.initial_panel_pass
                && !pass.is_empty()
            {
                cfg.admin_pass = hash_password(&pass);
            }
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

        let access_log_path = params
            .data_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.join("access.log"))
            .unwrap_or_else(|| PathBuf::from("access.log"));
        let audit = AccessLogService::spawn(access_log_path, cfg.access_log_enabled);

        let state = Arc::new(Self {
            data_path: params.data_path,
            config: RwLock::new(cfg),
            live,
            panel_auth: PanelSessionGuard::default(),
            audit,
            next_conn_id: AtomicU64::new(1),
            save_lock: Mutex::new(()),
            started_at: Instant::now(),
        });
        state.sync_and_save()?;
        Ok((state, newly_generated_pass))
    }

    fn write_snapshot_unlocked(&self, snapshot: &PanelConfig) -> Result<()> {
        if let Some(parent) = self.data_path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.data_path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(snapshot)?;
        std::fs::write(&tmp, text)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, &self.data_path)?;
        Ok(())
    }

    fn mutate_and_save<F, R>(&self, mutate: F) -> Result<R>
    where
        F: FnOnce(&mut PanelConfig) -> Result<R>,
    {
        let _save_guard = self.save_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut candidate = {
            let cfg = self.config.read().expect("config read lock");
            let mut c = cfg.clone();
            for link in &mut c.links {
                if let Some(live) = self.live.get(&link.id) {
                    link.tx_bytes = live.tx_bytes.load(Ordering::Relaxed);
                    link.rx_bytes = live.rx_bytes.load(Ordering::Relaxed);
                }
            }
            c
        };
        let result = mutate(&mut candidate)?;
        self.write_snapshot_unlocked(&candidate)?;
        *self.config.write().expect("config write lock") = candidate;
        Ok(result)
    }

    pub fn sync_and_save(&self) -> Result<()> {
        self.mutate_and_save(|_| Ok(()))
    }

    pub fn enforce_limits_and_kick(&self) {
        let to_kick: Vec<String> = {
            let cfg = self.config.read().expect("config read lock");
            cfg.links
                .iter()
                .filter_map(|link| {
                    let counters = self.live.get(&link.id)?;
                    let status = evaluate_link_status(link, counters.value());
                    if !status.is_active() && counters.online_conns.load(Ordering::Relaxed) > 0 {
                        Some(link.id.clone())
                    } else {
                        None
                    }
                })
                .collect()
        };
        for link_id in to_kick {
            if let Some(c) = self.live.get(&link_id) {
                info!("kicking active connections for non-active link {link_id}");
                c.kick_all();
            }
        }
    }

    pub fn data_path(&self) -> &Path {
        &self.data_path
    }

    // ---------- Access Audit Delegation ----------

    pub fn is_access_log_enabled(&self) -> bool {
        self.audit.is_enabled()
    }

    pub fn set_access_log_enabled(&self, enabled: bool) -> Result<bool> {
        self.mutate_and_save(|cfg| {
            cfg.access_log_enabled = enabled;
            Ok(())
        })?;
        self.audit.set_enabled_flag(enabled);
        Ok(enabled)
    }

    pub fn record_access(&self, rec: AccessLogRecord) {
        self.audit.record(rec);
    }

    pub fn record_panel_audit(
        &self,
        actor_label: &str,
        client_ip: &str,
        root: &str,
        detail: String,
        outcome: AccessOutcome,
    ) {
        self.audit
            .record_panel_audit(actor_label, client_ip, root, detail, outcome);
    }

    pub fn query_access_logs(
        &self,
        link_id: Option<&str>,
        proto: Option<&str>,
        keyword: Option<&str>,
    ) -> AccessLogsResponse {
        self.audit.query(link_id, proto, keyword)
    }

    pub fn clear_access_logs(&self) -> Result<()> {
        self.audit.clear()
    }

    // ---------- Panel Auth & Sessions Delegation ----------

    pub fn verify_login(&self, user: &str, pass: &str) -> Option<String> {
        let cfg = self.config.read().expect("config read lock");
        if constant_time_eq(user.trim(), &cfg.admin_user) && verify_password(pass, &cfg.admin_pass)
        {
            Some(self.panel_auth.issue_session())
        } else {
            None
        }
    }

    pub fn verify_session(&self, sid: &str) -> bool {
        self.panel_auth.verify_session(sid)
    }

    pub fn check_login_throttle(&self, ip: IpAddr) -> std::result::Result<(), Duration> {
        self.panel_auth.check_throttle(ip)
    }

    pub fn record_login_result(&self, ip: IpAddr, success: bool) {
        self.panel_auth.record_login_attempt(ip, success);
    }

    pub fn revoke_session(&self, sid: &str) {
        self.panel_auth.revoke_session(sid);
    }

    pub fn panel_aliases(&self) -> (String, String) {
        let cfg = self.config.read().expect("config read lock");
        (cfg.sni.clone(), cfg.public_host.clone())
    }

    pub fn update_admin_password(&self, old_pass: &str, new_pass: &str) -> Result<()> {
        if new_pass.len() < 6 {
            bail!("新密码长度至少 6 位");
        }
        self.mutate_and_save(|cfg| {
            if !verify_password(old_pass, &cfg.admin_pass) {
                bail!("原密码不正确");
            }
            cfg.admin_pass = hash_password(new_pass);
            Ok(())
        })?;
        self.panel_auth.revoke_all();
        Ok(())
    }

    // ---------- QUIC Data-Plane Token Auth & Live Connection Registry ----------

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
            .or_insert_with(|| {
                Arc::new(LiveCounters::from_persisted(link.tx_bytes, link.rx_bytes))
            })
            .clone();

        match evaluate_link_status(link, &counters) {
            LinkStatus::Active => Ok(AuthTicket {
                link_id: link.id.clone(),
                link_name: link.name.clone(),
                token: link.token.clone(),
                counters,
            }),
            LinkStatus::Disabled => bail!("link '{}' is disabled", link.name),
            LinkStatus::Expired => bail!("link '{}' is expired", link.name),
            LinkStatus::OverLimit => bail!("link '{}' exceeded traffic limit", link.name),
        }
    }

    pub fn register_conn(&self, ticket: &AuthTicket, conn: quinn::Connection) -> Option<ConnGuard> {
        let cfg = self.config.read().expect("config read lock");
        let link = cfg.links.iter().find(|l| l.id == ticket.link_id)?;
        if !constant_time_eq(&link.token, &ticket.token)
            || !evaluate_link_status(link, &ticket.counters).is_active()
        {
            return None;
        }
        let cid = self.next_conn_id.fetch_add(1, Ordering::Relaxed);
        ticket.counters.active_conns.insert(cid, conn);
        ticket.counters.online_conns.fetch_add(1, Ordering::Relaxed);
        Some(ConnGuard {
            counters: ticket.counters.clone(),
            cid,
        })
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
                .unwrap_or_else(|| {
                    Arc::new(LiveCounters::from_persisted(link.tx_bytes, link.rx_bytes))
                });
            if evaluate_link_status(link, &counters).is_active() {
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

        let view = self.mutate_and_save(|cfg| {
            cfg.links.push(record.clone());
            Ok(self.to_link_view(cfg, &record))
        })?;
        self.live
            .insert(record.id.clone(), Arc::new(LiveCounters::default()));
        info!("panel created link '{}' (id={})", view.name, view.id);
        Ok(view)
    }

    pub fn update_link(&self, id: &str, input: UpdateLinkInput) -> Result<LinkView> {
        let (view, kick_needed) = self.mutate_and_save(|cfg| {
            let mut kick = false;
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
                    kick = true;
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
            Ok((self.to_link_view(cfg, &cloned), kick))
        })?;

        if kick_needed
            && let Some(c) = self.live.get(id)
        {
            c.kick_all();
        }
        Ok(view)
    }

    pub fn reset_link_token(&self, id: &str) -> Result<LinkView> {
        let view = self.mutate_and_save(|cfg| {
            let idx = cfg
                .links
                .iter()
                .position(|l| l.id == id)
                .context("link not found")?;
            cfg.links[idx].token = generate_random_str(24);
            let cloned = cfg.links[idx].clone();
            Ok(self.to_link_view(cfg, &cloned))
        })?;
        if let Some(c) = self.live.get(id) {
            c.kick_all();
        }
        Ok(view)
    }

    pub fn reset_link_traffic(&self, id: &str) -> Result<LinkView> {
        let updated_link = self.mutate_and_save(|cfg| {
            let idx = cfg
                .links
                .iter()
                .position(|l| l.id == id)
                .context("link not found")?;
            cfg.links[idx].tx_bytes = 0;
            cfg.links[idx].rx_bytes = 0;
            Ok(cfg.links[idx].clone())
        })?;
        if let Some(c) = self.live.get(id) {
            c.tx_bytes.store(0, Ordering::Relaxed);
            c.rx_bytes.store(0, Ordering::Relaxed);
        }
        let cfg = self.config.read().expect("config read lock");
        Ok(self.to_link_view(&cfg, &updated_link))
    }

    pub fn delete_link(&self, id: &str) -> Result<()> {
        self.mutate_and_save(|cfg| {
            let before = cfg.links.len();
            cfg.links.retain(|l| l.id != id);
            if cfg.links.len() == before {
                bail!("link not found");
            }
            Ok(())
        })?;
        if let Some((_, c)) = self.live.remove(id) {
            c.kick_all();
        }
        Ok(())
    }

    pub fn update_node_settings(&self, input: UpdateNodeSettingsInput) -> Result<OverviewView> {
        if input.public_host.trim().is_empty() || input.udp_port == 0 {
            bail!("公网地址和端口不能为空");
        }
        self.mutate_and_save(|cfg| {
            cfg.public_host = input.public_host.trim().to_string();
            cfg.udp_port = input.udp_port;
            cfg.sni = input.sni.trim().to_string();
            cfg.insecure = input.insecure;
            Ok(())
        })?;
        Ok(self.overview())
    }

    pub fn build_subscription(
        &self,
        token: &str,
    ) -> Result<(SubscriptionPayload, u64, u64, Option<u64>)> {
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
            .unwrap_or_else(|| {
                Arc::new(LiveCounters::from_persisted(link.tx_bytes, link.rx_bytes))
            });

        let status = evaluate_link_status(link, &counters);
        if !status.is_active() {
            bail!("subscription unavailable: {:?}", status);
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
            .unwrap_or_else(|| {
                Arc::new(LiveCounters::from_persisted(link.tx_bytes, link.rx_bytes))
            });
        let tx = counters.tx_bytes.load(Ordering::Relaxed);
        let rx = counters.rx_bytes.load(Ordering::Relaxed);
        let online = counters.online_conns.load(Ordering::Relaxed);
        let status = evaluate_link_status(link, &counters);
        let share_uri = build_node_profile(cfg, link).to_share_uri();
        let hy2_uri = build_hy2_uri(cfg, link);

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
            hy2_uri,
            status,
        }
    }
}

fn build_hy2_uri(cfg: &PanelConfig, link: &LinkRecord) -> String {
    let mut node = build_node_profile(cfg, link);
    node.protocol = protocol::share::NodeProtocol::Hysteria2;
    node.to_share_uri()
}

fn build_node_profile(cfg: &PanelConfig, link: &LinkRecord) -> NodeProfile {
    let host = if cfg.public_host.contains(':') && !cfg.public_host.starts_with('[') {
        format!("[{}]", cfg.public_host)
    } else {
        cfg.public_host.clone()
    };
    NodeProfile {
        id: link.id.clone(),
        name: link.name.clone(),
        server: format!("{host}:{}", cfg.udp_port),
        sni: cfg.sni.clone(),
        token: link.token.clone(),
        protocol: protocol::share::NodeProtocol::Gacl,
        game_dup: link.game_dup,
        insecure: cfg.insecure,
        brutal_mbps: 0,
        source: None,
    }
}

pub fn evaluate_link_status(link: &LinkRecord, counters: &LiveCounters) -> LinkStatus {
    if !link.enabled {
        return LinkStatus::Disabled;
    }
    if let Some(exp) = &link.expire_at {
        let today = Local::now().format("%Y-%m-%d").to_string();
        if today.as_str() > exp.as_str() {
            return LinkStatus::Expired;
        }
    }
    if let Some(limit_gb) = link.traffic_limit_gb {
        let limit_bytes = (limit_gb * 1024.0 * 1024.0 * 1024.0) as u64;
        let used =
            counters.tx_bytes.load(Ordering::Relaxed) + counters.rx_bytes.load(Ordering::Relaxed);
        if used >= limit_bytes {
            return LinkStatus::OverLimit;
        }
    }
    LinkStatus::Active
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth_guard::{LOGIN_MAX_FAILURES, LOGIN_WINDOW};

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

        let disk_text = std::fs::read_to_string(&path).unwrap();
        assert!(disk_text.contains("pbkdf2$"));
        assert!(!disk_text.contains("legacyPlain123"));

        let sid = state.verify_login("admin", "legacyPlain123");
        assert!(sid.is_some());
        assert!(state.verify_session(sid.as_deref().unwrap()));
        assert!(state.verify_login("admin", "badpass").is_none());

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

    #[test]
    fn persist_failure_rolls_back_access_log_flag() {
        let dir = std::env::temp_dir().join(format!("gacl-rollback-{}", generate_random_str(8)));
        let data_path = dir.join("data.json");
        let (state, _) = AppState::load_or_init(InitParams {
            data_path: data_path.clone(),
            seed_token: "tok".to_string(),
            public_host: "127.0.0.1".to_string(),
            udp_port: 8443,
            sni: "localhost".to_string(),
            insecure: true,
            initial_panel_pass: Some("pass123".to_string()),
        })
        .unwrap();
        assert!(state.is_access_log_enabled());

        let tmp_blocker = data_path.with_extension("json.tmp");
        std::fs::create_dir_all(&tmp_blocker).unwrap();

        assert!(state.set_access_log_enabled(false).is_err());
        assert!(state.is_access_log_enabled());
        assert!(state.config.read().unwrap().access_log_enabled);

        let before_count = state.list_links().len();
        assert!(
            state
                .create_link(CreateLinkInput {
                    name: "should-not-stick".to_string(),
                    game_dup: true,
                    traffic_limit_gb: None,
                    expire_at: None,
                })
                .is_err()
        );
        assert_eq!(state.list_links().len(), before_count);
        assert_eq!(state.live.len(), before_count);

        std::fs::remove_dir_all(&tmp_blocker).unwrap();
        let link_id = state.list_links()[0].id.clone();
        state
            .live
            .get(&link_id)
            .unwrap()
            .tx_bytes
            .store(123, Ordering::Relaxed);
        let reset_view = state.reset_link_traffic(&link_id).unwrap();
        assert_eq!(reset_view.tx_bytes, 0);
        assert_eq!(reset_view.rx_bytes, 0);

        let log_blocker = dir.join("access.log");
        let _ = std::fs::remove_file(&log_blocker);
        std::fs::create_dir_all(&log_blocker).unwrap();
        assert!(state.clear_access_logs().is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
