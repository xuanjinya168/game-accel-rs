pub mod socks5;
pub mod tun_mode;

use crate::socks5::{ClientStats, UdpDispatchMap, run_mixed_proxy_server};
use anyhow::{Context, Result, bail};
use dashmap::DashMap;
use protocol::auth::{AUTH_OK, write_auth_request};
use protocol::frame::UdpFrame;
use protocol::pacer::UdpDupPacer;
use protocol::{ALPN_PROTOCOL, DEFAULT_INITIAL_MTU};
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
    pub server: SocketAddr,
    pub server_name: String,
    pub listen: SocketAddr,
    pub token: String,
    pub game_dup: bool,
    pub enable_tun: bool,
    pub insecure: bool,
}

pub async fn run_client_with_shutdown(
    opts: ClientOptions,
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

    let _ = rustls::crypto::ring::default_provider().install_default();
    let client_config = build_client_config(opts.insecure)?;
    let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(client_config);

    emit(format!(
        "正在连接 quic://{} (SNI={})...",
        opts.server, opts.server_name
    ));

    // Handshake + Auth with 8s per-attempt timeout, instant cancel via shutdown_rx,
    // and 1 automatic retry on transient UDP handshake packet loss.
    const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(8);
    const MAX_ATTEMPTS: usize = 2;

    let mut connected_conn = None;
    let mut last_err = None;

    for attempt in 1..=MAX_ATTEMPTS {
        if *shutdown_rx.borrow() {
            endpoint.close(0u32.into(), b"cancelled");
            return Ok(());
        }
        if attempt > 1 {
            emit(format!("首次握手/鉴权超时，正在自动重试 ({attempt}/{MAX_ATTEMPTS})..."));
        }

        let attempt_fut = async {
            let conn = endpoint
                .connect(opts.server, &opts.server_name)?
                .await
                .context("QUIC 握手失败")?;
            let (mut auth_send, mut auth_recv) =
                conn.open_bi().await.context("打开鉴权流失败")?;
            write_auth_request(&mut auth_send, &opts.token)
                .await
                .context("发送鉴权请求失败")?;
            let _ = auth_send.finish();
            let resp = auth_recv.read_u8().await.context("读取鉴权响应失败")?;
            if resp != AUTH_OK {
                conn.close(0u32.into(), b"auth_failed");
                bail!("服务端拒绝鉴权 (code={resp})");
            }
            Ok::<quinn::Connection, anyhow::Error>(conn)
        };

        let outcome = tokio::select! {
            _ = shutdown_rx.changed() => {
                endpoint.close(0u32.into(), b"cancelled");
                return Ok(());
            }
            res = tokio::time::timeout(HANDSHAKE_TIMEOUT, attempt_fut) => res,
        };

        match outcome {
            Ok(Ok(conn)) => {
                connected_conn = Some(conn);
                break;
            }
            Ok(Err(e)) => {
                let msg = format!("{e:#}");
                // Do not retry explicit server auth rejection
                if msg.contains("服务端拒绝鉴权") {
                    endpoint.close(0u32.into(), b"auth_failed");
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

    let Some(conn) = connected_conn else {
        endpoint.close(0u32.into(), b"connect_failed");
        return Err(last_err.unwrap_or_else(|| anyhow::anyhow!("连接失败")));
    };

    if *shutdown_rx.borrow() {
        conn.close(0u32.into(), b"cancelled");
        endpoint.close(0u32.into(), b"cancelled");
        return Ok(());
    }

    stats.connected.store(true, Ordering::Relaxed);
    stats
        .rtt_ms
        .store(conn.rtt().as_millis() as u64, Ordering::Relaxed);
    emit(format!(
        "已连接并认证成功 ({}，初始 RTT {} ms)",
        opts.server,
        conn.rtt().as_millis()
    ));

    // Periodic RTT updater
    let rtt_conn = conn.clone();
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

    // Background QUIC Datagram receiver -> dispatch by flow_id
    let udp_dispatch: UdpDispatchMap = Arc::new(DashMap::new());
    let dispatch_clone = udp_dispatch.clone();
    let dgram_conn = conn.clone();
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

    // Single shared connection-scoped UDP duplicate pacer (zero per-packet task spawning)
    let pacer = UdpDupPacer::spawn(conn.clone());

    // Optional TUN virtual NIC mode (runs alongside the local mixed proxy)
    let tun_handle = if opts.enable_tun {
        let server_ip = opts.server.ip();
        let tun_conn = conn.clone();
        let tun_pacer = pacer.clone();
        let tun_dispatch = udp_dispatch.clone();
        let tun_stats = stats.clone();
        let tun_log_tx = log_tx.clone();
        let tun_shutdown = shutdown_rx.clone();
        let game_dup = opts.game_dup;
        Some(tokio::spawn(async move {
            tun_mode::run_tun_mode(
                server_ip,
                tun_conn,
                tun_pacer,
                tun_dispatch,
                game_dup,
                tun_stats,
                tun_log_tx,
                tun_shutdown,
            )
            .await
        }))
    } else {
        None
    };

    let res = run_mixed_proxy_server(
        opts.listen,
        conn.clone(),
        pacer,
        udp_dispatch,
        opts.game_dup,
        stats.clone(),
        log_tx.clone(),
        shutdown_rx.clone(),
    )
    .await;

    if let Some(handle) = tun_handle {
        if let Ok(Err(e)) = handle.await {
            emit(format!("TUN 模式异常: {e:#}"));
        }
    }

    rtt_task.abort();
    dgram_task.abort();
    conn.close(0u32.into(), b"client_shutdown");
    endpoint.close(0u32.into(), b"client_shutdown");
    stats.reset();
    emit("连接已断开".to_string());
    res
}

/// Probe a node with a real QUIC + token authentication handshake (4s timeout)
/// and return the measured QUIC RTT in milliseconds.
pub async fn probe_node_rtt(
    server: SocketAddr,
    server_name: &str,
    token: &str,
    insecure: bool,
) -> Result<u64> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client_config = build_client_config(insecure)?;
    let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(client_config);

    let fut = async {
        let conn = endpoint
            .connect(server, server_name)?
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

fn build_client_config(insecure: bool) -> Result<ClientConfig> {
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
    transport.datagram_receive_buffer_size(Some(2 * 1024 * 1024));
    transport.datagram_send_buffer_size(2 * 1024 * 1024);
    transport.keep_alive_interval(Some(Duration::from_secs(10)));
    transport.max_idle_timeout(Some(Duration::from_secs(60).try_into()?));
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
