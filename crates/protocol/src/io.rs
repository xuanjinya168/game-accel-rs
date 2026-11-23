use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const COPY_BUF_SIZE: usize = 16 * 1024;

/// Copy bytes from `reader` to `writer` in 16 KB chunks, updating `counter` in real time
/// after every chunk instead of waiting for EOF.
///
/// Unlike `tokio::io::copy`, this:
/// 1. Keeps live throughput / traffic meters accurate during long-lived HTTP/2, YouTube,
///    or WebSocket TCP streams;
/// 2. Preserves all bytes already transferred even if the peer resets the connection.
pub async fn copy_with_counter<R, W>(
    reader: &mut R,
    writer: &mut W,
    counter: &AtomicU64,
) -> u64
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; COPY_BUF_SIZE];
    let mut total = 0u64;
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if writer.write_all(&buf[..n]).await.is_err() {
                    break;
                }
                let n64 = n as u64;
                counter.fetch_add(n64, Ordering::Relaxed);
                total += n64;
            }
            Err(_) => break,
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::duplex;

    #[tokio::test]
    async fn updates_counter_before_eof_and_keeps_bytes_on_reset() {
        let (mut src_w, mut src_r) = duplex(1024);
        let (mut dst_w, mut dst_r) = duplex(1024);
        let counter = Arc::new(AtomicU64::new(0));
        let counter_bg = counter.clone();

        let copy_task = tokio::spawn(async move {
            copy_with_counter(&mut src_r, &mut dst_w, &counter_bg).await
        });

        // Send first chunk WITHOUT closing the stream (simulating long-lived HTTP/2 / YouTube)
        src_w.write_all(&vec![b'A'; 400]).await.unwrap();
        let mut received = vec![0u8; 400];
        dst_r.read_exact(&mut received).await.unwrap();
        assert_eq!(
            counter.load(Ordering::Relaxed),
            400,
            "counter must update immediately while stream is still open"
        );

        // Drop destination reader to force a write error on next chunk (simulating TCP RST)
        drop(dst_r);
        let _ = src_w.write_all(&vec![b'B'; 200]).await;
        drop(src_w);

        let total = copy_task.await.unwrap();
        assert!(total >= 400, "already-delivered bytes must not be lost on error");
        assert_eq!(counter.load(Ordering::Relaxed), total);
    }
}
