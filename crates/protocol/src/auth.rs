use anyhow::{Context, Result, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const AUTH_MAGIC: &[u8; 4] = b"GACL";
pub const PROTOCOL_VERSION: u8 = 1;
pub const AUTH_OK: u8 = 0x00;
pub const AUTH_ERR: u8 = 0x01;

pub async fn write_auth_request<W: AsyncWrite + Unpin>(writer: &mut W, token: &str) -> Result<()> {
    let token_bytes = token.as_bytes();
    if token_bytes.len() > u8::MAX as usize {
        bail!("token too long (max 255 bytes)");
    }
    let mut buf = Vec::with_capacity(4 + 1 + 1 + token_bytes.len());
    buf.extend_from_slice(AUTH_MAGIC);
    buf.push(PROTOCOL_VERSION);
    buf.push(token_bytes.len() as u8);
    buf.extend_from_slice(token_bytes);
    writer.write_all(&buf).await.context("write auth request")?;
    Ok(())
}

pub async fn read_auth_request<R: AsyncRead + Unpin>(reader: &mut R) -> Result<String> {
    let mut magic = [0u8; 4];
    reader.read_exact(&mut magic).await.context("read auth magic")?;
    if &magic != AUTH_MAGIC {
        bail!("invalid auth magic");
    }
    read_auth_request_after_magic(reader).await
}

/// Read the remainder of a GACL auth request after the 4-byte `AUTH_MAGIC` prefix
/// has already been consumed (used by dual-protocol servers dispatching GACL vs Hysteria2).
pub async fn read_auth_request_after_magic<R: AsyncRead + Unpin>(reader: &mut R) -> Result<String> {
    let mut header = [0u8; 2];
    reader.read_exact(&mut header).await.context("read auth header")?;
    if header[0] != PROTOCOL_VERSION {
        bail!("unsupported protocol version: {}", header[0]);
    }
    let token_len = header[1] as usize;
    let mut token_buf = vec![0u8; token_len];
    reader
        .read_exact(&mut token_buf)
        .await
        .context("read auth token")?;
    String::from_utf8(token_buf).context("auth token is not valid utf-8")
}
