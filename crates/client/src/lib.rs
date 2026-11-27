pub mod direct;
pub(crate) mod dns_msg;
pub mod error;
pub(crate) mod http_proxy;
pub(crate) mod quic;
pub mod relay;
pub mod rules;
pub mod socks5;
pub mod telemetry;
pub mod tun_mode;
pub mod tunnel;

pub use error::{ConnectError, classify};

pub use quic::{resolve_server_addr, validate_server_addr};

use crate::quic::{bind_tuned_udp_socket, build_client_config, unspecified_bind_for};
use crate::relay::{ClientStats, UdpDispatchMap};
use crate::rules::{DomesticEgress, DomesticEgressCell, RouteModeCell, Router, RuleData};
use crate::socks5::run_mixed_proxy_server;
use crate::telemetry::JitterEstimator;
use crate::tunnel::{TunnelSession, Tunnels};
use anyhow::{Context, Result};
use dashmap::DashMap;
use protocol::auth::{AUTH_OK, write_auth_request};
use protocol::frame::UdpFrame;
use protocol::hy2::{
    Hy2UdpReassembler, decode_hy2_udp_message, encode_h3_control_stream, header_val,
    parse_hy2_addr, read_h3_headers, write_h3_auth_request,
};
use protocol::pacer::UdpDupPacer;
use protocol::share::NodeProtocol;
use quinn::Endpoint;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

#[derive(Debug, Clone)]
pub struct UpstreamOptions {
    /// Server `IP:port`, `[IPv6]:port`, or `domain:port`.
    pub server: String,
    pub server_name: String,
    pub token: String,
    pub protocol: NodeProtocol,
    pub game_dup: bool,
    pub insecure: bool,
    /// Optional Brutal congestion-control rate cap in Mbps for the TCP lane only (0 = BBR).
    pub brutal_mbps: u32,
}

#[derive(Debug, Clone)]
pub struct ClientOptions {
    pub primary: UpstreamOptions,
    /// Optional domestic node. Only dialled when `domestic` is `Secondary`.
    pub secondary: Option<UpstreamOptions>,
    /// Where the domestic (CN) bucket goes: DIRECT or through the secondary tunnel.
    pub domestic: DomesticEgress,
    pub listen: SocketAddr,
    pub enable_tun: bool,
    /// Split-routing mode, shared live with the UI (applies to new connections).
    pub route_mode: Arc<RouteModeCell>,
    /// Optional directory whose `direct-list.txt` / `proxy-list.txt` / `geoip-cn.txt`
    /// override the embedded rule data.
    pub rules_dir: Option<std::path::PathBuf>,
}

/// One established node: dual QUIC lanes, its tunnel session, and the endpoint/socket
/// state that must outlive the session.
struct Upstream {
    tunnel: Arc<TunnelSession>,
    tcp_conn: quinn::Connection,
    udp_conn: quinn::Connection,
    tcp_ep: Endpoint,
    udp_ep: Endpoint,
    server_addr: SocketAddr,
    // Keep HTTP/3 control streams & uni-stream drainers alive for the session lifetime.
    _tcp_authed: AuthedConn,
    _udp_authed: AuthedConn,
    _dgram_task: AbortOnDrop,
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.tcp_conn.close(0u32.into(), b"client_shutdown");
        self.udp_conn.close(0u32.into(), b"client_shutdown");
        self.tcp_ep.close(0u32.into(), b"client_shutdown");
        self.udp_ep.close(0u32.into(), b"client_shutdown");
    }
}

pub(crate) struct AbortOnDrop(pub(crate) tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub async fn run_client_with_shutdown(
    opts: ClientOptions,
    stats: Arc<ClientStats>,
    log_tx: Option<mpsc::UnboundedSender<String>>,
    mut ext_shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let emit = |msg: String| {
        info!("{}", msg);
        if let Some(tx) = &log_tx {
            let _ = tx.send(msg);
        }
    };

    // Internal shutdown flag raised by the caller's signal or by the QUIC connection dying,
    // so every data-plane task observes one consistent stop condition.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let _ext_shutdown_guard = {
        let tx = shutdown_tx.clone();
        AbortOnDrop(tokio::spawn(async move {
            loop {
                if *ext_shutdown_rx.borrow() {
                    break;
                }
                if ext_shutdown_rx.changed().await.is_err() {
                    break;
                }
            }
            let _ = tx.send(true);
        }))
    };

    let _ = rustls::crypto::ring::default_provider().install_default();

    // Primary and domestic-node handshakes run concurrently (only when in Dual mode).
    // Primary is mandatory; domestic is best-effort and degrades to DIRECT on failure.
    let want_secondary = opts.route_mode.get() == crate::rules::RouteMode::Dual
        && opts.secondary.is_some()
        && opts.domestic == DomesticEgress::Secondary;
    let secondary_opts = if want_secondary {
        opts.secondary.clone()
    } else {
        None
    };
    let mut primary_rx = shutdown_rx.clone();
    let mut secondary_rx = shutdown_rx.clone();
    let primary_log = log_tx.clone();
    let secondary_log = log_tx.clone();

    let (primary_res, secondary_res) = tokio::join!(
        connect_upstream(&opts.primary, &mut primary_rx, &primary_log, "主节点"),
        async move {
            match secondary_opts {
                Some(o) => {
                    connect_upstream(&o, &mut secondary_rx, &secondary_log, "国内节点").await
                }
                None => Ok(None),
            }
        },
    );

    let Some(primary) = primary_res? else {
        return Ok(());
    };
    if *shutdown_rx.borrow() {
        return Ok(());
    }

    // A failed or dead domestic lane only downgrades the CN bucket to DIRECT.
    let mut domestic = if opts.route_mode.get() == crate::rules::RouteMode::Dual {
        opts.domestic
    } else {
        DomesticEgress::Direct
    };
    let secondary = match secondary_res {
        Ok(Some(up)) => Some(up),
        Ok(None) => {
            if domestic == DomesticEgress::Secondary {
                domestic = DomesticEgress::Direct;
            }
            None
        }
        Err(e) => {
            emit(format!("国内节点连接失败（{e:#}），国内流量回退直连"));
            domestic = DomesticEgress::Direct;
            None
        }
    };
    let domestic_cell = DomesticEgressCell::new(domestic);

    let initial_rtt = primary.udp_conn.rtt().as_millis() as u64;
    stats
        .connected_at_unix
        .store(protocol::unix_now_secs(), Ordering::Relaxed);
    stats.rtt_ms.store(initial_rtt, Ordering::Relaxed);
    stats.jitter_tenths_ms.store(0, Ordering::Relaxed);
    stats.loss_bps.store(0, Ordering::Relaxed);
    stats.dup_active.store(false, Ordering::Relaxed);
    stats.connected.store(true, Ordering::Relaxed);
    if let Some(sec) = &secondary {
        let sec_rtt = sec.udp_conn.rtt().as_millis() as u64;
        stats.secondary_rtt_ms.store(sec_rtt, Ordering::Relaxed);
        stats.secondary_connected.store(true, Ordering::Relaxed);
    } else {
        stats.secondary_rtt_ms.store(0, Ordering::Relaxed);
        stats.secondary_connected.store(false, Ordering::Relaxed);
    }
    emit(format!(
        "主节点双通道已连接并认证成功 [{}] ({}，游戏 UDP 通道初始 RTT {} ms)",
        opts.primary.protocol.label(),
        primary.server_addr,
        initial_rtt
    ));
    if let Some(sec) = &secondary {
        emit(format!(
            "国内节点已连接 ({}，RTT {} ms)，国内流量将走该隧道",
            sec.server_addr,
            sec.udp_conn.rtt().as_millis()
        ));
    }

    // Primary watchdog: any lane death (idle timeout / server restart / network change)
    // tears the whole session down so both lanes reconnect cleanly together.
    let watchdog_tx = shutdown_tx.clone();
    let wd_tcp = primary.tcp_conn.clone();
    let wd_udp = primary.udp_conn.clone();
    let _watchdog_guard = AbortOnDrop(tokio::spawn(async move {
        tokio::select! {
            _ = wd_tcp.closed() => {}
            _ = wd_udp.closed() => {}
        }
        let _ = watchdog_tx.send(true);
    }));

    // Domestic lane death only downgrades NEW connections to DIRECT; the session survives.
    let _secondary_watchdog = secondary.as_ref().map(|sec| {
        let cell = domestic_cell.clone();
        let sec_stats = stats.clone();
        let wd_tcp = sec.tcp_conn.clone();
        let wd_udp = sec.udp_conn.clone();
        let log = log_tx.clone();
        AbortOnDrop(tokio::spawn(async move {
            tokio::select! {
                _ = wd_tcp.closed() => {}
                _ = wd_udp.closed() => {}
            }
            cell.set(DomesticEgress::Direct);
            sec_stats.secondary_connected.store(false, Ordering::Relaxed);
            sec_stats.secondary_rtt_ms.store(0, Ordering::Relaxed);
            let msg = "国内节点连接已断开，国内流量回退直连（仅影响新连接）".to_string();
            info!("{}", msg);
            if let Some(tx) = &log {
                let _ = tx.send(msg);
            }
        }))
    });

    // Periodic 1s Game Link Health sampler (primary + optional secondary RTT)
    let rtt_conn = primary.udp_conn.clone();
    let sec_rtt_conn = secondary.as_ref().map(|s| s.udp_conn.clone());
    let rtt_stats = stats.clone();
    let rtt_pacer = primary.tunnel.pacer().clone();
    let rtt_task = tokio::spawn(async move {
        let mut jitter_est = JitterEstimator::default();
        let _ = jitter_est.observe_rtt_ms(initial_rtt);
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            if rtt_conn.close_reason().is_some() {
                break;
            }
            let rtt = rtt_conn.rtt().as_millis() as u64;
            let jitter_tenths = jitter_est.observe_rtt_ms(rtt);
            rtt_stats.rtt_ms.store(rtt, Ordering::Relaxed);
            rtt_stats
                .jitter_tenths_ms
                .store(jitter_tenths, Ordering::Relaxed);
            rtt_stats
                .loss_bps
                .store(rtt_pacer.loss_bps(), Ordering::Relaxed);
            rtt_stats
                .dup_active
                .store(rtt_pacer.is_dup_active(), Ordering::Relaxed);
            if let Some(sec_conn) = &sec_rtt_conn
                && sec_conn.close_reason().is_none()
            {
                rtt_stats
                    .secondary_rtt_ms
                    .store(sec_conn.rtt().as_millis() as u64, Ordering::Relaxed);
            }
        }
    });

    let tunnels = Tunnels::new(
        primary.tunnel.clone(),
        secondary.as_ref().map(|sec| sec.tunnel.clone()),
    );

    // Split-routing tables (~140k entries) are parsed once per process, off the runtime.
    // Built before TUN starts: the mixed proxy and the TUN share one router + dialer.
    let rules_dir = opts.rules_dir.clone();
    let rule_data = tokio::task::spawn_blocking(move || RuleData::shared(rules_dir.as_deref()))
        .await
        .context("加载分流规则失败")?;
    emit(format!(
        "分流模式：{}（国内流量：{}；{}）",
        opts.route_mode.get().label(),
        domestic_cell.get().label(),
        rule_data.summary(),
    ));
    let router = Arc::new(Router::new(
        opts.route_mode.clone(),
        domestic_cell.clone(),
        rule_data,
    ));

    // Optional TUN virtual NIC mode (runs alongside the local mixed proxy).
    // Both node server IPs are pinned to the physical gateway so node traffic
    // never re-enters the TUN.
    let tun_handle = if opts.enable_tun {
        let mut server_ips = vec![primary.server_addr.ip()];
        if let Some(sec) = &secondary {
            server_ips.push(sec.server_addr.ip());
        }
        let tun_tunnels = tunnels.clone();
        let tun_stats = stats.clone();
        let tun_log_tx = log_tx.clone();
        let tun_shutdown = shutdown_rx.clone();
        let tun_router = router.clone();
        let tun_err_tx = log_tx.clone();
        Some(tokio::spawn(async move {
            let res = tun_mode::run_tun_mode(
                server_ips,
                tun_tunnels,
                tun_stats,
                tun_router,
                tun_log_tx,
                tun_shutdown,
            )
            .await;
            if let Err(e) = &res {
                info!("TUN 模式异常: {e:#}");
                if let Some(tx) = &tun_err_tx {
                    let _ = tx.send(format!("TUN 模式异常: {e:#}"));
                }
            }
            res
        }))
    } else {
        None
    };

    let res = run_mixed_proxy_server(
        opts.listen,
        tunnels.clone(),
        stats.clone(),
        router,
        log_tx.clone(),
        shutdown_rx.clone(),
    )
    .await;

    // Listener bind failure or shutdown: also stop TUN before awaiting its handle,
    // otherwise a failed mixed-proxy bind would block here forever.
    let _ = shutdown_tx.send(true);

    // Errors were already reported by the TUN task itself; this only awaits cleanup.
    if let Some(handle) = tun_handle {
        let _ = handle.await;
    }

    rtt_task.abort();
    // Dropping the upstreams aborts their datagram tasks and closes conns + endpoints.
    drop(primary);
    drop(secondary);
    stats.reset();
    emit("连接已断开".to_string());
    res
}

/// Establishes both QUIC lanes for one node. Returns `Ok(None)` when shutdown was requested
/// while handshaking (the caller should stop silently).
async fn connect_upstream(
    opts: &UpstreamOptions,
    shutdown_rx: &mut watch::Receiver<bool>,
    log_tx: &Option<mpsc::UnboundedSender<String>>,
    label: &str,
) -> Result<Option<Upstream>> {
    let note = |msg: String| {
        info!("{}", msg);
        if let Some(tx) = log_tx {
            let _ = tx.send(msg);
        }
    };

    let server_addr = tokio::select! {
        _ = shutdown_rx.changed() => return Ok(None),
        res = resolve_server_addr(&opts.server) => res?,
    };

    // Dual-lane QUIC architecture:
    // - `tcp_ep` / `tcp_conn`: carries bulk TCP streams (web, video, downloads)
    // - `udp_ep` / `udp_conn`: carries ONLY game/voice UDP datagrams on its own socket & cwnd,
    //   preventing TCP downloads or BBR ProbeRTT from bloating game ping or falsely triggering dual-send.
    let tcp_config = build_client_config(opts.insecure, false, opts.brutal_mbps)?;
    let udp_config = build_client_config(opts.insecure, true, 0)?;

    let bind_addr = unspecified_bind_for(server_addr);
    let mut tcp_endpoint = Endpoint::new(
        quinn::EndpointConfig::default(),
        None,
        bind_tuned_udp_socket(bind_addr, 8 * 1024 * 1024)?,
        Arc::new(quinn::TokioRuntime),
    )?;
    tcp_endpoint.set_default_client_config(tcp_config);

    let mut udp_endpoint = Endpoint::new(
        quinn::EndpointConfig::default(),
        None,
        bind_tuned_udp_socket(bind_addr, 2 * 1024 * 1024)?,
        Arc::new(quinn::TokioRuntime),
    )?;
    udp_endpoint.set_default_client_config(udp_config);

    let close_endpoints = |tcp_ep: &Endpoint, udp_ep: &Endpoint, reason: &[u8]| {
        tcp_ep.close(0u32.into(), reason);
        udp_ep.close(0u32.into(), reason);
    };

    let cc_label = if opts.brutal_mbps > 0 {
        format!("TCP=Brutal {}M / UDP=BBR", opts.brutal_mbps.clamp(1, 1000))
    } else {
        "TCP/UDP 拥塞隔离".to_string()
    };
    note(format!(
        "正在建立{label}双通道连接 [{}] quic://{} -> {} (SNI={}，{})...",
        opts.protocol.label(),
        opts.server,
        server_addr,
        opts.server_name,
        cc_label
    ));

    // Parallel Handshake + Auth for both lanes with 8s per-attempt timeout, instant cancel,
    // and 1 automatic retry on transient UDP handshake packet loss.
    const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(8);
    const MAX_ATTEMPTS: usize = 2;

    let mut connected_pair = None;
    let mut last_err = None;

    for attempt in 1..=MAX_ATTEMPTS {
        if *shutdown_rx.borrow() {
            close_endpoints(&tcp_endpoint, &udp_endpoint, b"cancelled");
            return Ok(None);
        }
        if attempt > 1 {
            note(format!(
                "{label}首次握手/鉴权超时，正在自动重试 ({attempt}/{MAX_ATTEMPTS})..."
            ));
        }

        let attempt_fut = async {
            let (tcp_authed, udp_authed) = tokio::try_join!(
                connect_and_auth(
                    &tcp_endpoint,
                    server_addr,
                    &opts.server_name,
                    &opts.token,
                    opts.protocol,
                ),
                connect_and_auth(
                    &udp_endpoint,
                    server_addr,
                    &opts.server_name,
                    &opts.token,
                    opts.protocol,
                ),
            )?;
            Ok::<(AuthedConn, AuthedConn), anyhow::Error>((tcp_authed, udp_authed))
        };

        let outcome = tokio::select! {
            _ = shutdown_rx.changed() => {
                close_endpoints(&tcp_endpoint, &udp_endpoint, b"cancelled");
                return Ok(None);
            }
            res = tokio::time::timeout(HANDSHAKE_TIMEOUT, attempt_fut) => res,
        };

        match outcome {
            Ok(Ok(pair)) => {
                connected_pair = Some(pair);
                break;
            }
            Ok(Err(e)) => {
                if !classify(&e).is_retryable() {
                    close_endpoints(&tcp_endpoint, &udp_endpoint, b"auth_failed");
                    return Err(e);
                }
                last_err = Some(e);
            }
            Err(_) => {
                last_err = Some(anyhow::anyhow!(
                    "QUIC 握手或鉴权超时（请检查网络、UDP 端口，或先关闭其他代理软件的 TUN 避免流量套娃/冲突）"
                ));
            }
        }
    }

    let Some((tcp_authed, udp_authed)) = connected_pair else {
        close_endpoints(&tcp_endpoint, &udp_endpoint, b"connect_failed");
        return Err(last_err.unwrap_or_else(|| anyhow::anyhow!("连接失败")));
    };
    let tcp_conn = tcp_authed.conn.clone();
    let udp_conn = udp_authed.conn.clone();

    if *shutdown_rx.borrow() {
        tcp_conn.close(0u32.into(), b"cancelled");
        udp_conn.close(0u32.into(), b"cancelled");
        close_endpoints(&tcp_endpoint, &udp_endpoint, b"cancelled");
        return Ok(None);
    }

    // Dedicated UDP duplicate pacer: samples loss ONLY on `udp_conn`, immune to TCP BBR loss
    let pacer = UdpDupPacer::spawn(udp_conn.clone());

    // Background QUIC Datagram receiver on dedicated UDP lane -> dispatch by flow_id
    let udp_dispatch: UdpDispatchMap = Arc::new(DashMap::new());
    let dgram_task = {
        let dispatch = udp_dispatch.clone();
        let dgram_conn = udp_conn.clone();
        let dgram_proto = opts.protocol;
        AbortOnDrop(tokio::spawn(async move {
            let mut hy2_reasm = Hy2UdpReassembler::default();
            while let Ok(raw) = dgram_conn.read_datagram().await {
                match dgram_proto {
                    NodeProtocol::Gacl => match UdpFrame::decode(raw) {
                        Ok(frame) => {
                            if let Some(tx) = dispatch.get(&frame.flow_id) {
                                let _ = tx.try_send(frame);
                            }
                        }
                        Err(e) => warn!("invalid udp frame from server: {:#}", e),
                    },
                    NodeProtocol::Hysteria2 => match decode_hy2_udp_message(&raw) {
                        Ok(msg) => {
                            if let Some((sid, pid, addr_str, payload)) = hy2_reasm.push(msg) {
                                match parse_hy2_addr(&addr_str) {
                                    Ok(addr) => {
                                        let flow_id = sid as u16;
                                        if let Some(tx) = dispatch.get(&flow_id) {
                                            let _ = tx.try_send(UdpFrame {
                                                flow_id,
                                                seq: pid,
                                                flags: 0,
                                                addr,
                                                payload,
                                            });
                                        }
                                    }
                                    Err(e) => warn!("invalid hy2 udp addr from server: {:#}", e),
                                }
                            }
                        }
                        Err(e) => warn!("invalid hy2 udp datagram from server: {:#}", e),
                    },
                }
            }
        }))
    };

    let tunnel = TunnelSession::new(
        tcp_conn.clone(),
        udp_conn.clone(),
        pacer,
        udp_dispatch,
        opts.protocol,
        opts.game_dup,
    );

    Ok(Some(Upstream {
        tunnel,
        tcp_conn,
        udp_conn,
        tcp_ep: tcp_endpoint,
        udp_ep: udp_endpoint,
        server_addr,
        _tcp_authed: tcp_authed,
        _udp_authed: udp_authed,
        _dgram_task: dgram_task,
    }))
}

struct AuthedConn {
    conn: quinn::Connection,
    _h3_ctrl: Option<quinn::SendStream>,
    _h3_uni_drain: Option<AbortOnDrop>,
}

async fn connect_and_auth(
    endpoint: &Endpoint,
    server: SocketAddr,
    server_name: &str,
    token: &str,
    protocol: NodeProtocol,
) -> Result<AuthedConn> {
    let conn = endpoint
        .connect(server, server_name)?
        .await
        .context("QUIC 握手失败")?;

    match protocol {
        NodeProtocol::Gacl => {
            let (mut auth_send, mut auth_recv) = conn.open_bi().await.context("打开鉴权流失败")?;
            write_auth_request(&mut auth_send, token)
                .await
                .context("发送鉴权请求失败")?;
            let _ = auth_send.finish();
            let resp = auth_recv.read_u8().await.context("读取鉴权响应失败")?;
            if resp != AUTH_OK {
                conn.close(0u32.into(), b"auth_failed");
                return Err(ConnectError::AuthRejected {
                    detail: format!("服务端拒绝鉴权 (code={resp})"),
                }
                .into());
            }
            Ok(AuthedConn {
                conn,
                _h3_ctrl: None,
                _h3_uni_drain: None,
            })
        }
        NodeProtocol::Hysteria2 => {
            // 1. Open HTTP/3 client control stream (empty SETTINGS) and keep it open.
            let mut ctrl_send = conn.open_uni().await.context("打开 HTTP/3 控制流失败")?;
            ctrl_send
                .write_all(&encode_h3_control_stream())
                .await
                .context("发送 HTTP/3 SETTINGS 失败")?;

            // Drain server's unidirectional HTTP/3 control / QPACK streams.
            let uni_conn = conn.clone();
            let uni_drain = AbortOnDrop(tokio::spawn(async move {
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
            }));

            // 2. Send HTTP/3 POST /auth with `hysteria-auth` header.
            let (mut auth_send, mut auth_recv) =
                conn.open_bi().await.context("打开 Hy2 鉴权流失败")?;
            write_h3_auth_request(&mut auth_send, token, "game-accel-hy2-client")
                .await
                .context("发送 Hy2 鉴权请求失败")?;
            let _ = auth_send.finish();

            let headers = read_h3_headers(&mut auth_recv)
                .await
                .context("读取 Hy2 鉴权响应失败")?;
            let status = header_val(&headers, ":status").unwrap_or("unknown");
            if status != "233" {
                conn.close(0u32.into(), b"auth_failed");
                return Err(ConnectError::AuthRejected {
                    detail: format!("服务端拒绝鉴权 (Hy2 HTTP/3 status={status})"),
                }
                .into());
            }
            Ok(AuthedConn {
                conn,
                _h3_ctrl: Some(ctrl_send),
                _h3_uni_drain: Some(uni_drain),
            })
        }
    }
}

/// Probe a node with a real QUIC + token authentication handshake (4s timeout)
/// and return the measured QUIC RTT in milliseconds. Supports `IP:port` and `domain:port`.
pub async fn probe_node_rtt(
    server: &str,
    server_name: &str,
    token: &str,
    insecure: bool,
    protocol: NodeProtocol,
) -> Result<u64> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client_config = build_client_config(insecure, true, 0)?;

    let fut = async {
        let server_addr = resolve_server_addr(server).await?;
        let mut endpoint = Endpoint::client(unspecified_bind_for(server_addr))?;
        endpoint.set_default_client_config(client_config);
        let authed = connect_and_auth(&endpoint, server_addr, server_name, token, protocol).await?;
        let rtt_ms = authed.conn.rtt().as_millis() as u64;
        authed.conn.close(0u32.into(), b"probe_done");
        endpoint.close(0u32.into(), b"probe_done");
        Ok::<u64, anyhow::Error>(rtt_ms)
    };

    tokio::time::timeout(Duration::from_secs(4), fut)
        .await
        .context("测速超时 (>4s)")?
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{ALPN_PROTOCOL, DATAGRAM_RECV_BUFFER};
    use quinn::TransportConfig;
    use rustls::pki_types::CertificateDer;

    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_during_handshake_returns_without_blocking_sender() {
        let stats = Arc::new(ClientStats::default());
        let (tx, rx) = watch::channel(false);
        let opts = ClientOptions {
            primary: UpstreamOptions {
                server: "127.0.0.1:9".to_string(),
                server_name: "localhost".to_string(),
                token: "t".to_string(),
                protocol: NodeProtocol::Gacl,
                game_dup: false,
                insecure: true,
                brutal_mbps: 0,
            },
            secondary: None,
            domestic: DomesticEgress::Direct,
            listen: "127.0.0.1:0".parse().unwrap(),
            enable_tun: false,
            route_mode: RouteModeCell::new(crate::rules::RouteMode::Rule),
            rules_dir: None,
        };
        let client = tokio::spawn(run_client_with_shutdown(opts, stats, None, rx));
        tokio::time::sleep(Duration::from_millis(300)).await;

        // An external shutdown during handshake must unblock the client promptly
        // (regression guard for the stop path: sender never blocks, client exits).
        let sent = tokio::time::timeout(
            Duration::from_secs(3),
            tokio::task::spawn_blocking(move || tx.send(true)),
        )
        .await;
        assert!(
            sent.expect("send(true) blocked").is_ok(),
            "no receiver left"
        );

        let exited = tokio::time::timeout(Duration::from_secs(3), client).await;
        assert!(
            matches!(exited, Ok(Ok(Ok(())))),
            "client did not exit after shutdown: {exited:?}"
        );
    }

    #[tokio::test]
    async fn server_addr_accepts_ip_ipv6_and_domain() {
        validate_server_addr("203.0.113.10:8443").unwrap();
        validate_server_addr("[2001:db8::1]:8443").unwrap();
        validate_server_addr("hk1.example.com:8443").unwrap();
        assert!(validate_server_addr("missing-port.example.com").is_err());
        assert!(validate_server_addr("example.com:0").is_err());

        let resolved = resolve_server_addr("localhost:8443").await.unwrap();
        assert_eq!(resolved.port(), 8443);
        assert!(resolved.ip().is_loopback());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hy2_client_end_to_end_auth_tcp_and_udp_loopback() {
        use protocol::hy2::{
            HY2_TCP_STREAM_TYPE, decode_hy2_udp_message, encode_hy2_udp_datagrams, read_varint,
            write_h3_response,
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let _ = rustls::crypto::ring::default_provider().install_default();

        // 1. Generate self-signed cert for local QUIC Hysteria2 test server
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert_der = CertificateDer::from(cert.cert.der().to_vec());
        let key_der = rustls::pki_types::PrivateKeyDer::Pkcs8(
            rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()),
        );
        let mut tls_srv = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)
            .unwrap();
        tls_srv.alpn_protocols = vec![ALPN_PROTOCOL.to_vec()];
        let quic_srv = quinn::crypto::rustls::QuicServerConfig::try_from(tls_srv).unwrap();
        let mut srv_cfg = quinn::ServerConfig::with_crypto(Arc::new(quic_srv));
        let mut transport = TransportConfig::default();
        transport.datagram_receive_buffer_size(Some(DATAGRAM_RECV_BUFFER));
        srv_cfg.transport_config(Arc::new(transport));

        let srv_ep =
            Endpoint::server(srv_cfg, "127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let srv_addr = srv_ep.local_addr().unwrap();

        // Spawn minimal Hysteria2 server handling Auth, 0x401 TCP echo, and UDP echo
        let srv_task = tokio::spawn(async move {
            while let Some(incoming) = srv_ep.accept().await {
                tokio::spawn(async move {
                    let Ok(conn) = incoming.await else { return };
                    let _ctrl = conn.open_uni().await.ok().map(|mut s| {
                        let bytes = encode_h3_control_stream();
                        tokio::spawn(async move {
                            let _ = s.write_all(&bytes).await;
                            std::future::pending::<()>().await;
                        })
                    });
                    let Ok((mut send, mut recv)) = conn.accept_bi().await else {
                        return;
                    };
                    let Ok(headers) = read_h3_headers(&mut recv).await else {
                        return;
                    };
                    let ok = header_val(&headers, ":method") == Some("POST")
                        && header_val(&headers, ":path") == Some("/auth")
                        && header_val(&headers, "hysteria-auth") == Some("hy2-test-token");
                    if !ok {
                        let _ = write_h3_response(&mut send, "404", &[]).await;
                        let _ = send.finish();
                        return;
                    }
                    let _ = write_h3_response(
                        &mut send,
                        "233",
                        &[("hysteria-udp", "true"), ("hysteria-cc-rx", "auto")],
                    )
                    .await;
                    let _ = send.finish();

                    // UDP echo loop
                    let udp_conn = conn.clone();
                    let udp_task = tokio::spawn(async move {
                        while let Ok(raw) = udp_conn.read_datagram().await {
                            if let Ok(msg) = decode_hy2_udp_message(&raw) {
                                let pkts = encode_hy2_udp_datagrams(
                                    msg.session_id,
                                    0, // intentionally 0 to verify client doesn't dedup-drop Hy2 replies
                                    &msg.addr,
                                    msg.data,
                                    1150,
                                );
                                for p in pkts {
                                    let _ = udp_conn.send_datagram(p);
                                }
                            }
                        }
                    });

                    // TCP 0x401 echo loop
                    while let Ok((mut ts, mut tr)) = conn.accept_bi().await {
                        tokio::spawn(async move {
                            if read_varint(&mut tr).await.ok() != Some(HY2_TCP_STREAM_TYPE) {
                                return;
                            }
                            let Ok(alen) = read_varint(&mut tr).await else {
                                return;
                            };
                            let mut abuf = vec![0u8; alen as usize];
                            if tr.read_exact(&mut abuf).await.is_err() {
                                return;
                            }
                            let Ok(plen) = read_varint(&mut tr).await else {
                                return;
                            };
                            if plen > 0 {
                                let mut pbuf = vec![0u8; plen as usize];
                                if tr.read_exact(&mut pbuf).await.is_err() {
                                    return;
                                }
                            }
                            // TCPResponse OK: status=0, msg_len=0, pad_len=0
                            if ts.write_all(&[0x00, 0x00, 0x00]).await.is_err() {
                                return;
                            }
                            let mut buf = [0u8; 1024];
                            while let Ok(Some(n)) = tr.read(&mut buf).await {
                                if n == 0 || ts.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                            let _ = ts.finish();
                        });
                    }
                    udp_task.abort();
                });
            }
        });

        // 2. Probe valid & invalid tokens over Hysteria2
        let srv_str = srv_addr.to_string();
        probe_node_rtt(
            &srv_str,
            "localhost",
            "hy2-test-token",
            true,
            NodeProtocol::Hysteria2,
        )
        .await
        .expect("valid hy2 probe should succeed");
        let bad_err = probe_node_rtt(
            &srv_str,
            "localhost",
            "wrong-token",
            true,
            NodeProtocol::Hysteria2,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(classify(&bad_err), ConnectError::AuthRejected { .. }),
            "expected auth rejection, got: {bad_err:#}"
        );

        // 3. Start full client in Hysteria2 + Global mode and test SOCKS5 TCP + UDP echo
        let free_port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let (shut_tx, shut_rx) = watch::channel(false);
        let stats = Arc::new(ClientStats::default());
        let opts = ClientOptions {
            primary: UpstreamOptions {
                server: srv_str,
                server_name: "localhost".to_string(),
                token: "hy2-test-token".to_string(),
                protocol: NodeProtocol::Hysteria2,
                game_dup: false,
                insecure: true,
                brutal_mbps: 0,
            },
            secondary: None,
            domestic: DomesticEgress::Direct,
            listen: free_port,
            enable_tun: false,
            route_mode: RouteModeCell::new(crate::rules::RouteMode::Global),
            rules_dir: None,
        };
        let client_stats = stats.clone();
        let client_task = tokio::spawn(run_client_with_shutdown(opts, client_stats, None, shut_rx));

        // Wait until connected & listening
        for _ in 0..40 {
            if stats.connected.load(Ordering::Relaxed)
                && tokio::net::TcpStream::connect(free_port).await.is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(stats.connected.load(Ordering::Relaxed));

        // 3a. SOCKS5 TCP CONNECT -> echo through Hysteria2 0x401 stream
        let mut s5 = tokio::net::TcpStream::connect(free_port).await.unwrap();
        s5.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut greet = [0u8; 2];
        s5.read_exact(&mut greet).await.unwrap();
        assert_eq!(greet, [0x05, 0x00]);
        // CONNECT 1.1.1.1:80
        s5.write_all(&[0x05, 0x01, 0x00, 0x01, 1, 1, 1, 1, 0, 80])
            .await
            .unwrap();
        let mut rep = [0u8; 10];
        s5.read_exact(&mut rep).await.unwrap();
        assert_eq!(rep[1], 0x00);
        s5.write_all(b"hello-hy2-tcp").await.unwrap();
        let mut echo = [0u8; 13];
        s5.read_exact(&mut echo).await.unwrap();
        assert_eq!(&echo, b"hello-hy2-tcp");
        drop(s5);

        // 3b. SOCKS5 UDP ASSOCIATE -> send TWO packets (server replies with packet_id=0 for both)
        let mut ctrl = tokio::net::TcpStream::connect(free_port).await.unwrap();
        ctrl.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        ctrl.read_exact(&mut greet).await.unwrap();
        ctrl.write_all(&[0x05, 0x03, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await
            .unwrap();
        ctrl.read_exact(&mut rep).await.unwrap();
        assert_eq!(rep[1], 0x00);
        let udp_relay = SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(rep[4], rep[5], rep[6], rep[7])),
            u16::from_be_bytes([rep[8], rep[9]]),
        );
        let local_udp = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        for msg in [b"hy2-udp-1", b"hy2-udp-2"] {
            let mut pkt = vec![0x00, 0x00, 0x00, 0x01, 8, 8, 8, 8, 0, 53];
            pkt.extend_from_slice(msg);
            local_udp.send_to(&pkt, udp_relay).await.unwrap();
            let mut rbuf = [0u8; 256];
            let (n, _) =
                tokio::time::timeout(Duration::from_secs(2), local_udp.recv_from(&mut rbuf))
                    .await
                    .expect("udp echo timeout")
                    .unwrap();
            assert_eq!(&rbuf[10..n], msg);
        }

        let _ = shut_tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(3), client_task).await;
        srv_task.abort();
    }
}
