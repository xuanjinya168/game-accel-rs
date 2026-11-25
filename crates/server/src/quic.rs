use anyhow::{Context, Result};
use protocol::{
    ALPN_PROTOCOL, DATAGRAM_RECV_BUFFER, DATAGRAM_SEND_BUFFER, DEFAULT_INITIAL_MTU,
    KEEP_ALIVE_INTERVAL, MAX_IDLE_TIMEOUT,
};
use quinn::crypto::rustls::QuicServerConfig;
use quinn::{ServerConfig, TransportConfig};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::fs::File;
use std::io::BufReader;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{info, warn};

pub(crate) fn bind_tuned_udp_socket(addr: SocketAddr) -> Result<std::net::UdpSocket> {
    let domain = if addr.is_ipv6() {
        socket2::Domain::IPV6
    } else {
        socket2::Domain::IPV4
    };
    let sock = socket2::Socket::new(domain, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))
        .context("create UDP socket")?;
    let _ = sock.set_recv_buffer_size(8 * 1024 * 1024);
    let _ = sock.set_send_buffer_size(8 * 1024 * 1024);
    sock.bind(&addr.into()).context("bind UDP socket")?;
    Ok(sock.into())
}

pub(crate) fn build_server_config(
    cert_path: Option<&std::path::Path>,
    key_path: Option<&std::path::Path>,
    dev_domain: &str,
    brutal_mbps: u32,
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
    transport.mtu_discovery_config(Some(quinn::MtuDiscoveryConfig::default()));
    transport.stream_receive_window(quinn::VarInt::from_u32(16 * 1024 * 1024));
    transport.receive_window(quinn::VarInt::from_u32(64 * 1024 * 1024));
    transport.send_window(32 * 1024 * 1024);
    // Browsers open many parallel connections; keep concurrency far above realistic usage.
    transport.max_concurrent_bidi_streams(quinn::VarInt::from_u32(4096));
    transport.datagram_receive_buffer_size(Some(DATAGRAM_RECV_BUFFER));
    transport.datagram_send_buffer_size(DATAGRAM_SEND_BUFFER);
    transport.keep_alive_interval(Some(KEEP_ALIVE_INTERVAL));
    transport.max_idle_timeout(Some(MAX_IDLE_TIMEOUT.try_into()?));
    if brutal_mbps > 0 {
        info!(
            "server congestion control: Brutal ({} Mbps target, queue-guarded; applies to listener QUIC transport)",
            brutal_mbps.clamp(1, 1000)
        );
        transport.congestion_controller_factory(Arc::new(
            protocol::brutal::BrutalConfig::new(brutal_mbps),
        ));
    } else {
        transport.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
    }

    server_config.transport_config(Arc::new(transport));
    Ok(server_config)
}
