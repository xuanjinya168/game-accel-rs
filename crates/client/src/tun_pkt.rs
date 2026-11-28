//! Self-contained IPv4/IPv6 + UDP packet codec with RFC 791 IPv4 fragment reassembly
//! and egress fragmentation for Wintun TUN mode.
//!
//! Bypasses `netstack-smoltcp`'s UDP stream (which returns `Poll::Ready(None)` — i.e. EOF —
//! on the first fragmented/truncated UDP packet, terminating the TUN data plane) while
//! keeping `netstack-smoltcp` solely for user-space TCP termination.

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};

const PROTO_TCP: u8 = 6;
const PROTO_UDP: u8 = 17;

const IPV4_MIN_HEADER: usize = 20;
const IPV6_HEADER_LEN: usize = 40;
const UDP_HEADER_LEN: usize = 8;

const FRAG_TTL: Duration = Duration::from_secs(10);
const MAX_PENDING_FRAGS: usize = 256;
const MAX_IP_PAYLOAD: usize = 65_535;

/// Classification of a raw IP packet read from the Wintun interface.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TunIngressPacket {
    /// Complete (or freshly reassembled) UDP datagram ready for direct/tunnel forwarding.
    Udp {
        payload: Vec<u8>,
        src: SocketAddr,
        dst: SocketAddr,
    },
    /// Complete IPv4/IPv6 TCP packet to forward into `netstack-smoltcp`.
    Tcp(Vec<u8>),
    /// Packet consumed as a partial IPv4 fragment, multicast/broadcast, non-TCP/UDP, or invalid.
    Drop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FragKey {
    src: Ipv4Addr,
    dst: Ipv4Addr,
    ident: u16,
    proto: u8,
}

struct FragBuffer {
    created: Instant,
    total_len: Option<usize>,
    chunks: BTreeMap<usize, Vec<u8>>,
}

impl FragBuffer {
    fn new(now: Instant) -> Self {
        Self {
            created: now,
            total_len: None,
            chunks: BTreeMap::new(),
        }
    }

    fn insert(&mut self, offset: usize, more_frags: bool, data: &[u8]) {
        let replace = match self.chunks.get(&offset) {
            Some(existing) => data.len() > existing.len(),
            None => true,
        };
        if replace {
            self.chunks.insert(offset, data.to_vec());
        }
        if !more_frags {
            self.total_len = Some(offset + data.len());
        }
    }

    fn try_assemble(&self) -> Option<Vec<u8>> {
        let total = self.total_len?;
        if total == 0 || total > MAX_IP_PAYLOAD {
            return None;
        }
        let mut covered = 0usize;
        for (&off, chunk) in &self.chunks {
            if off > covered {
                return None;
            }
            covered = covered.max(off + chunk.len());
            if covered >= total {
                break;
            }
        }
        if covered < total {
            return None;
        }
        let mut out = vec![0u8; total];
        for (&off, chunk) in &self.chunks {
            if off >= total {
                break;
            }
            let end = (off + chunk.len()).min(total);
            out[off..end].copy_from_slice(&chunk[..end - off]);
        }
        Some(out)
    }
}

/// Bounded IPv4 fragment reassembler + raw TUN packet classifier.
pub(crate) struct TunIngressParser {
    frags: HashMap<FragKey, FragBuffer>,
    last_sweep: Instant,
    tun_broadcast_v4: Ipv4Addr,
}

impl TunIngressParser {
    pub(crate) fn new(tun_broadcast_v4: Ipv4Addr) -> Self {
        Self {
            frags: HashMap::new(),
            last_sweep: Instant::now(),
            tun_broadcast_v4,
        }
    }

    /// Parse a raw IP packet read from the TUN adapter. Never errors or panics on malformed
    /// or fragmented input — callers can safely keep running forever.
    pub(crate) fn parse(&mut self, raw: &[u8]) -> TunIngressPacket {
        self.parse_at(raw, Instant::now())
    }

    pub(crate) fn parse_at(&mut self, raw: &[u8], now: Instant) -> TunIngressPacket {
        let Some(&first) = raw.first() else {
            return TunIngressPacket::Drop;
        };
        match first >> 4 {
            4 => self.parse_ipv4(raw, now),
            6 => self.parse_ipv6(raw),
            _ => TunIngressPacket::Drop,
        }
    }

    fn parse_ipv4(&mut self, raw: &[u8], now: Instant) -> TunIngressPacket {
        if raw.len() < IPV4_MIN_HEADER {
            return TunIngressPacket::Drop;
        }
        let ihl = ((raw[0] & 0x0F) as usize) * 4;
        if ihl < IPV4_MIN_HEADER || raw.len() < ihl {
            return TunIngressPacket::Drop;
        }
        let total_len = u16::from_be_bytes([raw[2], raw[3]]) as usize;
        if total_len < ihl || raw.len() < total_len {
            return TunIngressPacket::Drop;
        }
        let pkt = &raw[..total_len];
        let src_ip = Ipv4Addr::new(pkt[12], pkt[13], pkt[14], pkt[15]);
        let dst_ip = Ipv4Addr::new(pkt[16], pkt[17], pkt[18], pkt[19]);

        if !self.is_allowed_v4(src_ip) || !self.is_allowed_v4(dst_ip) {
            return TunIngressPacket::Drop;
        }

        let proto = pkt[9];
        if proto != PROTO_UDP && proto != PROTO_TCP {
            return TunIngressPacket::Drop;
        }

        let flags_frag = u16::from_be_bytes([pkt[6], pkt[7]]);
        let more_frags = (flags_frag & 0x2000) != 0;
        let frag_offset = ((flags_frag & 0x1FFF) as usize) * 8;
        let ip_payload = &pkt[ihl..total_len];

        if !more_frags && frag_offset == 0 {
            return match proto {
                PROTO_UDP => parse_udp_datagram(IpAddr::V4(src_ip), IpAddr::V4(dst_ip), ip_payload),
                PROTO_TCP => TunIngressPacket::Tcp(pkt.to_vec()),
                _ => TunIngressPacket::Drop,
            };
        }

        // Fragmented IPv4 packet: reassemble before upper-layer parsing.
        if ip_payload.is_empty() || frag_offset.saturating_add(ip_payload.len()) > MAX_IP_PAYLOAD {
            return TunIngressPacket::Drop;
        }
        if now.duration_since(self.last_sweep) >= FRAG_TTL {
            self.sweep(now);
        }

        let ident = u16::from_be_bytes([pkt[4], pkt[5]]);
        let key = FragKey {
            src: src_ip,
            dst: dst_ip,
            ident,
            proto,
        };

        if !self.frags.contains_key(&key) && self.frags.len() >= MAX_PENDING_FRAGS {
            self.sweep(now);
            if self.frags.len() >= MAX_PENDING_FRAGS
                && let Some(oldest_key) = self
                    .frags
                    .iter()
                    .min_by_key(|(_, v)| v.created)
                    .map(|(k, _)| *k)
            {
                self.frags.remove(&oldest_key);
            }
        }

        let entry = self
            .frags
            .entry(key)
            .or_insert_with(|| FragBuffer::new(now));
        entry.insert(frag_offset, more_frags, ip_payload);

        let Some(assembled) = entry.try_assemble() else {
            return TunIngressPacket::Drop;
        };
        self.frags.remove(&key);

        match proto {
            PROTO_UDP => parse_udp_datagram(IpAddr::V4(src_ip), IpAddr::V4(dst_ip), &assembled),
            PROTO_TCP => {
                // Rebuild a single unfragmented IPv4 packet for netstack-smoltcp if it fits u16.
                let Some(new_total) = u16::try_from(IPV4_MIN_HEADER + assembled.len()).ok() else {
                    return TunIngressPacket::Drop;
                };
                let mut rebuilt = Vec::with_capacity(new_total as usize);
                write_ipv4_header(&mut rebuilt, new_total, ident, 0, pkt[8], PROTO_TCP, src_ip, dst_ip);
                rebuilt.extend_from_slice(&assembled);
                TunIngressPacket::Tcp(rebuilt)
            }
            _ => TunIngressPacket::Drop,
        }
    }

    fn parse_ipv6(&self, raw: &[u8]) -> TunIngressPacket {
        if raw.len() < IPV6_HEADER_LEN {
            return TunIngressPacket::Drop;
        }
        let payload_len = u16::from_be_bytes([raw[4], raw[5]]) as usize;
        let total_len = IPV6_HEADER_LEN + payload_len;
        if raw.len() < total_len {
            return TunIngressPacket::Drop;
        }
        let mut src_octets = [0u8; 16];
        let mut dst_octets = [0u8; 16];
        src_octets.copy_from_slice(&raw[8..24]);
        dst_octets.copy_from_slice(&raw[24..40]);
        let src_ip = Ipv6Addr::from(src_octets);
        let dst_ip = Ipv6Addr::from(dst_octets);

        if src_ip.is_multicast()
            || src_ip.is_unspecified()
            || dst_ip.is_multicast()
            || dst_ip.is_unspecified()
        {
            return TunIngressPacket::Drop;
        }

        match raw[6] {
            PROTO_UDP => {
                parse_udp_datagram(IpAddr::V6(src_ip), IpAddr::V6(dst_ip), &raw[IPV6_HEADER_LEN..total_len])
            }
            PROTO_TCP => TunIngressPacket::Tcp(raw[..total_len].to_vec()),
            _ => TunIngressPacket::Drop,
        }
    }

    fn is_allowed_v4(&self, ip: Ipv4Addr) -> bool {
        !(ip.is_broadcast()
            || ip.is_multicast()
            || ip.is_unspecified()
            || ip == self.tun_broadcast_v4)
    }

    fn sweep(&mut self, now: Instant) {
        self.frags
            .retain(|_, buf| now.duration_since(buf.created) < FRAG_TTL);
        self.last_sweep = now;
    }
}

fn parse_udp_datagram(src_ip: IpAddr, dst_ip: IpAddr, udp: &[u8]) -> TunIngressPacket {
    if udp.len() < UDP_HEADER_LEN {
        return TunIngressPacket::Drop;
    }
    let src_port = u16::from_be_bytes([udp[0], udp[1]]);
    let dst_port = u16::from_be_bytes([udp[2], udp[3]]);
    let udp_len = u16::from_be_bytes([udp[4], udp[5]]) as usize;
    if udp_len < UDP_HEADER_LEN || udp_len > udp.len() {
        return TunIngressPacket::Drop;
    }
    TunIngressPacket::Udp {
        payload: udp[UDP_HEADER_LEN..udp_len].to_vec(),
        src: SocketAddr::new(src_ip, src_port),
        dst: SocketAddr::new(dst_ip, dst_port),
    }
}

/// Build one or more IP packets carrying a UDP datagram from `src` to `dst`, automatically
/// fragmenting IPv4 packets when `20 + 8 + payload.len() > mtu` so replies larger than
/// `TUN_MTU` are cleanly reassembled by Windows and delivered to the app / Hyper-V VM.
pub(crate) fn build_udp_ip_packets(
    payload: &[u8],
    src: SocketAddr,
    dst: SocketAddr,
    mtu: usize,
    next_ident: &AtomicU16,
) -> Vec<Vec<u8>> {
    let Some(udp_len_u16) = u16::try_from(UDP_HEADER_LEN + payload.len()).ok() else {
        return Vec::new();
    };
    match (src, dst) {
        (SocketAddr::V4(src_v4), SocketAddr::V4(dst_v4)) => {
            let mut udp = Vec::with_capacity(udp_len_u16 as usize);
            udp.extend_from_slice(&src_v4.port().to_be_bytes());
            udp.extend_from_slice(&dst_v4.port().to_be_bytes());
            udp.extend_from_slice(&udp_len_u16.to_be_bytes());
            udp.extend_from_slice(&[0, 0]); // checksum placeholder
            udp.extend_from_slice(payload);

            let csum = udp_v4_checksum(*src_v4.ip(), *dst_v4.ip(), &udp);
            udp[6..8].copy_from_slice(&csum.to_be_bytes());

            let ident = next_ident.fetch_add(1, Ordering::Relaxed);
            let eff_mtu = mtu.max(68);
            if IPV4_MIN_HEADER + udp.len() <= eff_mtu {
                let mut pkt = Vec::with_capacity(IPV4_MIN_HEADER + udp.len());
                write_ipv4_header(
                    &mut pkt,
                    (IPV4_MIN_HEADER + udp.len()) as u16,
                    ident,
                    0,
                    64,
                    PROTO_UDP,
                    *src_v4.ip(),
                    *dst_v4.ip(),
                );
                pkt.extend_from_slice(&udp);
                return vec![pkt];
            }

            let max_chunk = ((eff_mtu - IPV4_MIN_HEADER) / 8) * 8;
            if max_chunk == 0 {
                return Vec::new();
            }
            let mut out = Vec::with_capacity(udp.len().div_ceil(max_chunk));
            let mut offset = 0usize;
            while offset < udp.len() {
                let chunk_len = (udp.len() - offset).min(max_chunk);
                let more_frags = offset + chunk_len < udp.len();
                let flags_frag = ((offset / 8) as u16) | if more_frags { 0x2000 } else { 0 };
                let mut pkt = Vec::with_capacity(IPV4_MIN_HEADER + chunk_len);
                write_ipv4_header(
                    &mut pkt,
                    (IPV4_MIN_HEADER + chunk_len) as u16,
                    ident,
                    flags_frag,
                    64,
                    PROTO_UDP,
                    *src_v4.ip(),
                    *dst_v4.ip(),
                );
                pkt.extend_from_slice(&udp[offset..offset + chunk_len]);
                out.push(pkt);
                offset += chunk_len;
            }
            out
        }
        (SocketAddr::V6(src_v6), SocketAddr::V6(dst_v6)) => {
            let mut udp = Vec::with_capacity(udp_len_u16 as usize);
            udp.extend_from_slice(&src_v6.port().to_be_bytes());
            udp.extend_from_slice(&dst_v6.port().to_be_bytes());
            udp.extend_from_slice(&udp_len_u16.to_be_bytes());
            udp.extend_from_slice(&[0, 0]);
            udp.extend_from_slice(payload);

            let csum = udp_v6_checksum(*src_v6.ip(), *dst_v6.ip(), &udp);
            udp[6..8].copy_from_slice(&csum.to_be_bytes());

            let mut pkt = Vec::with_capacity(IPV6_HEADER_LEN + udp.len());
            pkt.extend_from_slice(&[0x60, 0x00, 0x00, 0x00]); // version=6, TC=0, flow=0
            pkt.extend_from_slice(&udp_len_u16.to_be_bytes());
            pkt.push(PROTO_UDP);
            pkt.push(64); // hop limit
            pkt.extend_from_slice(&src_v6.ip().octets());
            pkt.extend_from_slice(&dst_v6.ip().octets());
            pkt.extend_from_slice(&udp);
            vec![pkt]
        }
        _ => Vec::new(),
    }
}

#[allow(clippy::too_many_arguments)]
fn write_ipv4_header(
    out: &mut Vec<u8>,
    total_len: u16,
    ident: u16,
    flags_frag: u16,
    ttl: u8,
    proto: u8,
    src: Ipv4Addr,
    dst: Ipv4Addr,
) {
    let mut hdr = [0u8; IPV4_MIN_HEADER];
    hdr[0] = 0x45; // version 4, IHL 5 (20 bytes)
    hdr[1] = 0x00;
    hdr[2..4].copy_from_slice(&total_len.to_be_bytes());
    hdr[4..6].copy_from_slice(&ident.to_be_bytes());
    hdr[6..8].copy_from_slice(&flags_frag.to_be_bytes());
    hdr[8] = ttl;
    hdr[9] = proto;
    hdr[12..16].copy_from_slice(&src.octets());
    hdr[16..20].copy_from_slice(&dst.octets());
    let csum = finalize_checksum(checksum_add(0, &hdr));
    hdr[10..12].copy_from_slice(&csum.to_be_bytes());
    out.extend_from_slice(&hdr);
}

fn checksum_add(mut acc: u32, data: &[u8]) -> u32 {
    let (chunks, rem) = data.as_chunks::<2>();
    for &[hi, lo] in chunks {
        acc += u16::from_be_bytes([hi, lo]) as u32;
    }
    if let Some(&b) = rem.first() {
        acc += (b as u32) << 8;
    }
    acc
}

fn finalize_checksum(mut acc: u32) -> u16 {
    while (acc >> 16) != 0 {
        acc = (acc & 0xFFFF) + (acc >> 16);
    }
    !(acc as u16)
}

fn udp_v4_checksum(src: Ipv4Addr, dst: Ipv4Addr, udp: &[u8]) -> u16 {
    let mut acc = checksum_add(0, &src.octets());
    acc = checksum_add(acc, &dst.octets());
    acc += PROTO_UDP as u32;
    acc += udp.len() as u32;
    acc = checksum_add(acc, udp);
    let csum = finalize_checksum(acc);
    if csum == 0 { 0xFFFF } else { csum }
}

fn udp_v6_checksum(src: Ipv6Addr, dst: Ipv6Addr, udp: &[u8]) -> u16 {
    let mut acc = checksum_add(0, &src.octets());
    acc = checksum_add(acc, &dst.octets());
    acc = checksum_add(acc, &(udp.len() as u32).to_be_bytes());
    acc += PROTO_UDP as u32;
    acc = checksum_add(acc, udp);
    let csum = finalize_checksum(acc);
    if csum == 0 { 0xFFFF } else { csum }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_parser() -> TunIngressParser {
        TunIngressParser::new(Ipv4Addr::new(10, 0, 88, 255))
    }

    #[test]
    fn unfragmented_udp_roundtrip() {
        let ident = AtomicU16::new(1);
        let src: SocketAddr = "10.0.88.2:51234".parse().unwrap();
        let dst: SocketAddr = "119.29.29.29:20001".parse().unwrap();
        let payload = b"game-tick-payload-small".to_vec();

        let pkts = build_udp_ip_packets(&payload, src, dst, 1120, &ident);
        assert_eq!(pkts.len(), 1);
        // RFC 1071 IPv4 header checksum verifies to 0 when summed across the 20B header.
        assert_eq!(finalize_checksum(checksum_add(0, &pkts[0][..20])), 0);

        let mut parser = test_parser();
        assert_eq!(
            parser.parse(&pkts[0]),
            TunIngressPacket::Udp { payload, src, dst }
        );
    }

    #[test]
    fn ipv6_udp_roundtrip() {
        let ident = AtomicU16::new(1);
        let src: SocketAddr = "[2001:db8::1]:50000".parse().unwrap();
        let dst: SocketAddr = "[2001:4860:4860::8888]:53".parse().unwrap();
        let payload = b"v6-dns-query".to_vec();

        let pkts = build_udp_ip_packets(&payload, src, dst, 1120, &ident);
        assert_eq!(pkts.len(), 1);
        let mut parser = test_parser();
        assert_eq!(
            parser.parse(&pkts[0]),
            TunIngressPacket::Udp { payload, src, dst }
        );
    }

    #[test]
    fn fragmented_udp_larger_than_tun_mtu_reassembles_in_any_order() {
        let ident = AtomicU16::new(42);
        let src: SocketAddr = "10.0.88.2:54321".parse().unwrap();
        let dst: SocketAddr = "1.1.1.1:443".parse().unwrap();
        // 1400B UDP payload (typical Hyper-V VM / QUIC Initial / game state burst > 1092B)
        let payload: Vec<u8> = (0..1400).map(|i| (i % 251) as u8).collect();

        let pkts = build_udp_ip_packets(&payload, src, dst, 1120, &ident);
        assert_eq!(pkts.len(), 2, "1408B UDP must split into 2 IPv4 fragments at MTU 1120");
        assert!(pkts.iter().all(|p| p.len() <= 1120));

        // Verify out-of-order reassembly (Fragment 2 before Fragment 1)
        let mut parser = test_parser();
        assert_eq!(parser.parse(&pkts[1]), TunIngressPacket::Drop);
        assert_eq!(
            parser.parse(&pkts[0]),
            TunIngressPacket::Udp {
                payload: payload.clone(),
                src,
                dst,
            }
        );

        // Verify normal order (Fragment 1 then Fragment 2)
        let pkts2 = build_udp_ip_packets(&payload, src, dst, 1120, &ident);
        assert_eq!(parser.parse(&pkts2[0]), TunIngressPacket::Drop);
        assert_eq!(
            parser.parse(&pkts2[1]),
            TunIngressPacket::Udp { payload, src, dst }
        );
    }

    #[test]
    fn malformed_and_truncated_udp_never_breaks_subsequent_packets() {
        let mut parser = test_parser();
        // Truncated IP, garbage version, bad UDP length, broadcast
        assert_eq!(parser.parse(&[]), TunIngressPacket::Drop);
        assert_eq!(parser.parse(&[0x45, 0x00, 0x00]), TunIngressPacket::Drop);
        assert_eq!(parser.parse(&[0x90; 40]), TunIngressPacket::Drop);

        // Unfragmented IPv4 packet claiming UDP length 500 while only 12 bytes follow
        let mut bad_udp = Vec::new();
        write_ipv4_header(
            &mut bad_udp,
            32,
            7,
            0,
            64,
            PROTO_UDP,
            Ipv4Addr::new(10, 0, 88, 2),
            Ipv4Addr::new(8, 8, 8, 8),
        );
        bad_udp.extend_from_slice(&[0x10, 0x00, 0x00, 0x35, 0x01, 0xF4, 0x00, 0x00, 1, 2, 3, 4]);
        assert_eq!(parser.parse(&bad_udp), TunIngressPacket::Drop);

        // Subsequent valid packet must still parse cleanly
        let ident = AtomicU16::new(9);
        let src: SocketAddr = "10.0.88.2:40000".parse().unwrap();
        let dst: SocketAddr = "8.8.8.8:53".parse().unwrap();
        let valid = build_udp_ip_packets(b"ok", src, dst, 1120, &ident);
        assert_eq!(
            parser.parse(&valid[0]),
            TunIngressPacket::Udp {
                payload: b"ok".to_vec(),
                src,
                dst,
            }
        );
    }

    #[test]
    fn expired_partial_fragments_are_evicted() {
        let ident = AtomicU16::new(100);
        let src: SocketAddr = "10.0.88.2:12345".parse().unwrap();
        let dst: SocketAddr = "9.9.9.9:5000".parse().unwrap();
        let payload = vec![0xAB; 1300];
        let pkts = build_udp_ip_packets(&payload, src, dst, 1120, &ident);
        assert_eq!(pkts.len(), 2);

        let t0 = Instant::now();
        let mut parser = test_parser();
        assert_eq!(parser.parse_at(&pkts[0], t0), TunIngressPacket::Drop);
        assert_eq!(parser.frags.len(), 1);

        // After TTL, a new fragment triggers sweep of the abandoned partial entry
        let pkts2 = build_udp_ip_packets(&payload, src, dst, 1120, &ident);
        assert_eq!(
            parser.parse_at(&pkts2[0], t0 + FRAG_TTL + Duration::from_secs(1)),
            TunIngressPacket::Drop
        );
        assert_eq!(parser.frags.len(), 1);
    }
}
