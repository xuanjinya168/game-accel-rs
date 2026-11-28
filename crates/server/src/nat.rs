use crate::StreamContext;
use anyhow::Result;
use bytes::Bytes;
use dashmap::DashMap;
use protocol::dedup::DedupWindow;
use protocol::domain::{AccessLogRecord, AccessOutcome};
use protocol::frame::{FLAG_GAME_MODE, TargetAddr, UdpFrame};
use protocol::pacer::{UdpDupPacer, send_datagram_checked};
use protocol::sniff::extract_root_domain;
use protocol::unix_now_secs as now_secs;
use quinn::Connection;
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

const NAT_IDLE_TIMEOUT_SECS: u64 = 180;
const NAT_SOCKET_BUFFER: usize = 2 * 1024 * 1024;

pub(crate) fn bind_nat_socket() -> std::io::Result<(std::net::UdpSocket, bool)> {
    use socket2::{Domain, Protocol, Socket, Type};
    let build = |domain: Domain, addr: SocketAddr| -> std::io::Result<std::net::UdpSocket> {
        let s = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
        if domain == Domain::IPV6 {
            s.set_only_v6(false)?;
        }
        let _ = s.set_recv_buffer_size(NAT_SOCKET_BUFFER);
        let _ = s.set_send_buffer_size(NAT_SOCKET_BUFFER);
        s.set_nonblocking(true)?;
        s.bind(&addr.into())?;
        Ok(s.into())
    };
    match build(Domain::IPV6, SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))) {
        Ok(s) => Ok((s, true)),
        Err(_) => Ok((
            build(Domain::IPV4, SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))?,
            false,
        )),
    }
}

struct NatMeta {
    last_active: AtomicU64,
    game_mode: AtomicBool,
    tx_seq: AtomicU16,
}

struct NatEntry {
    socket: Arc<UdpSocket>,
    dual_stack: bool,
    meta: Arc<NatMeta>,
    dedup: Mutex<DedupWindow>,
    seen_full: AtomicBool,
    seen_targets: Mutex<HashSet<String>>,
    recv_task: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for NatEntry {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.recv_task.lock()
            && let Some(h) = guard.take()
        {
            h.abort();
        }
    }
}

/// Full Cone NAT (NAT1) table scoped to a single QUIC client connection.
pub struct FullConeNat {
    conn: Connection,
    pacer: UdpDupPacer,
    entries: Arc<DashMap<u16, Arc<NatEntry>>>,
    create_lock: tokio::sync::Mutex<()>,
    ctx: Arc<StreamContext>,
}

impl FullConeNat {
    pub(crate) fn new(conn: Connection, pacer: UdpDupPacer, ctx: Arc<StreamContext>) -> Self {
        let entries: Arc<DashMap<u16, Arc<NatEntry>>> = Arc::new(DashMap::new());

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
            ctx,
        }
    }

    pub async fn handle_datagram(&self, raw: Bytes) -> Result<()> {
        let frame = UdpFrame::decode(raw)?;
        let entry = self.get_or_create_entry(frame.flow_id).await?;
        entry.meta.last_active.store(now_secs(), Ordering::Relaxed);

        let is_game = (frame.flags & FLAG_GAME_MODE) != 0;
        entry.meta.game_mode.store(is_game, Ordering::Relaxed);

        let is_new = {
            let mut dedup = entry.dedup.lock().unwrap_or_else(|e| e.into_inner());
            dedup.check_and_insert(frame.seq)
        };
        if !is_new {
            return Ok(());
        }

        let (domain_host, dst) = match frame.addr {
            TargetAddr::Ip(addr) => (None, addr),
            TargetAddr::Domain(host, port) => {
                let resolved = crate::dns::resolve(&host, port)
                    .await?
                    .into_iter()
                    .find(|a| entry.dual_stack || a.is_ipv4());
                match resolved {
                    Some(addr) => (Some(host), addr),
                    None => {
                        warn!("failed to resolve udp target {host}:{port}");
                        return Ok(());
                    }
                }
            }
        };

        let send_addr = match (entry.dual_stack, dst) {
            (true, SocketAddr::V4(v4)) => {
                SocketAddr::new(IpAddr::V6(v4.ip().to_ipv6_mapped()), v4.port())
            }
            (false, SocketAddr::V6(_)) => {
                debug!("dropping udp to {dst}: host has no IPv6 socket");
                return Ok(());
            }
            _ => dst,
        };
        let sent = entry.socket.send_to(&frame.payload, send_addr).await?;
        self.ctx
            .counters
            .tx_bytes
            .fetch_add(sent as u64, Ordering::Relaxed);

        if self.ctx.state.is_access_log_enabled() && !entry.seen_full.load(Ordering::Relaxed) {
            let (host_str, target_str) = match domain_host {
                Some(h) => {
                    let t = format!("{h}:{}", dst.port());
                    (h, t)
                }
                None => (dst.ip().to_string(), dst.to_string()),
            };
            let should_log = {
                let mut seen = entry.seen_targets.lock().unwrap_or_else(|e| e.into_inner());
                if seen.len() >= 64 {
                    entry.seen_full.store(true, Ordering::Relaxed);
                    false
                } else {
                    let inserted = seen.insert(target_str.clone());
                    if seen.len() >= 64 {
                        entry.seen_full.store(true, Ordering::Relaxed);
                    }
                    inserted
                }
            };
            if should_log {
                self.ctx.state.record_access(AccessLogRecord {
                    link_id: self.ctx.link_id.clone(),
                    link_name: self.ctx.link_name.clone(),
                    client_ip: self.ctx.client_ip.clone(),
                    proto: "UDP".to_string(),
                    root_domain: extract_root_domain(&host_str),
                    target: target_str,
                    tx_bytes: sent as u64,
                    rx_bytes: 0,
                    duration_ms: 0,
                    outcome: AccessOutcome::Ok,
                });
            }
        }
        Ok(())
    }

    async fn get_or_create_entry(&self, flow_id: u16) -> Result<Arc<NatEntry>> {
        if let Some(existing) = self.entries.get(&flow_id) {
            return Ok(existing.clone());
        }

        let _guard = self.create_lock.lock().await;
        if let Some(existing) = self.entries.get(&flow_id) {
            return Ok(existing.clone());
        }

        let (std_sock, dual_stack) = bind_nat_socket()?;
        let socket = Arc::new(UdpSocket::from_std(std_sock)?);
        let meta = Arc::new(NatMeta {
            last_active: AtomicU64::new(now_secs()),
            game_mode: AtomicBool::new(false),
            tx_seq: AtomicU16::new(0),
        });

        let entry = Arc::new(NatEntry {
            socket: socket.clone(),
            dual_stack,
            meta: meta.clone(),
            dedup: Mutex::new(DedupWindow::new()),
            seen_full: AtomicBool::new(false),
            seen_targets: Mutex::new(HashSet::new()),
            recv_task: Mutex::new(None),
        });

        self.entries.insert(flow_id, entry.clone());

        let conn = self.conn.clone();
        let pacer = self.pacer.clone();
        let counters_rx = self.ctx.counters.clone();
        let handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                match socket.recv_from(&mut buf).await {
                    Ok((n, src_addr)) => {
                        let src_addr = SocketAddr::new(src_addr.ip().to_canonical(), src_addr.port());
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
