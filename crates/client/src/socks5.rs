use crate::AbortOnDrop;
use crate::error::ConnectError;
use crate::http_proxy::handle_http_proxy_client;
pub use crate::relay::{ClientStats, UdpDispatchMap};
use crate::relay::{SOCKS_FLOW_MIN, relay_direct};
use crate::rules::{Route, Router};
use crate::tunnel::Tunnels;
use anyhow::{Context, Result, bail};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use protocol::dedup::DedupWindow;
use protocol::frame::{ATYP_DOMAIN, ATYP_IPV4, ATYP_IPV6, TargetAddr, UdpFrame};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

/// Mixed (HTTP + HTTPS CONNECT + SOCKS5 TCP/UDP) proxy listener on a single port.
pub async fn run_mixed_proxy_server(
    listen_addr: SocketAddr,
    tunnels: Arc<Tunnels>,
    stats: Arc<ClientStats>,
    router: Arc<Router>,
    log_tx: Option<mpsc::UnboundedSender<String>>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let listener = TcpListener::bind(listen_addr)
        .await
        .map_err(|_| ConnectError::PortInUse {
            detail: format!("本地端口 {listen_addr} 被占用，无法监听聚合代理"),
        })?;
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
                let (stream, peer) = match accepted {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("local proxy accept error: {e}");
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let tunnels = tunnels.clone();
                let next_flow_id = next_flow_id.clone();
                let stats = stats.clone();
                let router = router.clone();

                tokio::spawn(async move {
                    let _ = stream.set_nodelay(true);
                    if let Err(e) = handle_mixed_client(stream, tunnels, next_flow_id, stats, router).await {
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
    tunnels: Arc<Tunnels>,
    next_flow_id: Arc<AtomicU16>,
    stats: Arc<ClientStats>,
    router: Arc<Router>,
) -> Result<()> {
    let mut first = [0u8; 1];
    let n = stream.peek(&mut first).await?;
    if n == 0 {
        return Ok(());
    }
    if first[0] == 0x05 {
        handle_socks5_client(stream, tunnels, next_flow_id, stats, router).await
    } else {
        handle_http_proxy_client(stream, tunnels, stats, router).await
    }
}

async fn handle_socks5_client(
    mut client: TcpStream,
    tunnels: Arc<Tunnels>,
    next_flow_id: Arc<AtomicU16>,
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
        0x01 => handle_tcp_connect(client, tunnels, target, stats, router).await,
        0x03 => handle_udp_associate(client, tunnels, next_flow_id, stats, router).await,
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
    tunnels: Arc<Tunnels>,
    target: TargetAddr,
    stats: Arc<ClientStats>,
    router: Arc<Router>,
) -> Result<()> {
    const SOCKS_OK: &[u8] = &[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0];
    const SOCKS_HOST_UNREACHABLE: &[u8] = &[0x05, 0x04, 0x00, 0x01, 0, 0, 0, 0, 0, 0];

    let route = router.decide(&target).await;
    let Some(tunnel) = tunnels.for_route(route) else {
        return relay_direct(
            &router,
            client,
            &target,
            Some(SOCKS_OK),
            &[],
            Some(SOCKS_HOST_UNREACHABLE),
        )
        .await;
    };

    let (quic_send, quic_recv) = tunnel.open_tcp(&target).await?;
    client.write_all(SOCKS_OK).await?;

    tunnel
        .relay_tcp(client, quic_send, quic_recv, stats, None)
        .await
        .with_context(|| format!("tcp to {}", target.to_host_port()))
}

async fn handle_udp_associate(
    mut tcp_ctrl: TcpStream,
    tunnels: Arc<Tunnels>,
    next_flow_id: Arc<AtomicU16>,
    stats: Arc<ClientStats>,
    router: Arc<Router>,
) -> Result<()> {
    let local_ip = tcp_ctrl.local_addr()?.ip();
    let udp_sock = Arc::new(UdpSocket::bind(SocketAddr::new(local_ip, 0)).await?);
    let bind_addr = udp_sock.local_addr()?;

    let mut reply = BytesMut::with_capacity(22);
    reply.put_slice(&[0x05, 0x00, 0x00]);
    TargetAddr::Ip(bind_addr).encode_into(&mut reply)?;
    tcp_ctrl.write_all(&reply).await?;

    stats.active_udp.fetch_add(1, Ordering::Relaxed);

    // Reply frames from every egress flow merge onto this channel -> one local UDP socket.
    let (out_tx, mut out_rx) = mpsc::channel::<(TargetAddr, Bytes)>(512);
    let (ctrl_closed_tx, ctrl_closed_rx) = watch::channel(false);

    let client_udp_addr: Arc<tokio::sync::RwLock<Option<SocketAddr>>> =
        Arc::new(tokio::sync::RwLock::new(None));

    let sock_in = udp_sock.clone();
    let tunnels_tx = tunnels.clone();
    let peer_addr_writer = client_udp_addr.clone();
    let stats_tx = stats.clone();
    let next_flow = next_flow_id.clone();
    let out_tx_flow = out_tx.clone();
    let mut ctrl_closed = ctrl_closed_rx.clone();
    let tx_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 2048];
        // Flow id + sequence counter per egress, reserved lazily on first use.
        let mut primary_lane: Option<(u16, u16)> = None;
        let mut secondary_lane: Option<(u16, u16)> = None;
        let mut direct: Option<(Arc<UdpSocket>, AbortOnDrop)> = None;
        loop {
            let received = tokio::select! {
                _ = ctrl_closed.changed() => break,
                res = sock_in.recv_from(&mut buf) => res,
            };
            let Ok((n, src)) = received else {
                break;
            };
            if *peer_addr_writer.read().await != Some(src) {
                *peer_addr_writer.write().await = Some(src);
            }
            let Ok((dst, payload)) = parse_socks5_udp_packet(Bytes::copy_from_slice(&buf[..n]))
            else {
                continue;
            };

            let route = match &dst {
                TargetAddr::Ip(SocketAddr::V6(_)) => Route::Primary,
                TargetAddr::Ip(a) => router.decide_ip_sync(a.ip()),
                TargetAddr::Domain(..) => router.decide(&dst).await,
            };

            if route == Route::Direct || tunnels_tx.for_route(route).is_none() {
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
                if let (Some((s, _)), Some(a)) = (&direct, dst_addr)
                    && let Err(e) = s.send_to(&payload, a).await
                {
                    debug!("direct udp send to {a} failed: {e}");
                }
                continue;
            }

            let Some(tunnel) = tunnels_tx.for_route(route) else {
                continue;
            };
            let is_primary = Arc::ptr_eq(tunnel, tunnels_tx.primary());
            let lane = if is_primary {
                &mut primary_lane
            } else {
                &mut secondary_lane
            };
            let (flow_id, cur_seq) = match *lane {
                Some((fid, seq)) => {
                    *lane = Some((fid, seq.wrapping_add(1)));
                    (fid, seq)
                }
                None => {
                    let (f_tx, f_rx) = mpsc::channel::<UdpFrame>(256);
                    let Some(fid) = tunnel.reserve_socks_flow(&next_flow, f_tx) else {
                        warn!("SOCKS5 UDP flow_id space exhausted on {route:?} egress");
                        continue;
                    };
                    let out = out_tx_flow.clone();
                    let dedup_rx = tunnel.should_dedup_rx();
                    tokio::spawn(pump_socks_udp_flow(f_rx, dedup_rx, out));
                    *lane = Some((fid, 1));
                    (fid, 0)
                }
            };

            let payload_len = payload.len();
            let is_game_pkt = tunnel.game_dup_enabled() && payload_len < 200;

            if !tunnel.send_udp(flow_id, cur_seq, is_game_pkt, dst, payload) {
                if is_primary {
                    break;
                }
                if let Some((fid, _)) = secondary_lane.take() {
                    tunnel.release_flow(fid);
                }
                continue;
            }
            stats_tx
                .tx_bytes
                .fetch_add(payload_len as u64, Ordering::Relaxed);
        }

        // The SOCKS control connection owns these flows: release them when it closes.
        if let Some((fid, _)) = primary_lane {
            tunnels_tx.primary().release_flow(fid);
        }
        if let (Some(sec), Some((fid, _))) = (tunnels_tx.secondary(), secondary_lane) {
            sec.release_flow(fid);
        }
        drop(direct);
    });
    drop(out_tx);

    let sock_out = udp_sock.clone();
    let peer_addr_reader = client_udp_addr.clone();
    let stats_rx = stats.clone();
    let rx_task = tokio::spawn(async move {
        while let Some((addr, payload)) = out_rx.recv().await {
            let Some(peer) = *peer_addr_reader.read().await else {
                continue;
            };
            if let Ok(pkt) = build_socks5_udp_packet(&addr, &payload) {
                if let Err(e) = sock_out.send_to(&pkt, peer).await {
                    warn!("local udp send error: {}", e);
                } else {
                    stats_rx
                        .rx_bytes
                        .fetch_add(payload.len() as u64, Ordering::Relaxed);
                }
            }
        }
    });

    let mut drain = [0u8; 16];
    while tcp_ctrl.read(&mut drain).await.unwrap_or(0) > 0 {}

    let _ = ctrl_closed_tx.send(true);
    // The tx task releases its reserved flows on exit; the rx task ends when it drops out_tx.
    let _ = tx_task.await;
    let _ = rx_task.await;
    stats.active_udp.fetch_sub(1, Ordering::Relaxed);
    Ok(())
}

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

async fn pump_socks_udp_flow(
    mut rx: mpsc::Receiver<UdpFrame>,
    dedup_rx: bool,
    out: mpsc::Sender<(TargetAddr, Bytes)>,
) {
    let mut dedup = DedupWindow::new();
    while let Some(frame) = rx.recv().await {
        if dedup_rx && !dedup.check_and_insert(frame.seq) {
            continue;
        }
        if out.send((frame.addr, frame.payload)).await.is_err() {
            break;
        }
    }
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
    fn socks5_udp_packet_roundtrip() {
        let addr = TargetAddr::Domain("dns.google".to_string(), 53);
        let raw = build_socks5_udp_packet(&addr, b"query").unwrap();
        let (decoded_addr, payload) = parse_socks5_udp_packet(raw).unwrap();
        assert_eq!(decoded_addr, addr);
        assert_eq!(payload, Bytes::from_static(b"query"));
    }
}
