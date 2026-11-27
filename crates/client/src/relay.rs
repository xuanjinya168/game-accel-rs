use crate::rules::Router;
use anyhow::{Context, Result, bail};
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use protocol::auth::AUTH_OK;
use protocol::domain::LinkQuality;
use protocol::frame::{TargetAddr, UdpFrame};
use protocol::share::NodeProtocol;
use quinn::Connection;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

pub type UdpDispatchMap = Arc<DashMap<u16, mpsc::Sender<UdpFrame>>>;

pub(crate) const SOCKS_FLOW_MIN: u16 = 1;
pub(crate) const SOCKS_FLOW_MAX: u16 = 32767;
pub(crate) const TUN_FLOW_MIN: u16 = 32768;
pub(crate) const TUN_FLOW_MAX: u16 = 65534;

/// Timeout for DIRECT (non-tunnelled) TCP dials from this machine.
pub(crate) const DIRECT_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug, Default)]
pub struct ClientStats {
    pub connected: AtomicBool,
    pub active_tcp: AtomicU64,
    pub active_udp: AtomicU64,
    pub tx_bytes: AtomicU64,
    pub rx_bytes: AtomicU64,
    pub rtt_ms: AtomicU64,
    /// Smoothed RFC 3550 jitter in tenths of a millisecond (`18` => `±1.8 ms`).
    pub jitter_tenths_ms: AtomicU32,
    /// Recent UDP lane packet loss ratio in basis points (`100` => `1.0%`).
    pub loss_bps: AtomicU32,
    /// Whether adaptive `< 200B` game packet dual-send is currently engaged.
    pub dup_active: AtomicBool,
    /// Unix timestamp (seconds) when the current tunnel session authenticated.
    pub connected_at_unix: AtomicU64,
    /// Live status of the optional domestic (secondary) tunnel in `Dual` mode.
    pub secondary_connected: AtomicBool,
    pub secondary_rtt_ms: AtomicU64,
}

impl ClientStats {
    pub fn reset(&self) {
        self.connected.store(false, Ordering::Relaxed);
        self.active_tcp.store(0, Ordering::Relaxed);
        self.active_udp.store(0, Ordering::Relaxed);
        self.tx_bytes.store(0, Ordering::Relaxed);
        self.rx_bytes.store(0, Ordering::Relaxed);
        self.rtt_ms.store(0, Ordering::Relaxed);
        self.jitter_tenths_ms.store(0, Ordering::Relaxed);
        self.loss_bps.store(0, Ordering::Relaxed);
        self.dup_active.store(false, Ordering::Relaxed);
        self.connected_at_unix.store(0, Ordering::Relaxed);
        self.secondary_connected.store(false, Ordering::Relaxed);
        self.secondary_rtt_ms.store(0, Ordering::Relaxed);
    }

    pub fn link_quality(&self) -> LinkQuality {
        LinkQuality {
            rtt_ms: self.rtt_ms.load(Ordering::Relaxed),
            jitter_tenths_ms: self.jitter_tenths_ms.load(Ordering::Relaxed),
            loss_bps: self.loss_bps.load(Ordering::Relaxed),
            dup_active: self.dup_active.load(Ordering::Relaxed),
        }
    }

    pub fn uptime_secs(&self) -> u64 {
        if !self.connected.load(Ordering::Relaxed) {
            return 0;
        }
        let started = self.connected_at_unix.load(Ordering::Relaxed);
        if started == 0 {
            0
        } else {
            protocol::unix_now_secs().saturating_sub(started)
        }
    }
}

/// Atomically reserve an unused `flow_id` strictly within `[min_id, max_id]` and insert `tx`
/// into `dispatch`, skipping any currently occupied IDs even across wraparound.
pub(crate) fn reserve_flow_id(
    cursor: &AtomicU16,
    dispatch: &UdpDispatchMap,
    tx: mpsc::Sender<UdpFrame>,
    min_id: u16,
    max_id: u16,
) -> Option<u16> {
    let span = (max_id - min_id) as usize + 1;
    let mut slot = Some(tx);
    for _ in 0..span {
        let prev = cursor
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                let clamped = if cur < min_id || cur > max_id {
                    min_id
                } else {
                    cur
                };
                let next = if clamped >= max_id {
                    min_id
                } else {
                    clamped + 1
                };
                Some(next)
            })
            .unwrap_or(min_id);
        let id = if prev < min_id || prev > max_id {
            min_id
        } else {
            prev
        };
        if let Entry::Vacant(v) = dispatch.entry(id) {
            v.insert(slot.take().expect("sender present"));
            return Some(id);
        }
    }
    None
}

/// Open a proxied TCP bidirectional QUIC stream and write the protocol-specific
/// open header (`GACL` or Hysteria2 `0x401`).
pub(crate) async fn open_proxy_tcp_stream(
    conn: &Connection,
    protocol: NodeProtocol,
    target: &TargetAddr,
) -> Result<(quinn::SendStream, quinn::RecvStream)> {
    let (mut quic_send, quic_recv) = conn.open_bi().await.context("open quic bi stream")?;
    match protocol {
        NodeProtocol::Gacl => target.write_tcp_open(&mut quic_send).await?,
        NodeProtocol::Hysteria2 => {
            protocol::hy2::write_hy2_tcp_request(&mut quic_send, target, 0).await?;
        }
    }
    Ok((quic_send, quic_recv))
}

/// Fast-open TCP relay over an already-opened QUIC stream whose open header has been
/// written but whose dial status has NOT been awaited.
pub(crate) async fn relay_fast_open<S>(
    stream: S,
    mut quic_send: quinn::SendStream,
    mut quic_recv: quinn::RecvStream,
    protocol: NodeProtocol,
    stats: Arc<ClientStats>,
    fail_reply: Option<&'static [u8]>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    stats.active_tcp.fetch_add(1, Ordering::Relaxed);
    let (mut s_read, mut s_write) = tokio::io::split(stream);

    let stats_u = stats.clone();
    let u = tokio::spawn(async move {
        let _ =
            protocol::io::copy_with_counter(&mut s_read, &mut quic_send, &stats_u.tx_bytes).await;
        let _ = quic_send.finish();
    });

    let stats_d = stats.clone();
    let d = tokio::spawn(async move {
        let ok = match protocol {
            NodeProtocol::Gacl => matches!(quic_recv.read_u8().await, Ok(AUTH_OK)),
            NodeProtocol::Hysteria2 => protocol::hy2::read_hy2_tcp_response(&mut quic_recv)
                .await
                .is_ok(),
        };
        if ok {
            let _ =
                protocol::io::copy_with_counter(&mut quic_recv, &mut s_write, &stats_d.rx_bytes)
                    .await;
        } else if let Some(reply) = fail_reply {
            let _ = s_write.write_all(reply).await;
        }
        let _ = s_write.shutdown().await;
        ok
    });

    let ok = d.await.unwrap_or(false);
    if ok {
        let _ = u.await;
    } else {
        u.abort();
    }
    stats.active_tcp.fetch_sub(1, Ordering::Relaxed);
    if ok {
        Ok(())
    } else {
        bail!("remote failed to connect")
    }
}

/// DIRECT outbound: dial `target` from this machine (pinned to physical NIC when TUN is
/// active so it never loops back into TUN), then splice.
pub(crate) async fn relay_direct(
    router: &Router,
    mut client: TcpStream,
    target: &TargetAddr,
    ok_reply: Option<&[u8]>,
    upstream_preamble: &[u8],
    fail_reply: Option<&[u8]>,
) -> Result<()> {
    let mut upstream = match router
        .dialer
        .connect_target(target, DIRECT_CONNECT_TIMEOUT)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            if let Some(reply) = fail_reply {
                let _ = client.write_all(reply).await;
            }
            bail!("direct connect {} failed: {e}", target.to_host_port());
        }
    };
    if let Some(reply) = ok_reply {
        client.write_all(reply).await?;
    }
    if !upstream_preamble.is_empty() {
        upstream.write_all(upstream_preamble).await?;
    }
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flow_id_partition_wraps_and_skips_occupied() {
        let dispatch: UdpDispatchMap = Arc::new(DashMap::new());
        let socks_cur = AtomicU16::new(SOCKS_FLOW_MAX - 1);
        let tun_cur = AtomicU16::new(TUN_FLOW_MAX - 1);

        let (tx0, _rx0) = mpsc::channel(1);
        dispatch.insert(SOCKS_FLOW_MIN, tx0);

        let id1 = reserve_flow_id(
            &socks_cur,
            &dispatch,
            mpsc::channel(1).0,
            SOCKS_FLOW_MIN,
            SOCKS_FLOW_MAX,
        )
        .unwrap();
        let id2 = reserve_flow_id(
            &socks_cur,
            &dispatch,
            mpsc::channel(1).0,
            SOCKS_FLOW_MIN,
            SOCKS_FLOW_MAX,
        )
        .unwrap();
        let id3 = reserve_flow_id(
            &socks_cur,
            &dispatch,
            mpsc::channel(1).0,
            SOCKS_FLOW_MIN,
            SOCKS_FLOW_MAX,
        )
        .unwrap();
        assert_eq!(id1, SOCKS_FLOW_MAX - 1);
        assert_eq!(id2, SOCKS_FLOW_MAX);
        assert_eq!(id3, SOCKS_FLOW_MIN + 1);
        assert!(id3 <= SOCKS_FLOW_MAX);

        let t1 = reserve_flow_id(
            &tun_cur,
            &dispatch,
            mpsc::channel(1).0,
            TUN_FLOW_MIN,
            TUN_FLOW_MAX,
        )
        .unwrap();
        let t2 = reserve_flow_id(
            &tun_cur,
            &dispatch,
            mpsc::channel(1).0,
            TUN_FLOW_MIN,
            TUN_FLOW_MAX,
        )
        .unwrap();
        let t3 = reserve_flow_id(
            &tun_cur,
            &dispatch,
            mpsc::channel(1).0,
            TUN_FLOW_MIN,
            TUN_FLOW_MAX,
        )
        .unwrap();
        assert_eq!(t1, TUN_FLOW_MAX - 1);
        assert_eq!(t2, TUN_FLOW_MAX);
        assert_eq!(t3, TUN_FLOW_MIN);

        let tiny_cur = AtomicU16::new(10);
        let tiny_map: UdpDispatchMap = Arc::new(DashMap::new());
        assert_eq!(
            reserve_flow_id(&tiny_cur, &tiny_map, mpsc::channel(1).0, 10, 11),
            Some(10)
        );
        assert_eq!(
            reserve_flow_id(&tiny_cur, &tiny_map, mpsc::channel(1).0, 10, 11),
            Some(11)
        );
        assert_eq!(
            reserve_flow_id(&tiny_cur, &tiny_map, mpsc::channel(1).0, 10, 11),
            None
        );
    }
}
