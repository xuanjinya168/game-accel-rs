use anyhow::{Context, Result, bail};
use protocol::{
    ALPN_PROTOCOL, DATAGRAM_RECV_BUFFER, DATAGRAM_SEND_BUFFER, DEFAULT_INITIAL_MTU,
    KEEP_ALIVE_INTERVAL, MAX_IDLE_TIMEOUT,
};
use quinn::crypto::rustls::QuicClientConfig;
use quinn::{ClientConfig, TransportConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

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
    let resolved: Vec<SocketAddr> =
        tokio::time::timeout(Duration::from_secs(4), tokio::net::lookup_host(s))
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

/// Local wildcard bind address matching the server's address family
/// (an IPv4-only socket cannot reach an IPv6 node and vice versa).
pub(crate) fn unspecified_bind_for(server: SocketAddr) -> SocketAddr {
    if server.is_ipv6() {
        SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 0))
    } else {
        SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, 0))
    }
}

pub(crate) fn bind_tuned_udp_socket(
    addr: SocketAddr,
    buf_bytes: usize,
) -> Result<std::net::UdpSocket> {
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

pub(crate) fn build_client_config(
    insecure: bool,
    is_udp_lane: bool,
    brutal_mbps: u32,
) -> Result<ClientConfig> {
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
    // Brutal is strictly restricted to the bulk TCP lane; the game UDP lane always uses BBR.
    if !is_udp_lane && brutal_mbps > 0 {
        transport.congestion_controller_factory(Arc::new(protocol::brutal::BrutalConfig::new(
            brutal_mbps,
        )));
    } else {
        transport.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
    }

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
