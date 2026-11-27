use crate::relay::{ClientStats, relay_direct};
use crate::rules::Router;
use crate::tunnel::Tunnels;
use anyhow::{Context, Result, bail};
use protocol::frame::TargetAddr;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Handle HTTP / HTTPS (CONNECT) proxy requests on the mixed port.
pub(crate) async fn handle_http_proxy_client(
    mut client: TcpStream,
    tunnels: Arc<Tunnels>,
    stats: Arc<ClientStats>,
    router: Arc<Router>,
) -> Result<()> {
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 1024];
    let header_end = loop {
        if buf.len() > 16 * 1024 {
            bail!("http header too large");
        }
        let n = client.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_double_crlf(&buf) {
            break pos;
        }
    };

    let header_str =
        std::str::from_utf8(&buf[..header_end]).context("invalid http header utf-8")?;
    let mut lines = header_str.split("\r\n");
    let request_line = lines.next().context("empty http request")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().context("missing http method")?;
    let raw_target = parts.next().context("missing http target")?;
    let version = parts.next().unwrap_or("HTTP/1.1");

    let leftover = &buf[header_end + 4..];
    const BAD_GATEWAY: &[u8] =
        b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

    let (target, ok_reply, upstream_preamble, fail_reply) =
        if method.eq_ignore_ascii_case("CONNECT") {
            (
                parse_host_port(raw_target, 443)?,
                Some(&b"HTTP/1.1 200 Connection Established\r\n\r\n"[..]),
                leftover.to_vec(),
                None,
            )
        } else {
            let (target, path) = parse_http_url(raw_target)?;
            let mut rebuilt = String::with_capacity(header_str.len() + 32);
            rebuilt.push_str(method);
            rebuilt.push(' ');
            rebuilt.push_str(&path);
            rebuilt.push(' ');
            rebuilt.push_str(version);
            rebuilt.push_str("\r\n");
            for line in lines {
                if line.is_empty() {
                    continue;
                }
                let lower = line.to_ascii_lowercase();
                if lower.starts_with("proxy-connection:")
                    || lower.starts_with("connection:")
                    || lower.starts_with("keep-alive:")
                {
                    continue;
                }
                rebuilt.push_str(line);
                rebuilt.push_str("\r\n");
            }
            rebuilt.push_str("Connection: close\r\n\r\n");
            let mut preamble = rebuilt.into_bytes();
            preamble.extend_from_slice(leftover);
            (target, None, preamble, Some(BAD_GATEWAY))
        };

    let route = router.decide(&target).await;
    let Some(tunnel) = tunnels.for_route(route) else {
        let fail = if ok_reply.is_some() {
            Some(BAD_GATEWAY)
        } else {
            fail_reply
        };
        return relay_direct(&router, client, &target, ok_reply, &upstream_preamble, fail).await;
    };

    let (mut quic_send, quic_recv) = tunnel.open_tcp(&target).await?;
    if let Some(reply) = ok_reply {
        client.write_all(reply).await?;
    }
    if !upstream_preamble.is_empty() {
        quic_send.write_all(&upstream_preamble).await?;
        stats
            .tx_bytes
            .fetch_add(upstream_preamble.len() as u64, Ordering::Relaxed);
    }
    tunnel
        .relay_tcp(client, quic_send, quic_recv, stats, fail_reply)
        .await
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_host_port(s: &str, default_port: u16) -> Result<TargetAddr> {
    if let Ok(sock) = s.parse::<SocketAddr>() {
        return Ok(TargetAddr::Ip(sock));
    }
    let (host, port) = if let Some(rest) = s.strip_prefix('[') {
        if let Some((ip_part, after)) = rest.split_once(']') {
            let port = after
                .strip_prefix(':')
                .and_then(|p| p.parse::<u16>().ok())
                .unwrap_or(default_port);
            (ip_part, port)
        } else {
            bail!("invalid ipv6 host: {s}");
        }
    } else if let Some((h, p)) = s.rsplit_once(':') {
        let port = p.parse::<u16>().context("invalid port")?;
        (h, port)
    } else {
        (s, default_port)
    };

    if let Ok(ip) = host.parse::<IpAddr>() {
        Ok(TargetAddr::Ip(SocketAddr::new(ip, port)))
    } else {
        Ok(TargetAddr::Domain(host.to_string(), port))
    }
}

fn parse_http_url(url: &str) -> Result<(TargetAddr, String)> {
    let without_scheme = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("HTTP://"))
        .unwrap_or(url);
    let (authority, path) = match without_scheme.find('/') {
        Some(idx) => (&without_scheme[..idx], &without_scheme[idx..]),
        None => (without_scheme, "/"),
    };
    let target = parse_host_port(authority, 80)?;
    Ok((target, path.to_string()))
}
