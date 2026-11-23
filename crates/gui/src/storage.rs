use crate::theme::ThemeMode;
use protocol::share::NodeProfile;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProxyMode {
    Tun,
    SystemProxy,
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscriptionEntry {
    pub name: String,
    pub url: String,
    pub last_updated: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub nodes: Vec<NodeProfile>,
    pub active_node_id: String,
    #[serde(default)]
    pub subscriptions: Vec<SubscriptionEntry>,
    pub proxy_mode: ProxyMode,
    pub listen_addr: String,
    pub theme_mode: ThemeMode,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            active_node_id: String::new(),
            nodes: Vec::new(),
            subscriptions: Vec::new(),
            proxy_mode: ProxyMode::Tun,
            listen_addr: "127.0.0.1:1080".to_string(),
            theme_mode: ThemeMode::Light,
        }
    }
}

impl AppConfig {
    pub fn active_node(&self) -> Option<&NodeProfile> {
        self.nodes
            .iter()
            .find(|n| n.id == self.active_node_id)
            .or_else(|| self.nodes.first())
    }
}

/// Legacy v0.1 single-node config for automatic migration.
#[derive(Deserialize)]
struct LegacyConfigV1 {
    server_addr: String,
    server_name: String,
    listen_addr: String,
    token: String,
    game_dup: bool,
    auto_sys_proxy: bool,
    #[serde(default)]
    enable_tun: bool,
    insecure: bool,
    theme_mode: ThemeMode,
}

pub fn config_dir() -> PathBuf {
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join("game-accel")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

pub fn load_config() -> AppConfig {
    let path = config_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return AppConfig::default();
    };

    if let Ok(mut cfg) = serde_json::from_str::<AppConfig>(&text) {
        if !cfg.nodes.is_empty() && cfg.active_node().is_none() {
            cfg.active_node_id = cfg.nodes[0].id.clone();
        }
        return cfg;
    }

    // Seamless migration from v0.1 single-node config
    if let Ok(old) = serde_json::from_str::<LegacyConfigV1>(&text) {
        let node = NodeProfile {
            id: "migrated-default".to_string(),
            name: format!("默认节点 ({})", old.server_name),
            server: old.server_addr,
            sni: old.server_name,
            token: old.token,
            game_dup: old.game_dup,
            insecure: old.insecure,
            source: None,
        };
        let proxy_mode = if old.enable_tun {
            ProxyMode::Tun
        } else if old.auto_sys_proxy {
            ProxyMode::SystemProxy
        } else {
            ProxyMode::Manual
        };
        let migrated = AppConfig {
            active_node_id: node.id.clone(),
            nodes: vec![node],
            subscriptions: Vec::new(),
            proxy_mode,
            listen_addr: old.listen_addr,
            theme_mode: old.theme_mode,
        };
        save_config(&migrated);
        return migrated;
    }

    AppConfig::default()
}

pub fn save_config(cfg: &AppConfig) {
    let path = config_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(cfg) {
        let _ = std::fs::write(path, text);
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.2} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

pub fn format_speed(bytes_per_sec: f64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    if bytes_per_sec >= MB {
        format!("{:.2} MB/s", bytes_per_sec / MB)
    } else if bytes_per_sec >= KB {
        format!("{:.1} KB/s", bytes_per_sec / KB)
    } else {
        format!("{:.0} B/s", bytes_per_sec)
    }
}
