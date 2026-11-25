//! Shared Hysteria2 wire protocol helpers (HTTP/3 + static QPACK + RFC 7541 Huffman,
//! `0x401` TCP proxy framing, and fragmented UDP datagram codec) used by both
//! `server` and `client`.

use crate::frame::TargetAddr;
use anyhow::{Context, Result, bail};
use bytes::{BufMut, Bytes, BytesMut};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const HY2_TCP_STREAM_TYPE: u64 = 0x401;
pub const H3_FRAME_DATA: u64 = 0x00;
pub const H3_FRAME_HEADERS: u64 = 0x01;
pub const H3_FRAME_SETTINGS: u64 = 0x04;
pub const H3_STREAM_CONTROL: u8 = 0x00;

/// Encode an HTTP/3 unidirectional Control Stream preamble with an empty SETTINGS frame
/// (QPACK dynamic table capacity = 0). Keep the `SendStream` open for the connection lifetime.
pub fn encode_h3_control_stream() -> Bytes {
    let mut ctrl = BytesMut::with_capacity(8);
    ctrl.put_u8(H3_STREAM_CONTROL);
    encode_varint(H3_FRAME_SETTINGS, &mut ctrl);
    encode_varint(0, &mut ctrl);
    ctrl.freeze()
}

/// QUIC variable-length integer (RFC 9000 Section 16).
pub fn encode_varint(val: u64, out: &mut BytesMut) {
    if val < 64 {
        out.put_u8(val as u8);
    } else if val < 16_384 {
        out.put_u16(0x4000 | (val as u16));
    } else if val < 1_073_741_824 {
        out.put_u32(0x8000_0000 | (val as u32));
    } else {
        out.put_u64(0xC000_0000_0000_0000 | val);
    }
}

pub fn decode_varint_slice(buf: &[u8]) -> Result<(u64, usize)> {
    let first = *buf.first().context("unexpected EOF reading varint")?;
    let len = 1usize << (first >> 6);
    if buf.len() < len {
        bail!("truncated varint (need {len} bytes, got {})", buf.len());
    }
    let mut val = (first & 0x3F) as u64;
    for &b in &buf[1..len] {
        val = (val << 8) | (b as u64);
    }
    Ok((val, len))
}

pub async fn read_varint<R: AsyncRead + Unpin>(r: &mut R) -> Result<u64> {
    let first = r.read_u8().await.context("read varint prefix")?;
    let len = 1usize << (first >> 6);
    let mut val = (first & 0x3F) as u64;
    for _ in 1..len {
        let b = r.read_u8().await.context("read varint body")?;
        val = (val << 8) | (b as u64);
    }
    Ok(val)
}

pub async fn write_varint<W: AsyncWrite + Unpin>(w: &mut W, val: u64) -> Result<()> {
    let mut buf = BytesMut::with_capacity(8);
    encode_varint(val, &mut buf);
    w.write_all(&buf).await?;
    Ok(())
}

// ============================================================================
// Hysteria2 TCP Proxy Stream Framing (`0x401`)
// ============================================================================

/// Write a Hysteria2 TCPRequest header:
/// `[varint(0x401)] [varint(addr_len)] [addr_utf8] [varint(pad_len)] [padding]`
pub async fn write_hy2_tcp_request<W: AsyncWrite + Unpin>(
    w: &mut W,
    target: &TargetAddr,
    pad_len: usize,
) -> Result<()> {
    let addr = target.to_host_port();
    let addr_bytes = addr.as_bytes();
    let mut buf = BytesMut::with_capacity(16 + addr_bytes.len() + pad_len);
    encode_varint(HY2_TCP_STREAM_TYPE, &mut buf);
    encode_varint(addr_bytes.len() as u64, &mut buf);
    buf.put_slice(addr_bytes);
    encode_varint(pad_len as u64, &mut buf);
    if pad_len > 0 {
        buf.put_bytes(0, pad_len);
    }
    w.write_all(&buf).await?;
    Ok(())
}

/// Read and validate a Hysteria2 TCPResponse header:
/// `[status: u8 (0=OK, 1=ERR)] [varint(msg_len)] [msg] [varint(pad_len)] [padding]`
pub async fn read_hy2_tcp_response<R: AsyncRead + Unpin>(r: &mut R) -> Result<()> {
    let status = r.read_u8().await.context("read hy2 tcp response status")?;
    let msg_len = read_varint(r).await.context("read hy2 tcp msg len")? as usize;
    if msg_len > 4096 {
        bail!("invalid hy2 tcp response msg length: {msg_len}");
    }
    let mut msg_buf = vec![0u8; msg_len];
    if msg_len > 0 {
        r.read_exact(&mut msg_buf).await?;
    }
    let pad_len = read_varint(r).await.context("read hy2 tcp pad len")? as usize;
    if pad_len > 4096 {
        bail!("invalid hy2 tcp response pad length: {pad_len}");
    }
    if pad_len > 0 {
        let mut pad = vec![0u8; pad_len];
        r.read_exact(&mut pad).await?;
    }
    if status != 0x00 {
        let msg = String::from_utf8_lossy(&msg_buf);
        bail!("hy2 tcp connect rejected (status={status}): {msg}");
    }
    Ok(())
}

// ============================================================================
// Hysteria2 UDP Datagram Codec & Reassembler
// ============================================================================

pub struct Hy2UdpMessage<'a> {
    pub session_id: u32,
    pub packet_id: u16,
    pub frag_id: u8,
    pub frag_count: u8,
    pub addr: String,
    pub data: &'a [u8],
}

pub fn decode_hy2_udp_message(buf: &[u8]) -> Result<Hy2UdpMessage<'_>> {
    if buf.len() < 9 {
        bail!("hy2 udp datagram too short");
    }
    let session_id = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let packet_id = u16::from_be_bytes([buf[4], buf[5]]);
    let frag_id = buf[6];
    let frag_count = buf[7];
    let (addr_len, vlen) = decode_varint_slice(&buf[8..])?;
    let addr_len = addr_len as usize;
    let addr_start = 8 + vlen;
    let addr_end = addr_start
        .checked_add(addr_len)
        .context("hy2 udp addr len overflow")?;
    if addr_end > buf.len() {
        bail!("hy2 udp addr truncated");
    }
    let addr = std::str::from_utf8(&buf[addr_start..addr_end])
        .context("hy2 udp addr utf-8")?
        .to_string();
    Ok(Hy2UdpMessage {
        session_id,
        packet_id,
        frag_id,
        frag_count,
        addr,
        data: &buf[addr_end..],
    })
}

pub fn encode_hy2_udp_datagrams(
    session_id: u32,
    packet_id: u16,
    addr: &str,
    payload: &[u8],
    max_dgram: usize,
) -> Vec<Bytes> {
    let addr_bytes = addr.as_bytes();
    let mut header_base = BytesMut::with_capacity(16 + addr_bytes.len());
    header_base.put_u32(session_id);
    header_base.put_u16(packet_id);
    header_base.put_u8(0); // frag_id placeholder
    header_base.put_u8(1); // frag_count placeholder
    encode_varint(addr_bytes.len() as u64, &mut header_base);
    header_base.put_slice(addr_bytes);

    let header_len = header_base.len();
    if header_len + payload.len() <= max_dgram {
        header_base.put_slice(payload);
        return vec![header_base.freeze()];
    }

    let max_chunk = max_dgram.saturating_sub(header_len);
    if max_chunk == 0 {
        return Vec::new();
    }
    let chunks: Vec<&[u8]> = payload.chunks(max_chunk).collect();
    if chunks.len() > u8::MAX as usize {
        return Vec::new();
    }
    let count = chunks.len() as u8;
    let mut out = Vec::with_capacity(chunks.len());
    for (idx, chunk) in chunks.into_iter().enumerate() {
        let mut pkt = BytesMut::with_capacity(header_len + chunk.len());
        pkt.put_u32(session_id);
        pkt.put_u16(packet_id);
        pkt.put_u8(idx as u8);
        pkt.put_u8(count);
        encode_varint(addr_bytes.len() as u64, &mut pkt);
        pkt.put_slice(addr_bytes);
        pkt.put_slice(chunk);
        out.push(pkt.freeze());
    }
    out
}

pub fn parse_hy2_addr(addr: &str) -> Result<TargetAddr> {
    if let Ok(sock) = addr.parse::<SocketAddr>() {
        return Ok(TargetAddr::Ip(sock));
    }
    let (host, port_str) = addr
        .rsplit_once(':')
        .with_context(|| format!("missing port in hy2 addr: {addr}"))?;
    let port: u16 = port_str
        .parse()
        .with_context(|| format!("invalid port in hy2 addr: {addr}"))?;
    let clean_host = host.trim_matches(['[', ']']);
    if let Ok(ip) = clean_host.parse::<IpAddr>() {
        return Ok(TargetAddr::Ip(SocketAddr::new(ip, port)));
    }
    if clean_host.is_empty() {
        bail!("empty host in hy2 addr: {addr}");
    }
    Ok(TargetAddr::Domain(clean_host.to_string(), port))
}

struct FragState {
    packet_id: u16,
    frag_count: u8,
    addr: String,
    frags: Vec<Option<Vec<u8>>>,
    received: u8,
}

/// Per-connection Hysteria2 UDP fragment reassembler (bounded to 512 active partial sessions).
#[derive(Default)]
pub struct Hy2UdpReassembler {
    sessions: HashMap<u32, FragState>,
}

impl Hy2UdpReassembler {
    /// Feed a decoded Hysteria2 UDP message. Returns `Some((session_id, packet_id, addr, payload))`
    /// once all fragments of the packet have arrived.
    pub fn push(&mut self, msg: Hy2UdpMessage<'_>) -> Option<(u32, u16, String, Bytes)> {
        if msg.frag_count == 0 || msg.frag_id >= msg.frag_count {
            return None;
        }
        if msg.frag_count == 1 {
            return Some((
                msg.session_id,
                msg.packet_id,
                msg.addr,
                Bytes::copy_from_slice(msg.data),
            ));
        }

        if self.sessions.len() >= 512 && !self.sessions.contains_key(&msg.session_id) {
            self.sessions.clear();
        }
        let entry = self.sessions.entry(msg.session_id).or_insert_with(|| FragState {
            packet_id: msg.packet_id,
            frag_count: msg.frag_count,
            addr: msg.addr.clone(),
            frags: vec![None; msg.frag_count as usize],
            received: 0,
        });
        if entry.packet_id != msg.packet_id || entry.frag_count != msg.frag_count {
            *entry = FragState {
                packet_id: msg.packet_id,
                frag_count: msg.frag_count,
                addr: msg.addr.clone(),
                frags: vec![None; msg.frag_count as usize],
                received: 0,
            };
        }
        let idx = msg.frag_id as usize;
        if entry.frags[idx].is_none() {
            entry.frags[idx] = Some(msg.data.to_vec());
            entry.received += 1;
        }
        if entry.received < entry.frag_count {
            return None;
        }
        let done = self.sessions.remove(&msg.session_id)?;
        let total: usize = done.frags.iter().flatten().map(Vec::len).sum();
        let mut assembled = BytesMut::with_capacity(total);
        for part in done.frags.into_iter().flatten() {
            assembled.put_slice(&part);
        }
        Some((msg.session_id, done.packet_id, done.addr, assembled.freeze()))
    }
}

// ============================================================================
// Minimal RFC 9114 HTTP/3 + RFC 9204 Static-Table QPACK + RFC 7541 Huffman
// ============================================================================

pub fn header_val<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Read an HTTP/3 HEADERS frame (skipping any GREASE / non-HEADERS frames) and decode
/// its static-table QPACK header block. Works for both requests and responses.
pub async fn read_h3_headers<R: AsyncRead + Unpin>(r: &mut R) -> Result<Vec<(String, String)>> {
    for _ in 0..8 {
        let frame_type = read_varint(r).await?;
        let frame_len = read_varint(r).await? as usize;
        if frame_len > 16 * 1024 {
            bail!("HTTP/3 frame too large: {frame_len}");
        }
        let mut payload = vec![0u8; frame_len];
        r.read_exact(&mut payload).await?;
        if frame_type == H3_FRAME_HEADERS {
            return decode_qpack_static_block(&payload);
        }
    }
    bail!("no HTTP/3 HEADERS frame found")
}

/// Write an HTTP/3 HEADERS frame for a Hysteria2 `POST /auth` request.
pub async fn write_h3_auth_request<W: AsyncWrite + Unpin>(
    w: &mut W,
    token: &str,
    padding: &str,
) -> Result<()> {
    let mut block = BytesMut::with_capacity(128 + token.len() + padding.len());
    // QPACK prefix: Required Insert Count = 0, Delta Base = 0
    block.put_u8(0x00);
    block.put_u8(0x00);
    // Indexed static 20 (:method POST) => 0xC0 | 20
    block.put_u8(0xC0 | 20);
    // Indexed static 23 (:scheme https) => 0xC0 | 23
    block.put_u8(0xC0 | 23);
    encode_qpack_literal(&mut block, ":authority", "hysteria");
    encode_qpack_literal(&mut block, ":path", "/auth");
    encode_qpack_literal(&mut block, "hysteria-auth", token);
    encode_qpack_literal(&mut block, "hysteria-cc-rx", "0");
    if !padding.is_empty() {
        encode_qpack_literal(&mut block, "hysteria-padding", padding);
    }

    let mut frame = BytesMut::with_capacity(block.len() + 16);
    encode_varint(H3_FRAME_HEADERS, &mut frame);
    encode_varint(block.len() as u64, &mut frame);
    frame.put_slice(&block);
    w.write_all(&frame).await?;
    Ok(())
}

/// Write an HTTP/3 HEADERS response frame, optionally followed by a DATA frame when `body` is non-empty.
pub async fn write_h3_response_with_body<W: AsyncWrite + Unpin>(
    w: &mut W,
    status: &str,
    extra_headers: &[(&str, &str)],
    body: &[u8],
) -> Result<()> {
    write_h3_response(w, status, extra_headers).await?;
    if !body.is_empty() {
        let mut frame = BytesMut::with_capacity(16);
        encode_varint(H3_FRAME_DATA, &mut frame);
        encode_varint(body.len() as u64, &mut frame);
        w.write_all(&frame).await?;
        w.write_all(body).await?;
    }
    Ok(())
}

/// Write an HTTP/3 HEADERS response frame.
pub async fn write_h3_response<W: AsyncWrite + Unpin>(
    w: &mut W,
    status: &str,
    extra_headers: &[(&str, &str)],
) -> Result<()> {
    let mut block = BytesMut::with_capacity(128);
    // QPACK prefix: Required Insert Count = 0, Delta Base = 0
    block.put_u8(0x00);
    block.put_u8(0x00);
    encode_qpack_literal(&mut block, ":status", status);
    for (k, v) in extra_headers {
        encode_qpack_literal(&mut block, k, v);
    }

    let mut frame = BytesMut::with_capacity(block.len() + 16);
    encode_varint(H3_FRAME_HEADERS, &mut frame);
    encode_varint(block.len() as u64, &mut frame);
    frame.put_slice(&block);
    w.write_all(&frame).await?;
    Ok(())
}

fn encode_qpack_literal(out: &mut BytesMut, name: &str, val: &str) {
    // RFC 9204 §4.5.6: Literal Field Line with Literal Name (001 N Hxxx, 3-bit prefix)
    encode_qpack_prefixed_int(out, 0x20, 3, name.len() as u64);
    out.put_slice(name.as_bytes());
    // Value string (H=0, 7-bit prefix)
    encode_qpack_prefixed_int(out, 0x00, 7, val.len() as u64);
    out.put_slice(val.as_bytes());
}

fn encode_qpack_prefixed_int(out: &mut BytesMut, first_mask: u8, prefix_bits: u8, mut val: u64) {
    let max_prefix = (1u64 << prefix_bits) - 1;
    if val < max_prefix {
        out.put_u8(first_mask | (val as u8));
    } else {
        out.put_u8(first_mask | (max_prefix as u8));
        val -= max_prefix;
        while val >= 128 {
            out.put_u8(((val & 0x7F) as u8) | 0x80);
            val >>= 7;
        }
        out.put_u8(val as u8);
    }
}

fn decode_qpack_prefixed_int(buf: &[u8], pos: &mut usize, prefix_bits: u8) -> Result<u64> {
    if *pos >= buf.len() {
        bail!("qpack unexpected EOF");
    }
    let mask = ((1u16 << prefix_bits) - 1) as u8;
    let mut val = (buf[*pos] & mask) as u64;
    *pos += 1;
    if val < mask as u64 {
        return Ok(val);
    }
    let mut shift = 0u32;
    loop {
        if *pos >= buf.len() {
            bail!("qpack truncated varint");
        }
        let b = buf[*pos];
        *pos += 1;
        val = val
            .checked_add(
                ((b & 0x7F) as u64)
                    .checked_shl(shift)
                    .context("qpack int overflow")?,
            )
            .context("qpack int overflow")?;
        if b & 0x80 == 0 {
            return Ok(val);
        }
        shift += 7;
        if shift > 56 {
            bail!("qpack varint too long");
        }
    }
}

fn decode_qpack_string(buf: &[u8], pos: &mut usize, prefix_bits: u8) -> Result<String> {
    if *pos >= buf.len() {
        bail!("qpack string EOF");
    }
    let huffman_bit = 1u8 << prefix_bits;
    let is_huffman = (buf[*pos] & huffman_bit) != 0;
    let len = decode_qpack_prefixed_int(buf, pos, prefix_bits)? as usize;
    if *pos + len > buf.len() {
        bail!("qpack string truncated");
    }
    let raw = &buf[*pos..*pos + len];
    *pos += len;
    let bytes = if is_huffman {
        huffman_decode(raw)?
    } else {
        raw.to_vec()
    };
    String::from_utf8(bytes).context("qpack header not utf-8")
}

pub fn decode_qpack_static_block(buf: &[u8]) -> Result<Vec<(String, String)>> {
    let mut pos = 0usize;
    let req_insert_count = decode_qpack_prefixed_int(buf, &mut pos, 8)?;
    let _delta_base = decode_qpack_prefixed_int(buf, &mut pos, 7)?;
    if req_insert_count != 0 {
        bail!("QPACK dynamic table not supported (RIC={req_insert_count})");
    }

    let mut headers = Vec::new();
    while pos < buf.len() {
        let b = buf[pos];
        if b & 0x80 != 0 {
            // Indexed Field Line (1 T xxxxxx)
            let is_static = (b & 0x40) != 0;
            let idx = decode_qpack_prefixed_int(buf, &mut pos, 6)? as usize;
            if is_static && let Some((k, v)) = qpack_static_entry(idx) {
                headers.push((k.to_string(), v.to_string()));
            }
        } else if b & 0xC0 == 0x40 {
            // Literal Field Line with Name Reference (01 N T xxxx)
            let is_static = (b & 0x10) != 0;
            let idx = decode_qpack_prefixed_int(buf, &mut pos, 4)? as usize;
            let val = decode_qpack_string(buf, &mut pos, 7)?;
            if is_static && let Some((k, _)) = qpack_static_entry(idx) {
                headers.push((k.to_string(), val));
            }
        } else if b & 0xE0 == 0x20 {
            // Literal Field Line with Literal Name (001 N H xxx)
            let name = decode_qpack_string(buf, &mut pos, 3)?;
            let val = decode_qpack_string(buf, &mut pos, 7)?;
            headers.push((name, val));
        } else {
            bail!("unsupported QPACK field line prefix: 0x{b:02x}");
        }
    }
    Ok(headers)
}

/// Subset of RFC 9204 Appendix A QPACK Static Table relevant to HTTP/3 requests & responses.
fn qpack_static_entry(idx: usize) -> Option<(&'static str, &'static str)> {
    Some(match idx {
        0 => (":authority", ""),
        1 => (":path", "/"),
        15 => (":method", "CONNECT"),
        16 => (":method", "DELETE"),
        17 => (":method", "GET"),
        18 => (":method", "HEAD"),
        19 => (":method", "OPTIONS"),
        20 => (":method", "POST"),
        21 => (":method", "PUT"),
        22 => (":scheme", "http"),
        23 => (":scheme", "https"),
        24 => (":status", "103"),
        25 => (":status", "200"),
        26 => (":status", "304"),
        27 => (":status", "404"),
        28 => (":status", "503"),
        29 => ("accept", "*/*"),
        31 => ("accept-encoding", "gzip, deflate, br"),
        46 => ("content-type", "application/json"),
        50 => ("host", ""),
        63 => (":status", "100"),
        64 => (":status", "204"),
        65 => (":status", "206"),
        66 => (":status", "302"),
        67 => (":status", "400"),
        68 => (":status", "403"),
        69 => (":status", "421"),
        70 => (":status", "425"),
        71 => (":status", "500"),
        95 => ("user-agent", ""),
        _ => return None,
    })
}

/// RFC 7541 Appendix B Huffman table: `(code, bit_len)` for byte values `0..=255`.
const HUFFMAN_CODES: [(u32, u8); 256] = [
    (0x1ff8, 13), (0x7fffd8, 23), (0xfffffe2, 28), (0xfffffe3, 28), (0xfffffe4, 28), (0xfffffe5, 28), (0xfffffe6, 28), (0xfffffe7, 28),
    (0xfffffe8, 28), (0xffffea, 24), (0x3ffffffc, 30), (0xfffffe9, 28), (0xfffffea, 28), (0x3ffffffd, 30), (0xfffffeb, 28), (0xfffffec, 28),
    (0xfffffed, 28), (0xfffffee, 28), (0xfffffef, 28), (0xffffff0, 28), (0xffffff1, 28), (0xffffff2, 28), (0x3ffffffe, 30), (0xffffff3, 28),
    (0xffffff4, 28), (0xffffff5, 28), (0xffffff6, 28), (0xffffff7, 28), (0xffffff8, 28), (0xffffff9, 28), (0xffffffa, 28), (0xffffffb, 28),
    (0x14, 6), (0x3f8, 10), (0x3f9, 10), (0xffa, 12), (0x1ff9, 13), (0x15, 6), (0xf8, 8), (0x7fa, 11),
    (0x3fa, 10), (0x3fb, 10), (0xf9, 8), (0x7fb, 11), (0xfa, 8), (0x16, 6), (0x17, 6), (0x18, 6),
    (0x0, 5), (0x1, 5), (0x2, 5), (0x19, 6), (0x1a, 6), (0x1b, 6), (0x1c, 6), (0x1d, 6),
    (0x1e, 6), (0x1f, 6), (0x5c, 7), (0xfb, 8), (0x7ffc, 15), (0x20, 6), (0xffb, 12), (0x3fc, 10),
    (0x1ffa, 13), (0x21, 6), (0x5d, 7), (0x5e, 7), (0x5f, 7), (0x60, 7), (0x61, 7), (0x62, 7),
    (0x63, 7), (0x64, 7), (0x65, 7), (0x66, 7), (0x67, 7), (0x68, 7), (0x69, 7), (0x6a, 7),
    (0x6b, 7), (0x6c, 7), (0x6d, 7), (0x6e, 7), (0x6f, 7), (0x70, 7), (0x71, 7), (0x72, 7),
    (0xfc, 8), (0x73, 7), (0xfd, 8), (0x1ffb, 13), (0x7fff0, 19), (0x1ffc, 13), (0x3ffc, 14), (0x22, 6),
    (0x7ffd, 15), (0x3, 5), (0x23, 6), (0x4, 5), (0x24, 6), (0x5, 5), (0x25, 6), (0x26, 6),
    (0x27, 6), (0x6, 5), (0x74, 7), (0x75, 7), (0x28, 6), (0x29, 6), (0x2a, 6), (0x7, 5),
    (0x2b, 6), (0x76, 7), (0x2c, 6), (0x8, 5), (0x9, 5), (0x2d, 6), (0x77, 7), (0x78, 7),
    (0x79, 7), (0x7a, 7), (0x7b, 7), (0x7ffe, 15), (0x7fc, 11), (0x3ffd, 14), (0x1ffd, 13), (0xffffffc, 28),
    (0xfffe6, 20), (0x3fffd2, 22), (0xfffe7, 20), (0xfffe8, 20), (0x3fffd3, 22), (0x3fffd4, 22), (0x3fffd5, 22), (0x7fffd9, 23),
    (0x3fffd6, 22), (0x7fffda, 23), (0x7fffdb, 23), (0x7fffdc, 23), (0x7fffdd, 23), (0x7fffde, 23), (0xffffeb, 24), (0x7fffdf, 23),
    (0xffffec, 24), (0xffffed, 24), (0x3fffd7, 22), (0x7fffe0, 23), (0xffffee, 24), (0x7fffe1, 23), (0x7fffe2, 23), (0x7fffe3, 23),
    (0x7fffe4, 23), (0x1fffdc, 21), (0x3fffd8, 22), (0x7fffe5, 23), (0x3fffd9, 22), (0x7fffe6, 23), (0x7fffe7, 23), (0xffffef, 24),
    (0x3fffda, 22), (0x1fffdd, 21), (0xfffe9, 20), (0x3fffdb, 22), (0x3fffdc, 22), (0x7fffe8, 23), (0x7fffe9, 23), (0x1fffde, 21),
    (0x7fffea, 23), (0x3fffdd, 22), (0x3fffde, 22), (0xfffff0, 24), (0x1fffdf, 21), (0x3fffdf, 22), (0x7fffeb, 23), (0x7fffec, 23),
    (0x1fffe0, 21), (0x1fffe1, 21), (0x3fffe0, 22), (0x1fffe2, 21), (0x7fffed, 23), (0x3fffe1, 22), (0x7fffee, 23), (0x7fffef, 23),
    (0xfffea, 20), (0x3fffe2, 22), (0x3fffe3, 22), (0x3fffe4, 22), (0x7ffff0, 23), (0x3fffe5, 22), (0x3fffe6, 22), (0x7ffff1, 23),
    (0x3ffffe0, 26), (0x3ffffe1, 26), (0xfffeb, 20), (0x7fff1, 19), (0x3fffe7, 22), (0x7ffff2, 23), (0x3fffe8, 22), (0x1ffffec, 25),
    (0x3ffffe2, 26), (0x3ffffe3, 26), (0x3ffffe4, 26), (0x7ffffde, 27), (0x7ffffdf, 27), (0x3ffffe5, 26), (0xfffff1, 24), (0x1ffffed, 25),
    (0x7fff2, 19), (0x1fffe3, 21), (0x3ffffe6, 26), (0x7ffffe0, 27), (0x7ffffe1, 27), (0x3ffffe7, 26), (0x7ffffe2, 27), (0xfffff2, 24),
    (0x1fffe4, 21), (0x1fffe5, 21), (0x3ffffe8, 26), (0x3ffffe9, 26), (0xffffffd, 28), (0x7ffffe3, 27), (0x7ffffe4, 27), (0x7ffffe5, 27),
    (0xfffec, 20), (0xfffff3, 24), (0xfffed, 20), (0x1fffe6, 21), (0x3fffe9, 22), (0x1fffe7, 21), (0x1fffe8, 21), (0x7ffff3, 23),
    (0x3fffea, 22), (0x3fffeb, 22), (0x1ffffee, 25), (0x1ffffef, 25), (0xfffff4, 24), (0xfffff5, 24), (0x3ffffea, 26), (0x7ffff4, 23),
    (0x3ffffeb, 26), (0x7ffffe6, 27), (0x3ffffec, 26), (0x3ffffed, 26), (0x7ffffe7, 27), (0x7ffffe8, 27), (0x7ffffe9, 27), (0x7ffffea, 27),
    (0x7ffffeb, 27), (0xffffffe, 28), (0x7ffffec, 27), (0x7ffffed, 27), (0x7ffffee, 27), (0x7ffffef, 27), (0x7fffff0, 27), (0x3ffffee, 26),
];

fn huffman_lookup() -> &'static HashMap<(u8, u32), u8> {
    static TABLE: std::sync::LazyLock<HashMap<(u8, u32), u8>> = std::sync::LazyLock::new(|| {
        let mut m = HashMap::with_capacity(256);
        for (sym, &(code, len)) in HUFFMAN_CODES.iter().enumerate() {
            m.insert((len, code), sym as u8);
        }
        m
    });
    &TABLE
}

fn huffman_decode(input: &[u8]) -> Result<Vec<u8>> {
    let table = huffman_lookup();
    let mut out = Vec::with_capacity(input.len());
    let mut acc: u32 = 0;
    let mut bits: u8 = 0;

    for &byte in input {
        for shift in (0..8).rev() {
            acc = (acc << 1) | (((byte >> shift) & 1) as u32);
            bits += 1;
            if bits >= 5 && let Some(&sym) = table.get(&(bits, acc)) {
                out.push(sym);
                acc = 0;
                bits = 0;
            } else if bits > 30 {
                bail!("invalid RFC 7541 Huffman sequence");
            }
        }
    }

    if bits > 7 {
        bail!("invalid Huffman padding length: {bits}");
    }
    if bits > 0 {
        let expected_pad = (1u32 << bits) - 1;
        if acc != expected_pad {
            bail!("invalid Huffman EOS padding");
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn huffman_encode(input: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut cur: u8 = 0;
        let mut cur_bits: u8 = 0;
        for &b in input {
            let (code, len) = HUFFMAN_CODES[b as usize];
            for i in (0..len).rev() {
                cur = (cur << 1) | (((code >> i) & 1) as u8);
                cur_bits += 1;
                if cur_bits == 8 {
                    out.push(cur);
                    cur = 0;
                    cur_bits = 0;
                }
            }
        }
        if cur_bits > 0 {
            cur = (cur << (8 - cur_bits)) | ((1u8 << (8 - cur_bits)) - 1);
            out.push(cur);
        }
        out
    }

    #[test]
    fn huffman_roundtrip_hy2_headers() {
        for s in ["/auth", "hysteria-auth", "iWIQciLnamSRXTdBhF2ka6ov", "233", "www.goldpeak.top"] {
            let enc = huffman_encode(s.as_bytes());
            let dec = huffman_decode(&enc).unwrap();
            assert_eq!(dec, s.as_bytes(), "failed for {s}");
        }
    }

    #[test]
    fn qpack_decodes_huffman_and_static_hy2_auth() {
        let mut blk = BytesMut::new();
        blk.put_u8(0x00);
        blk.put_u8(0x00);
        blk.put_u8(0xC0 | 20);
        blk.put_u8(0x51);
        let path_h = huffman_encode(b"/auth");
        encode_qpack_prefixed_int(&mut blk, 0x80, 7, path_h.len() as u64);
        blk.put_slice(&path_h);
        let name_h = huffman_encode(b"hysteria-auth");
        encode_qpack_prefixed_int(&mut blk, 0x28, 3, name_h.len() as u64);
        blk.put_slice(&name_h);
        let val_h = huffman_encode(b"tok-12345");
        encode_qpack_prefixed_int(&mut blk, 0x80, 7, val_h.len() as u64);
        blk.put_slice(&val_h);

        let headers = decode_qpack_static_block(&blk).unwrap();
        assert_eq!(header_val(&headers, ":method"), Some("POST"));
        assert_eq!(header_val(&headers, ":path"), Some("/auth"));
        assert_eq!(header_val(&headers, "hysteria-auth"), Some("tok-12345"));
    }

    #[tokio::test]
    async fn h3_auth_request_and_response_roundtrip() {
        let mut buf = Vec::new();
        write_h3_auth_request(&mut buf, "secret-token", "pad123").await.unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let headers = read_h3_headers(&mut cursor).await.unwrap();
        assert_eq!(header_val(&headers, ":method"), Some("POST"));
        assert_eq!(header_val(&headers, ":path"), Some("/auth"));
        assert_eq!(header_val(&headers, "hysteria-auth"), Some("secret-token"));
        assert_eq!(header_val(&headers, "hysteria-padding"), Some("pad123"));

        let mut resp = Vec::new();
        write_h3_response(&mut resp, "233", &[("hysteria-udp", "true")]).await.unwrap();
        let mut resp_cur = std::io::Cursor::new(resp);
        let resp_h = read_h3_headers(&mut resp_cur).await.unwrap();
        assert_eq!(header_val(&resp_h, ":status"), Some("233"));
        assert_eq!(header_val(&resp_h, "hysteria-udp"), Some("true"));
    }

    #[test]
    fn hy2_udp_datagram_fragmentation_and_reassembly() {
        let payload: Vec<u8> = (0..2500).map(|i| (i % 251) as u8).collect();
        let pkts = encode_hy2_udp_datagrams(42, 7, "1.1.1.1:53", &payload, 1000);
        assert!(pkts.len() >= 3);
        let mut reasm = Hy2UdpReassembler::default();
        let mut out = None;
        for p in &pkts {
            let m = decode_hy2_udp_message(p).unwrap();
            if let Some(done) = reasm.push(m) {
                out = Some(done);
            }
        }
        let (sid, pid, addr, data) = out.expect("reassembled");
        assert_eq!(sid, 42);
        assert_eq!(pid, 7);
        assert_eq!(addr, "1.1.1.1:53");
        assert_eq!(data.as_ref(), payload.as_slice());
    }
}
