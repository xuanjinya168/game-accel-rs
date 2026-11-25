//! Native Hysteria2 protocol support sharing the same QUIC/TLS 1.3 (ALPN `h3`) port,
//! user tokens, traffic counters, quota/expiry enforcement, and access logs as `gacl://`.
//!
//! Compatible with Shadowrocket (iOS), Clash Meta / Mihomo, sing-box, v2rayN, and
//! official Hysteria2 clients.

use crate::dns;
use crate::nat::bind_nat_socket;
use crate::sniff::{extract_root_domain, sniff_first_packet};
use crate::store::{AppState, AuthTicket, LiveCounters, generate_random_str};
use crate::{PanelRoute, TCP_CONNECT_TIMEOUT};
use anyhow::{Context, Result, bail};
use bytes::{BufMut, Bytes, BytesMut};
use dashmap::DashMap;
use protocol::frame::TargetAddr;
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use tracing::{debug, info};

const HY2_TCP_STREAM_TYPE: u64 = 0x401;
const HY2_UDP_IDLE_TIMEOUT_SECS: u64 = 60;
const H3_FRAME_HEADERS: u64 = 0x01;
const H3_FRAME_SETTINGS: u64 = 0x04;
const H3_STREAM_CONTROL: u8 = 0x00;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// QUIC variable-length integer (RFC 9000 Section 16).
pub(crate) fn encode_varint(val: u64, out: &mut BytesMut) {
    if val < 64 {
        out.put_u8(val as u8);
    } else if val < 16_384 {
        out.put_u16(0x4000 | (val as u16));
    } else if val < 1_073_741_824 {
        out.put_u32(0x8000_0000 | (val as u32));
    } else {
        out.put_u64(0xC000_0000_0000_0000 | val);
    }
}

pub(crate) fn decode_varint_slice(buf: &[u8]) -> Result<(u64, usize)> {
    let first = *buf.first().context("unexpected EOF reading varint")?;
    let len = 1usize << (first >> 6);
    if buf.len() < len {
        bail!("truncated varint (need {len} bytes, got {})", buf.len());
    }
    let mut val = (first & 0x3F) as u64;
    for &b in &buf[1..len] {
        val = (val << 8) | (b as u64);
    }
    Ok((val, len))
}

pub(crate) async fn read_varint<R: AsyncRead + Unpin>(r: &mut R) -> Result<u64> {
    let first = r.read_u8().await.context("read varint prefix")?;
    let len = 1usize << (first >> 6);
    let mut val = (first & 0x3F) as u64;
    for _ in 1..len {
        let b = r.read_u8().await.context("read varint body")?;
        val = (val << 8) | (b as u64);
    }
    Ok(val)
}

async fn write_varint<W: AsyncWrite + Unpin>(w: &mut W, val: u64) -> Result<()> {
    let mut buf = BytesMut::with_capacity(8);
    encode_varint(val, &mut buf);
    w.write_all(&buf).await?;
    Ok(())
}

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
) -> Result<()> {
    // 1. Send server HTTP/3 Control Stream (empty SETTINGS => QPACK dynamic table capacity 0).
    // Must stay open for the lifetime of the connection (RFC 9114 §6.2.1).
    let _ctrl_send = match conn.open_uni().await {
        Ok(mut s) => {
            let mut ctrl = BytesMut::with_capacity(8);
            ctrl.put_u8(H3_STREAM_CONTROL);
            encode_varint(H3_FRAME_SETTINGS, &mut ctrl);
            encode_varint(0, &mut ctrl);
            let _ = s.write_all(&ctrl).await;
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
    let headers = match read_h3_request_headers(&mut reader).await {
        Ok(h) => h,
        Err(e) => {
            let _ = write_h3_response(&mut auth_send, "404", &[]).await;
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
        let _ = write_h3_response(&mut auth_send, "404", &[]).await;
        let _ = auth_send.finish();
        uni_task.abort();
        bail!("non-hy2 HTTP/3 request ({method} {path})");
    }

    let ticket = match state.authenticate_token(auth_token) {
        Ok(t) => t,
        Err(e) => {
            let _ = write_h3_response(&mut auth_send, "404", &[]).await;
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

struct FragState {
    packet_id: u16,
    frag_count: u8,
    addr: String,
    frags: Vec<Option<Vec<u8>>>,
    received: u8,
}

struct Hy2UdpSession {
    socket: Arc<UdpSocket>,
    dual_stack: bool,
    last_active: Arc<AtomicU64>,
    frag: Mutex<Option<FragState>>,
    seen_targets: Mutex<HashSet<String>>,
    rx_task: JoinHandle<()>,
}

struct Hy2UdpManager {
    conn: quinn::Connection,
    sessions: Arc<DashMap<u32, Arc<Hy2UdpSession>>>,
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
            sessions,
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
        self.reaper.abort();
        let keys: Vec<u32> = self.sessions.iter().map(|e| *e.key()).collect();
        for k in keys {
            if let Some((_, sess)) = self.sessions.remove(&k) {
                sess.rx_task.abort();
            }
        }
    }

    async fn handle_datagram(&self, raw: Bytes) -> Result<()> {
        let msg = decode_hy2_udp_message(&raw)?;
        if msg.frag_count == 0 || msg.frag_id >= msg.frag_count {
            return Ok(());
        }

        let session = self.get_or_create_session(msg.session_id)?;
        session.last_active.store(now_secs(), Ordering::Relaxed);

        let (addr_str, payload): (String, Vec<u8>) = if msg.frag_count == 1 {
            (msg.addr, msg.data.to_vec())
        } else {
            let mut guard = session.frag.lock().expect("hy2 frag lock");
            let needs_reset = guard
                .as_ref()
                .is_none_or(|f| f.packet_id != msg.packet_id || f.frag_count != msg.frag_count);
            if needs_reset {
                *guard = Some(FragState {
                    packet_id: msg.packet_id,
                    frag_count: msg.frag_count,
                    addr: msg.addr.clone(),
                    frags: vec![None; msg.frag_count as usize],
                    received: 0,
                });
            }
            let st = guard.as_mut().unwrap();
            let idx = msg.frag_id as usize;
            if st.frags[idx].is_none() {
                st.frags[idx] = Some(msg.data.to_vec());
                st.received += 1;
            }
            if st.received < st.frag_count {
                return Ok(());
            }
            let done = guard.take().unwrap();
            let mut assembled = Vec::new();
            for part in done.frags.into_iter().flatten() {
                assembled.extend_from_slice(&part);
            }
            (done.addr, assembled)
        };

        let target = parse_hy2_addr(&addr_str)?;
        let (log_host, dst) = match target {
            TargetAddr::Ip(addr) => (addr.ip().to_string(), addr),
            TargetAddr::Domain(host, port) => {
                let resolved = dns::resolve(&host, port)
                    .await?
                    .into_iter()
                    .find(|a| session.dual_stack || a.is_ipv4());
                match resolved {
                    Some(addr) => (host, addr),
                    None => bail!("no address for {addr_str}"),
                }
            }
        };

        let is_new_target = {
            let mut seen = session.seen_targets.lock().expect("hy2 seen lock");
            seen.len() < 64 && seen.insert(addr_str.clone())
        };
        if is_new_target {
            self.state.record_access_log(
                &self.link_id,
                &self.link_name,
                &self.client_ip,
                "UDP",
                extract_root_domain(&log_host),
                addr_str,
                payload.len() as u64,
                0,
                0,
                "ok",
            );
        }

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
        Ok(())
    }

    fn get_or_create_session(&self, session_id: u32) -> Result<Arc<Hy2UdpSession>> {
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
            frag: Mutex::new(None),
            seen_targets: Mutex::new(HashSet::new()),
            rx_task,
        });

        match self.sessions.entry(session_id) {
            dashmap::mapref::entry::Entry::Occupied(o) => {
                entry.rx_task.abort();
                Ok(o.get().clone())
            }
            dashmap::mapref::entry::Entry::Vacant(v) => {
                v.insert(entry.clone());
                Ok(entry)
            }
        }
    }
}

struct Hy2UdpMessage<'a> {
    session_id: u32,
    packet_id: u16,
    frag_id: u8,
    frag_count: u8,
    addr: String,
    data: &'a [u8],
}

fn decode_hy2_udp_message(buf: &[u8]) -> Result<Hy2UdpMessage<'_>> {
    if buf.len() < 9 {
        bail!("hy2 udp datagram too short");
    }
    let session_id = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let packet_id = u16::from_be_bytes([buf[4], buf[5]]);
    let frag_id = buf[6];
    let frag_count = buf[7];
    let (addr_len, vlen) = decode_varint_slice(&buf[8..])?;
    let addr_len = addr_len as usize;
    let addr_start = 8 + vlen;
    let addr_end = addr_start
        .checked_add(addr_len)
        .context("hy2 udp addr len overflow")?;
    if addr_end > buf.len() {
        bail!("hy2 udp addr truncated");
    }
    let addr = std::str::from_utf8(&buf[addr_start..addr_end])
        .context("hy2 udp addr utf-8")?
        .to_string();
    Ok(Hy2UdpMessage {
        session_id,
        packet_id,
        frag_id,
        frag_count,
        addr,
        data: &buf[addr_end..],
    })
}

fn encode_hy2_udp_datagrams(
    session_id: u32,
    packet_id: u16,
    addr: &str,
    payload: &[u8],
    max_dgram: usize,
) -> Vec<Bytes> {
    let addr_bytes = addr.as_bytes();
    let mut header_base = BytesMut::with_capacity(16 + addr_bytes.len());
    header_base.put_u32(session_id);
    header_base.put_u16(packet_id);
    header_base.put_u8(0); // frag_id placeholder
    header_base.put_u8(1); // frag_count placeholder
    encode_varint(addr_bytes.len() as u64, &mut header_base);
    header_base.put_slice(addr_bytes);

    let header_len = header_base.len();
    if header_len + payload.len() <= max_dgram {
        header_base.put_slice(payload);
        return vec![header_base.freeze()];
    }

    let max_chunk = max_dgram.saturating_sub(header_len);
    if max_chunk == 0 {
        return Vec::new();
    }
    let chunks: Vec<&[u8]> = payload.chunks(max_chunk).collect();
    if chunks.len() > u8::MAX as usize {
        return Vec::new();
    }
    let count = chunks.len() as u8;
    let mut out = Vec::with_capacity(chunks.len());
    for (idx, chunk) in chunks.into_iter().enumerate() {
        let mut pkt = BytesMut::with_capacity(header_len + chunk.len());
        pkt.put_u32(session_id);
        pkt.put_u16(packet_id);
        pkt.put_u8(idx as u8);
        pkt.put_u8(count);
        encode_varint(addr_bytes.len() as u64, &mut pkt);
        pkt.put_slice(addr_bytes);
        pkt.put_slice(chunk);
        out.push(pkt.freeze());
    }
    out
}

fn parse_hy2_addr(addr: &str) -> Result<TargetAddr> {
    if let Ok(sock) = addr.parse::<SocketAddr>() {
        return Ok(TargetAddr::Ip(sock));
    }
    let (host, port_str) = addr
        .rsplit_once(':')
        .with_context(|| format!("missing port in hy2 addr: {addr}"))?;
    let port: u16 = port_str
        .parse()
        .with_context(|| format!("invalid port in hy2 addr: {addr}"))?;
    let clean_host = host.trim_matches(['[', ']']);
    if let Ok(ip) = clean_host.parse::<IpAddr>() {
        return Ok(TargetAddr::Ip(SocketAddr::new(ip, port)));
    }
    if clean_host.is_empty() {
        bail!("empty host in hy2 addr: {addr}");
    }
    Ok(TargetAddr::Domain(clean_host.to_string(), port))
}

// ============================================================================
// Minimal RFC 9114 HTTP/3 + RFC 9204 Static-Table QPACK + RFC 7541 Huffman
// ============================================================================

fn header_val<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

async fn read_h3_request_headers<R: AsyncRead + Unpin>(
    r: &mut R,
) -> Result<Vec<(String, String)>> {
    // Skip any non-HEADERS frames (e.g. GREASE) until we see HEADERS (0x01)
    for _ in 0..8 {
        let frame_type = read_varint(r).await?;
        let frame_len = read_varint(r).await? as usize;
        if frame_len > 16 * 1024 {
            bail!("HTTP/3 frame too large: {frame_len}");
        }
        let mut payload = vec![0u8; frame_len];
        r.read_exact(&mut payload).await?;
        if frame_type == H3_FRAME_HEADERS {
            return decode_qpack_static_block(&payload);
        }
    }
    bail!("no HTTP/3 HEADERS frame found")
}

async fn write_h3_response<W: AsyncWrite + Unpin>(
    w: &mut W,
    status: &str,
    extra_headers: &[(&str, &str)],
) -> Result<()> {
    let mut block = BytesMut::with_capacity(128);
    // QPACK prefix: Required Insert Count = 0, Delta Base = 0
    block.put_u8(0x00);
    block.put_u8(0x00);
    encode_qpack_literal(&mut block, ":status", status);
    for (k, v) in extra_headers {
        encode_qpack_literal(&mut block, k, v);
    }

    let mut frame = BytesMut::with_capacity(block.len() + 16);
    encode_varint(H3_FRAME_HEADERS, &mut frame);
    encode_varint(block.len() as u64, &mut frame);
    frame.put_slice(&block);
    w.write_all(&frame).await?;
    Ok(())
}

fn encode_qpack_literal(out: &mut BytesMut, name: &str, val: &str) {
    // RFC 9204 §4.5.6: Literal Field Line with Literal Name (001 N Hxxx, 3-bit prefix)
    encode_qpack_prefixed_int(out, 0x20, 3, name.len() as u64);
    out.put_slice(name.as_bytes());
    // Value string (H=0, 7-bit prefix)
    encode_qpack_prefixed_int(out, 0x00, 7, val.len() as u64);
    out.put_slice(val.as_bytes());
}

fn encode_qpack_prefixed_int(out: &mut BytesMut, first_mask: u8, prefix_bits: u8, mut val: u64) {
    let max_prefix = (1u64 << prefix_bits) - 1;
    if val < max_prefix {
        out.put_u8(first_mask | (val as u8));
    } else {
        out.put_u8(first_mask | (max_prefix as u8));
        val -= max_prefix;
        while val >= 128 {
            out.put_u8(((val & 0x7F) as u8) | 0x80);
            val >>= 7;
        }
        out.put_u8(val as u8);
    }
}

fn decode_qpack_prefixed_int(buf: &[u8], pos: &mut usize, prefix_bits: u8) -> Result<u64> {
    if *pos >= buf.len() {
        bail!("qpack unexpected EOF");
    }
    let mask = ((1u16 << prefix_bits) - 1) as u8;
    let mut val = (buf[*pos] & mask) as u64;
    *pos += 1;
    if val < mask as u64 {
        return Ok(val);
    }
    let mut shift = 0u32;
    loop {
        if *pos >= buf.len() {
            bail!("qpack truncated varint");
        }
        let b = buf[*pos];
        *pos += 1;
        val = val
            .checked_add(((b & 0x7F) as u64).checked_shl(shift).context("qpack int overflow")?)
            .context("qpack int overflow")?;
        if b & 0x80 == 0 {
            return Ok(val);
        }
        shift += 7;
        if shift > 56 {
            bail!("qpack varint too long");
        }
    }
}

fn decode_qpack_string(buf: &[u8], pos: &mut usize, prefix_bits: u8) -> Result<String> {
    if *pos >= buf.len() {
        bail!("qpack string EOF");
    }
    let huffman_bit = 1u8 << prefix_bits;
    let is_huffman = (buf[*pos] & huffman_bit) != 0;
    let len = decode_qpack_prefixed_int(buf, pos, prefix_bits)? as usize;
    if *pos + len > buf.len() {
        bail!("qpack string truncated");
    }
    let raw = &buf[*pos..*pos + len];
    *pos += len;
    let bytes = if is_huffman {
        huffman_decode(raw)?
    } else {
        raw.to_vec()
    };
    String::from_utf8(bytes).context("qpack header not utf-8")
}

fn decode_qpack_static_block(buf: &[u8]) -> Result<Vec<(String, String)>> {
    let mut pos = 0usize;
    let req_insert_count = decode_qpack_prefixed_int(buf, &mut pos, 8)?;
    let _delta_base = decode_qpack_prefixed_int(buf, &mut pos, 7)?;
    if req_insert_count != 0 {
        bail!("QPACK dynamic table not supported (RIC={req_insert_count})");
    }

    let mut headers = Vec::new();
    while pos < buf.len() {
        let b = buf[pos];
        if b & 0x80 != 0 {
            // Indexed Field Line (1 T xxxxxx)
            let is_static = (b & 0x40) != 0;
            let idx = decode_qpack_prefixed_int(buf, &mut pos, 6)? as usize;
            if is_static && let Some((k, v)) = qpack_static_entry(idx) {
                headers.push((k.to_string(), v.to_string()));
            }
        } else if b & 0xC0 == 0x40 {
            // Literal Field Line with Name Reference (01 N T xxxx)
            let is_static = (b & 0x10) != 0;
            let idx = decode_qpack_prefixed_int(buf, &mut pos, 4)? as usize;
            let val = decode_qpack_string(buf, &mut pos, 7)?;
            if is_static && let Some((k, _)) = qpack_static_entry(idx) {
                headers.push((k.to_string(), val));
            }
        } else if b & 0xE0 == 0x20 {
            // Literal Field Line with Literal Name (001 N H xxx)
            let name = decode_qpack_string(buf, &mut pos, 3)?;
            let val = decode_qpack_string(buf, &mut pos, 7)?;
            headers.push((name, val));
        } else {
            bail!("unsupported QPACK field line prefix: 0x{b:02x}");
        }
    }
    Ok(headers)
}

/// Subset of RFC 9204 Appendix A QPACK Static Table relevant to HTTP/3 requests.
fn qpack_static_entry(idx: usize) -> Option<(&'static str, &'static str)> {
    Some(match idx {
        0 => (":authority", ""),
        1 => (":path", "/"),
        15 => (":method", "CONNECT"),
        16 => (":method", "DELETE"),
        17 => (":method", "GET"),
        18 => (":method", "HEAD"),
        19 => (":method", "OPTIONS"),
        20 => (":method", "POST"),
        21 => (":method", "PUT"),
        22 => (":scheme", "http"),
        23 => (":scheme", "https"),
        24 => (":status", "103"),
        25 => (":status", "200"),
        26 => (":status", "304"),
        27 => (":status", "404"),
        28 => (":status", "503"),
        29 => ("accept", "*/*"),
        31 => ("accept-encoding", "gzip, deflate, br"),
        46 => ("content-type", "application/json"),
        50 => ("host", ""),
        95 => ("user-agent", ""),
        _ => return None,
    })
}

/// RFC 7541 Appendix B Huffman table: `(code, bit_len)` for byte values `0..=255`.
const HUFFMAN_CODES: [(u32, u8); 256] = [
    (0x1ff8, 13), (0x7fffd8, 23), (0xfffffe2, 28), (0xfffffe3, 28), (0xfffffe4, 28), (0xfffffe5, 28), (0xfffffe6, 28), (0xfffffe7, 28),
    (0xfffffe8, 28), (0xffffea, 24), (0x3ffffffc, 30), (0xfffffe9, 28), (0xfffffea, 28), (0x3ffffffd, 30), (0xfffffeb, 28), (0xfffffec, 28),
    (0xfffffed, 28), (0xfffffee, 28), (0xfffffef, 28), (0xffffff0, 28), (0xffffff1, 28), (0xffffff2, 28), (0x3ffffffe, 30), (0xffffff3, 28),
    (0xffffff4, 28), (0xffffff5, 28), (0xffffff6, 28), (0xffffff7, 28), (0xffffff8, 28), (0xffffff9, 28), (0xffffffa, 28), (0xffffffb, 28),
    (0x14, 6), (0x3f8, 10), (0x3f9, 10), (0xffa, 12), (0x1ff9, 13), (0x15, 6), (0xf8, 8), (0x7fa, 11),
    (0x3fa, 10), (0x3fb, 10), (0xf9, 8), (0x7fb, 11), (0xfa, 8), (0x16, 6), (0x17, 6), (0x18, 6),
    (0x0, 5), (0x1, 5), (0x2, 5), (0x19, 6), (0x1a, 6), (0x1b, 6), (0x1c, 6), (0x1d, 6),
    (0x1e, 6), (0x1f, 6), (0x5c, 7), (0xfb, 8), (0x7ffc, 15), (0x20, 6), (0xffb, 12), (0x3fc, 10),
    (0x1ffa, 13), (0x21, 6), (0x5d, 7), (0x5e, 7), (0x5f, 7), (0x60, 7), (0x61, 7), (0x62, 7),
    (0x63, 7), (0x64, 7), (0x65, 7), (0x66, 7), (0x67, 7), (0x68, 7), (0x69, 7), (0x6a, 7),
    (0x6b, 7), (0x6c, 7), (0x6d, 7), (0x6e, 7), (0x6f, 7), (0x70, 7), (0x71, 7), (0x72, 7),
    (0xfc, 8), (0x73, 7), (0xfd, 8), (0x1ffb, 13), (0x7fff0, 19), (0x1ffc, 13), (0x3ffc, 14), (0x22, 6),
    (0x7ffd, 15), (0x3, 5), (0x23, 6), (0x4, 5), (0x24, 6), (0x5, 5), (0x25, 6), (0x26, 6),
    (0x27, 6), (0x6, 5), (0x74, 7), (0x75, 7), (0x28, 6), (0x29, 6), (0x2a, 6), (0x7, 5),
    (0x2b, 6), (0x76, 7), (0x2c, 6), (0x8, 5), (0x9, 5), (0x2d, 6), (0x77, 7), (0x78, 7),
    (0x79, 7), (0x7a, 7), (0x7b, 7), (0x7ffe, 15), (0x7fc, 11), (0x3ffd, 14), (0x1ffd, 13), (0xffffffc, 28),
    (0xfffe6, 20), (0x3fffd2, 22), (0xfffe7, 20), (0xfffe8, 20), (0x3fffd3, 22), (0x3fffd4, 22), (0x3fffd5, 22), (0x7fffd9, 23),
    (0x3fffd6, 22), (0x7fffda, 23), (0x7fffdb, 23), (0x7fffdc, 23), (0x7fffdd, 23), (0x7fffde, 23), (0xffffeb, 24), (0x7fffdf, 23),
    (0xffffec, 24), (0xffffed, 24), (0x3fffd7, 22), (0x7fffe0, 23), (0xffffee, 24), (0x7fffe1, 23), (0x7fffe2, 23), (0x7fffe3, 23),
    (0x7fffe4, 23), (0x1fffdc, 21), (0x3fffd8, 22), (0x7fffe5, 23), (0x3fffd9, 22), (0x7fffe6, 23), (0x7fffe7, 23), (0xffffef, 24),
    (0x3fffda, 22), (0x1fffdd, 21), (0xfffe9, 20), (0x3fffdb, 22), (0x3fffdc, 22), (0x7fffe8, 23), (0x7fffe9, 23), (0x1fffde, 21),
    (0x7fffea, 23), (0x3fffdd, 22), (0x3fffde, 22), (0xfffff0, 24), (0x1fffdf, 21), (0x3fffdf, 22), (0x7fffeb, 23), (0x7fffec, 23),
    (0x1fffe0, 21), (0x1fffe1, 21), (0x3fffe0, 22), (0x1fffe2, 21), (0x7fffed, 23), (0x3fffe1, 22), (0x7fffee, 23), (0x7fffef, 23),
    (0xfffea, 20), (0x3fffe2, 22), (0x3fffe3, 22), (0x3fffe4, 22), (0x7ffff0, 23), (0x3fffe5, 22), (0x3fffe6, 22), (0x7ffff1, 23),
    (0x3ffffe0, 26), (0x3ffffe1, 26), (0xfffeb, 20), (0x7fff1, 19), (0x3fffe7, 22), (0x7ffff2, 23), (0x3fffe8, 22), (0x1ffffec, 25),
    (0x3ffffe2, 26), (0x3ffffe3, 26), (0x3ffffe4, 26), (0x7ffffde, 27), (0x7ffffdf, 27), (0x3ffffe5, 26), (0xfffff1, 24), (0x1ffffed, 25),
    (0x7fff2, 19), (0x1fffe3, 21), (0x3ffffe6, 26), (0x7ffffe0, 27), (0x7ffffe1, 27), (0x3ffffe7, 26), (0x7ffffe2, 27), (0xfffff2, 24),
    (0x1fffe4, 21), (0x1fffe5, 21), (0x3ffffe8, 26), (0x3ffffe9, 26), (0xffffffd, 28), (0x7ffffe3, 27), (0x7ffffe4, 27), (0x7ffffe5, 27),
    (0xfffec, 20), (0xfffff3, 24), (0xfffed, 20), (0x1fffe6, 21), (0x3fffe9, 22), (0x1fffe7, 21), (0x1fffe8, 21), (0x7ffff3, 23),
    (0x3fffea, 22), (0x3fffeb, 22), (0x1ffffee, 25), (0x1ffffef, 25), (0xfffff4, 24), (0xfffff5, 24), (0x3ffffea, 26), (0x7ffff4, 23),
    (0x3ffffeb, 26), (0x7ffffe6, 27), (0x3ffffec, 26), (0x3ffffed, 26), (0x7ffffe7, 27), (0x7ffffe8, 27), (0x7ffffe9, 27), (0x7ffffea, 27),
    (0x7ffffeb, 27), (0xffffffe, 28), (0x7ffffec, 27), (0x7ffffed, 27), (0x7ffffee, 27), (0x7ffffef, 27), (0x7fffff0, 27), (0x3ffffee, 26),
];

fn huffman_lookup() -> &'static HashMap<(u8, u32), u8> {
    static TABLE: std::sync::LazyLock<HashMap<(u8, u32), u8>> = std::sync::LazyLock::new(|| {
        let mut m = HashMap::with_capacity(256);
        for (sym, &(code, len)) in HUFFMAN_CODES.iter().enumerate() {
            m.insert((len, code), sym as u8);
        }
        m
    });
    &TABLE
}

fn huffman_decode(input: &[u8]) -> Result<Vec<u8>> {
    let table = huffman_lookup();
    let mut out = Vec::with_capacity(input.len());
    let mut acc: u32 = 0;
    let mut bits: u8 = 0;

    for &byte in input {
        for shift in (0..8).rev() {
            acc = (acc << 1) | (((byte >> shift) & 1) as u32);
            bits += 1;
            if bits >= 5 && let Some(&sym) = table.get(&(bits, acc)) {
                out.push(sym);
                acc = 0;
                bits = 0;
            } else if bits > 30 {
                bail!("invalid RFC 7541 Huffman sequence");
            }
        }
    }

    if bits > 7 {
        bail!("invalid Huffman padding length: {bits}");
    }
    if bits > 0 {
        let expected_pad = (1u32 << bits) - 1;
        if acc != expected_pad {
            bail!("invalid Huffman EOS padding");
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn huffman_encode(input: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut cur: u8 = 0;
        let mut cur_bits: u8 = 0;
        for &b in input {
            let (code, len) = HUFFMAN_CODES[b as usize];
            for i in (0..len).rev() {
                cur = (cur << 1) | (((code >> i) & 1) as u8);
                cur_bits += 1;
                if cur_bits == 8 {
                    out.push(cur);
                    cur = 0;
                    cur_bits = 0;
                }
            }
        }
        if cur_bits > 0 {
            cur = (cur << (8 - cur_bits)) | ((1u8 << (8 - cur_bits)) - 1);
            out.push(cur);
        }
        out
    }

    #[test]
    fn huffman_roundtrip_hy2_headers() {
        for s in ["/auth", "hysteria-auth", "iWIQciLnamSRXTdBhF2ka6ov", "233", "www.goldpeak.top"] {
            let enc = huffman_encode(s.as_bytes());
            let dec = huffman_decode(&enc).unwrap();
            assert_eq!(dec, s.as_bytes(), "failed for {s}");
        }
    }

    #[test]
    fn qpack_decodes_huffman_and_static_hy2_auth() {
        // Construct a realistic HTTP/3 POST /auth QPACK block like quic-go / Shadowrocket:
        // Prefix (0, 0) + Indexed Static 20 (:method POST) + Literal NameRef Static 1 (:path "/auth" huffman)
        // + Literal Name ("hysteria-auth" huffman) & Value ("tok-12345" huffman)
        let mut blk = BytesMut::new();
        blk.put_u8(0x00);
        blk.put_u8(0x00);
        // Indexed static 20 => 0xC0 | 20 = 0xD4
        blk.put_u8(0xC0 | 20);
        // Literal name ref static 1 (:path) => 0x51, value huffman "/auth"
        blk.put_u8(0x51);
        let path_h = huffman_encode(b"/auth");
        encode_qpack_prefixed_int(&mut blk, 0x80, 7, path_h.len() as u64);
        blk.put_slice(&path_h);
        // Literal name ("hysteria-auth" huffman): 001 0 1 xxx => 0x28
        let name_h = huffman_encode(b"hysteria-auth");
        encode_qpack_prefixed_int(&mut blk, 0x28, 3, name_h.len() as u64);
        blk.put_slice(&name_h);
        let val_h = huffman_encode(b"tok-12345");
        encode_qpack_prefixed_int(&mut blk, 0x80, 7, val_h.len() as u64);
        blk.put_slice(&val_h);

        let headers = decode_qpack_static_block(&blk).unwrap();
        assert_eq!(header_val(&headers, ":method"), Some("POST"));
        assert_eq!(header_val(&headers, ":path"), Some("/auth"));
        assert_eq!(header_val(&headers, "hysteria-auth"), Some("tok-12345"));
    }

    #[test]
    fn hy2_udp_datagram_fragmentation_roundtrip() {
        let payload: Vec<u8> = (0..2500).map(|i| (i % 251) as u8).collect();
        let pkts = encode_hy2_udp_datagrams(42, 7, "1.1.1.1:53", &payload, 1000);
        assert!(pkts.len() >= 3);
        let mut assembled = Vec::new();
        for (i, p) in pkts.iter().enumerate() {
            let m = decode_hy2_udp_message(p).unwrap();
            assert_eq!(m.session_id, 42);
            assert_eq!(m.packet_id, 7);
            assert_eq!(m.frag_id as usize, i);
            assert_eq!(m.frag_count as usize, pkts.len());
            assert_eq!(m.addr, "1.1.1.1:53");
            assembled.extend_from_slice(m.data);
        }
        assert_eq!(assembled, payload);
    }
}
