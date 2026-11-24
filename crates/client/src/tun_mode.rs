use crate::socks5::{ClientStats, TUN_FLOW_MAX, TUN_FLOW_MIN, UdpDispatchMap, reserve_flow_id};
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use netstack_smoltcp::StackBuilder;
use protocol::TUN_MTU;
use protocol::auth::AUTH_OK;
use protocol::dedup::DedupWindow;
use protocol::frame::{FLAG_GAME_MODE, TargetAddr, UdpFrame};
use protocol::pacer::{UdpDupPacer, send_datagram_checked};
use quinn::Connection;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

const TUN_UDP_IDLE_TIMEOUT_SECS: u64 = 60;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

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
        if cols.len() >= 4 && cols[0] == "0.0.0.0" && cols[1] == "0.0.0.0" {
            if let Ok(gw) = cols[2].parse::<Ipv4Addr>() {
                if gw != TUN_GW && !gw.is_unspecified() {
                    return Ok(gw);
                }
            }
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

#[allow(clippy::too_many_arguments)]
pub async fn run_tun_mode(
    server_ip: IpAddr,
    conn: Connection,
    pacer: UdpDupPacer,
    udp_dispatch: UdpDispatchMap,
    game_dup: bool,
    stats: Arc<ClientStats>,
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
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let candidate = dir.join("wintun.dll");
                if candidate.is_file() {
                    builder = builder.wintun_file(candidate.to_string_lossy().into_owned());
                }
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
    let _route_guard = RouteGuard::install(server_ip, if_index)?;
    emit(format!(
        "TUN 虚拟网卡已启动 ({TUN_NAME} {TUN_IP}/{TUN_PREFIX}, MTU {TUN_MTU}, 全局路由已接管)"
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

    // TCP handler: netstack TcpListener -> QUIC Stream
    let tcp_task = if let Some(mut listener) = tcp_listener {
        let conn = conn.clone();
        let stats = stats.clone();
        Some(tokio::spawn(async move {
            while let Some((stream, _local, _remote)) = listener.next().await {
                let conn = conn.clone();
                let stats = stats.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_tun_tcp(stream, conn, stats).await {
                        debug!("tun tcp ended: {:#}", e);
                    }
                });
            }
        }))
    } else {
        None
    };

    // UDP handler: netstack UdpSocket <-> QUIC Datagram
    // Reserve flow_ids 32768..=65534 for TUN sessions so they never collide with SOCKS5 (1..=32767).
    let tun_flow_ids: Arc<DashMap<SocketAddr, Arc<TunUdpSession>>> = Arc::new(DashMap::new());
    let next_tun_flow = Arc::new(AtomicU16::new(TUN_FLOW_MIN));

    // Periodic idle reaper (60s timeout) prevents ephemeral DNS/UDP sockets from leaking flow_ids
    let reaper_flows = tun_flow_ids.clone();
    let reaper_task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(15));
        loop {
            tick.tick().await;
            let now = now_secs();
            reaper_flows.retain(|_, session| {
                now.saturating_sub(session.last_active.load(Ordering::Relaxed))
                    < TUN_UDP_IDLE_TIMEOUT_SECS
            });
        }
    });

    let (udp_tx_task, udp_rx_task) = if let Some(udp_sock) = udp_socket {
        let (mut udp_read, mut udp_write) = udp_sock.split();
        let (in_tx, mut in_rx) = mpsc::channel::<(SocketAddr, UdpFrame)>(1024);

        let conn_out = conn.clone();
        let pacer_tx = pacer.clone();
        let stats_tx = stats.clone();
        let flow_map = tun_flow_ids.clone();
        let dispatch = udp_dispatch.clone();
        let next_flow = next_tun_flow.clone();

        let tx_task = tokio::spawn(async move {
            while let Some((payload, local_src, remote_dst)) = udp_read.next().await {
                let now = now_secs();
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
                    let in_tx_clone = in_tx.clone();
                    let rx_handle = tokio::spawn(async move {
                        let mut dedup = DedupWindow::new();
                        while let Some(frame) = f_rx.recv().await {
                            if !dedup.check_and_insert(frame.seq) {
                                continue;
                            }
                            last_active_rx.store(now_secs(), Ordering::Relaxed);
                            if in_tx_clone.send((local_src, frame)).await.is_err() {
                                break;
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
                let flags = if is_game_pkt { FLAG_GAME_MODE } else { 0 };

                let frame = UdpFrame {
                    flow_id: session.flow_id,
                    seq,
                    flags,
                    addr: TargetAddr::Ip(remote_dst),
                    payload: payload_bytes,
                };
                if let Ok(encoded) = frame.encode() {
                    if !send_datagram_checked(&conn_out, encoded.clone()) {
                        break;
                    }
                    stats_tx
                        .tx_bytes
                        .fetch_add(payload_len as u64, Ordering::Relaxed);

                    if is_game_pkt {
                        pacer_tx.schedule_dup(encoded);
                    }
                }
            }
        });

        let stats_rx = stats.clone();
        let rx_task = tokio::spawn(async move {
            while let Some((local_dst, frame)) = in_rx.recv().await {
                if let TargetAddr::Ip(remote_src) = frame.addr {
                    let len = frame.payload.len() as u64;
                    if let Err(e) = udp_write
                        .send((frame.payload.to_vec(), remote_src, local_dst))
                        .await
                    {
                        warn!("tun udp write error: {}", e);
                    } else {
                        stats_rx.rx_bytes.fetch_add(len, Ordering::Relaxed);
                    }
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
    emit("TUN 虚拟网卡与系统路由已安全还原".to_string());
    Ok(())
}

async fn handle_tun_tcp(
    stream: netstack_smoltcp::TcpStream,
    conn: Connection,
    stats: Arc<ClientStats>,
) -> Result<()> {
    let dst = *stream.remote_addr();
    let (mut quic_send, mut quic_recv) = conn.open_bi().await.context("open quic bi stream")?;
    TargetAddr::Ip(dst).write_tcp_open(&mut quic_send).await?;

    let status = quic_recv.read_u8().await.context("read server tcp status")?;
    if status != AUTH_OK {
        bail!("remote failed to connect to {dst}");
    }

    stats.active_tcp.fetch_add(1, Ordering::Relaxed);
    let (mut t_read, mut t_write) = tokio::io::split(stream);

    let stats_u = stats.clone();
    let u = tokio::spawn(async move {
        let _ = protocol::io::copy_with_counter(&mut t_read, &mut quic_send, &stats_u.tx_bytes).await;
        let _ = quic_send.finish();
    });

    let stats_d = stats.clone();
    let d = tokio::spawn(async move {
        let _ = protocol::io::copy_with_counter(&mut quic_recv, &mut t_write, &stats_d.rx_bytes).await;
        let _ = t_write.shutdown().await;
    });

    let _ = tokio::join!(u, d);
    stats.active_tcp.fetch_sub(1, Ordering::Relaxed);
    Ok(())
}
