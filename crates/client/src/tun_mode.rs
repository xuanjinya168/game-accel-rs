use crate::rules::{DOMESTIC_DNS, DnsRoute, Route, RouteMode, Router};
use crate::socks5::{
    ClientStats, TUN_FLOW_MAX, TUN_FLOW_MIN, UdpDispatchMap, open_proxy_tcp_stream,
    relay_fast_open, reserve_flow_id, send_proxy_udp_packet,
};
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use netstack_smoltcp::StackBuilder;
use protocol::TUN_MTU;
use protocol::dedup::DedupWindow;
use protocol::frame::{TargetAddr, UdpFrame};
use protocol::pacer::UdpDupPacer;
use protocol::share::NodeProtocol;
use protocol::unix_now_secs as now_secs;
use quinn::Connection;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

const TUN_UDP_IDLE_TIMEOUT_SECS: u64 = 60;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const TUN_NAME: &str = "GameAccel";
const TUN_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 88, 2);
const TUN_GW: Ipv4Addr = Ipv4Addr::new(10, 0, 88, 1);
const TUN_PREFIX: u8 = 24;

/// Check whether the current process has Administrator privileges on Windows.
pub fn is_elevated() -> bool {
    #[cfg(target_os = "windows")]
    {
        Command::new("net")
            .args(["session"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "windows"))]
    {
        true
    }
}

#[cfg(target_os = "windows")]
fn run_hidden(program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .with_context(|| format!("failed to execute {program}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("{program} {:?} failed: {} {}", args, stdout.trim(), stderr.trim());
    }
    Ok(stdout)
}

/// Detect the current default IPv4 physical gateway on Windows.
#[cfg(target_os = "windows")]
fn detect_physical_gateway() -> Result<Ipv4Addr> {
    let out = run_hidden("route", &["print", "0.0.0.0"])?;
    for line in out.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        // Typical row: 0.0.0.0  0.0.0.0  192.168.1.1  192.168.1.100  25
        if cols.len() >= 4 && cols[0] == "0.0.0.0" && cols[1] == "0.0.0.0"
            && let Ok(gw) = cols[2].parse::<Ipv4Addr>()
                && gw != TUN_GW && !gw.is_unspecified() {
                    return Ok(gw);
                }
    }
    bail!("无法自动检测物理默认网关，请检查网络连接")
}

/// RAII guard that pins the VPS server IP to the physical gateway and installs
/// two half-default routes (`0.0.0.0/1` and `128.0.0.0/1`) onto the TUN interface.
/// On drop (or even crash), the original `0.0.0.0/0` default route stays untouched.
struct RouteGuard {
    server_v4: Option<Ipv4Addr>,
}

impl RouteGuard {
    #[cfg(target_os = "windows")]
    fn install(server_ip: IpAddr, tun_if_index: u32) -> Result<Self> {
        let phys_gw = detect_physical_gateway()?;
        let gw_str = phys_gw.to_string();

        // 1. Pin VPS server IP via physical gateway so QUIC never loops back into TUN
        let server_v4 = match server_ip {
            IpAddr::V4(v4) => {
                let ip_str = v4.to_string();
                let _ = run_hidden("route", &["delete", &ip_str]);
                run_hidden(
                    "route",
                    &["add", &ip_str, "mask", "255.255.255.255", &gw_str, "metric", "1"],
                )
                .context("添加服务器直连防回环路由失败")?;
                Some(v4)
            }
            IpAddr::V6(_) => None,
        };

        // 2. Configure DNS on the TUN adapter (routed through QUIC to prevent DNS pollution)
        let _ = run_hidden(
            "netsh",
            &[
                "interface",
                "ip",
                "set",
                "dns",
                &format!("name={TUN_NAME}"),
                "static",
                "1.1.1.1",
            ],
        );
        let _ = run_hidden(
            "netsh",
            &[
                "interface",
                "ip",
                "add",
                "dns",
                &format!("name={TUN_NAME}"),
                "8.8.8.8",
                "index=2",
            ],
        );

        // 3. Install 0.0.0.0/1 + 128.0.0.0/1 onto TUN (safer than replacing 0.0.0.0/0).
        // Build the guard first so a partial failure runs Drop and removes the routes
        // that did get added; otherwise half the internet stays routed to a dead TUN.
        let guard = Self { server_v4 };
        if let Err(e) = guard.install_half_default_routes(tun_if_index) {
            drop(guard);
            return Err(e);
        }

        Ok(guard)
    }

    #[cfg(target_os = "windows")]
    fn install_half_default_routes(&self, tun_if_index: u32) -> Result<()> {
        let tun_gw_str = TUN_GW.to_string();
        let if_str = tun_if_index.to_string();

        run_hidden(
            "route",
            &[
                "add",
                "0.0.0.0",
                "mask",
                "128.0.0.0",
                &tun_gw_str,
                "metric",
                "5",
                "if",
                &if_str,
            ],
        )
        .context("添加 TUN 路由 0.0.0.0/1 失败")?;

        run_hidden(
            "route",
            &[
                "add",
                "128.0.0.0",
                "mask",
                "128.0.0.0",
                &tun_gw_str,
                "metric",
                "5",
                "if",
                &if_str,
            ],
        )
        .context("添加 TUN 路由 128.0.0.0/1 失败")?;
        Ok(())
    }

    #[cfg(not(target_os = "windows"))]
    fn install(_server_ip: IpAddr, _tun_if_index: u32) -> Result<Self> {
        Ok(Self { server_v4: None })
    }
}

impl Drop for RouteGuard {
    fn drop(&mut self) {
        #[cfg(target_os = "windows")]
        {
            // Scope the deletes to our TUN gateway so a half-default route that belongs
            // to another VPN/proxy tool is never removed.
            let tun_gw_str = TUN_GW.to_string();
            let _ = run_hidden(
                "route",
                &["delete", "0.0.0.0", "mask", "128.0.0.0", &tun_gw_str],
            );
            let _ = run_hidden(
                "route",
                &["delete", "128.0.0.0", "mask", "128.0.0.0", &tun_gw_str],
            );
            if let Some(v4) = self.server_v4 {
                let ip_str = v4.to_string();
                let _ = run_hidden("route", &["delete", &ip_str, "mask", "255.255.255.255"]);
            }
        }
    }
}

struct TunUdpSession {
    flow_id: u16,
    tx_seq: AtomicU16,
    last_active: Arc<AtomicU64>,
    rx_task: JoinHandle<()>,
    dispatch: UdpDispatchMap,
    stats: Arc<ClientStats>,
}

impl Drop for TunUdpSession {
    fn drop(&mut self) {
        self.rx_task.abort();
        self.dispatch.remove(&self.flow_id);
        self.stats
            .active_udp
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            })
            .ok();
    }
}

/// Per-source direct UDP session (bound to physical NIC so it never loops back into TUN).
struct TunDirectUdpSession {
    socket: Arc<tokio::net::UdpSocket>,
    last_active: Arc<AtomicU64>,
    rx_task: JoinHandle<()>,
}

impl Drop for TunDirectUdpSession {
    fn drop(&mut self) {
        self.rx_task.abort();
    }
}

/// Clears `DirectDialer` interface pinning whenever TUN exits or fails.
struct DialerIfGuard(Arc<Router>);

impl Drop for DialerIfGuard {
    fn drop(&mut self) {
        self.0.dialer.set_interface(None);
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run_tun_mode(
    server_ip: IpAddr,
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
    let emit = |msg: String| {
        info!("{}", msg);
        if let Some(tx) = &log_tx {
            let _ = tx.send(msg);
        }
    };

    if !is_elevated() {
        bail!("TUN 模式需要管理员权限，请点击「以管理员身份重启」后再开启 TUN");
    }

    let mut builder = tun_rs::DeviceBuilder::new()
        .name(TUN_NAME)
        .ipv4(TUN_IP, TUN_PREFIX, None);

    // On Windows, `builder.mtu(1200)` also attempts to set IPv6 MTU = 1200, which fails with
    // `ERROR_INVALID_PARAMETER (os error 87)` because IPv6 requires MTU >= 1280.
    // Use `mtu_v4` on Windows and explicitly locate `wintun.dll` beside the executable.
    #[cfg(target_os = "windows")]
    {
        builder = builder.mtu_v4(TUN_MTU);
        if let Ok(exe) = std::env::current_exe()
            && let Some(dir) = exe.parent() {
                let candidate = dir.join("wintun.dll");
                if candidate.is_file() {
                    builder = builder.wintun_file(candidate.to_string_lossy().into_owned());
                }
            }
    }
    #[cfg(not(target_os = "windows"))]
    {
        builder = builder.mtu(TUN_MTU);
    }

    let dev = builder
        .build_async()
        .context("创建 Wintun 虚拟网卡失败（请确认同目录存在 wintun.dll 且已用管理员身份运行）")?;

    let if_index = dev.if_index().context("获取 TUN 网卡索引失败")?;
    // Detect physical IPv4 interface BEFORE installing TUN half-default routes,
    // so direct connections (`IP_UNICAST_IF`) bypass TUN without needing 8k route entries.
    let phys_if = crate::direct::detect_physical_interface_v4().filter(|&idx| idx != if_index);
    router.dialer.set_interface(phys_if);
    let _dialer_guard = DialerIfGuard(router.clone());

    let _route_guard = RouteGuard::install(server_ip, if_index)?;
    emit(format!(
        "TUN 虚拟网卡已启动 ({TUN_NAME} {TUN_IP}/{TUN_PREFIX}, MTU {TUN_MTU}, 物理出口网卡 {:?}, 智能分流已启用)",
        phys_if
    ));

    let dev = Arc::new(dev);

    // Build smoltcp userspace TCP/IP stack
    let (stack, runner, udp_socket, tcp_listener) = StackBuilder::default()
        .enable_tcp(true)
        .enable_udp(true)
        .mtu(TUN_MTU as usize)
        .build()
        .context("初始化用户态 TCP/IP 协议栈失败")?;

    let runner_task = runner.map(tokio::spawn);
    let (mut stack_sink, mut stack_stream) = stack.split();

    // Pump 1: Wintun -> netstack
    let dev_in = dev.clone();
    let tun_to_stack = tokio::spawn(async move {
        let mut buf = vec![0u8; 2048];
        loop {
            match dev_in.recv(&mut buf).await {
                Ok(n) if n > 0 => {
                    if stack_sink.send(buf[..n].to_vec()).await.is_err() {
                        break;
                    }
                }
                Ok(_) => continue,
                Err(e) => {
                    debug!("tun recv ended: {}", e);
                    break;
                }
            }
        }
    });

    // Pump 2: netstack -> Wintun
    let dev_out = dev.clone();
    let stack_to_tun = tokio::spawn(async move {
        while let Some(pkt) = stack_stream.next().await {
            match pkt {
                Ok(frame) => {
                    if let Err(e) = dev_out.send(&frame).await {
                        debug!("tun send error: {}", e);
                    }
                }
                Err(e) => {
                    debug!("netstack stream error: {}", e);
                    break;
                }
            }
        }
    });

    // TCP handler: netstack TcpListener -> Direct (physical NIC) or QUIC TCP Stream lane
    let tcp_task = if let Some(mut listener) = tcp_listener {
        let conn = tcp_conn.clone();
        let stats = stats.clone();
        let router = router.clone();
        Some(tokio::spawn(async move {
            while let Some((stream, _local, _remote)) = listener.next().await {
                let conn = conn.clone();
                let stats = stats.clone();
                let router = router.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_tun_tcp(stream, conn, protocol, stats, router).await {
                        debug!("tun tcp ended: {:#}", e);
                    }
                });
            }
        }))
    } else {
        None
    };

    // UDP handler: netstack UdpSocket <-> Direct UDP / Split DNS / QUIC Datagram
    // Reserve flow_ids 32768..=65534 for TUN sessions so they never collide with SOCKS5 (1..=32767).
    let tun_flow_ids: Arc<DashMap<SocketAddr, Arc<TunUdpSession>>> = Arc::new(DashMap::new());
    let direct_flow_ids: Arc<DashMap<SocketAddr, Arc<TunDirectUdpSession>>> =
        Arc::new(DashMap::new());
    let next_tun_flow = Arc::new(AtomicU16::new(TUN_FLOW_MIN));

    // Periodic idle reaper (60s timeout) prevents ephemeral DNS/UDP sockets from leaking flow_ids
    let reaper_flows = tun_flow_ids.clone();
    let reaper_direct = direct_flow_ids.clone();
    let reaper_task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(15));
        loop {
            tick.tick().await;
            let now = now_secs();
            reaper_flows.retain(|_, session| {
                now.saturating_sub(session.last_active.load(Ordering::Relaxed))
                    < TUN_UDP_IDLE_TIMEOUT_SECS
            });
            reaper_direct.retain(|_, session| {
                now.saturating_sub(session.last_active.load(Ordering::Relaxed))
                    < TUN_UDP_IDLE_TIMEOUT_SECS
            });
        }
    });

    let (udp_tx_task, udp_rx_task) = if let Some(udp_sock) = udp_socket {
        let (mut udp_read, mut udp_write) = udp_sock.split();
        // Unified return channel into TUN netstack: (payload, remote_src, local_dst, count_rx)
        let (out_tx, mut out_rx) =
            mpsc::channel::<(Vec<u8>, SocketAddr, SocketAddr, bool)>(1024);

        let conn_out = udp_conn.clone();
        let pacer_tx = pacer.clone();
        let stats_tx = stats.clone();
        let flow_map = tun_flow_ids.clone();
        let direct_map = direct_flow_ids.clone();
        let dispatch = udp_dispatch.clone();
        let next_flow = next_tun_flow.clone();
        let router_udp = router.clone();
        let out_tx_tunnel = out_tx.clone();

        let tx_task = tokio::spawn(async move {
            while let Some((payload, local_src, remote_dst)) = udp_read.next().await {
                let now = now_secs();

                // 1. Split DNS (UDP port 53) & direct IPv4 UDP routing
                let direct_target: Option<(SocketAddr, Option<SocketAddr>)> =
                    if remote_dst.port() == 53 && remote_dst.is_ipv4() {
                        match router_udp.decide_dns_query(&payload) {
                            DnsRoute::Domestic => {
                                // Rewrite domestic DNS query to 223.5.5.5:53, and spoof reply
                                // source back to `remote_dst` so Windows DNS client accepts it.
                                Some((DOMESTIC_DNS, Some(remote_dst)))
                            }
                            DnsRoute::Direct => Some((remote_dst, None)),
                            DnsRoute::Proxy => None,
                        }
                    } else if remote_dst.is_ipv4()
                        && router_udp.decide(&TargetAddr::Ip(remote_dst)).await == Route::Direct
                    {
                        Some((remote_dst, None))
                    } else {
                        None
                    };

                if let Some((actual_dst, spoof_reply_src)) = direct_target {
                    let d_session = if let Some(existing) = direct_map.get(&local_src) {
                        existing.last_active.store(now, Ordering::Relaxed);
                        existing.clone()
                    } else {
                        let sock = match router_udp.dialer.bind_udp_v4() {
                            Ok(s) => Arc::new(s),
                            Err(e) => {
                                warn!("TUN bind direct UDP failed: {e}");
                                continue;
                            }
                        };
                        let last_active = Arc::new(AtomicU64::new(now));
                        let last_active_rx = last_active.clone();
                        let sock_rx = sock.clone();
                        let out_tx_direct = out_tx.clone();
                        let router_rx = router_udp.clone();
                        let rx_handle = tokio::spawn(async move {
                            let mut buf = vec![0u8; 2048];
                            loop {
                                let Ok((n, src)) = sock_rx.recv_from(&mut buf).await else {
                                    break;
                                };
                                last_active_rx.store(now_secs(), Ordering::Relaxed);
                                let data = buf[..n].to_vec();
                                if src.port() == 53 {
                                    router_rx.record_dns_answer(&data);
                                }
                                let reply_src = match spoof_reply_src {
                                    Some(orig) if src == actual_dst => orig,
                                    _ => SocketAddr::new(src.ip().to_canonical(), src.port()),
                                };
                                if out_tx_direct
                                    .send((data, reply_src, local_src, false))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        });
                        let session = Arc::new(TunDirectUdpSession {
                            socket: sock,
                            last_active,
                            rx_task: rx_handle,
                        });
                        direct_map.insert(local_src, session.clone());
                        session
                    };

                    if let Err(e) = d_session.socket.send_to(&payload, actual_dst).await {
                        debug!("tun direct udp send to {actual_dst} failed: {e}");
                    }
                    continue;
                }

                // 2. Tunnelled UDP path (QUIC Datagram)
                let session = if let Some(existing) = flow_map.get(&local_src) {
                    existing.last_active.store(now, Ordering::Relaxed);
                    existing.clone()
                } else {
                    let (f_tx, mut f_rx) = mpsc::channel::<UdpFrame>(256);
                    let Some(fid) = reserve_flow_id(
                        &next_flow,
                        &dispatch,
                        f_tx,
                        TUN_FLOW_MIN,
                        TUN_FLOW_MAX,
                    ) else {
                        warn!("TUN UDP flow_id space exhausted, dropping packet from {local_src}");
                        continue;
                    };

                    let last_active = Arc::new(AtomicU64::new(now));
                    let last_active_rx = last_active.clone();
                    let out_tx_clone = out_tx_tunnel.clone();
                    let router_hint = router_udp.clone();
                    let rx_handle = tokio::spawn(async move {
                        let mut dedup = DedupWindow::new();
                        while let Some(frame) = f_rx.recv().await {
                            if protocol == NodeProtocol::Gacl && !dedup.check_and_insert(frame.seq) {
                                continue;
                            }
                            last_active_rx.store(now_secs(), Ordering::Relaxed);
                            if let TargetAddr::Ip(remote_src) = frame.addr {
                                if remote_src.port() == 53 {
                                    router_hint.record_dns_answer(&frame.payload);
                                }
                                if out_tx_clone
                                    .send((frame.payload.to_vec(), remote_src, local_src, true))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }
                    });

                    stats_tx.active_udp.fetch_add(1, Ordering::Relaxed);
                    let session = Arc::new(TunUdpSession {
                        flow_id: fid,
                        tx_seq: AtomicU16::new(0),
                        last_active,
                        rx_task: rx_handle,
                        dispatch: dispatch.clone(),
                        stats: stats_tx.clone(),
                    });
                    flow_map.insert(local_src, session.clone());
                    session
                };

                let seq = session.tx_seq.fetch_add(1, Ordering::Relaxed);
                let payload_len = payload.len();
                let payload_bytes = Bytes::from(payload);

                // Enable game dual-send only for small non-DNS, non-QUIC-443 UDP packets
                let is_game_pkt =
                    game_dup && payload_len < 200 && remote_dst.port() != 53 && remote_dst.port() != 443;

                if !send_proxy_udp_packet(
                    protocol,
                    &conn_out,
                    &pacer_tx,
                    session.flow_id,
                    seq,
                    is_game_pkt,
                    TargetAddr::Ip(remote_dst),
                    payload_bytes,
                ) {
                    break;
                }
                stats_tx
                    .tx_bytes
                    .fetch_add(payload_len as u64, Ordering::Relaxed);
            }
        });

        let stats_rx = stats.clone();
        let rx_task = tokio::spawn(async move {
            while let Some((data, remote_src, local_dst, count_rx)) = out_rx.recv().await {
                let len = data.len() as u64;
                if let Err(e) = udp_write.send((data, remote_src, local_dst)).await {
                    warn!("tun udp write error: {}", e);
                } else if count_rx {
                    stats_rx.rx_bytes.fetch_add(len, Ordering::Relaxed);
                }
            }
        });

        (Some(tx_task), Some(rx_task))
    } else {
        (None, None)
    };

    // Wait for shutdown signal
    loop {
        if *shutdown_rx.borrow() {
            break;
        }
        if shutdown_rx.changed().await.is_err() {
            break;
        }
    }

    // Cleanup all background tasks & dispatch entries (RAII Drop on TunUdpSession handles per-flow cleanup)
    reaper_task.abort();
    tun_to_stack.abort();
    stack_to_tun.abort();
    if let Some(t) = tcp_task {
        t.abort();
    }
    if let Some(t) = udp_tx_task {
        t.abort();
    }
    if let Some(t) = udp_rx_task {
        t.abort();
    }
    if let Some(t) = runner_task {
        t.abort();
    }
    tun_flow_ids.clear();
    direct_flow_ids.clear();
    emit("TUN 虚拟网卡与系统路由已安全还原".to_string());
    Ok(())
}

async fn handle_tun_tcp(
    mut stream: netstack_smoltcp::TcpStream,
    conn: Connection,
    protocol: NodeProtocol,
    stats: Arc<ClientStats>,
    router: Arc<Router>,
) -> Result<()> {
    let dst = *stream.remote_addr();
    // Tunnel-only server panel IP (198.18.0.1) must always traverse the tunnel.
    let is_panel_ip = dst.ip() == IpAddr::V4(Ipv4Addr::new(198, 18, 0, 1));

    // If DNS hints didn't record `dst` (e.g. browser DoH or pre-cached DNS) on client-first
    // ports 80/443 in Rule mode, opportunistically sniff TLS SNI / HTTP Host from the first
    // chunk so domain rules still take precedence over raw IP ranges.
    let mut first_chunk: Vec<u8> = Vec::new();
    let route = if is_panel_ip {
        Route::Proxy
    } else if router.mode() == RouteMode::Rule
        && matches!(dst.port(), 80 | 443)
        && !router.has_dns_hint(dst.ip())
    {
        let mut buf = vec![0u8; 4096];
        if let Ok(Ok(n)) = tokio::time::timeout(Duration::from_millis(50), stream.read(&mut buf)).await
            && n > 0 {
                first_chunk.extend_from_slice(&buf[..n]);
            }
        if let Some((_, host)) = protocol::sniff::sniff_first_packet(&first_chunk) {
            router.decide(&TargetAddr::Domain(host, dst.port())).await
        } else {
            router.decide(&TargetAddr::Ip(dst)).await
        }
    } else {
        router.decide(&TargetAddr::Ip(dst)).await
    };

    if route == Route::Direct {
        let mut upstream = tokio::time::timeout(
            Duration::from_secs(10),
            router.dialer.connect_tcp(dst),
        )
        .await
        .context("direct tun tcp timeout")?
        .with_context(|| format!("direct tun tcp connect {dst}"))?;
        if !first_chunk.is_empty() {
            upstream.write_all(&first_chunk).await?;
        }
        let _ = tokio::io::copy_bidirectional(&mut stream, &mut upstream).await;
        return Ok(());
    }

    let (mut quic_send, quic_recv) =
        open_proxy_tcp_stream(&conn, protocol, &TargetAddr::Ip(dst)).await?;
    if !first_chunk.is_empty() {
        quic_send.write_all(&first_chunk).await?;
        stats
            .tx_bytes
            .fetch_add(first_chunk.len() as u64, Ordering::Relaxed);
    }

    // Fast-open: the smoltcp handshake with the app is already complete, so start
    // relaying immediately; the server's dial status is checked inside the relay.
    relay_fast_open(stream, quic_send, quic_recv, protocol, stats, None)
        .await
        .with_context(|| format!("tcp to {dst}"))
}
