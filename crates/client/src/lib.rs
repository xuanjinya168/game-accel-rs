pub mod direct;
pub(crate) mod dns_msg;
pub mod rules;
pub mod socks5;
pub mod tun_mode;

use crate::rules::{RouteModeCell, Router, RuleData};
use crate::socks5::{ClientStats, UdpDispatchMap, run_mixed_proxy_server};
use anyhow::{Context, Result, bail};
use dashmap::DashMap;
use protocol::auth::{AUTH_OK, write_auth_request};
use protocol::frame::UdpFrame;
use protocol::pacer::UdpDupPacer;
use protocol::{
    ALPN_PROTOCOL, DATAGRAM_RECV_BUFFER, DATAGRAM_SEND_BUFFER, DEFAULT_INITIAL_MTU,
    KEEP_ALIVE_INTERVAL, MAX_IDLE_TIMEOUT,
};
use quinn::crypto::rustls::QuicClientConfig;
use quinn::{ClientConfig, Endpoint, TransportConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

#[derive(Debug, Clone)]
pub struct ClientOptions {
    /// Server `IP:port`, `[IPv6]:port`, or `domain:port`.
    pub server: String,
    pub server_name: String,
    pub listen: SocketAddr,
    pub token: String,
    pub game_dup: bool,
    pub enable_tun: bool,
    pub insecure: bool,
    /// Split-routing mode, shared live with the UI (applies to new connections).
    pub route_mode: Arc<RouteModeCell>,
    /// Optional directory whose `direct-list.txt` / `proxy-list.txt` / `geoip-cn.txt`
    /// override the embedded rule data.
    pub rules_dir: Option<std::path::PathBuf>,
}

/// Fast synchronous syntax validation for `host:port` / `ip:port` / `[ipv6]:port`.
pub fn validate_server_addr(raw: &str) -> Result<()> {
    let s = raw.trim();
    if s.parse::<SocketAddr>().is_ok() {
        return Ok(());
    }
    let (host, port_str) = if let Some(rest) = s.strip_prefix('[') {
        let (h, after) = rest.split_once(']').context("IPv6 地址缺少 ']'")?;
        let p = after.strip_prefix(':').context("地址缺少 ':端口'")?;
        (h.trim(), p.trim())
    } else {
        let (h, p) = s.rsplit_once(':').context("地址缺少 ':端口'")?;
        (h.trim(), p.trim())
    };
    if host.is_empty() || host.contains(char::is_whitespace) {
        bail!("服务器主机名无效");
    }
    let port: u16 = port_str.parse().context("端口号无效")?;
    if port == 0 {
        bail!("端口号不能为 0");
    }
    Ok(())
}

/// Resolve server `IP:port` or `domain:port` (prefers IPv4 first so TUN route pinning works
/// on dual-stack domains, falling back to IPv6 when IPv6-only).
pub async fn resolve_server_addr(raw: &str) -> Result<SocketAddr> {
    let s = raw.trim();
    if let Ok(addr) = s.parse::<SocketAddr>() {
        return Ok(addr);
    }
    validate_server_addr(s)?;
    let resolved: Vec<SocketAddr> = tokio::time::timeout(
        Duration::from_secs(4),
        tokio::net::lookup_host(s),
    )
    .await
    .with_context(|| format!("解析节点域名超时: {s}"))?
    .with_context(|| format!("解析节点域名失败: {s}"))?
    .collect();
    resolved
        .iter()
        .copied()
        .find(SocketAddr::is_ipv4)
        .or_else(|| resolved.into_iter().next())
        .with_context(|| format!("节点域名无可用 IP: {s}"))
}

/// Markers inside connect error messages that mean retrying cannot help.
/// Callers (GUI reconnect policy) match on these instead of duplicating the wording.
pub const ERR_AUTH_REJECTED: &str = "服务端拒绝鉴权";
pub const ERR_PORT_IN_USE: &str = "被占用，无法监听";

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
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
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
    let server_addr = tokio::select! {
        _ = shutdown_rx.changed() => return Ok(()),
        res = resolve_server_addr(&opts.server) => res?,
    };

    // Dual-lane QUIC architecture:
    // - `tcp_ep` / `tcp_conn`: carries bulk TCP streams (web, video, downloads)
    // - `udp_ep` / `udp_conn`: carries ONLY game/voice UDP datagrams on its own socket & cwnd,
    //   preventing TCP downloads or BBR ProbeRTT from bloating game ping or falsely triggering dual-send.
    let tcp_config = build_client_config(opts.insecure, false)?;
    let udp_config = build_client_config(opts.insecure, true)?;

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

    emit(format!(
        "正在建立双通道连接 quic://{} -> {} (SNI={}，TCP/UDP 拥塞隔离)...",
        opts.server, server_addr, opts.server_name
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
            return Ok(());
        }
        if attempt > 1 {
            emit(format!("首次握手/鉴权超时，正在自动重试 ({attempt}/{MAX_ATTEMPTS})..."));
        }

        let attempt_fut = async {
            let (tcp_conn, udp_conn) = tokio::try_join!(
                connect_and_auth(&tcp_endpoint, server_addr, &opts.server_name, &opts.token),
                connect_and_auth(&udp_endpoint, server_addr, &opts.server_name, &opts.token),
            )?;
            Ok::<(quinn::Connection, quinn::Connection), anyhow::Error>((tcp_conn, udp_conn))
        };

        let outcome = tokio::select! {
            _ = shutdown_rx.changed() => {
                close_endpoints(&tcp_endpoint, &udp_endpoint, b"cancelled");
                return Ok(());
            }
            res = tokio::time::timeout(HANDSHAKE_TIMEOUT, attempt_fut) => res,
        };

        match outcome {
            Ok(Ok(pair)) => {
                connected_pair = Some(pair);
                break;
            }
            Ok(Err(e)) => {
                let msg = format!("{e:#}");
                // Do not retry explicit server auth rejection
                if msg.contains(ERR_AUTH_REJECTED) {
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

    let Some((tcp_conn, udp_conn)) = connected_pair else {
        close_endpoints(&tcp_endpoint, &udp_endpoint, b"connect_failed");
        return Err(last_err.unwrap_or_else(|| anyhow::anyhow!("连接失败")));
    };

    if *shutdown_rx.borrow() {
        tcp_conn.close(0u32.into(), b"cancelled");
        udp_conn.close(0u32.into(), b"cancelled");
        close_endpoints(&tcp_endpoint, &udp_endpoint, b"cancelled");
        return Ok(());
    }

    stats.connected.store(true, Ordering::Relaxed);
    stats
        .rtt_ms
        .store(udp_conn.rtt().as_millis() as u64, Ordering::Relaxed);
    emit(format!(
        "双通道已连接并认证成功 ({}，游戏 UDP 通道初始 RTT {} ms)",
        server_addr,
        udp_conn.rtt().as_millis()
    ));

    // Watchdog: if EITHER lane dies (idle timeout / server restart / network change),
    // tear the session down so both lanes reconnect cleanly together.
    let watchdog_tx = shutdown_tx.clone();
    let wd_tcp = tcp_conn.clone();
    let wd_udp = udp_conn.clone();
    let _watchdog_guard = AbortOnDrop(tokio::spawn(async move {
        tokio::select! {
            _ = wd_tcp.closed() => {}
            _ = wd_udp.closed() => {}
        }
        let _ = watchdog_tx.send(true);
    }));

    // Periodic RTT updater: tracks the dedicated unbloated UDP game lane
    let rtt_conn = udp_conn.clone();
    let rtt_stats = stats.clone();
    let rtt_task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            if rtt_conn.close_reason().is_some() {
                break;
            }
            rtt_stats
                .rtt_ms
                .store(rtt_conn.rtt().as_millis() as u64, Ordering::Relaxed);
        }
    });

    // Background QUIC Datagram receiver on dedicated UDP lane -> dispatch by flow_id
    let udp_dispatch: UdpDispatchMap = Arc::new(DashMap::new());
    let dispatch_clone = udp_dispatch.clone();
    let dgram_conn = udp_conn.clone();
    let dgram_task = tokio::spawn(async move {
        loop {
            match dgram_conn.read_datagram().await {
                Ok(raw) => match UdpFrame::decode(raw) {
                    Ok(frame) => {
                        if let Some(tx) = dispatch_clone.get(&frame.flow_id) {
                            let _ = tx.try_send(frame);
                        }
                    }
                    Err(e) => warn!("invalid udp frame from server: {:#}", e),
                },
                Err(_) => break,
            }
        }
    });

    // Dedicated UDP duplicate pacer: samples loss ONLY on `udp_conn`, immune to TCP BBR loss
    let pacer = UdpDupPacer::spawn(udp_conn.clone());

    // Split-routing tables (~140k entries) are parsed once per process, off the runtime.
    // Built before TUN starts: the mixed proxy and the TUN share one router + dialer.
    let rules_dir = opts.rules_dir.clone();
    let rule_data = tokio::task::spawn_blocking(move || RuleData::shared(rules_dir.as_deref()))
        .await
        .context("加载分流规则失败")?;
    emit(format!(
        "分流模式：{}（{}）",
        opts.route_mode.get().label(),
        rule_data.summary(),
    ));
    let router = Arc::new(Router::new(opts.route_mode.clone(), rule_data));

    // Optional TUN virtual NIC mode (runs alongside the local mixed proxy)
    let tun_handle = if opts.enable_tun {
        let server_ip = server_addr.ip();
        let tun_tcp_conn = tcp_conn.clone();
        let tun_udp_conn = udp_conn.clone();
        let tun_pacer = pacer.clone();
        let tun_dispatch = udp_dispatch.clone();
        let tun_stats = stats.clone();
        let tun_log_tx = log_tx.clone();
        let tun_shutdown = shutdown_rx.clone();
        let tun_router = router.clone();
        let game_dup = opts.game_dup;
        let tun_err_tx = log_tx.clone();
        Some(tokio::spawn(async move {
            let res = tun_mode::run_tun_mode(
                server_ip,
                tun_tcp_conn,
                tun_udp_conn,
                tun_pacer,
                tun_dispatch,
                game_dup,
                tun_stats,
                tun_router,
                tun_log_tx,
                tun_shutdown,
            )
            .await;
            // Report right away: this handle is only joined when the whole session ends.
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
        tcp_conn.clone(),
        udp_conn.clone(),
        pacer,
        udp_dispatch,
        opts.game_dup,
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
    dgram_task.abort();
    tcp_conn.close(0u32.into(), b"client_shutdown");
    udp_conn.close(0u32.into(), b"client_shutdown");
    close_endpoints(&tcp_endpoint, &udp_endpoint, b"client_shutdown");
    stats.reset();
    emit("连接已断开".to_string());
    res
}

async fn connect_and_auth(
    endpoint: &Endpoint,
    server: SocketAddr,
    server_name: &str,
    token: &str,
) -> Result<quinn::Connection> {
    let conn = endpoint
        .connect(server, server_name)?
        .await
        .context("QUIC 握手失败")?;
    let (mut auth_send, mut auth_recv) = conn.open_bi().await.context("打开鉴权流失败")?;
    write_auth_request(&mut auth_send, token)
        .await
        .context("发送鉴权请求失败")?;
    let _ = auth_send.finish();
    let resp = auth_recv.read_u8().await.context("读取鉴权响应失败")?;
    if resp != AUTH_OK {
        conn.close(0u32.into(), b"auth_failed");
        bail!("{ERR_AUTH_REJECTED} (code={resp})");
    }
    Ok(conn)
}

/// Probe a node with a real QUIC + token authentication handshake (4s timeout)
/// and return the measured QUIC RTT in milliseconds. Supports `IP:port` and `domain:port`.
pub async fn probe_node_rtt(
    server: &str,
    server_name: &str,
    token: &str,
    insecure: bool,
) -> Result<u64> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client_config = build_client_config(insecure, true)?;

    let fut = async {
        let server_addr = resolve_server_addr(server).await?;
        let mut endpoint = Endpoint::client(unspecified_bind_for(server_addr))?;
        endpoint.set_default_client_config(client_config);
        let conn = endpoint
            .connect(server_addr, server_name)?
            .await
            .context("QUIC handshake failed")?;
        let (mut send, mut recv) = conn.open_bi().await?;
        write_auth_request(&mut send, token).await?;
        let _ = send.finish();
        let resp = recv.read_u8().await.context("read auth response")?;
        let rtt_ms = conn.rtt().as_millis() as u64;
        conn.close(0u32.into(), b"probe_done");
        endpoint.close(0u32.into(), b"probe_done");
        if resp != AUTH_OK {
            bail!("鉴权失败 (code={resp})");
        }
        Ok::<u64, anyhow::Error>(rtt_ms)
    };

    tokio::time::timeout(Duration::from_secs(4), fut)
        .await
        .context("测速超时 (>4s)")?
}

/// Local wildcard bind address matching the server's address family
/// (an IPv4-only socket cannot reach an IPv6 node and vice versa).
fn unspecified_bind_for(server: SocketAddr) -> SocketAddr {
    if server.is_ipv6() {
        SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 0))
    } else {
        SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, 0))
    }
}

fn bind_tuned_udp_socket(addr: SocketAddr, buf_bytes: usize) -> Result<std::net::UdpSocket> {
    let domain = if addr.is_ipv6() {
        socket2::Domain::IPV6
    } else {
        socket2::Domain::IPV4
    };
    let sock = socket2::Socket::new(domain, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))
        .context("create UDP socket")?;
    // Raise OS UDP socket buffers (Windows defaults to ~64 KB which overflows under high-BDP QUIC)
    let _ = sock.set_recv_buffer_size(buf_bytes);
    let _ = sock.set_send_buffer_size(buf_bytes);
    sock.bind(&addr.into()).context("bind UDP socket")?;
    Ok(sock.into())
}

fn build_client_config(insecure: bool, is_udp_lane: bool) -> Result<ClientConfig> {
    let mut tls_config = if insecure {
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
            .with_no_client_auth()
    } else {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth()
    };
    tls_config.alpn_protocols = vec![ALPN_PROTOCOL.to_vec()];

    let quic_crypto = QuicClientConfig::try_from(tls_config)?;
    let mut client_config = ClientConfig::new(Arc::new(quic_crypto));

    let mut transport = TransportConfig::default();
    transport.initial_mtu(DEFAULT_INITIAL_MTU);
    transport.mtu_discovery_config(Some(quinn::MtuDiscoveryConfig::default()));
    if is_udp_lane {
        // Dedicated game UDP datagram lane: minimal stream overhead, bounded 128 KB send buffer
        transport.max_concurrent_bidi_streams(quinn::VarInt::from_u32(4));
        transport.datagram_receive_buffer_size(Some(DATAGRAM_RECV_BUFFER));
        transport.datagram_send_buffer_size(DATAGRAM_SEND_BUFFER);
    } else {
        // Dedicated bulk TCP stream lane: large BDP windows, high stream concurrency
        transport.stream_receive_window(quinn::VarInt::from_u32(16 * 1024 * 1024));
        transport.receive_window(quinn::VarInt::from_u32(64 * 1024 * 1024));
        transport.send_window(32 * 1024 * 1024);
        transport.max_concurrent_bidi_streams(quinn::VarInt::from_u32(1024));
        transport.datagram_receive_buffer_size(None);
    }
    transport.keep_alive_interval(Some(KEEP_ALIVE_INTERVAL));
    transport.max_idle_timeout(Some(MAX_IDLE_TIMEOUT.try_into()?));
    transport.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));

    client_config.transport_config(Arc::new(transport));
    Ok(client_config)
}

#[derive(Debug)]
struct SkipServerVerification;

impl ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ED25519,
            SignatureScheme::RSA_PSS_SHA256,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_during_handshake_returns_without_blocking_sender() {
        let stats = Arc::new(ClientStats::default());
        let (tx, rx) = watch::channel(false);
        let opts = ClientOptions {
            server: "127.0.0.1:9".to_string(),
            server_name: "localhost".to_string(),
            listen: "127.0.0.1:0".parse().unwrap(),
            token: "t".to_string(),
            game_dup: false,
            enable_tun: false,
            insecure: true,
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
        assert!(sent.expect("send(true) blocked").is_ok(), "no receiver left");

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
}
