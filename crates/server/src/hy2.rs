//! Native Hysteria2 protocol support sharing the same QUIC/TLS 1.3 (ALPN `h3`) port,
//! user tokens, traffic counters, quota/expiry enforcement, and access logs as `gacl://`.

use crate::dns;
use crate::masquerade::{serve_masquerade_followup, serve_masquerade_h3};
use crate::nat::bind_nat_socket;
use crate::store::{AppState, generate_random_str};
use crate::{PanelRoute, StreamContext, TCP_CONNECT_TIMEOUT, TcpAuditMeta, splice_and_audit_tcp};
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use dashmap::DashMap;
use protocol::domain::{AccessLogRecord, AccessOutcome};
use protocol::frame::TargetAddr;
use protocol::hy2::{
    HY2_TCP_STREAM_TYPE, Hy2UdpReassembler, decode_hy2_udp_message, encode_h3_control_stream,
    encode_hy2_udp_datagrams, header_val, parse_hy2_addr, read_h3_headers, write_h3_response,
    write_varint,
};
#[cfg(test)]
pub(crate) use protocol::hy2::encode_varint;
pub(crate) use protocol::hy2::read_varint;
use protocol::sniff::extract_root_domain;
use protocol::unix_now_secs as now_secs;
use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWriteExt};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use tracing::{debug, info};

const HY2_UDP_IDLE_TIMEOUT_SECS: u64 = 180;

struct PrefixedReader<R> {
    prefix: [u8; 4],
    pos: usize,
    inner: R,
}

impl<R: AsyncRead + Unpin> PrefixedReader<R> {
    fn new(prefix: [u8; 4], inner: R) -> Self {
        Self {
            prefix,
            pos: 0,
            inner,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for PrefixedReader<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.pos < self.prefix.len() {
            let avail = &self.prefix[self.pos..];
            let n = avail.len().min(buf.remaining());
            buf.put_slice(&avail[..n]);
            self.pos += n;
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

pub(crate) async fn handle_hy2_connection(
    conn: quinn::Connection,
    first_four: [u8; 4],
    mut auth_send: quinn::SendStream,
    auth_recv: quinn::RecvStream,
    state: Arc<AppState>,
    panel_route: Option<Arc<PanelRoute>>,
    masquerade_dir: Option<Arc<PathBuf>>,
) -> Result<()> {
    let _ctrl_send = match conn.open_uni().await {
        Ok(mut s) => {
            let _ = s.write_all(&encode_h3_control_stream()).await;
            Some(s)
        }
        Err(_) => None,
    };

    let uni_conn = conn.clone();
    let uni_task = tokio::spawn(async move {
        while let Ok(mut uni) = uni_conn.accept_uni().await {
            tokio::spawn(async move {
                let mut sink = [0u8; 512];
                while let Ok(Some(n)) = uni.read(&mut sink).await {
                    if n == 0 {
                        break;
                    }
                }
            });
        }
    });

    let mut reader = PrefixedReader::new(first_four, auth_recv);
    let headers = match read_h3_headers(&mut reader).await {
        Ok(h) => h,
        Err(e) => {
            let _ = serve_masquerade_h3(
                &mut auth_send,
                "GET",
                "/404",
                masquerade_dir.as_deref().map(PathBuf::as_path),
            )
            .await;
            let _ = auth_send.finish();
            uni_task.abort();
            return Err(e).context("decode Hysteria2 HTTP/3 auth request");
        }
    };

    let method = header_val(&headers, ":method").unwrap_or("");
    let path = header_val(&headers, ":path").unwrap_or("");
    let auth_token = header_val(&headers, "hysteria-auth").unwrap_or("");
    let client_ip = conn.remote_address().ip().to_string();

    if !method.eq_ignore_ascii_case("POST") || path != "/auth" || auth_token.is_empty() {
        let _ = serve_masquerade_h3(
            &mut auth_send,
            method,
            path,
            masquerade_dir.as_deref().map(PathBuf::as_path),
        )
        .await;
        let _ = auth_send.finish();
        if let Some(root) = masquerade_dir.as_deref() {
            serve_masquerade_followup(&conn, root).await;
        }
        uni_task.abort();
        bail!("non-hy2 HTTP/3 request ({method} {path})");
    }

    let ticket = match state.authenticate_token(auth_token) {
        Ok(t) => t,
        Err(e) => {
            let _ = serve_masquerade_h3(
                &mut auth_send,
                "POST",
                "/auth",
                masquerade_dir.as_deref().map(PathBuf::as_path),
            )
            .await;
            let _ = auth_send.finish();
            state.record_access(AccessLogRecord {
                link_id: String::new(),
                link_name: "未授权客户端 (Hy2)".to_string(),
                client_ip,
                proto: "AUTH".to_string(),
                root_domain: "-".to_string(),
                target: format!("{e:#}"),
                tx_bytes: 0,
                rx_bytes: 0,
                duration_ms: 0,
                outcome: AccessOutcome::Denied,
            });
            uni_task.abort();
            return Err(e);
        }
    };

    let padding = generate_random_str(24);
    write_h3_response(
        &mut auth_send,
        "233",
        &[
            ("hysteria-udp", "true"),
            ("hysteria-cc-rx", "auto"),
            ("hysteria-padding", &padding),
        ],
    )
    .await?;
    let _ = auth_send.finish();

    info!(
        "Hysteria2 client authenticated: {} (link={})",
        client_ip, ticket.link_name
    );

    let Some(_conn_guard) = state.register_conn(&ticket, conn.clone()) else {
        conn.close(0u32.into(), b"revoked");
        uni_task.abort();
        bail!("link '{}' revoked before registration", ticket.link_name);
    };

    let ctx = Arc::new(StreamContext {
        counters: ticket.counters.clone(),
        state,
        link_id: ticket.link_id.clone(),
        link_name: ticket.link_name.clone(),
        client_ip,
        panel_route,
    });
    let res = run_hy2_session(conn, ctx).await;

    uni_task.abort();
    res
}

fn hy2_datagram_needs_dns(raw: &[u8]) -> bool {
    let Ok(msg) = decode_hy2_udp_message(raw) else {
        return false;
    };
    matches!(parse_hy2_addr(&msg.addr), Ok(TargetAddr::Domain(..)))
}

async fn run_hy2_session(conn: quinn::Connection, ctx: Arc<StreamContext>) -> Result<()> {
    let udp_nat = Arc::new(Hy2UdpManager::new(conn.clone(), ctx.clone()));
    let dgram_conn = conn.clone();
    let dgram_nat = udp_nat.clone();
    // IP-targeted (game) UDP datagrams are handled inline to preserve fragment order and avoid
    // per-packet task spawn / permit drops; only domain targets offload DNS onto `resolve_slots`.
    let resolve_slots = Arc::new(tokio::sync::Semaphore::new(32));
    let dgram_task = tokio::spawn(async move {
        while let Ok(data) = dgram_conn.read_datagram().await {
            if hy2_datagram_needs_dns(&data) {
                let nat = dgram_nat.clone();
                match resolve_slots.clone().try_acquire_owned() {
                    Ok(permit) => {
                        tokio::spawn(async move {
                            let _permit = permit;
                            if let Err(e) = nat.handle_datagram(data).await {
                                debug!("hy2 udp datagram error: {:#}", e);
                            }
                        });
                    }
                    Err(_) => {
                        debug!(
                            "dropping domain-targeted hy2 udp datagram: dns resolve queue full"
                        );
                    }
                }
            } else if let Err(e) = dgram_nat.handle_datagram(data).await {
                debug!("hy2 udp datagram error: {:#}", e);
            }
        }
    });

    while let Ok((send, recv)) = conn.accept_bi().await {
        let stream_ctx = ctx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_hy2_tcp_stream(send, recv, &stream_ctx).await {
                debug!("hy2 tcp stream ended: {:#}", e);
            }
        });
    }

    dgram_task.abort();
    udp_nat.shutdown();
    Ok(())
}

async fn handle_hy2_tcp_stream(
    mut quic_send: quinn::SendStream,
    mut quic_recv: quinn::RecvStream,
    ctx: &StreamContext,
) -> Result<()> {
    let started = Instant::now();
    let stream_type = read_varint(&mut quic_recv).await?;
    if stream_type != HY2_TCP_STREAM_TYPE {
        bail!("unsupported hy2 bi-stream type: 0x{stream_type:x}");
    }

    let addr_len = read_varint(&mut quic_recv).await? as usize;
    if addr_len == 0 || addr_len > 2048 {
        bail!("invalid hy2 tcp addr length: {addr_len}");
    }
    let mut addr_buf = vec![0u8; addr_len];
    quic_recv.read_exact(&mut addr_buf).await?;
    let addr_str = String::from_utf8(addr_buf).context("invalid utf-8 in hy2 tcp addr")?;

    let pad_len = read_varint(&mut quic_recv).await? as usize;
    if pad_len > 4096 {
        bail!("invalid hy2 tcp padding length: {pad_len}");
    }
    if pad_len > 0 {
        let mut pad = vec![0u8; pad_len];
        quic_recv.read_exact(&mut pad).await?;
    }

    let target = parse_hy2_addr(&addr_str)?;
    let internal_panel_addr = ctx
        .panel_route
        .as_ref()
        .and_then(|route| route.resolve_target(&target));
    let is_internal_panel = internal_panel_addr.is_some();

    let host_port = target.to_host_port();
    let (host_name, port, was_ip) = match &target {
        TargetAddr::Domain(h, p) => (h.clone(), *p, false),
        TargetAddr::Ip(addr) => (addr.ip().to_string(), addr.port(), true),
    };

    let dial_addr = internal_panel_addr
        .map(TargetAddr::Ip)
        .unwrap_or_else(|| target.clone());
    let tcp = match dns::connect_tcp(&dial_addr, TCP_CONNECT_TIMEOUT).await {
        Ok(s) => {
            let _ = s.set_nodelay(true);
            quic_send.write_all(&[0x00, 0x00, 0x00]).await?;
            s
        }
        Err(e) => {
            let msg = format!("{e}");
            let msg_bytes = msg.as_bytes();
            let _ = quic_send.write_u8(0x01).await;
            let _ = write_varint(&mut quic_send, msg_bytes.len() as u64).await;
            let _ = quic_send.write_all(msg_bytes).await;
            let _ = quic_send.write_u8(0x00).await;
            let _ = quic_send.finish();
            if !is_internal_panel {
                ctx.state.record_access(AccessLogRecord {
                    link_id: ctx.link_id.clone(),
                    link_name: ctx.link_name.clone(),
                    client_ip: ctx.client_ip.clone(),
                    proto: "TCP".to_string(),
                    root_domain: extract_root_domain(&host_name),
                    target: host_port,
                    tx_bytes: 0,
                    rx_bytes: 0,
                    duration_ms: started.elapsed().as_millis() as u64,
                    outcome: AccessOutcome::Error,
                });
            }
            return Err(e).with_context(|| format!("hy2 dial {addr_str}"));
        }
    };

    splice_and_audit_tcp(
        quic_send,
        quic_recv,
        tcp,
        TcpAuditMeta {
            host_name,
            port,
            was_ip,
            is_internal_panel,
            started,
        },
        ctx,
    )
    .await
}

struct Hy2UdpSession {
    socket: Arc<UdpSocket>,
    dual_stack: bool,
    last_active: Arc<AtomicU64>,
    seen_full: AtomicBool,
    seen_targets: Mutex<HashSet<String>>,
    rx_task: JoinHandle<()>,
}

struct Hy2UdpManager {
    conn: quinn::Connection,
    closed: AtomicBool,
    sessions: Arc<DashMap<u32, Arc<Hy2UdpSession>>>,
    reasm: Mutex<Hy2UdpReassembler>,
    ctx: Arc<StreamContext>,
    tx_packet_id: Arc<AtomicU16>,
    reaper: JoinHandle<()>,
}

impl Hy2UdpManager {
    fn new(conn: quinn::Connection, ctx: Arc<StreamContext>) -> Self {
        let sessions: Arc<DashMap<u32, Arc<Hy2UdpSession>>> = Arc::new(DashMap::new());
        let sessions_bg = sessions.clone();
        let reaper = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(15));
            loop {
                tick.tick().await;
                let now = now_secs();
                let expired: Vec<u32> = sessions_bg
                    .iter()
                    .filter_map(|e| {
                        let last = e.value().last_active.load(Ordering::Relaxed);
                        if now.saturating_sub(last) > HY2_UDP_IDLE_TIMEOUT_SECS {
                            Some(*e.key())
                        } else {
                            None
                        }
                    })
                    .collect();
                for sid in expired {
                    if let Some((_, sess)) = sessions_bg.remove(&sid) {
                        sess.rx_task.abort();
                    }
                }
            }
        });
        Self {
            conn,
            closed: AtomicBool::new(false),
            sessions,
            reasm: Mutex::new(Hy2UdpReassembler::default()),
            ctx,
            tx_packet_id: Arc::new(AtomicU16::new(1)),
            reaper,
        }
    }

    fn shutdown(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.reaper.abort();
        let keys: Vec<u32> = self.sessions.iter().map(|e| *e.key()).collect();
        for k in keys {
            if let Some((_, sess)) = self.sessions.remove(&k) {
                sess.rx_task.abort();
            }
        }
    }

    async fn handle_datagram(&self, raw: Bytes) -> Result<()> {
        if self.closed.load(Ordering::Acquire) {
            return Ok(());
        }
        let msg = decode_hy2_udp_message(&raw)?;
        if msg.frag_count == 0 || msg.frag_id >= msg.frag_count {
            return Ok(());
        }

        let session = self.get_or_create_session(msg.session_id)?;
        session.last_active.store(now_secs(), Ordering::Relaxed);

        let (addr_str, payload): (String, Bytes) = if msg.frag_count == 1 {
            (msg.addr, Bytes::copy_from_slice(msg.data))
        } else {
            let assembled = {
                let mut reasm = self.reasm.lock().unwrap_or_else(|e| e.into_inner());
                reasm.push(msg)
            };
            let Some((_, _, addr_s, data)) = assembled else {
                return Ok(());
            };
            (addr_s, data)
        };

        let target = parse_hy2_addr(&addr_str)?;
        let (domain_host, dst) = match target {
            TargetAddr::Ip(addr) => (None, addr),
            TargetAddr::Domain(host, port) => {
                let resolved = dns::resolve(&host, port)
                    .await?
                    .into_iter()
                    .find(|a| session.dual_stack || a.is_ipv4());
                match resolved {
                    Some(addr) => (Some(host), addr),
                    None => bail!("no address for {addr_str}"),
                }
            }
        };

        let send_addr = match (session.dual_stack, dst) {
            (true, SocketAddr::V4(v4)) => {
                SocketAddr::new(IpAddr::V6(v4.ip().to_ipv6_mapped()), v4.port())
            }
            (false, SocketAddr::V6(_)) => return Ok(()),
            _ => dst,
        };
        let sent = session.socket.send_to(&payload, send_addr).await?;
        self.ctx
            .counters
            .tx_bytes
            .fetch_add(sent as u64, Ordering::Relaxed);

        if self.ctx.state.is_access_log_enabled() && !session.seen_full.load(Ordering::Relaxed) {
            let is_new_target = {
                let mut seen = session.seen_targets.lock().unwrap_or_else(|e| e.into_inner());
                if seen.len() >= 64 {
                    session.seen_full.store(true, Ordering::Relaxed);
                    false
                } else {
                    let inserted = seen.insert(addr_str.clone());
                    if seen.len() >= 64 {
                        session.seen_full.store(true, Ordering::Relaxed);
                    }
                    inserted
                }
            };
            if is_new_target {
                let log_host = domain_host.unwrap_or_else(|| dst.ip().to_string());
                self.ctx.state.record_access(AccessLogRecord {
                    link_id: self.ctx.link_id.clone(),
                    link_name: self.ctx.link_name.clone(),
                    client_ip: self.ctx.client_ip.clone(),
                    proto: "UDP".to_string(),
                    root_domain: extract_root_domain(&log_host),
                    target: addr_str,
                    tx_bytes: sent as u64,
                    rx_bytes: 0,
                    duration_ms: 0,
                    outcome: AccessOutcome::Ok,
                });
            }
        }
        Ok(())
    }

    fn get_or_create_session(&self, session_id: u32) -> Result<Arc<Hy2UdpSession>> {
        if self.closed.load(Ordering::Acquire) {
            bail!("hy2 udp manager closed");
        }
        if let Some(existing) = self.sessions.get(&session_id) {
            return Ok(existing.clone());
        }
        let (std_sock, dual_stack) = bind_nat_socket()?;
        let socket = Arc::new(UdpSocket::from_std(std_sock)?);
        let last_active = Arc::new(AtomicU64::new(now_secs()));

        let rx_sock = socket.clone();
        let rx_conn = self.conn.clone();
        let rx_counters = self.ctx.counters.clone();
        let rx_last = last_active.clone();
        let rx_pid = self.tx_packet_id.clone();

        let rx_task = tokio::spawn(async move {
            let mut buf = vec![0u8; 65_535];
            'recv: while let Ok((n, src_addr)) = rx_sock.recv_from(&mut buf).await {
                rx_last.store(now_secs(), Ordering::Relaxed);
                rx_counters.rx_bytes.fetch_add(n as u64, Ordering::Relaxed);
                let src_addr = SocketAddr::new(src_addr.ip().to_canonical(), src_addr.port());
                let addr_str = src_addr.to_string();
                let max_dgram = rx_conn.max_datagram_size().unwrap_or(1150);
                let pid = rx_pid.fetch_add(1, Ordering::Relaxed);
                for pkt in
                    encode_hy2_udp_datagrams(session_id, pid, &addr_str, &buf[..n], max_dgram)
                {
                    if !protocol::pacer::send_datagram_checked(&rx_conn, pkt) {
                        break 'recv;
                    }
                }
            }
        });

        let entry = Arc::new(Hy2UdpSession {
            socket,
            dual_stack,
            last_active,
            seen_full: AtomicBool::new(false),
            seen_targets: Mutex::new(HashSet::new()),
            rx_task,
        });

        match self.sessions.entry(session_id) {
            dashmap::mapref::entry::Entry::Occupied(o) => {
                entry.rx_task.abort();
                Ok(o.get().clone())
            }
            dashmap::mapref::entry::Entry::Vacant(v) => {
                if self.closed.load(Ordering::Acquire) {
                    entry.rx_task.abort();
                    bail!("hy2 udp manager closed");
                }
                v.insert(entry.clone());
                Ok(entry)
            }
        }
    }
}
