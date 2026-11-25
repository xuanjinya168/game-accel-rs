//! Lightweight first-packet protocol & domain sniffer (zero MITM, zero extra latency).
//! Extracts real TLS SNI hostnames (even when client sends raw IPs in TUN mode)
//! and HTTP Host headers.

pub fn sniff_first_packet(data: &[u8]) -> Option<(&'static str, String)> {
    if let Some(sni) = parse_tls_sni(data) {
        return Some(("HTTPS", sni));
    }
    if let Some(host) = parse_http_host(data) {
        return Some(("HTTP", host));
    }
    None
}

/// Extract the primary (root / eTLD+1) domain from a hostname.
pub fn extract_root_domain(host: &str) -> String {
    let h = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if h.is_empty() {
        return "-".to_string();
    }
    if h.parse::<std::net::IpAddr>().is_ok() || h.contains(':') {
        return h;
    }

    let parts: Vec<&str> = h.split('.').collect();
    if parts.len() <= 2 {
        return h;
    }

    let last2 = format!("{}.{}", parts[parts.len() - 2], parts[parts.len() - 1]);
    let is_two_part_tld = matches!(
        last2.as_str(),
        "co.uk"
            | "org.uk"
            | "ac.uk"
            | "com.cn"
            | "net.cn"
            | "org.cn"
            | "gov.cn"
            | "edu.cn"
            | "com.hk"
            | "com.tw"
            | "org.tw"
            | "co.jp"
            | "ne.jp"
            | "or.jp"
            | "co.kr"
            | "com.au"
            | "com.sg"
            | "com.br"
    );

    if is_two_part_tld && parts.len() >= 3 {
        format!(
            "{}.{}.{}",
            parts[parts.len() - 3],
            parts[parts.len() - 2],
            parts[parts.len() - 1]
        )
    } else {
        last2
    }
}

/// Parse TLS ClientHello SNI extension (RFC 6066).
fn parse_tls_sni(data: &[u8]) -> Option<String> {
    if data.len() < 43 || data[0] != 0x16 || data[1] != 0x03 {
        return None;
    }
    let record_len = u16::from_be_bytes([data[3], data[4]]) as usize;
    let payload = data.get(5..data.len().min(5 + record_len))?;

    if payload.len() < 38 || payload[0] != 0x01 {
        return None;
    }
    let mut pos = 4 + 2 + 32;

    let sid_len = *payload.get(pos)? as usize;
    pos += 1 + sid_len;

    let cs_len = u16::from_be_bytes([*payload.get(pos)?, *payload.get(pos + 1)?]) as usize;
    pos += 2 + cs_len;

    let comp_len = *payload.get(pos)? as usize;
    pos += 1 + comp_len;

    let ext_total = u16::from_be_bytes([*payload.get(pos)?, *payload.get(pos + 1)?]) as usize;
    pos += 2;
    let ext_end = (pos + ext_total).min(payload.len());

    while pos + 4 <= ext_end {
        let ext_type = u16::from_be_bytes([payload[pos], payload[pos + 1]]);
        let ext_len = u16::from_be_bytes([payload[pos + 2], payload[pos + 3]]) as usize;
        pos += 4;
        if pos + ext_len > ext_end {
            break;
        }
        if ext_type == 0x0000 && ext_len >= 5 {
            let mut p = pos + 2;
            let end = pos + ext_len;
            while p + 3 <= end {
                let name_type = payload[p];
                let name_len = u16::from_be_bytes([payload[p + 1], payload[p + 2]]) as usize;
                p += 3;
                if p + name_len > end {
                    break;
                }
                if name_type == 0x00
                    && let Ok(sni) = std::str::from_utf8(&payload[p..p + name_len]) {
                        let clean = sni.trim();
                        if !clean.is_empty() {
                            return Some(clean.to_string());
                        }
                    }
                p += name_len;
            }
        }
        pos += ext_len;
    }
    None
}

/// Parse HTTP/1.x `Host:` header from first packet.
fn parse_http_host(data: &[u8]) -> Option<String> {
    const METHODS: &[&[u8]] = &[
        b"GET ",
        b"POST ",
        b"PUT ",
        b"DELETE ",
        b"HEAD ",
        b"OPTIONS ",
        b"PATCH ",
    ];
    if !METHODS.iter().any(|m| data.starts_with(m)) {
        return None;
    }
    let text = std::str::from_utf8(&data[..data.len().min(2048)]).ok()?;
    for line in text.split("\r\n").skip(1) {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':')
            && k.eq_ignore_ascii_case("host") {
                let host = v.trim();
                let clean = host.split(':').next().unwrap_or(host).trim();
                if !clean.is_empty() {
                    return Some(clean.to_string());
                }
            }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_domain_cases() {
        assert_eq!(extract_root_domain("www.youtube.com"), "youtube.com");
        assert_eq!(
            extract_root_domain("rr3---sn-a5mekn6k.googlevideo.com"),
            "googlevideo.com"
        );
        assert_eq!(extract_root_domain("news.bbc.co.uk"), "bbc.co.uk");
        assert_eq!(extract_root_domain("api.github.com"), "github.com");
        assert_eq!(extract_root_domain("142.250.72.14"), "142.250.72.14");
    }

    #[test]
    fn http_host_sniff() {
        let req = b"GET /watch?v=123 HTTP/1.1\r\nHost: www.example.com:8080\r\n\r\n";
        assert_eq!(
            sniff_first_packet(req),
            Some(("HTTP", "www.example.com".to_string()))
        );
    }

    #[test]
    fn tls_sni_sniff() {
        let host = b"www.youtube.com";
        let mut sni_ext = Vec::new();
        sni_ext.extend_from_slice(&((host.len() + 3) as u16).to_be_bytes());
        sni_ext.push(0x00);
        sni_ext.extend_from_slice(&(host.len() as u16).to_be_bytes());
        sni_ext.extend_from_slice(host);

        let mut exts = Vec::new();
        exts.extend_from_slice(&0x0000u16.to_be_bytes());
        exts.extend_from_slice(&(sni_ext.len() as u16).to_be_bytes());
        exts.extend_from_slice(&sni_ext);

        let mut body = Vec::new();
        body.extend_from_slice(&[0x03, 0x03]);
        body.extend_from_slice(&[0u8; 32]);
        body.push(0);
        body.extend_from_slice(&2u16.to_be_bytes());
        body.extend_from_slice(&[0x13, 0x01]);
        body.push(1);
        body.push(0);
        body.extend_from_slice(&(exts.len() as u16).to_be_bytes());
        body.extend_from_slice(&exts);

        let mut hs = Vec::new();
        hs.push(0x01);
        let blen = body.len();
        hs.extend_from_slice(&[(blen >> 16) as u8, (blen >> 8) as u8, blen as u8]);
        hs.extend_from_slice(&body);

        let mut rec = Vec::new();
        rec.extend_from_slice(&[0x16, 0x03, 0x01]);
        rec.extend_from_slice(&(hs.len() as u16).to_be_bytes());
        rec.extend_from_slice(&hs);

        assert_eq!(
            sniff_first_packet(&rec),
            Some(("HTTPS", "www.youtube.com".to_string()))
        );
    }
}
