use crate::relay::{ClientStats, TUN_FLOW_MIN};
use crate::rules::{DOMESTIC_DNS, DnsRoute, Route, Router};
use crate::tun_pkt::{TunIngressPacket, TunIngressParser, build_udp_ip_packets};
use crate::tunnel::{TunnelSession, Tunnels};
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use netstack_smoltcp::StackBuilder;
use protocol::TUN_MTU;
use protocol::dedup::DedupWindow;
use protocol::frame::{TargetAddr, UdpFrame};
use protocol::unix_now_secs as now_secs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

const TUN_UDP_IDLE_TIMEOUT_SECS: u64 = 180;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const TUN_NAME: &str = "GameAccel";
const TUN_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 88, 2);
const TUN_GW: Ipv4Addr = Ipv4Addr::new(10, 0, 88, 1);
const TUN_BROADCAST: Ipv4Addr = Ipv4Addr::new(10, 0, 88, 255);
const TUN_PREFIX: u8 = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum TunnelSlot {
    Primary,
    Secondary,
}

type TunUdpMap = Arc<DashMap<(SocketAddr, TunnelSlot), Arc<TunUdpSession>>>;
type DirectUdpMap = Arc<DashMap<(SocketAddr, Option<SocketAddr>), Arc<TunDirectUdpSession>>>;

async fn await_opt<T>(
    opt: &mut Option<JoinHandle<T>>,
) -> std::result::Result<T, tokio::task::JoinError> {
    match opt {
        Some(h) => h.await,
        None => std::future::pending().await,
    }
}

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
        bail!(
            "{program} {:?} failed: {} {}",
            args,
            stdout.trim(),
            stderr.trim()
        );
    }
    Ok(stdout)
}

/// Detect the current default IPv4 physical gateway on Windows.
#[cfg(target_os = "windows")]
fn detect_physical_gateway() -> Result<Ipv4Addr> {
    let out = run_hidden("route", &["print", "0.0.0.0"])?;
    for line in out.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() >= 4
            && cols[0] == "0.0.0.0"
            && cols[1] == "0.0.0.0"
            && let Ok(gw) = cols[2].parse::<Ipv4Addr>()
            && gw != TUN_GW
            && !gw.is_unspecified()
        {
            return Ok(gw);
        }
    }
    bail!("无法自动检测物理默认网关，请检查网络连接")
}

/// RAII guard that pins every node server IP to the physical gateway and installs
/// two half-default routes (`0.0.0.0/1` and `128.0.0.0/1`) onto the TUN interface.
struct RouteGuard {
    server_v4: Vec<Ipv4Addr>,
}

impl RouteGuard {
    #[cfg(target_os = "windows")]
    fn install(server_ips: &[IpAddr], tun_if_index: u32) -> Result<Self> {
        let phys_gw = detect_physical_gateway()?;
        let gw_str = phys_gw.to_string();

        let mut server_v4: Vec<Ipv4Addr> = Vec::new();
        for ip in server_ips {
            if let IpAddr::V4(v4) = ip {
                if server_v4.contains(v4) {
                    continue;
                }
                let ip_str = v4.to_string();
                let _ = run_hidden("route", &["delete", &ip_str]);
                run_hidden(
                    "route",
                    &[
                        "add",
                        &ip_str,
                        "mask",
                        "255.255.255.255",
                        &gw_str,
                        "metric",
                        "1",
                    ],
                )
                .with_context(|| format!("添加节点直连防回环路由失败 ({ip_str})"))?;
                server_v4.push(*v4);
            }
        }

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
    fn install(_server_ips: &[IpAddr], _tun_if_index: u32) -> Result<Self> {
        Ok(Self {
            server_v4: Vec::new(),
        })
    }
}

impl Drop for RouteGuard {
    fn drop(&mut self) {
        #[cfg(target_os = "windows")]
        {
            let tun_gw_str = TUN_GW.to_string();
            let _ = run_hidden(
                "route",
                &["delete", "0.0.0.0", "mask", "128.0.0.0", &tun_gw_str],
            );
            let _ = run_hidden(
                "route",
                &["delete", "128.0.0.0", "mask", "128.0.0.0", &tun_gw_str],
            );
            for v4 in &self.server_v4 {
                let ip_str = v4.to_string();
                let _ = run_hidden("route", &["delete", &ip_str, "mask", "255.255.255.255"]);
            }
        }
    }
}

struct TunUdpSession {
    flow_id: u16,
    tunnel: Arc<TunnelSession>,
    tx_seq: AtomicU16,
    last_active: Arc<AtomicU64>,
    rx_task: JoinHandle<()>,
    stats: Arc<ClientStats>,
}

impl Drop for TunUdpSession {
    fn drop(&mut self) {
        self.rx_task.abort();
        self.tunnel.release_flow(self.flow_id);
        self.stats
            .active_udp
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            })
            .ok();
    }
}

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

struct DialerIfGuard(Arc<Router>);

impl Drop for DialerIfGuard {
    fn drop(&mut self) {
        self.0.dialer.set_interface(None);
    }
}

/// Transient Windows Wintun creation errors that happen while NDIS/PnP is still finishing
/// asynchronous teardown of the previous adapter with the same name (`GameAccel`).
fn is_retryable_wintun_create_error(err: &std::io::Error) -> bool {
    // 1247 = ERROR_ALREADY_INITIALIZED ("试图进行初始操作，但是初始化已完成。")
    // 183  = ERROR_ALREADY_EXISTS
    matches!(err.raw_os_error(), Some(1247 | 183))
}

fn build_tun_device_sync() -> std::io::Result<tun_rs::AsyncDevice> {
    let mut builder = tun_rs::DeviceBuilder::new()
        .name(TUN_NAME)
        .ipv4(TUN_IP, TUN_PREFIX, None);

    #[cfg(target_os = "windows")]
    {
        builder = builder.mtu_v4(TUN_MTU);
        if let Ok(exe) = std::env::current_exe()
            && let Some(dir) = exe.parent()
        {
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

    builder.build_async()
}

async fn drop_tun_device_cleanly(dev: tun_rs::AsyncDevice) {
    // `AsyncDevice::drop` triggers tun-rs's interrupt event and calls `inner.shutdown()`,
    // waking the background reader thread so `WinTunAdapter::drop` (`WintunCloseAdapter`) runs.
    let _ = tokio::task::spawn_blocking(move || drop(dev)).await;
    // Give tun-rs's interrupted background reader thread & Windows NDIS a short beat to
    // finish WintunCloseAdapter before the next session recreates the adapter name.
    #[cfg(target_os = "windows")]
    tokio::time::sleep(Duration::from_millis(120)).await;
}

/// Creates the Wintun/TUN device off the Tokio async worker pool with cancellable retry
/// on transient Windows PnP teardown overlap (`os error 1247` / `183`).
/// Returns `Ok(None)` if shutdown was requested before or during device creation.
async fn create_tun_device_cancellable(
    shutdown_rx: &mut watch::Receiver<bool>,
) -> Result<Option<tun_rs::AsyncDevice>> {
    const MAX_ATTEMPTS: usize = 6;
    const RETRY_DELAY: Duration = Duration::from_millis(250);

    for attempt in 1..=MAX_ATTEMPTS {
        if *shutdown_rx.borrow() {
            return Ok(None);
        }
        let build_res = tokio::task::spawn_blocking(build_tun_device_sync)
            .await
            .context("创建 Wintun 虚拟网卡任务异常中止")?;

        match build_res {
            Ok(dev) => {
                if *shutdown_rx.borrow() {
                    drop_tun_device_cleanly(dev).await;
                    return Ok(None);
                }
                return Ok(Some(dev));
            }
            Err(e) if attempt < MAX_ATTEMPTS && is_retryable_wintun_create_error(&e) => {
                warn!(
                    "Wintun 网卡仍在释放中（{e}），{}ms 后重试 ({attempt}/{MAX_ATTEMPTS})",
                    RETRY_DELAY.as_millis()
                );
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            return Ok(None);
                        }
                    }
                    _ = tokio::time::sleep(RETRY_DELAY) => {}
                }
            }
            Err(e) => {
                return Err(e).context(
                    "创建 Wintun 虚拟网卡失败（请确认同目录存在 wintun.dll 且已用管理员身份运行）",
                );
            }
        }
    }
    Ok(None)
}

pub async fn run_tun_mode(
    server_ips: Vec<IpAddr>,
    tunnels: Arc<Tunnels>,
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

    if *shutdown_rx.borrow() {
        return Ok(());
    }

    if !is_elevated() {
        bail!("TUN 模式需要管理员权限，请点击「以管理员身份重启」后再开启 TUN");
    }

    let Some(dev) = create_tun_device_cancellable(&mut shutdown_rx).await? else {
        return Ok(());
    };

    let if_index = dev.if_index().context("获取 TUN 网卡索引失败")?;
    let phys_if = crate::direct::detect_physical_interface_v4().filter(|&idx| idx != if_index);
    router.dialer.set_interface(phys_if);
    let _dialer_guard = DialerIfGuard(router.clone());

    let route_guard = tokio::task::spawn_blocking(move || RouteGuard::install(&server_ips, if_index))
        .await
        .context("安装 TUN 路由任务异常中止")??;
    if *shutdown_rx.borrow() {
        let _ = tokio::task::spawn_blocking(move || drop(route_guard)).await;
        drop_tun_device_cleanly(dev).await;
        return Ok(());
    }
    emit(format!(
        "TUN 虚拟网卡已启动 ({TUN_NAME} {TUN_IP}/{TUN_PREFIX}, MTU {TUN_MTU}, 物理出口网卡 {:?}, 智能分流已启用)",
        phys_if
    ));

    let dev = Arc::new(dev);

    // Use `netstack-smoltcp` solely for user-space TCP; UDP is parsed/reassembled directly in
    // `tun_pkt` so IPv4-fragmented packets (e.g. from Hyper-V VMs or QUIC > TUN_MTU) can never
    // terminate `netstack-smoltcp`'s UDP stream and silently black-hole the TUN adapter.
    let (stack, runner, _udp_socket, tcp_listener) = StackBuilder::default()
        .enable_tcp(true)
        .enable_udp(false)
        .mtu(TUN_MTU as usize)
        .build()
        .context("初始化用户态 TCP/IP 协议栈失败")?;

    let mut runner_task = runner.map(tokio::spawn);
    let (mut stack_sink, mut stack_stream) = stack.split();

    let (udp_in_tx, mut udp_in_rx) = mpsc::channel::<(Vec<u8>, SocketAddr, SocketAddr)>(2048);
    let (udp_egress_tx, mut udp_egress_rx) = mpsc::channel::<Vec<u8>>(2048);

    let dev_in = dev.clone();
    let mut tun_to_stack = Some(tokio::spawn(async move {
        let mut buf = vec![0u8; 65_535];
        let mut parser = TunIngressParser::new(TUN_BROADCAST);
        let mut udp_dropped: u64 = 0;
        loop {
            match dev_in.recv(&mut buf).await {
                Ok(n) if n > 0 => match parser.parse(&buf[..n]) {
                    TunIngressPacket::Udp { payload, src, dst } => {
                        match udp_in_tx.try_send((payload, src, dst)) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                udp_dropped += 1;
                                if udp_dropped == 1 || udp_dropped.is_multiple_of(500) {
                                    warn!(
                                        "TUN UDP ingress queue full, dropped {udp_dropped} packets so far"
                                    );
                                }
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                warn!("TUN UDP ingress channel closed, stopping TUN reader");
                                break;
                            }
                        }
                    }
                    TunIngressPacket::Tcp(pkt) => {
                        if let Err(e) = stack_sink.send(pkt).await {
                            if e.kind() == std::io::ErrorKind::InvalidInput {
                                debug!("ignoring invalid TCP IP packet from TUN: {e}");
                                continue;
                            }
                            warn!("TUN TCP stack sink closed: {e}");
                            break;
                        }
                    }
                    TunIngressPacket::Drop => {}
                },
                Ok(_) => continue,
                Err(e) => {
                    warn!("tun recv ended: {e}");
                    break;
                }
            }
        }
    }));

    // Single serialized Wintun writer serving both game UDP replies (prioritized) and TCP stack output.
    let dev_out = dev.clone();
    let mut stack_to_tun = Some(tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                udp_pkt = udp_egress_rx.recv() => {
                    let Some(frame) = udp_pkt else {
                        break;
                    };
                    if let Err(e) = dev_out.send(&frame).await {
                        debug!("tun udp send error: {e}");
                    }
                }
                tcp_pkt = stack_stream.next() => {
                    match tcp_pkt {
                        Some(Ok(frame)) => {
                            if let Err(e) = dev_out.send(&frame).await {
                                debug!("tun tcp send error: {e}");
                            }
                        }
                        Some(Err(e)) => {
                            warn!("netstack stream error: {e}");
                            break;
                        }
                        None => {
                            warn!("netstack stream closed");
                            break;
                        }
                    }
                }
            }
        }
    }));

    let tcp_task = if let Some(mut listener) = tcp_listener {
        let tunnels_tcp = tunnels.clone();
        let stats = stats.clone();
        let router = router.clone();
        Some(tokio::spawn(async move {
            while let Some((stream, _local, _remote)) = listener.next().await {
                let tunnels_tcp = tunnels_tcp.clone();
                let stats = stats.clone();
                let router = router.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_tun_tcp(stream, tunnels_tcp, stats, router).await {
                        debug!("tun tcp ended: {:#}", e);
                    }
                });
            }
        }))
    } else {
        None
    };

    let tun_flow_ids: TunUdpMap = Arc::new(DashMap::new());
    let direct_flow_ids: DirectUdpMap = Arc::new(DashMap::new());
    let next_tun_flow = Arc::new(AtomicU16::new(TUN_FLOW_MIN));

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

    let (out_tx, mut out_rx) = mpsc::channel::<(Vec<u8>, SocketAddr, SocketAddr, bool)>(1024);

    let tunnels_udp = tunnels.clone();
    let stats_tx = stats.clone();
    let flow_map = tun_flow_ids.clone();
    let direct_map = direct_flow_ids.clone();
    let next_flow = next_tun_flow.clone();
    let router_udp = router.clone();
    let out_tx_tunnel = out_tx.clone();

    let mut udp_tx_task = Some(tokio::spawn(async move {
        while let Some((payload, local_src, remote_dst)) = udp_in_rx.recv().await {
            let now = now_secs();

            // DNS: domestic names go to AliDNS directly; foreign queries ride the primary
            // tunnel. Everything else asks the router for its egress bucket.
            let (route, direct_target): (Route, Option<(SocketAddr, Option<SocketAddr>)>) =
                if remote_dst.port() == 53 && remote_dst.is_ipv4() {
                    match router_udp.decide_dns_query(&payload) {
                        DnsRoute::Domestic => {
                            (Route::Direct, Some((DOMESTIC_DNS, Some(remote_dst))))
                        }
                        DnsRoute::Direct => (Route::Direct, Some((remote_dst, None))),
                        DnsRoute::Proxy => (Route::Primary, None),
                    }
                } else if remote_dst.is_ipv4() {
                    let route = router_udp.decide_ip_sync(remote_dst.ip());
                    let direct = (route == Route::Direct
                        || tunnels_udp.for_route_live_udp(route).is_none())
                    .then_some((remote_dst, None));
                    (route, direct)
                } else {
                    (Route::Primary, None)
                };

            if let Some((actual_dst, spoof_reply_src)) = direct_target {
                let direct_key = (local_src, spoof_reply_src);
                let d_session = if let Some(existing) = direct_map.get(&direct_key) {
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
                        let mut buf = vec![0u8; 4096];
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
                    direct_map.insert(direct_key, session.clone());
                    session
                };

                if let Err(e) = d_session.socket.send_to(&payload, actual_dst).await {
                    debug!("tun direct udp send to {actual_dst} failed: {e}");
                }
                continue;
            }

            // Tunnelled UDP path: pick the egress tunnel for this datagram's route.
            let Some(tunnel_udp) = tunnels_udp.for_route_live_udp(route) else {
                warn!("TUN UDP route {route:?} has no tunnel, dropping packet from {local_src}");
                continue;
            };
            let slot = if tunnels_udp.is_primary(tunnel_udp) {
                TunnelSlot::Primary
            } else {
                TunnelSlot::Secondary
            };
            let flow_key = (local_src, slot);

            let session = match flow_map.get(&flow_key).map(|e| e.clone()) {
                Some(existing) if Arc::ptr_eq(&existing.tunnel, tunnel_udp) => {
                    existing.last_active.store(now, Ordering::Relaxed);
                    existing
                }
                stale => {
                    if stale.is_some() {
                        flow_map.remove(&flow_key);
                    }
                    drop(stale);
                    let (f_tx, mut f_rx) = mpsc::channel::<UdpFrame>(256);
                    let Some(fid) = tunnel_udp.reserve_tun_flow(&next_flow, f_tx) else {
                        warn!("TUN UDP flow_id space exhausted, dropping packet from {local_src}");
                        continue;
                    };

                    let last_active = Arc::new(AtomicU64::new(now));
                    let last_active_rx = last_active.clone();
                    let out_tx_clone = out_tx_tunnel.clone();
                    let router_hint = router_udp.clone();
                    let dedup_rx = tunnel_udp.should_dedup_rx();
                    let rx_handle = tokio::spawn(async move {
                        let mut dedup = DedupWindow::new();
                        while let Some(frame) = f_rx.recv().await {
                            if dedup_rx && !dedup.check_and_insert(frame.seq) {
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
                        tunnel: tunnel_udp.clone(),
                        tx_seq: AtomicU16::new(0),
                        last_active,
                        rx_task: rx_handle,
                        stats: stats_tx.clone(),
                    });
                    flow_map.insert(flow_key, session.clone());
                    session
                }
            };

            let seq = session.tx_seq.fetch_add(1, Ordering::Relaxed);
            let payload_len = payload.len();
            let payload_bytes = Bytes::from(payload);

            let is_game_pkt = session.tunnel.game_dup_enabled()
                && payload_len < 200
                && remote_dst.port() != 53
                && remote_dst.port() != 443;

            if !session.tunnel.send_udp(
                session.flow_id,
                seq,
                is_game_pkt,
                TargetAddr::Ip(remote_dst),
                payload_bytes,
            ) {
                flow_map.remove(&flow_key);
                if slot == TunnelSlot::Primary {
                    warn!("TUN UDP primary tunnel closed, stopping TUN UDP tx loop");
                    break;
                }
                // Secondary (domestic) lane died mid-session: drop only this secondary flow so
                // subsequent packets fall back to DIRECT without killing the TUN data plane.
                warn!(
                    "TUN UDP secondary tunnel closed for {local_src}; releasing flow to fall back direct"
                );
                continue;
            }
            stats_tx
                .tx_bytes
                .fetch_add(payload_len as u64, Ordering::Relaxed);
        }
    }));

    let stats_rx = stats.clone();
    let mut udp_rx_task = Some(tokio::spawn(async move {
        let next_ip_id = AtomicU16::new(1);
        while let Some((data, remote_src, local_dst, count_rx)) = out_rx.recv().await {
            let len = data.len() as u64;
            let pkts = build_udp_ip_packets(
                &data,
                remote_src,
                local_dst,
                TUN_MTU as usize,
                &next_ip_id,
            );
            let mut sent_ok = !pkts.is_empty();
            for pkt in pkts {
                if udp_egress_tx.send(pkt).await.is_err() {
                    sent_ok = false;
                    break;
                }
            }
            if !sent_ok {
                if udp_egress_tx.is_closed() {
                    warn!("TUN UDP egress channel closed");
                    break;
                }
                continue;
            }
            if count_rx {
                stats_rx.rx_bytes.fetch_add(len, Ordering::Relaxed);
            }
        }
    }));

    let mut fatal_reason: Option<String> = None;
    tokio::select! {
        _ = async {
            loop {
                if *shutdown_rx.borrow() {
                    break;
                }
                if shutdown_rx.changed().await.is_err() {
                    break;
                }
            }
        } => {}
        res = await_opt(&mut tun_to_stack) => {
            tun_to_stack = None;
            if !*shutdown_rx.borrow() {
                fatal_reason = Some(format!("TUN 读包任务异常退出 ({res:?})"));
            }
        }
        res = await_opt(&mut stack_to_tun) => {
            stack_to_tun = None;
            if !*shutdown_rx.borrow() {
                fatal_reason = Some(format!("TUN 写包任务异常退出 ({res:?})"));
            }
        }
        res = await_opt(&mut udp_tx_task) => {
            udp_tx_task = None;
            if !*shutdown_rx.borrow() {
                fatal_reason = Some(format!("TUN UDP 上行任务异常退出 ({res:?})"));
            }
        }
        res = await_opt(&mut udp_rx_task) => {
            udp_rx_task = None;
            if !*shutdown_rx.borrow() {
                fatal_reason = Some(format!("TUN UDP 下行任务异常退出 ({res:?})"));
            }
        }
        res = await_opt(&mut runner_task) => {
            runner_task = None;
            if !*shutdown_rx.borrow() {
                fatal_reason = Some(format!("TUN TCP 协议栈异常退出 ({res:?})"));
            }
        }
    }

    // Abort & await every task holding `Arc<AsyncDevice>` so `AsyncDevice::drop` runs
    // right here (triggering tun-rs's interrupt event & `WintunCloseAdapter`) before we
    // release the process-wide session lock.
    reaper_task.abort();
    if let Some(t) = tun_to_stack.take() {
        t.abort();
        let _ = t.await;
    }
    if let Some(t) = stack_to_tun.take() {
        t.abort();
        let _ = t.await;
    }
    if let Some(t) = tcp_task {
        t.abort();
        let _ = t.await;
    }
    if let Some(t) = udp_tx_task.take() {
        t.abort();
        let _ = t.await;
    }
    if let Some(t) = udp_rx_task.take() {
        t.abort();
        let _ = t.await;
    }
    if let Some(t) = runner_task.take() {
        t.abort();
        let _ = t.await;
    }
    let _ = reaper_task.await;

    tun_flow_ids.clear();
    direct_flow_ids.clear();

    // Restore system routes and close Wintun adapter completely BEFORE releasing the
    // process-wide session lock or emitting the restored log.
    let _ = tokio::task::spawn_blocking(move || drop(route_guard)).await;
    match Arc::try_unwrap(dev) {
        Ok(owned_dev) => drop_tun_device_cleanly(owned_dev).await,
        Err(shared_dev) => {
            let _ = tokio::task::spawn_blocking(move || drop(shared_dev)).await;
            #[cfg(target_os = "windows")]
            tokio::time::sleep(Duration::from_millis(120)).await;
        }
    }
    emit("TUN 虚拟网卡与系统路由已安全还原".to_string());
    if let Some(reason) = fatal_reason {
        bail!("{reason}");
    }
    Ok(())
}

async fn handle_tun_tcp(
    mut stream: netstack_smoltcp::TcpStream,
    tunnels: Arc<Tunnels>,
    stats: Arc<ClientStats>,
    router: Arc<Router>,
) -> Result<()> {
    let dst = *stream.remote_addr();
    let is_panel_ip = dst.ip() == IpAddr::V4(Ipv4Addr::new(198, 18, 0, 1));

    let mut first_chunk: Vec<u8> = Vec::new();
    let route = if is_panel_ip {
        Route::Primary
    } else if router.mode().is_split()
        && matches!(dst.port(), 80 | 443)
        && !router.has_dns_hint(dst.ip())
    {
        let mut buf = vec![0u8; 4096];
        if let Ok(Ok(n)) =
            tokio::time::timeout(Duration::from_millis(50), stream.read(&mut buf)).await
            && n > 0
        {
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

    // No tunnel for this route (Direct, or the domestic lane is gone) => dial out directly.
    let Some(tunnel) = tunnels.for_route(route) else {
        let mut upstream =
            tokio::time::timeout(Duration::from_secs(10), router.dialer.connect_tcp(dst))
                .await
                .context("direct tun tcp timeout")?
                .with_context(|| format!("direct tun tcp connect {dst}"))?;
        if !first_chunk.is_empty() {
            upstream.write_all(&first_chunk).await?;
        }
        let _ = tokio::io::copy_bidirectional(&mut stream, &mut upstream).await;
        return Ok(());
    };

    let (mut quic_send, quic_recv) = tunnel.open_tcp(&TargetAddr::Ip(dst)).await?;
    if !first_chunk.is_empty() {
        quic_send.write_all(&first_chunk).await?;
        stats
            .tx_bytes
            .fetch_add(first_chunk.len() as u64, Ordering::Relaxed);
    }

    tunnel
        .relay_tcp(stream, quic_send, quic_recv, stats, None)
        .await
        .with_context(|| format!("tcp to {dst}"))
}

#[cfg(test)]
mod tests {
    use super::is_retryable_wintun_create_error;

    #[test]
    fn wintun_retryable_create_errors() {
        assert!(is_retryable_wintun_create_error(
            &std::io::Error::from_raw_os_error(1247)
        ));
        assert!(is_retryable_wintun_create_error(
            &std::io::Error::from_raw_os_error(183)
        ));
        assert!(!is_retryable_wintun_create_error(
            &std::io::Error::from_raw_os_error(126)
        ));
    }
}
