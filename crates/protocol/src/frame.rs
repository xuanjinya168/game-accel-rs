use anyhow::{Context, Result, bail};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const STREAM_TYPE_TCP: u8 = 0x01;

pub const ATYP_IPV4: u8 = 0x01;
pub const ATYP_DOMAIN: u8 = 0x03;
pub const ATYP_IPV6: u8 = 0x04;

pub const FLAG_DUP: u8 = 0b0000_0001;
pub const FLAG_GAME_MODE: u8 = 0b0000_0010;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetAddr {
    Ip(SocketAddr),
    Domain(String, u16),
}

impl TargetAddr {
    pub fn to_host_port(&self) -> String {
        match self {
            TargetAddr::Ip(addr) => addr.to_string(),
            TargetAddr::Domain(host, port) => format!("{host}:{port}"),
        }
    }

    pub fn encode_into(&self, buf: &mut BytesMut) -> Result<()> {
        match self {
            TargetAddr::Ip(SocketAddr::V4(v4)) => {
                buf.put_u8(ATYP_IPV4);
                buf.put_slice(&v4.ip().octets());
                buf.put_u16(v4.port());
            }
            TargetAddr::Ip(SocketAddr::V6(v6)) => {
                buf.put_u8(ATYP_IPV6);
                buf.put_slice(&v6.ip().octets());
                buf.put_u16(v6.port());
            }
            TargetAddr::Domain(domain, port) => {
                let bytes = domain.as_bytes();
                if bytes.len() > u8::MAX as usize {
                    bail!("domain too long: {domain}");
                }
                buf.put_u8(ATYP_DOMAIN);
                buf.put_u8(bytes.len() as u8);
                buf.put_slice(bytes);
                buf.put_u16(*port);
            }
        }
        Ok(())
    }

    pub fn decode_from(buf: &mut Bytes) -> Result<Self> {
        if !buf.has_remaining() {
            bail!("empty target address");
        }
        let atyp = buf.get_u8();
        match atyp {
            ATYP_IPV4 => {
                if buf.remaining() < 6 {
                    bail!("truncated ipv4 address");
                }
                let ip = Ipv4Addr::new(buf.get_u8(), buf.get_u8(), buf.get_u8(), buf.get_u8());
                let port = buf.get_u16();
                Ok(TargetAddr::Ip(SocketAddr::from((ip, port))))
            }
            ATYP_IPV6 => {
                if buf.remaining() < 18 {
                    bail!("truncated ipv6 address");
                }
                let mut octets = [0u8; 16];
                buf.copy_to_slice(&mut octets);
                let ip = Ipv6Addr::from(octets);
                let port = buf.get_u16();
                Ok(TargetAddr::Ip(SocketAddr::from((ip, port))))
            }
            ATYP_DOMAIN => {
                if !buf.has_remaining() {
                    bail!("truncated domain len");
                }
                let len = buf.get_u8() as usize;
                if buf.remaining() < len + 2 {
                    bail!("truncated domain bytes");
                }
                let domain_bytes = buf.split_to(len);
                let domain =
                    String::from_utf8(domain_bytes.to_vec()).context("invalid utf-8 domain")?;
                let port = buf.get_u16();
                Ok(TargetAddr::Domain(domain, port))
            }
            other => bail!("unknown address type: {other}"),
        }
    }

    pub async fn write_tcp_open<W: AsyncWrite + Unpin>(&self, writer: &mut W) -> Result<()> {
        let mut buf = BytesMut::with_capacity(64);
        buf.put_u8(STREAM_TYPE_TCP);
        self.encode_into(&mut buf)?;
        writer.write_all(&buf).await.context("write tcp open frame")
    }

    pub async fn read_tcp_open<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Self> {
        let stream_type = reader.read_u8().await.context("read stream type")?;
        if stream_type != STREAM_TYPE_TCP {
            bail!("unexpected stream type: {stream_type}");
        }
        let atyp = reader.read_u8().await.context("read atyp")?;
        match atyp {
            ATYP_IPV4 => {
                let mut raw = [0u8; 6];
                reader.read_exact(&mut raw).await?;
                let ip = Ipv4Addr::new(raw[0], raw[1], raw[2], raw[3]);
                let port = u16::from_be_bytes([raw[4], raw[5]]);
                Ok(TargetAddr::Ip(SocketAddr::from((ip, port))))
            }
            ATYP_IPV6 => {
                let mut raw = [0u8; 18];
                reader.read_exact(&mut raw).await?;
                let mut octets = [0u8; 16];
                octets.copy_from_slice(&raw[0..16]);
                let ip = Ipv6Addr::from(octets);
                let port = u16::from_be_bytes([raw[16], raw[17]]);
                Ok(TargetAddr::Ip(SocketAddr::from((ip, port))))
            }
            ATYP_DOMAIN => {
                let len = reader.read_u8().await? as usize;
                let mut raw = vec![0u8; len + 2];
                reader.read_exact(&mut raw).await?;
                let domain =
                    String::from_utf8(raw[..len].to_vec()).context("invalid utf-8 domain")?;
                let port = u16::from_be_bytes([raw[len], raw[len + 1]]);
                Ok(TargetAddr::Domain(domain, port))
            }
            other => bail!("unknown atyp: {other}"),
        }
    }
}

/// Compact UDP frame carried inside QUIC Datagram (RFC 9221).
/// Format: [FlowID: 2B][Seq: 2B][Flags: 1B][TargetAddr][Payload]
#[derive(Debug, Clone)]
pub struct UdpFrame {
    pub flow_id: u16,
    pub seq: u16,
    pub flags: u8,
    pub addr: TargetAddr,
    pub payload: Bytes,
}

impl UdpFrame {
    pub fn encode(&self) -> Result<Bytes> {
        let mut buf = BytesMut::with_capacity(32 + self.payload.len());
        buf.put_u16(self.flow_id);
        buf.put_u16(self.seq);
        buf.put_u8(self.flags);
        self.addr.encode_into(&mut buf)?;
        buf.put_slice(&self.payload);
        Ok(buf.freeze())
    }

    pub fn decode(mut data: Bytes) -> Result<Self> {
        if data.remaining() < 5 {
            bail!("udp frame too short");
        }
        let flow_id = data.get_u16();
        let seq = data.get_u16();
        let flags = data.get_u8();
        let addr = TargetAddr::decode_from(&mut data)?;
        Ok(Self {
            flow_id,
            seq,
            flags,
            addr,
            payload: data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn udp_frame_roundtrip() {
        let frame = UdpFrame {
            flow_id: 42,
            seq: 1337,
            flags: FLAG_GAME_MODE | FLAG_DUP,
            addr: TargetAddr::Domain("youtube.com".into(), 443),
            payload: Bytes::from_static(b"hello-game"),
        };
        let encoded = frame.encode().unwrap();
        let decoded = UdpFrame::decode(encoded).unwrap();
        assert_eq!(decoded.flow_id, 42);
        assert_eq!(decoded.seq, 1337);
        assert_eq!(decoded.flags, FLAG_GAME_MODE | FLAG_DUP);
        assert_eq!(decoded.addr, TargetAddr::Domain("youtube.com".into(), 443));
        assert_eq!(decoded.payload, Bytes::from_static(b"hello-game"));
    }
}
