//! Core QUIC/UDP & HTTP/3 Hysteria2 server library and Web Admin Panel boundary.
//!
//! Structured per *Zero To Production in Rust* (Palmieri) and *Clean Architecture* (Martin):
//! `lib.rs` exposes cohesive server orchestration and domain services for black-box testing,
//! while `main.rs` remains a thin CLI configuration adapter.

pub mod audit_log;
pub mod auth_guard;
pub mod crypto;
pub(crate) mod dns;
mod hy2;
pub(crate) mod masquerade;
pub(crate) mod nat;
pub mod panel;
pub(crate) mod quic;
pub mod store;

use crate::nat::FullConeNat;
use crate::quic::{bind_tuned_udp_socket, build_server_config};
use crate::store::{AppState, InitParams, LiveCounters};
use anyhow::{Context, Result};
use protocol::auth::{AUTH_ERR, AUTH_MAGIC, AUTH_OK, read_auth_request_after_magic};
use protocol::domain::{AccessLogRecord, AccessOutcome};
use protocol::frame::{ATYP_DOMAIN, TargetAddr};
use protocol::pacer::UdpDupPacer;
use protocol::sniff::{extract_root_domain, sniff_first_packet};
use quinn::Endpoint;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tracing::{error, info, warn};

const INTERNAL_PANEL_HOST: &str = "panel.gacl.local";
const INTERNAL_PANEL_TUN_IP: IpAddr = IpAddr::V4(Ipv4Addr::new(198, 18, 0, 1));
pub(crate) const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Validated runtime options passed from the CLI binary into the server engine.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    pub listen: SocketAddr,
    pub panel_listen: String,
    pub data_file: PathBuf,
    pub token: String,
    pub panel_pass: Option<String>,
    pub public_host: String,
    pub sni: String,
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
    pub dev_domain: String,
    pub masquerade_dir: Option<PathBuf>,
    pub brutal_mbps: u32,
}

/// Maps authenticated tunnel-only panel aliases (`panel.gacl.local`, `198.18.0.1`,
/// or `public_host:panel_port` / `sni:panel_port`) to the loopback Web Admin Panel
/// so port 9090 never needs to be exposed on the public internet.
#[derive(Clone)]
pub(crate) struct PanelRoute {
    pub(crate) local_addr: SocketAddr,
    pub(crate) state: Arc<AppState>,
}

impl PanelRoute {
    pub(crate) fn resolve_target(&self, target: &TargetAddr) -> Option<SocketAddr> {
        let panel_port = self.local_addr.port();
        match target {
            TargetAddr::Domain(host, port) => {
                if host.eq_ignore_ascii_case(INTERNAL_PANEL_HOST)
                    && (*port == 80 || *port == panel_port)
                {
                    return Some(self.local_addr);
                }
                if *port == panel_port {
                    let (sni, public_host) = self.state.panel_aliases();
                    if host.eq_ignore_ascii_case(&sni) || host.eq_ignore_ascii_case(&public_host) {
                        return Some(self.local_addr);
                    }
                }
                None
            }
            TargetAddr::Ip(addr) => {
                if addr.ip() == INTERNAL_PANEL_TUN_IP
                    && (addr.port() == 80 || addr.port() == panel_port)
                {
                    return Some(self.local_addr);
                }
                if addr.port() == panel_port
                    && self.state.panel_aliases().1 == addr.ip().to_string()
                {
                    return Some(self.local_addr);
                }
                None
            }
        }
    }
}

/// Connection-scoped authenticated identity & accounting context shared across all
/// TCP streams and UDP NAT sessions of a single client connection.
pub(crate) struct StreamContext {
    pub(crate) counters: Arc<LiveCounters>,
    pub(crate) state: Arc<AppState>,
    pub(crate) link_id: String,
    pub(crate) link_name: String,
    pub(crate) client_ip: String,
    pub(crate) panel_route: Option<Arc<PanelRoute>>,
}

/// Per-stream target metadata used by the shared zero-stall TCP splice & audit helper.
pub(crate) struct TcpAuditMeta {
    pub(crate) host_name: String,
    pub(crate) port: u16,
    pub(crate) was_ip: bool,
    pub(crate) is_internal_panel: bool,
    pub(crate) started: Instant,
}

/// Shared zero-stall bidirectional TCP splice and first-packet protocol/SNI sniffer
/// used by both `GACL` and `Hysteria2` streams.
pub(crate) async fn splice_and_audit_tcp(
    mut quic_send: quinn::SendStream,
    mut quic_recv: quinn::RecvStream,
    tcp: tokio::net::TcpStream,
    meta: TcpAuditMeta,
    ctx: &StreamContext,
) -> Result<()> {
    let (mut tcp_read, mut tcp_write) = tcp.into_split();

    // Start reverse pipe (server -> client) IMMEDIATELY so server-first protocols
    // (SSH, MySQL, game banners) suffer ZERO sniff delay, counting bytes per chunk in real time.
    let c_rx = ctx.counters.clone();
    let s2c = tokio::spawn(async move {
        let n_total =
            protocol::io::copy_with_counter(&mut tcp_read, &mut quic_send, &c_rx.rx_bytes).await;
        let _ = quic_send.finish();
        n_total
    });

    // Forward pipe (client -> server): opportunistically sniff the very first client chunk
    // without blocking `s2c` or adding any timeout latency, then stream-count in real time.
    let c_tx = ctx.counters.clone();
    let initial_host = meta.host_name;
    let port = meta.port;
    let was_ip = meta.was_ip;
    let c2s = tokio::spawn(async move {
        let mut proto = if port == 443 {
            "HTTPS"
        } else if port == 80 {
            "HTTP"
        } else {
            "TCP"
        };
        let mut final_host = initial_host;
        let mut n_total = 0u64;

        let mut first_buf = vec![0u8; 4096];
        if let Ok(Some(first_n)) = quic_recv.read(&mut first_buf).await
            && first_n > 0
        {
            if let Some((sniffed_proto, sniffed_host)) = sniff_first_packet(&first_buf[..first_n]) {
                proto = sniffed_proto;
                if was_ip || final_host.is_empty() {
                    final_host = sniffed_host;
                }
            }
            if tcp_write.write_all(&first_buf[..first_n]).await.is_ok() {
                c_tx.tx_bytes.fetch_add(first_n as u64, Ordering::Relaxed);
                n_total += first_n as u64;
                n_total += protocol::io::copy_with_counter(
                    &mut quic_recv,
                    &mut tcp_write,
                    &c_tx.tx_bytes,
                )
                .await;
            }
        }
        let _ = tcp_write.shutdown().await;
        (n_total, proto, final_host)
    });

    let (tx_res, rx_res) = tokio::join!(c2s, s2c);
    let (tx_bytes, proto, final_host) = tx_res.unwrap_or((0, "TCP", String::new()));
    let rx_bytes = rx_res.unwrap_or(0);

    if !meta.is_internal_panel {
        ctx.state.record_access(AccessLogRecord {
            link_id: ctx.link_id.clone(),
            link_name: ctx.link_name.clone(),
            client_ip: ctx.client_ip.clone(),
            proto: proto.to_string(),
            root_domain: extract_root_domain(&final_host),
            target: format!("{final_host}:{port}"),
            tx_bytes,
            rx_bytes,
            duration_ms: meta.started.elapsed().as_millis() as u64,
            outcome: AccessOutcome::Ok,
        });
    }
    Ok(())
}

/// Bootstraps persistent state, background quota/persistence workers, optional Web Admin Panel,
/// and the main dual-protocol (`GACL` + `Hysteria2`) QUIC accept loop.
pub async fn run_server(opts: ServerOptions) -> Result<()> {
    let has_real_cert = opts.cert.is_some() && opts.key.is_some();
    let effective_sni = if opts.sni != "localhost" {
        opts.sni.clone()
    } else {
        opts.dev_domain.clone()
    };

    let (state, generated_pass) = AppState::load_or_init(InitParams {
        data_path: opts.data_file.clone(),
        seed_token: opts.token.clone(),
        public_host: opts.public_host.clone(),
        udp_port: opts.listen.port(),
        sni: effective_sni,
        insecure: !has_real_cert,
        initial_panel_pass: opts.panel_pass.clone(),
    })?;

    info!("state store loaded from {:?}", state.data_path());
    if let Some(pass) = generated_pass {
        info!("============================================================");
        info!("  Web Admin Panel initialized!");
        info!("  Username: admin");
        info!("  Password: {}", pass);
        info!("============================================================");
    }

    // Periodic 15s traffic counter persistence + online quota/expiry enforcement
    let flush_state = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(15));
        loop {
            tick.tick().await;
            flush_state.enforce_limits_and_kick();
            if let Err(e) = flush_state.sync_and_save() {
                warn!("failed to flush traffic counters: {:#}", e);
            }
        }
    });

    // Optional built-in Web Admin Panel (default 127.0.0.1:9090, reachable inside authenticated tunnel)
    let panel_route = if !opts.panel_listen.eq_ignore_ascii_case("none") {
        let panel_addr: SocketAddr = opts
            .panel_listen
            .parse()
            .with_context(|| format!("invalid --panel-listen address: {}", opts.panel_listen))?;
        let panel_state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = panel::run_panel_server(panel_addr, panel_state).await {
                error!("web panel server error: {:#}", e);
            }
        });
        info!(
            "tunnel-only panel aliases enabled: http://{INTERNAL_PANEL_HOST} and http://{INTERNAL_PANEL_TUN_IP} -> {panel_addr}"
        );
        Some(Arc::new(PanelRoute {
            local_addr: panel_addr,
            state: state.clone(),
        }))
    } else {
        None
    };

    let server_config = build_server_config(
        opts.cert.as_deref(),
        opts.key.as_deref(),
        &opts.dev_domain,
        opts.brutal_mbps,
    )?;
    let udp_sock = bind_tuned_udp_socket(opts.listen)?;
    let endpoint = Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(server_config),
        udp_sock,
        Arc::new(quinn::TokioRuntime),
    )?;
    let masquerade_dir = opts.masquerade_dir.map(Arc::new);
    if let Some(dir) = &masquerade_dir {
        info!("HTTP/3 static masquerade enabled from {:?}", dir);
    }
    info!("QUIC server listening on udp://{}", opts.listen);

    while let Some(incoming) = endpoint.accept().await {
        let state = state.clone();
        let panel_route = panel_route.clone();
        let masquerade_dir = masquerade_dir.clone();
        tokio::spawn(async move {
            match incoming.await {
                Ok(conn) => {
                    let remote = conn.remote_address();
                    info!("new QUIC connection from {}", remote);
                    if let Err(e) =
                        handle_connection(conn, state, panel_route, masquerade_dir).await
                    {
                        warn!("connection {} closed: {:#}", remote, e);
                    }
                }
                Err(e) => warn!("incoming handshake failed: {}", e),
            }
        });
    }

    Ok(())
}

pub(crate) async fn handle_connection(
    conn: quinn::Connection,
    state: Arc<AppState>,
    panel_route: Option<Arc<PanelRoute>>,
    masquerade_dir: Option<Arc<PathBuf>>,
) -> Result<()> {
    let (mut auth_send, mut auth_recv) = conn.accept_bi().await.context("accept auth stream")?;
    let mut first_four = [0u8; 4];
    auth_recv
        .read_exact(&mut first_four)
        .await
        .context("read protocol prefix")?;

    // Dual-protocol dispatch on the same QUIC/ALPN-h3 port:
    // - `GACL` prefix => native game-accel-rs client
    // - anything else => HTTP/3 Hysteria2 auth (Shadowrocket / Clash / sing-box) or HTTP/3 masquerade
    if &first_four != AUTH_MAGIC {
        return hy2::handle_hy2_connection(
            conn,
            first_four,
            auth_send,
            auth_recv,
            state,
            panel_route,
            masquerade_dir,
        )
        .await;
    }

    let client_token = match read_auth_request_after_magic(&mut auth_recv).await {
        Ok(t) => t,
        Err(e) => {
            let _ = auth_send.write_all(&[AUTH_ERR]).await;
            let _ = auth_send.finish();
            return Err(e);
        }
    };

    let client_ip = conn.remote_address().ip().to_string();
    let ticket = match state.authenticate_token(&client_token) {
        Ok(t) => t,
        Err(e) => {
            let _ = auth_send.write_all(&[AUTH_ERR]).await;
            let _ = auth_send.finish();
            state.record_access(AccessLogRecord {
                link_id: String::new(),
                link_name: "未授权客户端".to_string(),
                client_ip,
                proto: "AUTH".to_string(),
                root_domain: "-".to_string(),
                target: format!("{e:#}"),
                tx_bytes: 0,
                rx_bytes: 0,
                duration_ms: 0,
                outcome: AccessOutcome::Denied,
            });
            conn.close(0u32.into(), b"unauthorized");
            return Err(e);
        }
    };

    let Some(_conn_guard) = state.register_conn(&ticket, conn.clone()) else {
        conn.close(0u32.into(), b"revoked");
        anyhow::bail!("link '{}' revoked before registration", ticket.link_name);
    };
    auth_send.write_all(&[AUTH_OK]).await?;
    let _ = auth_send.finish();
    info!(
        "client {} authenticated as link '{}' ({})",
        conn.remote_address(),
        ticket.link_name,
        ticket.link_id
    );

    let ctx = Arc::new(StreamContext {
        counters: ticket.counters.clone(),
        state,
        link_id: ticket.link_id.clone(),
        link_name: ticket.link_name.clone(),
        client_ip,
        panel_route,
    });

    // UDP Full Cone NAT loop over QUIC Datagrams (single connection-scoped dup pacer)
    let pacer = UdpDupPacer::spawn(conn.clone());
    let nat = Arc::new(FullConeNat::new(conn.clone(), pacer, ctx.clone()));
    let dgram_conn = conn.clone();
    // Domain targets may block on DNS; they are moved off the receive loop so one slow
    // lookup cannot stall every IP-targeted (game) flow, with bounded concurrency.
    let resolve_slots = Arc::new(tokio::sync::Semaphore::new(32));
    let dgram_task = tokio::spawn(async move {
        while let Ok(data) = dgram_conn.read_datagram().await {
            let needs_resolve = data.get(5) == Some(&ATYP_DOMAIN);
            if needs_resolve {
                match resolve_slots.clone().try_acquire_owned() {
                    Ok(permit) => {
                        let nat = nat.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
                            if let Err(e) = nat.handle_datagram(data).await {
                                warn!("udp datagram error: {:#}", e);
                            }
                        });
                    }
                    Err(_) => {
                        warn!("dropping domain-targeted udp datagram: dns resolve queue full");
                    }
                }
            } else if let Err(e) = nat.handle_datagram(data).await {
                warn!("udp datagram error: {:#}", e);
            }
        }
    });

    // Accept TCP proxy streams over QUIC Streams
    while let Ok((send, recv)) = conn.accept_bi().await {
        let stream_ctx = ctx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_tcp_stream(send, recv, &stream_ctx).await {
                warn!("tcp stream error: {:#}", e);
            }
        });
    }

    dgram_task.abort();
    Ok(())
}

async fn handle_tcp_stream(
    mut quic_send: quinn::SendStream,
    mut quic_recv: quinn::RecvStream,
    ctx: &StreamContext,
) -> Result<()> {
    let started = Instant::now();
    let target = TargetAddr::read_tcp_open(&mut quic_recv).await?;
    let internal_panel_addr = ctx
        .panel_route
        .as_ref()
        .and_then(|route| route.resolve_target(&target));
    let is_internal_panel = internal_panel_addr.is_some();

    let host_port = target.to_host_port();
    let dial_target = internal_panel_addr
        .map(|a| a.to_string())
        .unwrap_or_else(|| host_port.clone());
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
            quic_send.write_all(&[AUTH_OK]).await?;
            s
        }
        Err(e) => {
            let _ = quic_send.write_all(&[AUTH_ERR]).await;
            let _ = quic_send.finish();
            if !is_internal_panel {
                ctx.state.record_access(AccessLogRecord {
                    link_id: ctx.link_id.clone(),
                    link_name: ctx.link_name.clone(),
                    client_ip: ctx.client_ip.clone(),
                    proto: "TCP".to_string(),
                    root_domain: extract_root_domain(&host_name),
                    target: host_port,
                    tx_bytes: 0,
                    rx_bytes: 0,
                    duration_ms: started.elapsed().as_millis() as u64,
                    outcome: AccessOutcome::Error,
                });
            }
            return Err(e).with_context(|| format!("connect target {dial_target}"));
        }
    };

    splice_and_audit_tcp(
        quic_send,
        quic_recv,
        tcp,
        TcpAuditMeta {
            host_name,
            port,
            was_ip,
            is_internal_panel,
            started,
        },
        ctx,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use protocol::ALPN_PROTOCOL;
    use protocol::auth::write_auth_request;
    use protocol::frame::{FLAG_GAME_MODE, UdpFrame};
    use quinn::crypto::rustls::QuicClientConfig;
    use quinn::{ClientConfig, TransportConfig};
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, SignatureScheme};
    use std::collections::HashSet;
    use std::sync::Mutex;
    use tokio::io::AsyncReadExt;
    use tokio::net::{TcpListener, UdpSocket};

    #[derive(Debug)]
    struct TestSkipVerify;
    impl ServerCertVerifier for TestSkipVerify {
        fn verify_server_cert(
            &self,
            _: &CertificateDer<'_>,
            _: &[CertificateDer<'_>],
            _: &ServerName<'_>,
            _: &[u8],
            _: UnixTime,
        ) -> std::result::Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            vec![
                SignatureScheme::ECDSA_NISTP256_SHA256,
                SignatureScheme::RSA_PKCS1_SHA256,
                SignatureScheme::ED25519,
            ]
        }
    }

    async fn spawn_loopback_quic() -> (Endpoint, quinn::Connection, PathBuf) {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let data_path = std::env::temp_dir().join(format!(
            "game-accel-loopback-{}.json",
            store::generate_random_str(8)
        ));
        let (state, _) = AppState::load_or_init(InitParams {
            data_path: data_path.clone(),
            seed_token: "test-tok-loopback".to_string(),
            public_host: "127.0.0.1".to_string(),
            udp_port: 0,
            sni: "localhost".to_string(),
            insecure: true,
            initial_panel_pass: Some("testpass123".to_string()),
        })
        .unwrap();

        let server_cfg = build_server_config(None, None, "localhost", 0).unwrap();
        let server_ep = Endpoint::server(server_cfg, "127.0.0.1:0".parse().unwrap()).unwrap();
        let server_addr = server_ep.local_addr().unwrap();

        let ep_bg = server_ep.clone();
        let st_bg = state.clone();
        tokio::spawn(async move {
            if let Some(inc) = ep_bg.accept().await
                && let Ok(conn) = inc.await
            {
                let _ = handle_connection(conn, st_bg, None, None).await;
            }
        });

        let mut tls = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(TestSkipVerify))
            .with_no_client_auth();
        tls.alpn_protocols = vec![ALPN_PROTOCOL.to_vec()];
        let mut client_cfg = ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls).unwrap()));
        let mut tp = TransportConfig::default();
        tp.datagram_receive_buffer_size(Some(1024 * 1024));
        tp.datagram_send_buffer_size(1024 * 1024);
        client_cfg.transport_config(Arc::new(tp));

        let mut client_ep = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        client_ep.set_default_client_config(client_cfg);
        let conn = client_ep
            .connect(server_addr, "localhost")
            .unwrap()
            .await
            .unwrap();

        let (mut s, mut r) = conn.open_bi().await.unwrap();
        write_auth_request(&mut s, "test-tok-loopback").await.unwrap();
        let _ = s.finish();
        assert_eq!(r.read_u8().await.unwrap(), AUTH_OK);

        (client_ep, conn, data_path)
    }

    #[tokio::test]
    async fn server_first_tcp_banner_zero_stall() {
        let (ep, conn, data_path) = spawn_loopback_quic().await;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut s, _)) = listener.accept().await {
                let _ = s.write_all(b"SSH-2.0-FastBanner\r\n").await;
                let _ = s.shutdown().await;
            }
        });

        let started = Instant::now();
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        TargetAddr::Ip(target_addr)
            .write_tcp_open(&mut send)
            .await
            .unwrap();
        assert_eq!(recv.read_u8().await.unwrap(), AUTH_OK);

        let mut banner = vec![0u8; 64];
        let n = tokio::time::timeout(Duration::from_millis(120), recv.read(&mut banner))
            .await
            .expect("server-first banner must NOT stall on sniff timeout!")
            .unwrap()
            .unwrap();
        assert_eq!(&banner[..n], b"SSH-2.0-FastBanner\r\n");
        assert!(
            started.elapsed() < Duration::from_millis(120),
            "banner took {:?}, expected < 120ms",
            started.elapsed()
        );

        let _ = send.finish();
        conn.close(0u32.into(), b"done");
        ep.close(0u32.into(), b"done");
        let _ = std::fs::remove_file(&data_path);
    }

    #[tokio::test]
    async fn hysteria2_auth_tcp_and_udp_end_to_end() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let data_path = std::env::temp_dir().join(format!(
            "game-accel-hy2-e2e-{}.json",
            store::generate_random_str(8)
        ));
        let (state, _) = AppState::load_or_init(InitParams {
            data_path: data_path.clone(),
            seed_token: "hy2-test-token-999".to_string(),
            public_host: "127.0.0.1".to_string(),
            udp_port: 8443,
            sni: "localhost".to_string(),
            insecure: true,
            initial_panel_pass: Some("testpass123".to_string()),
        })
        .unwrap();

        let server_cfg = build_server_config(None, None, "localhost", 0).unwrap();
        let server_ep = Endpoint::server(server_cfg, "127.0.0.1:0".parse().unwrap()).unwrap();
        let server_addr = server_ep.local_addr().unwrap();

        let ep_bg = server_ep.clone();
        let st_bg = state.clone();
        tokio::spawn(async move {
            if let Some(inc) = ep_bg.accept().await
                && let Ok(conn) = inc.await
            {
                let _ = handle_connection(conn, st_bg, None, None).await;
            }
        });

        let mut tls = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(TestSkipVerify))
            .with_no_client_auth();
        tls.alpn_protocols = vec![ALPN_PROTOCOL.to_vec()];
        let mut client_ep = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        client_ep.set_default_client_config(ClientConfig::new(Arc::new(
            QuicClientConfig::try_from(tls).unwrap(),
        )));

        let conn = client_ep
            .connect(server_addr, "localhost")
            .unwrap()
            .await
            .unwrap();

        let (mut auth_send, mut auth_recv) = conn.open_bi().await.unwrap();
        let mut qpack = bytes::BytesMut::new();
        qpack.extend_from_slice(&[0x00, 0x00, 0xD4]);
        qpack.extend_from_slice(&[0x51, 0x05]);
        qpack.extend_from_slice(b"/auth");
        qpack.extend_from_slice(&[0x27, 0x06]);
        qpack.extend_from_slice(b"hysteria-auth");
        qpack.extend_from_slice(&[18]);
        qpack.extend_from_slice(b"hy2-test-token-999");

        let mut h3_frame = bytes::BytesMut::new();
        hy2::encode_varint(0x01, &mut h3_frame);
        hy2::encode_varint(qpack.len() as u64, &mut h3_frame);
        h3_frame.extend_from_slice(&qpack);
        auth_send.write_all(&h3_frame).await.unwrap();
        let _ = auth_send.finish();

        let resp_bytes = auth_recv.read_to_end(4096).await.unwrap();
        let resp_str = String::from_utf8_lossy(&resp_bytes);
        assert!(resp_str.contains("233"), "expected 233 auth ok, got {resp_str:?}");
        assert!(resp_str.contains("hysteria-udp"), "expected hysteria-udp header");

        let tcp_echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tcp_addr = tcp_echo.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut s, _)) = tcp_echo.accept().await {
                let mut buf = [0u8; 64];
                if let Ok(n) = s.read(&mut buf).await {
                    let _ = s.write_all(&buf[..n]).await;
                }
            }
        });

        let (mut tcp_send, mut tcp_recv) = conn.open_bi().await.unwrap();
        let mut tcp_req = bytes::BytesMut::new();
        hy2::encode_varint(0x401, &mut tcp_req);
        let addr_s = tcp_addr.to_string();
        hy2::encode_varint(addr_s.len() as u64, &mut tcp_req);
        tcp_req.extend_from_slice(addr_s.as_bytes());
        hy2::encode_varint(0, &mut tcp_req);
        tcp_send.write_all(&tcp_req).await.unwrap();
        tcp_send.write_all(b"hello-hy2-tcp").await.unwrap();

        let status = tcp_recv.read_u8().await.unwrap();
        assert_eq!(status, 0x00, "hy2 tcp dial status must be OK");
        let msg_len = hy2::read_varint(&mut tcp_recv).await.unwrap();
        assert_eq!(msg_len, 0);
        let pad_len = hy2::read_varint(&mut tcp_recv).await.unwrap();
        assert_eq!(pad_len, 0);
        let mut echoed = [0u8; 13];
        tcp_recv.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"hello-hy2-tcp");

        let udp_echo = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let udp_addr = udp_echo.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 256];
            if let Ok((n, peer)) = udp_echo.recv_from(&mut buf).await {
                let _ = udp_echo.send_to(&buf[..n], peer).await;
            }
        });

        let mut dgram = bytes::BytesMut::new();
        dgram.extend_from_slice(&77u32.to_be_bytes());
        dgram.extend_from_slice(&1u16.to_be_bytes());
        dgram.extend_from_slice(&[0u8, 1u8]);
        let udp_s = udp_addr.to_string();
        hy2::encode_varint(udp_s.len() as u64, &mut dgram);
        dgram.extend_from_slice(udp_s.as_bytes());
        dgram.extend_from_slice(b"hello-hy2-udp");
        conn.send_datagram(dgram.freeze()).unwrap();

        let reply = tokio::time::timeout(Duration::from_secs(3), conn.read_datagram())
            .await
            .expect("udp reply timeout")
            .unwrap();
        assert_eq!(u32::from_be_bytes([reply[0], reply[1], reply[2], reply[3]]), 77);
        assert!(reply.ends_with(b"hello-hy2-udp"));

        conn.close(0u32.into(), b"done");
        client_ep.close(0u32.into(), b"done");
        server_ep.close(0u32.into(), b"done");
        let _ = std::fs::remove_file(&data_path);
    }

    #[tokio::test]
    async fn fast_open_pipelined_payload_and_failed_dial() {
        let (ep, conn, data_path) = spawn_loopback_quic().await;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut s, _)) = listener.accept().await {
                let mut buf = [0u8; 64];
                if let Ok(n) = s.read(&mut buf).await {
                    let _ = s.write_all(&buf[..n]).await;
                }
            }
        });

        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        TargetAddr::Ip(target_addr)
            .write_tcp_open(&mut send)
            .await
            .unwrap();
        send.write_all(b"client-hello").await.unwrap();
        assert_eq!(recv.read_u8().await.unwrap(), AUTH_OK);
        let mut echoed = [0u8; 12];
        recv.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"client-hello");

        let (mut send2, mut recv2) = conn.open_bi().await.unwrap();
        TargetAddr::Ip("127.0.0.1:1".parse().unwrap())
            .write_tcp_open(&mut send2)
            .await
            .unwrap();
        let _ = send2.write_all(b"doomed").await;
        assert_eq!(recv2.read_u8().await.unwrap(), AUTH_ERR);

        conn.close(0u32.into(), b"done");
        ep.close(0u32.into(), b"done");
        let _ = std::fs::remove_file(&data_path);
    }

    #[tokio::test]
    async fn full_cone_nat_concurrent_creation_no_race() {
        let (ep, conn, data_path) = spawn_loopback_quic().await;

        let echo = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let echo_addr = echo.local_addr().unwrap();
        let seen_ports: Arc<Mutex<HashSet<u16>>> = Arc::new(Mutex::new(HashSet::new()));
        let seen_bg = seen_ports.clone();
        let echo_bg = echo.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            while let Ok((n, peer)) = echo_bg.recv_from(&mut buf).await {
                seen_bg.lock().unwrap().insert(peer.port());
                let _ = echo_bg.send_to(&buf[..n], peer).await;
            }
        });

        for seq in 0u16..8 {
            let frame = UdpFrame {
                flow_id: 77,
                seq,
                flags: FLAG_GAME_MODE,
                addr: TargetAddr::Ip(echo_addr),
                payload: Bytes::from(format!("pkt-{seq}")),
            };
            conn.send_datagram(frame.encode().unwrap()).unwrap();
        }

        let mut received = HashSet::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while received.len() < 8 && Instant::now() < deadline {
            if let Ok(Ok(raw)) =
                tokio::time::timeout(Duration::from_millis(300), conn.read_datagram()).await
            {
                let f = UdpFrame::decode(raw).unwrap();
                assert_eq!(f.flow_id, 77);
                received.insert(String::from_utf8(f.payload.to_vec()).unwrap());
            }
        }
        assert_eq!(received.len(), 8, "all 8 UDP echo packets must return");
        assert_eq!(
            seen_ports.lock().unwrap().len(),
            1,
            "FullConeNat must create exactly ONE UDP socket under concurrent first-packet burst"
        );

        conn.close(0u32.into(), b"done");
        ep.close(0u32.into(), b"done");
        let _ = std::fs::remove_file(&data_path);
    }

    #[test]
    fn panel_route_resolves_only_allowed_aliases() {
        let local: SocketAddr = "127.0.0.1:9090".parse().unwrap();
        let data_path = std::env::temp_dir().join(format!(
            "game-accel-panelroute-{}.json",
            store::generate_random_str(8)
        ));
        let (state, _) = AppState::load_or_init(InitParams {
            data_path: data_path.clone(),
            seed_token: "test-tok-route".to_string(),
            public_host: "203.0.113.10".to_string(),
            udp_port: 8443,
            sni: "node.example.com".to_string(),
            insecure: true,
            initial_panel_pass: None,
        })
        .unwrap();
        let route = PanelRoute {
            local_addr: local,
            state: state.clone(),
        };

        assert_eq!(
            route.resolve_target(&TargetAddr::Domain("panel.gacl.local".into(), 80)),
            Some(local)
        );
        assert_eq!(
            route.resolve_target(&TargetAddr::Domain("PANEL.GACL.LOCAL".into(), 9090)),
            Some(local)
        );
        assert_eq!(
            route.resolve_target(&TargetAddr::Ip("198.18.0.1:80".parse().unwrap())),
            Some(local)
        );
        assert_eq!(
            route.resolve_target(&TargetAddr::Domain("node.example.com".into(), 9090)),
            Some(local)
        );
        assert_eq!(
            route.resolve_target(&TargetAddr::Ip("203.0.113.10:9090".parse().unwrap())),
            Some(local)
        );

        assert_eq!(
            route.resolve_target(&TargetAddr::Domain("node.example.com".into(), 443)),
            None
        );
        assert_eq!(
            route.resolve_target(&TargetAddr::Domain("example.com".into(), 80)),
            None
        );
        assert_eq!(
            route.resolve_target(&TargetAddr::Ip("198.18.0.1:22".parse().unwrap())),
            None
        );

        state
            .update_node_settings(crate::store::UpdateNodeSettingsInput {
                public_host: "198.51.100.7".to_string(),
                udp_port: 8443,
                sni: "new.example.com".to_string(),
                insecure: true,
            })
            .unwrap();
        assert_eq!(
            route.resolve_target(&TargetAddr::Domain("new.example.com".into(), 9090)),
            Some(local)
        );
        assert_eq!(
            route.resolve_target(&TargetAddr::Ip("198.51.100.7:9090".parse().unwrap())),
            Some(local)
        );
        assert_eq!(
            route.resolve_target(&TargetAddr::Domain("node.example.com".into(), 9090)),
            None,
            "stale sni must no longer be routed to the panel"
        );

        let _ = std::fs::remove_file(&data_path);
    }
}
