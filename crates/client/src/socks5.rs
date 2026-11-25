use crate::rules::{Route, Router};
use crate::{AbortOnDrop, ERR_PORT_IN_USE};
use anyhow::{Context, Result, bail};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use protocol::auth::AUTH_OK;
use protocol::dedup::DedupWindow;
use protocol::frame::{
    ATYP_DOMAIN, ATYP_IPV4, ATYP_IPV6, FLAG_GAME_MODE, TargetAddr, UdpFrame,
};
use protocol::pacer::{UdpDupPacer, send_datagram_checked};
use protocol::share::NodeProtocol;
use quinn::Connection;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

pub type UdpDispatchMap = Arc<DashMap<u16, mpsc::Sender<UdpFrame>>>;

pub(crate) const SOCKS_FLOW_MIN: u16 = 1;
pub(crate) const SOCKS_FLOW_MAX: u16 = 32767;
pub(crate) const TUN_FLOW_MIN: u16 = 32768;
pub(crate) const TUN_FLOW_MAX: u16 = 65534;

/// Timeout for DIRECT (non-tunnelled) TCP dials from this machine.
const DIRECT_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Atomically reserve an unused `flow_id` strictly within `[min_id, max_id]` and insert `tx`
/// into `dispatch`, skipping any currently occupied IDs even across wraparound.
pub(crate) fn reserve_flow_id(
    cursor: &AtomicU16,
    dispatch: &UdpDispatchMap,
    tx: mpsc::Sender<UdpFrame>,
    min_id: u16,
    max_id: u16,
) -> Option<u16> {
    let span = (max_id - min_id) as usize + 1;
    let mut slot = Some(tx);
    for _ in 0..span {
        let prev = cursor
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                let clamped = if cur < min_id || cur > max_id {
                    min_id
                } else {
                    cur
                };
                let next = if clamped >= max_id {
                    min_id
                } else {
                    clamped + 1
                };
                Some(next)
            })
            .unwrap_or(min_id);
        let id = if prev < min_id || prev > max_id {
            min_id
        } else {
            prev
        };
        if let Entry::Vacant(v) = dispatch.entry(id) {
            v.insert(slot.take().expect("sender present"));
            return Some(id);
        }
    }
    None
}

#[derive(Debug, Default)]
pub struct ClientStats {
    pub connected: AtomicBool,
    pub active_tcp: AtomicU64,
    pub active_udp: AtomicU64,
    pub tx_bytes: AtomicU64,
    pub rx_bytes: AtomicU64,
    pub rtt_ms: AtomicU64,
}

impl ClientStats {
    pub fn reset(&self) {
        self.connected.store(false, Ordering::Relaxed);
        self.active_tcp.store(0, Ordering::Relaxed);
        self.active_udp.store(0, Ordering::Relaxed);
        self.tx_bytes.store(0, Ordering::Relaxed);
        self.rx_bytes.store(0, Ordering::Relaxed);
        self.rtt_ms.store(0, Ordering::Relaxed);
    }
}

/// Mixed (HTTP + HTTPS CONNECT + SOCKS5 TCP/UDP) proxy listener on a single port.
/// Uses `tcp_conn` for TCP streams and dedicated `udp_conn` for SOCKS5 UDP datagrams.
#[allow(clippy::too_many_arguments)]
pub async fn run_mixed_proxy_server(
    listen_addr: SocketAddr,
    tcp_conn: Connection,
    udp_conn: Connection,
    pacer: UdpDupPacer,
    udp_dispatch: UdpDispatchMap,
    protocol: NodeProtocol,
    game_dup: bool,
    stats: Arc<ClientStats>,
    router: Arc<Router>,
    log_tx: Option<mpsc::UnboundedSender<String>>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let listener = TcpListener::bind(listen_addr)
        .await
        .with_context(|| format!("本地端口 {listen_addr} {ERR_PORT_IN_USE}聚合代理"))?;
    let next_flow_id = Arc::new(AtomicU16::new(SOCKS_FLOW_MIN));
    let msg = format!("本地聚合代理 (HTTP + SOCKS5) 已监听 {listen_addr}");
    info!("{}", msg);
    if let Some(tx) = &log_tx {
        let _ = tx.send(msg);
    }

    loop {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                // A transient accept failure (peer RST before accept, EMFILE, ...) must not
                // tear down the whole tunnel: log, back off briefly, keep serving.
                let (stream, peer) = match accepted {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("local proxy accept error: {e}");
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let tcp_conn = tcp_conn.clone();
                let udp_conn = udp_conn.clone();
                let pacer = pacer.clone();
                let dispatch = udp_dispatch.clone();
                let next_flow_id = next_flow_id.clone();
                let stats = stats.clone();
                let router = router.clone();

                tokio::spawn(async move {
                    let _ = stream.set_nodelay(true);
                    if let Err(e) = handle_mixed_client(
                        stream, tcp_conn, udp_conn, pacer, dispatch, next_flow_id, protocol,
                        game_dup, stats, router,
                    )
                    .await
                    {
                        debug!("proxy client {} ended: {:#}", peer, e);
                    }
                });
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_mixed_client(
    stream: TcpStream,
    tcp_conn: Connection,
    udp_conn: Connection,
    pacer: UdpDupPacer,
    udp_dispatch: UdpDispatchMap,
    next_flow_id: Arc<AtomicU16>,
    protocol: NodeProtocol,
    game_dup: bool,
    stats: Arc<ClientStats>,
    router: Arc<Router>,
) -> Result<()> {
    let mut first = [0u8; 1];
    let n = stream.peek(&mut first).await?;
    if n == 0 {
        return Ok(());
    }
    if first[0] == 0x05 {
        handle_socks5_client(
            stream,
            tcp_conn,
            udp_conn,
            pacer,
            udp_dispatch,
            next_flow_id,
            protocol,
            game_dup,
            stats,
            router,
        )
        .await
    } else {
        handle_http_proxy_client(stream, tcp_conn, protocol, stats, router).await
    }
}

/// Handle HTTP / HTTPS (CONNECT) proxy requests on the mixed port.
async fn handle_http_proxy_client(
    mut client: TcpStream,
    conn: Connection,
    protocol: NodeProtocol,
    stats: Arc<ClientStats>,
    router: Arc<Router>,
) -> Result<()> {
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 1024];
    let header_end = loop {
        if buf.len() > 16 * 1024 {
            bail!("http header too large");
        }
        let n = client.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_double_crlf(&buf) {
            break pos;
        }
    };

    let header_str = std::str::from_utf8(&buf[..header_end]).context("invalid http header utf-8")?;
    let mut lines = header_str.split("\r\n");
    let request_line = lines.next().context("empty http request")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().context("missing http method")?;
    let raw_target = parts.next().context("missing http target")?;
    let version = parts.next().unwrap_or("HTTP/1.1");

    let leftover = &buf[header_end + 4..];
    const BAD_GATEWAY: &[u8] =
        b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

    // `ok_reply` is written to the local client once the upstream path is ready;
    // `upstream_preamble` is the first bytes to send upstream (rebuilt request + body).
    let (target, ok_reply, upstream_preamble, fail_reply): (
        TargetAddr,
        Option<&'static [u8]>,
        Vec<u8>,
        Option<&'static [u8]>,
    ) = if method.eq_ignore_ascii_case("CONNECT") {
        (
            parse_host_port(raw_target, 443)?,
            Some(b"HTTP/1.1 200 Connection Established\r\n\r\n"),
            leftover.to_vec(),
            // After 200 was sent (proxy fast-open) there is no way to report an error.
            None,
        )
    } else {
        // Plain HTTP proxy request: e.g. GET http://example.com/path HTTP/1.1
        let (target, path) = parse_http_url(raw_target)?;
        let mut rebuilt = String::with_capacity(header_str.len() + 32);
        rebuilt.push_str(method);
        rebuilt.push(' ');
        rebuilt.push_str(&path);
        rebuilt.push(' ');
        rebuilt.push_str(version);
        rebuilt.push_str("\r\n");
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let lower = line.to_ascii_lowercase();
            if lower.starts_with("proxy-connection:")
                || lower.starts_with("connection:")
                || lower.starts_with("keep-alive:")
            {
                continue;
            }
            rebuilt.push_str(line);
            rebuilt.push_str("\r\n");
        }
        // This connection is bound to ONE origin. A keep-alive browser would reuse the
        // proxy connection for a request to a different host and it would be sent to the
        // wrong server, so force one request per proxy connection.
        rebuilt.push_str("Connection: close\r\n\r\n");
        let mut preamble = rebuilt.into_bytes();
        preamble.extend_from_slice(leftover);
        (target, None, preamble, Some(BAD_GATEWAY))
    };

    if router.decide(&target).await == Route::Direct {
        // CONNECT: a local dial is fast, so wait for it and report failure honestly.
        let fail = if ok_reply.is_some() { Some(BAD_GATEWAY) } else { fail_reply };
        return relay_direct(&router, client, &target, ok_reply, &upstream_preamble, fail).await;
    }

    let (mut quic_send, quic_recv) = open_proxy_tcp_stream(&conn, protocol, &target).await?;
    // Fast-open: answer the client right away instead of waiting a full tunnel RTT for
    // the server's dial status; the first payload travels with the open request.
    if let Some(reply) = ok_reply {
        client.write_all(reply).await?;
    }
    if !upstream_preamble.is_empty() {
        quic_send.write_all(&upstream_preamble).await?;
        stats
            .tx_bytes
            .fetch_add(upstream_preamble.len() as u64, Ordering::Relaxed);
    }
    relay_fast_open(client, quic_send, quic_recv, protocol, stats, fail_reply).await
}

/// DIRECT outbound: dial `target` from this machine (pinned to physical NIC when TUN is
/// active so it never loops back into TUN), then splice.
async fn relay_direct(
    router: &Router,
    mut client: TcpStream,
    target: &TargetAddr,
    ok_reply: Option<&[u8]>,
    upstream_preamble: &[u8],
    fail_reply: Option<&[u8]>,
) -> Result<()> {
    let mut upstream = match router.dialer.connect_target(target, DIRECT_CONNECT_TIMEOUT).await {
        Ok(s) => s,
        Err(e) => {
            if let Some(reply) = fail_reply {
                let _ = client.write_all(reply).await;
            }
            bail!("direct connect {} failed: {e}", target.to_host_port());
        }
    };
    if let Some(reply) = ok_reply {
        client.write_all(reply).await?;
    }
    if !upstream_preamble.is_empty() {
        upstream.write_all(upstream_preamble).await?;
    }
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    Ok(())
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_host_port(s: &str, default_port: u16) -> Result<TargetAddr> {
    if let Ok(sock) = s.parse::<SocketAddr>() {
        return Ok(TargetAddr::Ip(sock));
    }
    let (host, port) = if let Some(rest) = s.strip_prefix('[') {
        if let Some((ip_part, after)) = rest.split_once(']') {
            let port = after
                .strip_prefix(':')
                .and_then(|p| p.parse::<u16>().ok())
                .unwrap_or(default_port);
            (ip_part, port)
        } else {
            bail!("invalid ipv6 host: {s}");
        }
    } else if let Some((h, p)) = s.rsplit_once(':') {
        let port = p.parse::<u16>().context("invalid port")?;
        (h, port)
    } else {
        (s, default_port)
    };

    if let Ok(ip) = host.parse::<IpAddr>() {
        Ok(TargetAddr::Ip(SocketAddr::new(ip, port)))
    } else {
        Ok(TargetAddr::Domain(host.to_string(), port))
    }
}

fn parse_http_url(url: &str) -> Result<(TargetAddr, String)> {
    let without_scheme = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("HTTP://"))
        .unwrap_or(url);
    let (authority, path) = match without_scheme.find('/') {
        Some(idx) => (&without_scheme[..idx], &without_scheme[idx..]),
        None => (without_scheme, "/"),
    };
    let target = parse_host_port(authority, 80)?;
    Ok((target, path.to_string()))
}

/// Open a proxied TCP bidirectional QUIC stream and write the protocol-specific
/// open header (`GACL` or Hysteria2 `0x401`).
pub(crate) async fn open_proxy_tcp_stream(
    conn: &Connection,
    protocol: NodeProtocol,
    target: &TargetAddr,
) -> Result<(quinn::SendStream, quinn::RecvStream)> {
    let (mut quic_send, quic_recv) = conn.open_bi().await.context("open quic bi stream")?;
    match protocol {
        NodeProtocol::Gacl => target.write_tcp_open(&mut quic_send).await?,
        NodeProtocol::Hysteria2 => {
            protocol::hy2::write_hy2_tcp_request(&mut quic_send, target, 0).await?;
        }
    }
    Ok((quic_send, quic_recv))
}

/// Encode and send a proxied UDP packet on `conn` using either `GACL` (`UdpFrame` + optional
/// game dual-send) or `Hysteria2` (with automatic fragmentation when larger than `max_dgram`).
/// Returns `false` if the QUIC connection is closed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn send_proxy_udp_packet(
    protocol: NodeProtocol,
    conn: &Connection,
    pacer: &UdpDupPacer,
    flow_id: u16,
    seq: u16,
    is_game_pkt: bool,
    dst: TargetAddr,
    payload: Bytes,
) -> bool {
    match protocol {
        NodeProtocol::Gacl => {
            let flags = if is_game_pkt { FLAG_GAME_MODE } else { 0 };
            let frame = UdpFrame {
                flow_id,
                seq,
                flags,
                addr: dst,
                payload,
            };
            if let Ok(encoded) = frame.encode() {
                if !send_datagram_checked(conn, encoded.clone()) {
                    return false;
                }
                if is_game_pkt {
                    pacer.schedule_dup(encoded);
                }
            }
            true
        }
        NodeProtocol::Hysteria2 => {
            let max_dgram = conn.max_datagram_size().unwrap_or(1150);
            let addr_str = dst.to_host_port();
            let pkts = protocol::hy2::encode_hy2_udp_datagrams(
                flow_id as u32,
                seq,
                &addr_str,
                &payload,
                max_dgram,
            );
            for pkt in pkts {
                if !send_datagram_checked(conn, pkt) {
                    return false;
                }
            }
            true
        }
    }
}

/// Fast-open TCP relay over an already-opened QUIC stream whose open header has been
/// written but whose dial status has NOT been awaited.
///
/// The local side was already told "connected", so the application's first bytes (TLS
/// ClientHello, HTTP request) are pipelined behind the open header instead of waiting
/// one full tunnel RTT for the server's status byte. The status header is consumed at
/// the head of the downstream: on failure the local connection is closed (after an
/// optional protocol-specific `fail_reply`).
pub(crate) async fn relay_fast_open<S>(
    stream: S,
    mut quic_send: quinn::SendStream,
    mut quic_recv: quinn::RecvStream,
    protocol: NodeProtocol,
    stats: Arc<ClientStats>,
    fail_reply: Option<&'static [u8]>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    stats.active_tcp.fetch_add(1, Ordering::Relaxed);
    let (mut s_read, mut s_write) = tokio::io::split(stream);

    let stats_u = stats.clone();
    let u = tokio::spawn(async move {
        let _ = protocol::io::copy_with_counter(&mut s_read, &mut quic_send, &stats_u.tx_bytes).await;
        let _ = quic_send.finish();
    });

    let stats_d = stats.clone();
    let d = tokio::spawn(async move {
        let ok = match protocol {
            NodeProtocol::Gacl => matches!(quic_recv.read_u8().await, Ok(AUTH_OK)),
            NodeProtocol::Hysteria2 => {
                protocol::hy2::read_hy2_tcp_response(&mut quic_recv)
                    .await
                    .is_ok()
            }
        };
        if ok {
            let _ =
                protocol::io::copy_with_counter(&mut quic_recv, &mut s_write, &stats_d.rx_bytes).await;
        } else if let Some(reply) = fail_reply {
            let _ = s_write.write_all(reply).await;
        }
        let _ = s_write.shutdown().await;
        ok
    });

    let ok = d.await.unwrap_or(false);
    if ok {
        let _ = u.await;
    } else {
        // Remote dial failed: stop forwarding local bytes into a dead stream.
        u.abort();
    }
    stats.active_tcp.fetch_sub(1, Ordering::Relaxed);
    if ok {
        Ok(())
    } else {
        bail!("remote failed to connect")
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_socks5_client(
    mut client: TcpStream,
    tcp_conn: Connection,
    udp_conn: Connection,
    pacer: UdpDupPacer,
    udp_dispatch: UdpDispatchMap,
    next_flow_id: Arc<AtomicU16>,
    protocol: NodeProtocol,
    game_dup: bool,
    stats: Arc<ClientStats>,
    router: Arc<Router>,
) -> Result<()> {
    let mut head = [0u8; 2];
    client.read_exact(&mut head).await?;
    if head[0] != 0x05 {
        bail!("unsupported socks version: {}", head[0]);
    }
    let mut methods = vec![0u8; head[1] as usize];
    client.read_exact(&mut methods).await?;
    client.write_all(&[0x05, 0x00]).await?;

    let mut req = [0u8; 4];
    client.read_exact(&mut req).await?;
    if req[0] != 0x05 {
        bail!("invalid socks5 request version");
    }
    let cmd = req[1];
    let target = read_socks5_addr(&mut client, req[3]).await?;

    match cmd {
        0x01 => handle_tcp_connect(client, tcp_conn, protocol, target, stats, router).await,
        0x03 => {
            handle_udp_associate(
                client,
                udp_conn,
                pacer,
                udp_dispatch,
                next_flow_id,
                protocol,
                game_dup,
                stats,
                router,
            )
            .await
        }
        other => {
            let _ = client
                .write_all(&[0x05, 0x07, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await;
            bail!("unsupported socks5 cmd: {other}");
        }
    }
}

async fn handle_tcp_connect(
    mut client: TcpStream,
    conn: Connection,
    protocol: NodeProtocol,
    target: TargetAddr,
    stats: Arc<ClientStats>,
    router: Arc<Router>,
) -> Result<()> {
    const SOCKS_OK: &[u8] = &[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0];
    const SOCKS_HOST_UNREACHABLE: &[u8] = &[0x05, 0x04, 0x00, 0x01, 0, 0, 0, 0, 0, 0];

    if router.decide(&target).await == Route::Direct {
        return relay_direct(&router, client, &target, Some(SOCKS_OK), &[], Some(SOCKS_HOST_UNREACHABLE)).await;
    }

    let (quic_send, quic_recv) = open_proxy_tcp_stream(&conn, protocol, &target).await?;

    // Fast-open: report success immediately; the dial status is checked in the relay.
    client.write_all(SOCKS_OK).await?;

    relay_fast_open(client, quic_send, quic_recv, protocol, stats, None)
        .await
        .with_context(|| format!("tcp to {}", target.to_host_port()))
}

#[allow(clippy::too_many_arguments)]
async fn handle_udp_associate(
    mut tcp_ctrl: TcpStream,
    conn: Connection,
    pacer: UdpDupPacer,
    udp_dispatch: UdpDispatchMap,
    next_flow_id: Arc<AtomicU16>,
    protocol: NodeProtocol,
    game_dup: bool,
    stats: Arc<ClientStats>,
    router: Arc<Router>,
) -> Result<()> {
    let local_ip = tcp_ctrl.local_addr()?.ip();
    let udp_sock = Arc::new(UdpSocket::bind(SocketAddr::new(local_ip, 0)).await?);
    let bind_addr = udp_sock.local_addr()?;

    let (tx, mut rx) = mpsc::channel::<UdpFrame>(512);
    let Some(flow_id) = reserve_flow_id(
        &next_flow_id,
        &udp_dispatch,
        tx,
        SOCKS_FLOW_MIN,
        SOCKS_FLOW_MAX,
    ) else {
        let _ = tcp_ctrl
            .write_all(&[0x05, 0x01, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await;
        bail!("SOCKS5 UDP flow_id space exhausted");
    };

    let mut reply = BytesMut::with_capacity(22);
    reply.put_slice(&[0x05, 0x00, 0x00]);
    TargetAddr::Ip(bind_addr).encode_into(&mut reply)?;
    if let Err(e) = tcp_ctrl.write_all(&reply).await {
        udp_dispatch.remove(&flow_id);
        return Err(e.into());
    }

    stats.active_udp.fetch_add(1, Ordering::Relaxed);

    let client_udp_addr: Arc<tokio::sync::RwLock<Option<SocketAddr>>> =
        Arc::new(tokio::sync::RwLock::new(None));

    let sock_in = udp_sock.clone();
    let conn_out = conn.clone();
    let peer_addr_writer = client_udp_addr.clone();
    let stats_tx = stats.clone();
    let tx_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 2048];
        let mut seq: u16 = 0;
        // Lazily created DIRECT socket for rule-matched (e.g. domestic) UDP targets,
        // with its reply pump; aborted together with this task.
        let mut direct: Option<(Arc<UdpSocket>, AbortOnDrop)> = None;
        loop {
            let Ok((n, src)) = sock_in.recv_from(&mut buf).await else {
                break;
            };
            // Hot path: take the write lock only when the client's UDP source changes.
            if *peer_addr_writer.read().await != Some(src) {
                *peer_addr_writer.write().await = Some(src);
            }
            let Ok((dst, payload)) = parse_socks5_udp_packet(Bytes::copy_from_slice(&buf[..n]))
            else {
                continue;
            };

            let is_v6_ip = matches!(dst, TargetAddr::Ip(SocketAddr::V6(_)));
            if !is_v6_ip && router.decide(&dst).await == Route::Direct {
                if direct.is_none() {
                    match router.dialer.bind_udp_v4() {
                        Ok(s) => {
                            let s = Arc::new(s);
                            let pump = spawn_direct_udp_reply_pump(
                                s.clone(),
                                sock_in.clone(),
                                peer_addr_writer.clone(),
                            );
                            direct = Some((s, AbortOnDrop(pump)));
                        }
                        Err(e) => {
                            warn!("bind direct udp socket failed: {e}");
                            continue;
                        }
                    }
                }
                let dst_addr = match &dst {
                    TargetAddr::Ip(a) => Some(*a),
                    TargetAddr::Domain(h, p) => router
                        .resolve_cached(&h.to_ascii_lowercase())
                        .await
                        .filter(IpAddr::is_ipv4)
                        .map(|ip| SocketAddr::new(ip, *p)),
                };
                if let (Some((s, _)), Some(a)) = (&direct, dst_addr) {
                    if let Err(e) = s.send_to(&payload, a).await {
                        debug!("direct udp send to {a} failed: {e}");
                    }
                }
                continue;
            }

            let cur_seq = seq;
            seq = seq.wrapping_add(1);
            let payload_len = payload.len();
            let is_game_pkt = game_dup && payload_len < 200;

            if !send_proxy_udp_packet(
                protocol,
                &conn_out,
                &pacer,
                flow_id,
                cur_seq,
                is_game_pkt,
                dst,
                payload,
            ) {
                break;
            }
            stats_tx
                .tx_bytes
                .fetch_add(payload_len as u64, Ordering::Relaxed);
        }
    });

    let sock_out = udp_sock.clone();
    let peer_addr_reader = client_udp_addr.clone();
    let stats_rx = stats.clone();
    let rx_task = tokio::spawn(async move {
        let mut dedup = DedupWindow::new();
        while let Some(frame) = rx.recv().await {
            if protocol == NodeProtocol::Gacl && !dedup.check_and_insert(frame.seq) {
                continue;
            }
            let Some(peer) = *peer_addr_reader.read().await else {
                continue;
            };
            if let Ok(pkt) = build_socks5_udp_packet(&frame.addr, &frame.payload) {
                if let Err(e) = sock_out.send_to(&pkt, peer).await {
                    warn!("local udp send error: {}", e);
                } else {
                    stats_rx
                        .rx_bytes
                        .fetch_add(frame.payload.len() as u64, Ordering::Relaxed);
                }
            }
        }
    });

    let mut drain = [0u8; 16];
    while tcp_ctrl.read(&mut drain).await.unwrap_or(0) > 0 {}

    tx_task.abort();
    rx_task.abort();
    udp_dispatch.remove(&flow_id);
    stats.active_udp.fetch_sub(1, Ordering::Relaxed);
    Ok(())
}

/// Forward replies received on a DIRECT UDP socket back to the SOCKS5 client,
/// wrapped in the SOCKS5 UDP header with the real remote source address.
fn spawn_direct_udp_reply_pump(
    direct: Arc<UdpSocket>,
    local: Arc<UdpSocket>,
    peer: Arc<tokio::sync::RwLock<Option<SocketAddr>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf = vec![0u8; 2048];
        loop {
            let Ok((n, src)) = direct.recv_from(&mut buf).await else {
                break;
            };
            let Some(client) = *peer.read().await else {
                continue;
            };
            let src = SocketAddr::new(src.ip().to_canonical(), src.port());
            if let Ok(pkt) = build_socks5_udp_packet(&TargetAddr::Ip(src), &buf[..n]) {
                let _ = local.send_to(&pkt, client).await;
            }
        }
    })
}

async fn read_socks5_addr(stream: &mut TcpStream, atyp: u8) -> Result<TargetAddr> {
    match atyp {
        ATYP_IPV4 => {
            let mut raw = [0u8; 6];
            stream.read_exact(&mut raw).await?;
            let ip = Ipv4Addr::new(raw[0], raw[1], raw[2], raw[3]);
            let port = u16::from_be_bytes([raw[4], raw[5]]);
            Ok(TargetAddr::Ip(SocketAddr::from((ip, port))))
        }
        ATYP_IPV6 => {
            let mut raw = [0u8; 18];
            stream.read_exact(&mut raw).await?;
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&raw[..16]);
            let ip = Ipv6Addr::from(octets);
            let port = u16::from_be_bytes([raw[16], raw[17]]);
            Ok(TargetAddr::Ip(SocketAddr::from((ip, port))))
        }
        ATYP_DOMAIN => {
            let len = stream.read_u8().await? as usize;
            let mut raw = vec![0u8; len + 2];
            stream.read_exact(&mut raw).await?;
            let host = String::from_utf8(raw[..len].to_vec())?;
            let port = u16::from_be_bytes([raw[len], raw[len + 1]]);
            Ok(TargetAddr::Domain(host, port))
        }
        other => bail!("unsupported socks5 atyp: {other}"),
    }
}

fn parse_socks5_udp_packet(mut data: Bytes) -> Result<(TargetAddr, Bytes)> {
    if data.remaining() < 4 {
        bail!("socks5 udp packet too short");
    }
    let _rsv = data.get_u16();
    let frag = data.get_u8();
    if frag != 0 {
        bail!("fragmented socks5 udp not supported");
    }
    let addr = TargetAddr::decode_from(&mut data)?;
    Ok((addr, data))
}

fn build_socks5_udp_packet(addr: &TargetAddr, payload: &[u8]) -> Result<Bytes> {
    let mut buf = BytesMut::with_capacity(32 + payload.len());
    buf.put_slice(&[0x00, 0x00, 0x00]);
    addr.encode_into(&mut buf)?;
    buf.put_slice(payload);
    Ok(buf.freeze())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flow_id_partition_wraps_and_skips_occupied() {
        let dispatch: UdpDispatchMap = Arc::new(DashMap::new());
        let socks_cur = AtomicU16::new(SOCKS_FLOW_MAX - 1);
        let tun_cur = AtomicU16::new(TUN_FLOW_MAX - 1);

        // Pre-occupy SOCKS_FLOW_MIN so wraparound must skip it
        let (tx0, _rx0) = mpsc::channel(1);
        dispatch.insert(SOCKS_FLOW_MIN, tx0);

        let id1 = reserve_flow_id(&socks_cur, &dispatch, mpsc::channel(1).0, SOCKS_FLOW_MIN, SOCKS_FLOW_MAX).unwrap();
        let id2 = reserve_flow_id(&socks_cur, &dispatch, mpsc::channel(1).0, SOCKS_FLOW_MIN, SOCKS_FLOW_MAX).unwrap();
        let id3 = reserve_flow_id(&socks_cur, &dispatch, mpsc::channel(1).0, SOCKS_FLOW_MIN, SOCKS_FLOW_MAX).unwrap();
        assert_eq!(id1, SOCKS_FLOW_MAX - 1);
        assert_eq!(id2, SOCKS_FLOW_MAX);
        assert_eq!(id3, SOCKS_FLOW_MIN + 1); // wrapped and skipped occupied SOCKS_FLOW_MIN
        assert!(id3 <= SOCKS_FLOW_MAX);

        // TUN partition wraps strictly inside [TUN_FLOW_MIN, TUN_FLOW_MAX]
        let t1 = reserve_flow_id(&tun_cur, &dispatch, mpsc::channel(1).0, TUN_FLOW_MIN, TUN_FLOW_MAX).unwrap();
        let t2 = reserve_flow_id(&tun_cur, &dispatch, mpsc::channel(1).0, TUN_FLOW_MIN, TUN_FLOW_MAX).unwrap();
        let t3 = reserve_flow_id(&tun_cur, &dispatch, mpsc::channel(1).0, TUN_FLOW_MIN, TUN_FLOW_MAX).unwrap();
        assert_eq!(t1, TUN_FLOW_MAX - 1);
        assert_eq!(t2, TUN_FLOW_MAX);
        assert_eq!(t3, TUN_FLOW_MIN);

        // Exhaustion returns None
        let tiny_cur = AtomicU16::new(10);
        let tiny_map: UdpDispatchMap = Arc::new(DashMap::new());
        assert_eq!(reserve_flow_id(&tiny_cur, &tiny_map, mpsc::channel(1).0, 10, 11), Some(10));
        assert_eq!(reserve_flow_id(&tiny_cur, &tiny_map, mpsc::channel(1).0, 10, 11), Some(11));
        assert_eq!(reserve_flow_id(&tiny_cur, &tiny_map, mpsc::channel(1).0, 10, 11), None);
    }

    #[test]
    fn socks5_udp_packet_roundtrip() {
        let addr = TargetAddr::Domain("dns.google".to_string(), 53);
        let raw = build_socks5_udp_packet(&addr, b"query").unwrap();
        let (decoded_addr, payload) = parse_socks5_udp_packet(raw).unwrap();
        assert_eq!(decoded_addr, addr);
        assert_eq!(payload, Bytes::from_static(b"query"));
    }
}
