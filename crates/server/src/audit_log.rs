//! Bounded in-memory ring buffer and non-blocking rotating disk writer for server access logs.

use anyhow::{Result, bail};
use chrono::Local;
use protocol::domain::{AccessLogRecord, AccessOutcome};
use serde::Serialize;
use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

const MAX_MEM_ACCESS_LOGS: usize = 500;
const MAX_ACCESS_LOG_FILE_BYTES: u64 = 10 * 1024 * 1024; // 10 MB rotation
const ACCESS_LOG_QUEUE_CAP: usize = 1024;

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
    pub status: &'static str,
}

#[derive(Debug, Serialize)]
pub struct AccessLogsResponse {
    pub enabled: bool,
    pub total_buffered: usize,
    pub logs: Vec<AccessLogEntry>,
}

enum LogOp {
    Line(String),
    Truncate(std::sync::mpsc::SyncSender<std::result::Result<(), String>>),
}

pub struct AccessLogService {
    enabled: AtomicBool,
    next_id: AtomicU64,
    ring: Mutex<VecDeque<AccessLogEntry>>,
    tx: std::sync::mpsc::SyncSender<LogOp>,
}

impl AccessLogService {
    pub fn spawn(log_path: PathBuf, initially_enabled: bool) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel::<LogOp>(ACCESS_LOG_QUEUE_CAP);
        let _ = std::thread::Builder::new()
            .name("access-log-writer".to_string())
            .spawn(move || {
                while let Ok(op) = rx.recv() {
                    match op {
                        LogOp::Line(line) => {
                            if let Ok(meta) = std::fs::metadata(&log_path)
                                && meta.len() >= MAX_ACCESS_LOG_FILE_BYTES
                            {
                                let old = log_path.with_extension("log.old");
                                let _ = std::fs::rename(&log_path, old);
                            }
                            if let Ok(mut f) = OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(&log_path)
                            {
                                let _ = writeln!(f, "{line}");
                            }
                        }
                        LogOp::Truncate(ack) => {
                            let res = std::fs::write(&log_path, b"").map_err(|e| e.to_string());
                            let _ = ack.send(res);
                        }
                    }
                }
            });

        Self {
            enabled: AtomicBool::new(initially_enabled),
            next_id: AtomicU64::new(1),
            ring: Mutex::new(VecDeque::with_capacity(MAX_MEM_ACCESS_LOGS)),
            tx,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn set_enabled_flag(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn record(&self, rec: AccessLogRecord) {
        if !self.is_enabled() {
            return;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let time = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let status_str = rec.outcome.as_str();
        let line = format!(
            "[{time}] [{status_str}] [{}] user=\"{}\" ip={} root={} target={} tx={} rx={} dur={}ms",
            rec.proto,
            rec.link_name,
            rec.client_ip,
            rec.root_domain,
            rec.target,
            rec.tx_bytes,
            rec.rx_bytes,
            rec.duration_ms
        );
        let entry = AccessLogEntry {
            id,
            time,
            link_id: rec.link_id,
            link_name: rec.link_name,
            client_ip: rec.client_ip,
            proto: rec.proto,
            root_domain: rec.root_domain,
            target: rec.target,
            tx_bytes: rec.tx_bytes,
            rx_bytes: rec.rx_bytes,
            duration_ms: rec.duration_ms,
            status: status_str,
        };

        {
            let mut ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
            if ring.len() >= MAX_MEM_ACCESS_LOGS {
                ring.pop_front();
            }
            ring.push_back(entry);
        }

        // Non-blocking enqueue so disk I/O never stalls UDP/TCP forwarding.
        let _ = self.tx.try_send(LogOp::Line(line));
    }

    pub fn record_panel_audit(
        &self,
        actor_label: &str,
        client_ip: &str,
        root: &str,
        detail: String,
        outcome: AccessOutcome,
    ) {
        self.record(AccessLogRecord {
            link_id: String::new(),
            link_name: actor_label.to_string(),
            client_ip: client_ip.to_string(),
            proto: "PANEL".to_string(),
            root_domain: root.to_string(),
            target: detail,
            tx_bytes: 0,
            rx_bytes: 0,
            duration_ms: 0,
            outcome,
        });
    }

    pub fn query(
        &self,
        link_id: Option<&str>,
        proto: Option<&str>,
        keyword: Option<&str>,
    ) -> AccessLogsResponse {
        let ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        let total_buffered = ring.len();
        let kw = keyword
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty());
        let lid = link_id.map(str::trim).filter(|s| !s.is_empty());
        let pr = proto
            .map(|s| s.trim().to_ascii_uppercase())
            .filter(|s| !s.is_empty());

        let logs: Vec<AccessLogEntry> = ring
            .iter()
            .rev()
            .filter(|e| {
                if let Some(id) = lid
                    && e.link_id != id
                {
                    return false;
                }
                if let Some(p) = &pr
                    && e.proto != *p
                {
                    return false;
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
            enabled: self.is_enabled(),
            total_buffered,
            logs,
        }
    }

    pub fn clear(&self) -> Result<()> {
        let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel(1);
        self.tx
            .send(LogOp::Truncate(ack_tx))
            .map_err(|_| anyhow::anyhow!("access log writer stopped"))?;
        match ack_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(())) => {
                if let Ok(mut ring) = self.ring.lock() {
                    ring.clear();
                }
                Ok(())
            }
            Ok(Err(e)) => bail!("清空磁盘访问日志失败: {e}"),
            Err(_) => bail!("清空磁盘访问日志超时"),
        }
    }
}
