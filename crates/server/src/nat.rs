use crate::sniff::extract_root_domain;
use crate::store::{AppState, LiveCounters};
use anyhow::Result;
use bytes::Bytes;
use dashmap::DashMap;
use protocol::dedup::DedupWindow;
use protocol::frame::{FLAG_GAME_MODE, TargetAddr, UdpFrame};
use protocol::pacer::{UdpDupPacer, send_datagram_checked};
use quinn::Connection;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

const NAT_IDLE_TIMEOUT_SECS: u64 = 60;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

struct NatMeta {
    last_active: AtomicU64,
    game_mode: AtomicBool,
    tx_seq: AtomicU16,
}

struct NatEntry {
    socket: Arc<UdpSocket>,
    meta: Arc<NatMeta>,
    dedup: Mutex<DedupWindow>,
    seen_targets: Mutex<HashSet<String>>,
    recv_task: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for NatEntry {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.recv_task.lock() {
            if let Some(h) = guard.take() {
                h.abort();
            }
        }
    }
}

/// Full Cone NAT (NAT1) table scoped to a single QUIC client connection.
/// Lock-free fast path via DashMap + synchronized slow-path creation so concurrent
/// first packets for the same `flow_id` never leak duplicate sockets or race `recv_task`.
pub struct FullConeNat {
    conn: Connection,
    pacer: UdpDupPacer,
    entries: Arc<DashMap<u16, Arc<NatEntry>>>,
    create_lock: tokio::sync::Mutex<()>,
    counters: Arc<LiveCounters>,
    state: Arc<AppState>,
    link_id: String,
    link_name: String,
    client_ip: String,
}

impl FullConeNat {
    pub fn new(
        conn: Connection,
        pacer: UdpDupPacer,
        counters: Arc<LiveCounters>,
        state: Arc<AppState>,
        link_id: String,
        link_name: String,
        client_ip: String,
    ) -> Self {
        let entries: Arc<DashMap<u16, Arc<NatEntry>>> = Arc::new(DashMap::new());

        // Background idle reaper
        let reaper_entries = entries.clone();
        let reaper_conn = conn.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(15));
            loop {
                interval.tick().await;
                if reaper_conn.close_reason().is_some() {
                    reaper_entries.clear();
                    break;
                }
                let now = now_secs();
                reaper_entries.retain(|_, entry| {
                    now.saturating_sub(entry.meta.last_active.load(Ordering::Relaxed))
                        < NAT_IDLE_TIMEOUT_SECS
                });
            }
        });

        Self {
            conn,
            pacer,
            entries,
            create_lock: tokio::sync::Mutex::new(()),
            counters,
            state,
            link_id,
            link_name,
            client_ip,
        }
    }

    pub async fn handle_datagram(&self, raw: Bytes) -> Result<()> {
        let frame = UdpFrame::decode(raw)?;
        let entry = self.get_or_create_entry(frame.flow_id).await?;
        entry.meta.last_active.store(now_secs(), Ordering::Relaxed);

        let is_game = (frame.flags & FLAG_GAME_MODE) != 0;
        entry.meta.game_mode.store(is_game, Ordering::Relaxed);

        // Deduplicate packets from client-side dual-send
        let is_new = {
            let mut dedup = entry.dedup.lock().unwrap_or_else(|e| e.into_inner());
            dedup.check_and_insert(frame.seq)
        };
        if !is_new {
            return Ok(());
        }

        let (host_str, target_str, dst) = match frame.addr {
            TargetAddr::Ip(addr) => (addr.ip().to_string(), addr.to_string(), addr),
            TargetAddr::Domain(host, port) => {
                let target_s = format!("{host}:{port}");
                let resolved = tokio::net::lookup_host((host.as_str(), port)).await?.next();
                match resolved {
                    Some(addr) => (host, target_s, addr),
                    None => {
                        warn!("failed to resolve udp target {}", target_s);
                        return Ok(());
                    }
                }
            }
        };

        let sent = entry.socket.send_to(&frame.payload, dst).await?;
        self.counters
            .tx_bytes
            .fetch_add(sent as u64, Ordering::Relaxed);

        let should_log = {
            let mut seen = entry.seen_targets.lock().unwrap_or_else(|e| e.into_inner());
            if seen.len() < 64 && !seen.contains(&target_str) {
                seen.insert(target_str.clone());
                true
            } else {
                false
            }
        };
        if should_log {
            self.state.record_access_log(
                &self.link_id,
                &self.link_name,
                &self.client_ip,
                "UDP",
                extract_root_domain(&host_str),
                target_str,
                sent as u64,
                0,
                0,
                "ok",
            );
        }
        Ok(())
    }

    async fn get_or_create_entry(&self, flow_id: u16) -> Result<Arc<NatEntry>> {
        // Lock-free fast path
        if let Some(existing) = self.entries.get(&flow_id) {
            return Ok(existing.clone());
        }

        // Synchronized creation slow path (eliminates duplicate socket race)
        let _guard = self.create_lock.lock().await;
        if let Some(existing) = self.entries.get(&flow_id) {
            return Ok(existing.clone());
        }

        let socket = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
        let meta = Arc::new(NatMeta {
            last_active: AtomicU64::new(now_secs()),
            game_mode: AtomicBool::new(false),
            tx_seq: AtomicU16::new(0),
        });

        let entry = Arc::new(NatEntry {
            socket: socket.clone(),
            meta: meta.clone(),
            dedup: Mutex::new(DedupWindow::new()),
            seen_targets: Mutex::new(HashSet::new()),
            recv_task: Mutex::new(None),
        });

        // Insert BEFORE spawning receive task, and pass `Arc<NatMeta>` directly
        // so `recv_task` can never race map insertion.
        self.entries.insert(flow_id, entry.clone());

        let conn = self.conn.clone();
        let pacer = self.pacer.clone();
        let counters_rx = self.counters.clone();
        let handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                match socket.recv_from(&mut buf).await {
                    Ok((n, src_addr)) => {
                        meta.last_active.store(now_secs(), Ordering::Relaxed);
                        counters_rx.rx_bytes.fetch_add(n as u64, Ordering::Relaxed);
                        let is_game = meta.game_mode.load(Ordering::Relaxed);
                        let seq = meta.tx_seq.fetch_add(1, Ordering::Relaxed);

                        let payload = Bytes::copy_from_slice(&buf[..n]);
                        let flags = if is_game { FLAG_GAME_MODE } else { 0 };
                        let frame = UdpFrame {
                            flow_id,
                            seq,
                            flags,
                            addr: TargetAddr::Ip(src_addr),
                            payload,
                        };
                        if let Ok(encoded) = frame.encode() {
                            if !send_datagram_checked(&conn, encoded.clone()) {
                                break;
                            }
                            // True zero-copy duplicate via shared connection-scoped 2ms pacer
                            if is_game && n < 200 {
                                pacer.schedule_dup(encoded);
                            }
                        }
                    }
                    Err(e) => {
                        debug!("udp recv error on flow {}: {}", flow_id, e);
                        break;
                    }
                }
            }
        });

        *entry.recv_task.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
        Ok(entry)
    }
}
