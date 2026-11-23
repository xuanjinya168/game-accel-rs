mod nat;
mod panel;
mod sniff;
mod store;

use crate::nat::FullConeNat;
use crate::sniff::{extract_root_domain, sniff_first_packet};
use crate::store::{AppState, InitParams, LiveCounters};
use anyhow::{Context, Result};
use clap::Parser;
use protocol::auth::{AUTH_ERR, AUTH_OK, read_auth_request};
use protocol::frame::{TargetAddr, UdpFrame};
use protocol::pacer::UdpDupPacer;
use protocol::{ALPN_PROTOCOL, DEFAULT_INITIAL_MTU};
use quinn::crypto::rustls::QuicServerConfig;
use quinn::{Endpoint, ServerConfig, TransportConfig};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::fs::File;
use std::io::BufReader;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tracing::{error, info, warn};

#[derive(Parser, Debug)]
#[command(
    name = "game-accel-server",
    about = "All-in-one QUIC/UDP game accelerator & built-in Web Admin Panel"
)]
struct Args {
    /// QUIC UDP listen address
    #[arg(short, long, default_value = "0.0.0.0:8443")]
    listen: SocketAddr,

    /// Built-in Web Admin Panel TCP listen address (default loopback-only; use "none" to disable)
    #[arg(long, default_value = "127.0.0.1:9090")]
    panel_listen: String,

    /// Persistent panel & multi-link state JSON file
    #[arg(long, default_value = "/etc/game-accel/data.json")]
    data_file: PathBuf,

    /// Initial link token used when data-file does not yet exist
    #[arg(short, long, default_value = "dev-secret-token")]
    token: String,

    /// Optional initial admin password override for Web Panel
    #[arg(long)]
    panel_pass: Option<String>,

    /// Public IP or domain embedded into generated gacl:// links
    #[arg(long, default_value = "127.0.0.1")]
    public_host: String,

    /// TLS SNI domain embedded into generated gacl:// links
    #[arg(long, default_value = "localhost")]
    sni: String,

    /// Path to PEM certificate chain (fullchain.pem)
    #[arg(long)]
    cert: Option<PathBuf>,

    /// Path to PEM private key (privkey.pem)
    #[arg(long)]
    key: Option<PathBuf>,

    /// Self-signed cert SAN domain when --cert/--key are omitted
    #[arg(long, default_value = "localhost")]
    dev_domain: String,
}

const INTERNAL_PANEL_HOST: &str = "panel.gacl.local";
const INTERNAL_PANEL_TUN_IP: IpAddr = IpAddr::V4(Ipv4Addr::new(198, 18, 0, 1));

/// Maps authenticated tunnel-only panel aliases (`panel.gacl.local`, `198.18.0.1`,
/// or `public_host:panel_port` / `sni:panel_port`) to the loopback Web Admin Panel
/// so port 9090 never needs to be exposed on the public internet.
#[derive(Clone)]
struct PanelRoute {
    local_addr: SocketAddr,
    state: Arc<AppState>,
}

impl PanelRoute {
    fn resolve_target(&self, target: &TargetAddr) -> Option<SocketAddr> {
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let _ = rustls::crypto::ring::default_provider().install_default();
    let args = Args::parse();

    let has_real_cert = args.cert.is_some() && args.key.is_some();
    let effective_sni = if args.sni != "localhost" {
        args.sni.clone()
    } else {
        args.dev_domain.clone()
    };

    let (state, generated_pass) = AppState::load_or_init(InitParams {
        data_path: args.data_file.clone(),
        seed_token: args.token.clone(),
        public_host: args.public_host.clone(),
        udp_port: args.listen.port(),
        sni: effective_sni,
        insecure: !has_real_cert,
        initial_panel_pass: args.panel_pass.clone(),
    })?;

    info!("state store loaded from {:?}", state.data_path());
    if let Some(pass) = generated_pass {
        info!("============================================================");
        info!("  Web Admin Panel initialized!");
        info!("  Username: admin");
        info!("  Password: {}", pass);
        info!("============================================================");
    }

    // Periodic 15s traffic counter persistence task
    let flush_state = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(15));
        loop {
            tick.tick().await;
            if let Err(e) = flush_state.sync_and_save() {
                warn!("failed to flush traffic counters: {:#}", e);
            }
        }
    });

    // Optional built-in Web Admin Panel (default 127.0.0.1:9090, reachable inside authenticated tunnel)
    let panel_route = if !args.panel_listen.eq_ignore_ascii_case("none") {
        let panel_addr: SocketAddr = args
            .panel_listen
            .parse()
            .with_context(|| format!("invalid --panel-listen address: {}", args.panel_listen))?;
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

    let server_config =
        build_server_config(args.cert.as_deref(), args.key.as_deref(), &args.dev_domain)?;
    let endpoint = Endpoint::server(server_config, args.listen)?;
    info!("QUIC server listening on udp://{}", args.listen);

    while let Some(incoming) = endpoint.accept().await {
        let state = state.clone();
        let panel_route = panel_route.clone();
        tokio::spawn(async move {
            match incoming.await {
                Ok(conn) => {
                    let remote = conn.remote_address();
                    info!("new QUIC connection from {}", remote);
                    if let Err(e) = handle_connection(conn, state, panel_route).await {
                        warn!("connection {} closed: {:#}", remote, e);
                    }
                }
                Err(e) => warn!("incoming handshake failed: {}", e),
            }
        });
    }

    Ok(())
}

fn build_server_config(
    cert_path: Option<&std::path::Path>,
    key_path: Option<&std::path::Path>,
    dev_domain: &str,
) -> Result<ServerConfig> {
    let (certs, key) = match (cert_path, key_path) {
        (Some(c_path), Some(k_path)) => {
            info!(
                "loading TLS certificate from {:?} and key from {:?}",
                c_path, k_path
            );
            let mut cert_reader = BufReader::new(
                File::open(c_path).with_context(|| format!("open cert {:?}", c_path))?,
            );
            let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_reader)
                .collect::<std::result::Result<Vec<_>, _>>()
                .context("parse PEM certificates")?;
            if certs.is_empty() {
                anyhow::bail!("no certificates found in {:?}", c_path);
            }

            let mut key_reader = BufReader::new(
                File::open(k_path).with_context(|| format!("open key {:?}", k_path))?,
            );
            let key = rustls_pemfile::private_key(&mut key_reader)
                .context("parse PEM private key")?
                .with_context(|| format!("no private key found in {:?}", k_path))?;
            (certs, key)
        }
        _ => {
            warn!(
                "no --cert/--key provided; generating self-signed dev cert for {}",
                dev_domain
            );
            let cert = rcgen::generate_simple_self_signed(vec![dev_domain.to_string()])?;
            let cert_der = CertificateDer::from(cert.cert);
            let key_der =
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()));
            (vec![cert_der], key_der)
        }
    };

    let mut tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    tls_config.alpn_protocols = vec![ALPN_PROTOCOL.to_vec()];

    let quic_crypto = QuicServerConfig::try_from(tls_config)?;
    let mut server_config = ServerConfig::with_crypto(Arc::new(quic_crypto));

    let mut transport = TransportConfig::default();
    transport.initial_mtu(DEFAULT_INITIAL_MTU);
    transport.datagram_receive_buffer_size(Some(2 * 1024 * 1024));
    transport.datagram_send_buffer_size(2 * 1024 * 1024);
    transport.keep_alive_interval(Some(Duration::from_secs(10)));
    transport.max_idle_timeout(Some(Duration::from_secs(60).try_into()?));
    transport.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));

    server_config.transport_config(Arc::new(transport));
    Ok(server_config)
}

async fn handle_connection(
    conn: quinn::Connection,
    state: Arc<AppState>,
    panel_route: Option<Arc<PanelRoute>>,
) -> Result<()> {
    let (mut auth_send, mut auth_recv) = conn.accept_bi().await.context("accept auth stream")?;
    let client_token = match read_auth_request(&mut auth_recv).await {
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
            state.record_access_log(
                "",
                "未授权客户端",
                &client_ip,
                "AUTH",
                "-".to_string(),
                format!("{e:#}"),
                0,
                0,
                0,
                "denied",
            );
            conn.close(0u32.into(), b"unauthorized");
            return Err(e);
        }
    };

    auth_send.write_all(&[AUTH_OK]).await?;
    let _ = auth_send.finish();

    let cid = state.register_conn(&ticket, conn.clone());
    info!(
        "client {} authenticated as link '{}' ({})",
        conn.remote_address(),
        ticket.link_name,
        ticket.link_id
    );

    // UDP Full Cone NAT loop over QUIC Datagrams (single connection-scoped dup pacer)
    let pacer = UdpDupPacer::spawn(conn.clone());
    let nat = Arc::new(FullConeNat::new(
        conn.clone(),
        pacer,
        ticket.counters.clone(),
        state.clone(),
        ticket.link_id.clone(),
        ticket.link_name.clone(),
        client_ip.clone(),
    ));
    let dgram_conn = conn.clone();
    // Domain targets may block on DNS; they are moved off the receive loop so one slow
    // lookup cannot stall every IP-targeted (game) flow, with bounded concurrency.
    let resolve_slots = Arc::new(tokio::sync::Semaphore::new(32));
    let dgram_task = tokio::spawn(async move {
        loop {
            match dgram_conn.read_datagram().await {
                Ok(data) => {
                    let needs_resolve = matches!(
                        UdpFrame::decode(data.clone()).map(|f| f.addr),
                        Ok(TargetAddr::Domain(..))
                    );
                    if needs_resolve {
                        let Ok(permit) = resolve_slots.clone().acquire_owned().await else {
                            break;
                        };
                        let nat = nat.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
                            if let Err(e) = nat.handle_datagram(data).await {
                                warn!("udp datagram error: {:#}", e);
                            }
                        });
                    } else if let Err(e) = nat.handle_datagram(data).await {
                        warn!("udp datagram error: {:#}", e);
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Accept TCP proxy streams over QUIC Streams
    loop {
        match conn.accept_bi().await {
            Ok((send, recv)) => {
                let counters = ticket.counters.clone();
                let st = state.clone();
                let lid = ticket.link_id.clone();
                let lname = ticket.link_name.clone();
                let cip = client_ip.clone();
                let proute = panel_route.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        handle_tcp_stream(send, recv, counters, st, lid, lname, cip, proute).await
                    {
                        warn!("tcp stream error: {:#}", e);
                    }
                });
            }
            Err(_) => break,
        }
    }

    dgram_task.abort();
    state.unregister_conn(&ticket, cid);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_tcp_stream(
    mut quic_send: quinn::SendStream,
    mut quic_recv: quinn::RecvStream,
    counters: Arc<LiveCounters>,
    state: Arc<AppState>,
    link_id: String,
    link_name: String,
    client_ip: String,
    panel_route: Option<Arc<PanelRoute>>,
) -> Result<()> {
    let started = Instant::now();
    let target = TargetAddr::read_tcp_open(&mut quic_recv).await?;
    let internal_panel_addr = panel_route
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

    let tcp = match TcpStream::connect(&dial_target).await {
        Ok(s) => {
            let _ = s.set_nodelay(true);
            quic_send.write_all(&[AUTH_OK]).await?;
            s
        }
        Err(e) => {
            let _ = quic_send.write_all(&[AUTH_ERR]).await;
            let _ = quic_send.finish();
            if !is_internal_panel {
                state.record_access_log(
                    &link_id,
                    &link_name,
                    &client_ip,
                    "TCP",
                    extract_root_domain(&host_name),
                    host_port.clone(),
                    0,
                    0,
                    started.elapsed().as_millis() as u64,
                    "error",
                );
            }
            return Err(e).with_context(|| format!("connect target {dial_target}"));
        }
    };

    let (mut tcp_read, mut tcp_write) = tcp.into_split();

    // Start reverse pipe (server -> client) IMMEDIATELY so server-first protocols
    // (SSH, MySQL, game banners) suffer ZERO sniff delay, and count bytes per chunk in real time.
    let c_rx = counters.clone();
    let s2c = tokio::spawn(async move {
        let n_total =
            protocol::io::copy_with_counter(&mut tcp_read, &mut quic_send, &c_rx.rx_bytes).await;
        let _ = quic_send.finish();
        n_total
    });

    // Forward pipe (client -> server): opportunistically sniff the very first client chunk
    // without blocking `s2c` or adding any timeout latency, then stream-count in real time.
    let c_tx = counters.clone();
    let initial_host = host_name;
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
        match quic_recv.read(&mut first_buf).await {
            Ok(Some(first_n)) if first_n > 0 => {
                if let Some((sniffed_proto, sniffed_host)) = sniff_first_packet(&first_buf[..first_n])
                {
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
            _ => {}
        }
        let _ = tcp_write.shutdown().await;
        (n_total, proto, final_host)
    });

    let (tx_res, rx_res) = tokio::join!(c2s, s2c);
    let (tx_bytes, proto, final_host) = tx_res.unwrap_or((0, "TCP", String::new()));
    let rx_bytes = rx_res.unwrap_or(0);
    let display_target = format!("{final_host}:{port}");

    if !is_internal_panel {
        state.record_access_log(
            &link_id,
            &link_name,
            &client_ip,
            proto,
            extract_root_domain(&final_host),
            display_target,
            tx_bytes,
            rx_bytes,
            started.elapsed().as_millis() as u64,
            "ok",
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use protocol::auth::write_auth_request;
    use protocol::frame::{FLAG_GAME_MODE, UdpFrame};
    use quinn::ClientConfig;
    use quinn::crypto::rustls::QuicClientConfig;
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{ServerName, UnixTime};
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

        let server_cfg = build_server_config(None, None, "localhost").unwrap();
        let server_ep = Endpoint::server(server_cfg, "127.0.0.1:0".parse().unwrap()).unwrap();
        let server_addr = server_ep.local_addr().unwrap();

        let ep_bg = server_ep.clone();
        let st_bg = state.clone();
        tokio::spawn(async move {
            if let Some(inc) = ep_bg.accept().await {
                if let Ok(conn) = inc.await {
                    let _ = handle_connection(conn, st_bg, None).await;
                }
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

        // Target TCP server speaks IMMEDIATELY upon accept (like SSH / MySQL / game login servers)
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

        // Read banner WITHOUT sending any client payload first!
        let mut banner = vec![0u8; 64];
        let n = tokio::time::timeout(Duration::from_millis(120), recv.read(&mut banner))
            .await
            .expect("server-first banner must NOT stall on 250ms sniff timeout!")
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
    async fn full_cone_nat_concurrent_creation_no_race() {
        let (ep, conn, data_path) = spawn_loopback_quic().await;

        // Local UDP echo server tracking unique NAT source ports seen
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

        // Blast 8 concurrent datagrams for the exact same flow_id before NAT entry exists
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

        // Verify return path is alive and receives echoed payloads
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

        // Unrelated ports/hosts must NOT be rerouted to loopback
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

        // Aliases follow panel setting changes without a server restart
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
