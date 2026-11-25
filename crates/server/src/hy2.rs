//! Native Hysteria2 protocol support sharing the same QUIC/TLS 1.3 (ALPN `h3`) port,
//! user tokens, traffic counters, quota/expiry enforcement, and access logs as `gacl://`.
//!
//! Compatible with Shadowrocket (iOS), Clash Meta / Mihomo, sing-box, v2rayN, and
//! official Hysteria2 clients.

use crate::dns;
use crate::masquerade::{serve_masquerade_followup, serve_masquerade_h3};
use crate::nat::bind_nat_socket;
use crate::store::{AppState, AuthTicket, LiveCounters, generate_random_str};
use crate::{PanelRoute, TCP_CONNECT_TIMEOUT};
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use dashmap::DashMap;
use protocol::frame::TargetAddr;
use protocol::hy2::{
    HY2_TCP_STREAM_TYPE, Hy2UdpReassembler, decode_hy2_udp_message, encode_h3_control_stream,
    encode_hy2_udp_datagrams, header_val, parse_hy2_addr, read_h3_headers, write_h3_response,
    write_varint,
};
#[cfg(test)]
pub(crate) use protocol::hy2::encode_varint;
pub(crate) use protocol::hy2::read_varint;
use protocol::sniff::{extract_root_domain, sniff_first_packet};
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

const HY2_UDP_IDLE_TIMEOUT_SECS: u64 = 60;

/// Reader that yields a small already-peeked prefix before delegating to `inner`.
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

/// Handle a Hysteria2 connection after the first 4 bytes of its initial bi-stream
/// did not match `b"GACL"`.
pub(crate) async fn handle_hy2_connection(
    conn: quinn::Connection,
    first_four: [u8; 4],
    mut auth_send: quinn::SendStream,
    auth_recv: quinn::RecvStream,
    state: Arc<AppState>,
    panel_route: Option<Arc<PanelRoute>>,
    masquerade_dir: Option<Arc<PathBuf>>,
) -> Result<()> {
    // 1. Send server HTTP/3 Control Stream (empty SETTINGS => QPACK dynamic table capacity 0).
    // Must stay open for the lifetime of the connection (RFC 9114 §6.2.1).
    let _ctrl_send = match conn.open_uni().await {
        Ok(mut s) => {
            let _ = s.write_all(&encode_h3_control_stream()).await;
            Some(s)
        }
        Err(_) => None,
    };

    // Drain client's unidirectional HTTP/3 control / QPACK streams in the background.
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

    // 2. Read HTTP/3 HEADERS frame on the first bi-stream and decode QPACK headers.
    let mut reader = PrefixedReader::new(first_four, auth_recv);
    let headers = match read_h3_headers(&mut reader).await {
        Ok(h) => h,
        Err(e) => {
            let _ = serve_masquerade_h3(&mut auth_send, "GET", "/404", masquerade_dir.as_deref().map(PathBuf::as_path)).await;
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
        let _ = serve_masquerade_h3(&mut auth_send, method, path, masquerade_dir.as_deref().map(PathBuf::as_path)).await;
        let _ = auth_send.finish();
        // Serve follow-up HTTP/3 asset requests (CSS/JS/images) on this same QUIC connection
        // so real browsers / HTTP/3 probes see a complete working website.
        if let Some(root) = masquerade_dir.as_deref() {
            serve_masquerade_followup(&conn, root).await;
        }
        uni_task.abort();
        bail!("non-hy2 HTTP/3 request ({method} {path})");
    }

    let ticket = match state.authenticate_token(auth_token) {
        Ok(t) => t,
        Err(e) => {
            let _ = serve_masquerade_h3(&mut auth_send, "POST", "/auth", masquerade_dir.as_deref().map(PathBuf::as_path)).await;
            let _ = auth_send.finish();
            state.record_access_log(
                "",
                "未授权客户端 (Hy2)",
                &client_ip,
                "AUTH",
                "-".to_string(),
                format!("{e:#}"),
                0,
                0,
                0,
                "denied",
            );
            uni_task.abort();
            return Err(e);
        }
    };

    // 3. Respond with HTTP/3 233 Auth OK (standard Hysteria2 handshake).
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

    let cid = state.register_conn(&ticket, conn.clone());
    let res = run_hy2_session(conn, &ticket, state.clone(), client_ip, panel_route).await;

    uni_task.abort();
    state.unregister_conn(&ticket, cid);
    res
}

async fn run_hy2_session(
    conn: quinn::Connection,
    ticket: &AuthTicket,
    state: Arc<AppState>,
    client_ip: String,
    panel_route: Option<Arc<PanelRoute>>,
) -> Result<()> {
    // Background Hysteria2 UDP datagram relay
    let udp_nat = Arc::new(Hy2UdpManager::new(
        conn.clone(),
        ticket.counters.clone(),
        state.clone(),
        ticket.link_id.clone(),
        ticket.link_name.clone(),
        client_ip.clone(),
    ));
    let dgram_conn = conn.clone();
    let dgram_nat = udp_nat.clone();
    let resolve_slots = Arc::new(tokio::sync::Semaphore::new(32));
    let dgram_task = tokio::spawn(async move {
        while let Ok(data) = dgram_conn.read_datagram().await {
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
                    debug!("dropping hy2 udp datagram: dispatch queue full");
                }
            }
        }
    });

    // Accept Hysteria2 TCP proxy streams (`0x401`)
    while let Ok((send, recv)) = conn.accept_bi().await {
        let counters = ticket.counters.clone();
        let st = state.clone();
        let lid = ticket.link_id.clone();
        let lname = ticket.link_name.clone();
        let cip = client_ip.clone();
        let proute = panel_route.clone();
        tokio::spawn(async move {
            if let Err(e) =
                handle_hy2_tcp_stream(send, recv, counters, st, lid, lname, cip, proute).await
            {
                debug!("hy2 tcp stream ended: {:#}", e);
            }
        });
    }

    dgram_task.abort();
    udp_nat.shutdown();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_hy2_tcp_stream(
    mut quic_send: quinn::SendStream,
    mut quic_recv: quinn::RecvStream,
    counters: Arc<LiveCounters>,
    state: Arc<AppState>,
    link_id: String,
    link_name: String,
    client_ip: String,
    panel_route: Option<Arc<PanelRoute>>,
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
    let internal_panel_addr = panel_route
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
            // Hysteria2 TCPResponse: status=0x00, msg_len=0, padding_len=0
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
                state.record_access_log(
                    &link_id,
                    &link_name,
                    &client_ip,
                    "TCP",
                    extract_root_domain(&host_name),
                    host_port,
                    0,
                    0,
                    started.elapsed().as_millis() as u64,
                    "error",
                );
            }
            return Err(e).with_context(|| format!("hy2 dial {addr_str}"));
        }
    };

    let (mut tcp_read, mut tcp_write) = tcp.into_split();
    let c_tx = counters.clone();
    let c_rx = counters.clone();

    let s2c = tokio::spawn(async move {
        let rx = protocol::io::copy_with_counter(&mut tcp_read, &mut quic_send, &c_rx.rx_bytes).await;
        let _ = quic_send.finish();
        rx
    });

    let c2s = tokio::spawn(async move {
        let mut first_buf = vec![0u8; 4096];
        let mut tx = 0u64;
        let mut proto = if port == 443 {
            "HTTPS"
        } else if port == 80 {
            "HTTP"
        } else {
            "TCP"
        };
        let mut final_host = host_name;

        if let Ok(Some(n)) = quic_recv.read(&mut first_buf).await
            && n > 0
        {
            let slice = &first_buf[..n];
            if let Some((p, h)) = sniff_first_packet(slice) {
                proto = p;
                if was_ip || final_host.is_empty() {
                    final_host = h;
                }
            }
            if tcp_write.write_all(slice).await.is_ok() {
                c_tx.tx_bytes.fetch_add(n as u64, Ordering::Relaxed);
                tx += n as u64;
                tx += protocol::io::copy_with_counter(&mut quic_recv, &mut tcp_write, &c_tx.tx_bytes)
                    .await;
            }
        }
        let _ = tcp_write.shutdown().await;
        (tx, proto, final_host)
    });

    let (tx_res, rx_res) = tokio::join!(c2s, s2c);
    let (tx_bytes, proto, final_host) = tx_res.unwrap_or((0, "TCP", String::new()));
    let rx_bytes = rx_res.unwrap_or(0);

    if !is_internal_panel {
        state.record_access_log(
            &link_id,
            &link_name,
            &client_ip,
            proto,
            extract_root_domain(&final_host),
            format!("{final_host}:{port}"),
            tx_bytes,
            rx_bytes,
            started.elapsed().as_millis() as u64,
            "ok",
        );
    }
    Ok(())
}

// ============================================================================
// Hysteria2 UDP Datagram Relay (with fragmentation & reassembly)
// ============================================================================

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
    counters: Arc<LiveCounters>,
    state: Arc<AppState>,
    link_id: String,
    link_name: String,
    client_ip: String,
    tx_packet_id: Arc<AtomicU16>,
    reaper: JoinHandle<()>,
}

impl Hy2UdpManager {
    fn new(
        conn: quinn::Connection,
        counters: Arc<LiveCounters>,
        state: Arc<AppState>,
        link_id: String,
        link_name: String,
        client_ip: String,
    ) -> Self {
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
            counters,
            state,
            link_id,
            link_name,
            client_ip,
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
        self.counters
            .tx_bytes
            .fetch_add(sent as u64, Ordering::Relaxed);

        if self.state.is_access_log_enabled() && !session.seen_full.load(Ordering::Relaxed) {
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
                self.state.record_access_log(
                    &self.link_id,
                    &self.link_name,
                    &self.client_ip,
                    "UDP",
                    extract_root_domain(&log_host),
                    addr_str,
                    sent as u64,
                    0,
                    0,
                    "ok",
                );
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
        let rx_counters = self.counters.clone();
        let rx_last = last_active.clone();
        let rx_pid = self.tx_packet_id.clone();

        let rx_task = tokio::spawn(async move {
            let mut buf = vec![0u8; 65_535];
            while let Ok((n, src_addr)) = rx_sock.recv_from(&mut buf).await {
                rx_last.store(now_secs(), Ordering::Relaxed);
                rx_counters.rx_bytes.fetch_add(n as u64, Ordering::Relaxed);
                let src_addr = SocketAddr::new(src_addr.ip().to_canonical(), src_addr.port());
                let addr_str = src_addr.to_string();
                let max_dgram = rx_conn.max_datagram_size().unwrap_or(1150);
                let pid = rx_pid.fetch_add(1, Ordering::Relaxed);
                for pkt in encode_hy2_udp_datagrams(session_id, pid, &addr_str, &buf[..n], max_dgram) {
                    if rx_conn.send_datagram(pkt).is_err() {
                        break;
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

