//! Minimal DNS wire-format helpers for split DNS in TUN mode: read the question name
//! of a query, and the A/AAAA addresses of a response (to learn IP -> domain hints).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

const HEADER_LEN: usize = 12;

/// Read a (possibly compressed) name starting at `pos`; returns (name, position right
/// after the name in the original record).
fn read_name(msg: &[u8], mut pos: usize) -> Option<(String, usize)> {
    let mut name = String::new();
    let mut end_after: Option<usize> = None;
    let mut jumps = 0;
    loop {
        let len = *msg.get(pos)? as usize;
        if len == 0 {
            let next = pos + 1;
            return Some((name, end_after.unwrap_or(next)));
        }
        if len & 0xC0 == 0xC0 {
            let ptr = ((len & 0x3F) << 8) | *msg.get(pos + 1)? as usize;
            end_after.get_or_insert(pos + 2);
            jumps += 1;
            if jumps > 16 || ptr >= msg.len() {
                return None;
            }
            pos = ptr;
            continue;
        }
        if len > 63 {
            return None;
        }
        let label = msg.get(pos + 1..pos + 1 + len)?;
        if !name.is_empty() {
            name.push('.');
        }
        for &b in label {
            name.push((b as char).to_ascii_lowercase());
        }
        if name.len() > 253 {
            return None;
        }
        pos += 1 + len;
    }
}

/// First question name of a DNS message (query or response), lowercase, no trailing dot.
pub fn question_name(msg: &[u8]) -> Option<String> {
    if msg.len() < HEADER_LEN || u16::from_be_bytes([msg[4], msg[5]]) == 0 {
        return None;
    }
    read_name(msg, HEADER_LEN)
        .map(|(n, _)| n)
        .filter(|n| !n.is_empty())
}

/// For a DNS response: the question name and every A/AAAA address in the answer section.
pub fn answer_addresses(msg: &[u8]) -> Option<(String, Vec<IpAddr>)> {
    if msg.len() < HEADER_LEN || msg[2] & 0x80 == 0 {
        return None; // not a response
    }
    let qdcount = u16::from_be_bytes([msg[4], msg[5]]) as usize;
    let ancount = u16::from_be_bytes([msg[6], msg[7]]) as usize;
    if qdcount == 0 {
        return None;
    }
    let (qname, mut pos) = read_name(msg, HEADER_LEN)?;
    pos += 4; // qtype + qclass
    for _ in 1..qdcount {
        let (_, p) = read_name(msg, pos)?;
        pos = p + 4;
    }
    let mut ips = Vec::new();
    for _ in 0..ancount {
        let (_, p) = read_name(msg, pos)?;
        let rr = msg.get(p..p + 10)?;
        let rtype = u16::from_be_bytes([rr[0], rr[1]]);
        let rdlen = u16::from_be_bytes([rr[8], rr[9]]) as usize;
        let rdata = msg.get(p + 10..p + 10 + rdlen)?;
        match (rtype, rdlen) {
            (1, 4) => ips.push(IpAddr::V4(Ipv4Addr::new(
                rdata[0], rdata[1], rdata[2], rdata[3],
            ))),
            (28, 16) => {
                let mut o = [0u8; 16];
                o.copy_from_slice(rdata);
                ips.push(IpAddr::V6(Ipv6Addr::from(o)));
            }
            _ => {}
        }
        pos = p + 10 + rdlen;
    }
    Some((qname, ips))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Build a query for `name` (type A).
    pub(crate) fn build_query(id: u16, name: &str) -> Vec<u8> {
        let mut m = Vec::new();
        m.extend_from_slice(&id.to_be_bytes());
        m.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
        for label in name.split('.') {
            m.push(label.len() as u8);
            m.extend_from_slice(label.as_bytes());
        }
        m.push(0);
        m.extend_from_slice(&[0, 1, 0, 1]);
        m
    }

    #[test]
    fn parses_query_and_compressed_response() {
        let q = build_query(0x1234, "WWW.Baidu.com");
        assert_eq!(question_name(&q).as_deref(), Some("www.baidu.com"));
        assert!(answer_addresses(&q).is_none(), "a query is not a response");

        // Response: CNAME (compressed) + A record, answers point back to the question name.
        let mut r = q.clone();
        r[2] = 0x81;
        r[3] = 0x80;
        r[7] = 2; // ancount = 2
        // CNAME www.baidu.com -> www.a.shifen.com
        r.extend_from_slice(&[0xC0, 0x0C, 0, 5, 0, 1, 0, 0, 0, 60]);
        let cname: Vec<u8> = [
            &[3u8][..],
            b"www",
            &[1],
            b"a",
            &[6],
            b"shifen",
            &[0xC0, 0x16],
        ]
        .concat();
        r.extend_from_slice(&(cname.len() as u16).to_be_bytes());
        let cname_off = r.len();
        r.extend_from_slice(&cname);
        // A record for the CNAME target (pointer to it)
        r.extend_from_slice(&[
            0xC0 | ((cname_off >> 8) as u8),
            cname_off as u8,
            0,
            1,
            0,
            1,
            0,
            0,
            0,
            60,
            0,
            4,
        ]);
        r.extend_from_slice(&[183, 2, 172, 177]);

        let (name, ips) = answer_addresses(&r).unwrap();
        assert_eq!(name, "www.baidu.com");
        assert_eq!(ips, vec!["183.2.172.177".parse::<IpAddr>().unwrap()]);
    }

    #[test]
    fn rejects_garbage_and_pointer_loops() {
        assert!(question_name(&[0u8; 5]).is_none());
        let mut looped = build_query(1, "a.com");
        looped[12] = 0xC0;
        looped[13] = 12; // pointer to itself
        assert!(question_name(&looped).is_none());
    }
}
