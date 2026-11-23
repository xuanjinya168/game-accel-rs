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
use quinn::Connection;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

pub type UdpDispatchMap = Arc<DashMap<u16, mpsc::Sender<UdpFrame>>>;

pub(crate) const SOCKS_FLOW_MIN: u16 = 1;
pub(crate) const SOCKS_FLOW_MAX: u16 = 32767;
pub(crate) const TUN_FLOW_MIN: u16 = 32768;
pub(crate) const TUN_FLOW_MAX: u16 = 65534;

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
pub async fn run_mixed_proxy_server(
    listen_addr: SocketAddr,
    conn: Connection,
    pacer: UdpDupPacer,
    udp_dispatch: UdpDispatchMap,
    game_dup: bool,
    stats: Arc<ClientStats>,
    log_tx: Option<mpsc::UnboundedSender<String>>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let listener = TcpListener::bind(listen_addr)
        .await
        .with_context(|| format!("本地端口 {listen_addr} 被占用，无法监听聚合代理"))?;
    let next_flow_id = Arc::new(AtomicU16::new(SOCKS_FLOW_MIN));
    let msg = format!("本地聚合代理 (HTTP + SOCKS5) 已监听 {listen_addr}");
    info!("{}", msg);
    if let Some(tx) = &log_tx {
        let _ = tx.send(msg);
    }

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                let (stream, peer) = accepted?;
                let conn = conn.clone();
                let pacer = pacer.clone();
                let dispatch = udp_dispatch.clone();
                let next_flow_id = next_flow_id.clone();
                let stats = stats.clone();

                tokio::spawn(async move {
                    let _ = stream.set_nodelay(true);
                    if let Err(e) =
                        handle_mixed_client(stream, conn, pacer, dispatch, next_flow_id, game_dup, stats).await
                    {
                        debug!("proxy client {} ended: {:#}", peer, e);
                    }
                });
            }
        }
    }
    Ok(())
}

async fn handle_mixed_client(
    stream: TcpStream,
    conn: Connection,
    pacer: UdpDupPacer,
    udp_dispatch: UdpDispatchMap,
    next_flow_id: Arc<AtomicU16>,
    game_dup: bool,
    stats: Arc<ClientStats>,
) -> Result<()> {
    let mut first = [0u8; 1];
    let n = stream.peek(&mut first).await?;
    if n == 0 {
        return Ok(());
    }
    if first[0] == 0x05 {
        handle_socks5_client(stream, conn, pacer, udp_dispatch, next_flow_id, game_dup, stats).await
    } else {
        handle_http_proxy_client(stream, conn, stats).await
    }
}

/// Handle HTTP / HTTPS (CONNECT) proxy requests on the mixed port.
async fn handle_http_proxy_client(
    mut client: TcpStream,
    conn: Connection,
    stats: Arc<ClientStats>,
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

    if method.eq_ignore_ascii_case("CONNECT") {
        let target = parse_host_port(raw_target, 443)?;
        let (mut quic_send, mut quic_recv) = conn.open_bi().await.context("open quic bi stream")?;
        target.write_tcp_open(&mut quic_send).await?;

        let status = quic_recv.read_u8().await.context("read server tcp status")?;
        if status != AUTH_OK {
            let _ = client
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                .await;
            bail!("remote failed to connect to {}", target.to_host_port());
        }

        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;

        if !leftover.is_empty() {
            quic_send.write_all(leftover).await?;
            stats
                .tx_bytes
                .fetch_add(leftover.len() as u64, Ordering::Relaxed);
        }

        pipe_tcp_quic(client, quic_send, quic_recv, stats).await
    } else {
        // Plain HTTP proxy request: e.g. GET http://example.com/path HTTP/1.1
        let (target, path) = parse_http_url(raw_target)?;
        let (mut quic_send, mut quic_recv) = conn.open_bi().await.context("open quic bi stream")?;
        target.write_tcp_open(&mut quic_send).await?;

        let status = quic_recv.read_u8().await.context("read server tcp status")?;
        if status != AUTH_OK {
            let _ = client
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                .await;
            bail!("remote failed to connect to {}", target.to_host_port());
        }

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
            if line.to_ascii_lowercase().starts_with("proxy-connection:") {
                continue;
            }
            rebuilt.push_str(line);
            rebuilt.push_str("\r\n");
        }
        rebuilt.push_str("\r\n");

        quic_send.write_all(rebuilt.as_bytes()).await?;
        stats
            .tx_bytes
            .fetch_add(rebuilt.len() as u64, Ordering::Relaxed);
        if !leftover.is_empty() {
            quic_send.write_all(leftover).await?;
            stats
                .tx_bytes
                .fetch_add(leftover.len() as u64, Ordering::Relaxed);
        }

        pipe_tcp_quic(client, quic_send, quic_recv, stats).await
    }
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

async fn pipe_tcp_quic(
    client: TcpStream,
    mut quic_send: quinn::SendStream,
    mut quic_recv: quinn::RecvStream,
    stats: Arc<ClientStats>,
) -> Result<()> {
    stats.active_tcp.fetch_add(1, Ordering::Relaxed);
    let (mut c_read, mut c_write) = client.into_split();

    let stats_u = stats.clone();
    let u = tokio::spawn(async move {
        let _ = protocol::io::copy_with_counter(&mut c_read, &mut quic_send, &stats_u.tx_bytes).await;
        let _ = quic_send.finish();
    });
    let stats_d = stats.clone();
    let d = tokio::spawn(async move {
        let _ = protocol::io::copy_with_counter(&mut quic_recv, &mut c_write, &stats_d.rx_bytes).await;
        let _ = c_write.shutdown().await;
    });

    let _ = tokio::join!(u, d);
    stats.active_tcp.fetch_sub(1, Ordering::Relaxed);
    Ok(())
}

async fn handle_socks5_client(
    mut client: TcpStream,
    conn: Connection,
    pacer: UdpDupPacer,
    udp_dispatch: UdpDispatchMap,
    next_flow_id: Arc<AtomicU16>,
    game_dup: bool,
    stats: Arc<ClientStats>,
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
        0x01 => handle_tcp_connect(client, conn, target, stats).await,
        0x03 => {
            handle_udp_associate(
                client,
                conn,
                pacer,
                udp_dispatch,
                next_flow_id,
                game_dup,
                stats,
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
    target: TargetAddr,
    stats: Arc<ClientStats>,
) -> Result<()> {
    let (mut quic_send, mut quic_recv) = conn.open_bi().await.context("open quic bi stream")?;
    target.write_tcp_open(&mut quic_send).await?;

    let status = quic_recv.read_u8().await.context("read server tcp status")?;
    if status != AUTH_OK {
        let _ = client
            .write_all(&[0x05, 0x05, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await;
        bail!("remote failed to connect to {}", target.to_host_port());
    }

    client
        .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
        .await?;

    pipe_tcp_quic(client, quic_send, quic_recv, stats).await
}

async fn handle_udp_associate(
    mut tcp_ctrl: TcpStream,
    conn: Connection,
    pacer: UdpDupPacer,
    udp_dispatch: UdpDispatchMap,
    next_flow_id: Arc<AtomicU16>,
    game_dup: bool,
    stats: Arc<ClientStats>,
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
        loop {
            let Ok((n, src)) = sock_in.recv_from(&mut buf).await else {
                break;
            };
            {
                let mut guard = peer_addr_writer.write().await;
                if *guard != Some(src) {
                    *guard = Some(src);
                }
            }
            let Ok((dst, payload)) = parse_socks5_udp_packet(Bytes::copy_from_slice(&buf[..n]))
            else {
                continue;
            };

            let cur_seq = seq;
            seq = seq.wrapping_add(1);
            let flags = if game_dup { FLAG_GAME_MODE } else { 0 };
            let payload_len = payload.len();

            let frame = UdpFrame {
                flow_id,
                seq: cur_seq,
                flags,
                addr: dst,
                payload,
            };
            if let Ok(encoded) = frame.encode() {
                if !send_datagram_checked(&conn_out, encoded.clone()) {
                    break;
                }
                stats_tx
                    .tx_bytes
                    .fetch_add(payload_len as u64, Ordering::Relaxed);
                if game_dup && payload_len < 200 {
                    pacer.schedule_dup(encoded);
                }
            }
        }
    });

    let sock_out = udp_sock.clone();
    let peer_addr_reader = client_udp_addr.clone();
    let stats_rx = stats.clone();
    let rx_task = tokio::spawn(async move {
        let mut dedup = DedupWindow::new();
        while let Some(frame) = rx.recv().await {
            if !dedup.check_and_insert(frame.seq) {
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
