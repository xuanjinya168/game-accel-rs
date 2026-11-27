//! `TunnelSession` deep module (*A Philosophy of Software Design*, Ousterhout).
//!
//! Hides dual-lane QUIC connection separation (`tcp_conn` vs `udp_conn`), adaptive `< 200B`
//! packet duplication pacing, UDP flow-ID partitioning, and `GACL` vs `Hysteria2` wire encoding
//! behind a narrow, protocol-agnostic interface consumed by SOCKS5, HTTP proxy, and Wintun TUN.

use crate::relay::{
    ClientStats, SOCKS_FLOW_MAX, SOCKS_FLOW_MIN, TUN_FLOW_MAX, TUN_FLOW_MIN, UdpDispatchMap,
    open_proxy_tcp_stream, relay_fast_open, reserve_flow_id,
};
use crate::rules::Route;
use anyhow::Result;
use bytes::Bytes;
use protocol::frame::{FLAG_GAME_MODE, TargetAddr, UdpFrame};
use protocol::pacer::{UdpDupPacer, send_datagram_checked};
use protocol::share::NodeProtocol;
use quinn::Connection;
use std::sync::Arc;
use std::sync::atomic::AtomicU16;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

pub struct TunnelSession {
    tcp_conn: Connection,
    udp_conn: Connection,
    pacer: UdpDupPacer,
    udp_dispatch: UdpDispatchMap,
    protocol: NodeProtocol,
    game_dup: bool,
}

/// Both upstream tunnels of the session, keyed by the router's verdict.
pub struct Tunnels {
    primary: Arc<TunnelSession>,
    secondary: Option<Arc<TunnelSession>>,
}

impl Tunnels {
    pub fn new(primary: Arc<TunnelSession>, secondary: Option<Arc<TunnelSession>>) -> Arc<Self> {
        Arc::new(Self { primary, secondary })
    }

    pub fn primary(&self) -> &Arc<TunnelSession> {
        &self.primary
    }

    pub fn secondary(&self) -> Option<&Arc<TunnelSession>> {
        self.secondary.as_ref()
    }

    /// Tunnel that should carry `route`. `Direct` yields `None`; a missing domestic lane also
    /// yields `None`, so callers never send domestic traffic through the foreign node.
    pub fn for_route(&self, route: Route) -> Option<&Arc<TunnelSession>> {
        match route {
            Route::Primary => Some(&self.primary),
            Route::Secondary => self.secondary.as_ref(),
            Route::Direct => None,
        }
    }
}

impl TunnelSession {
    pub(crate) fn new(
        tcp_conn: Connection,
        udp_conn: Connection,
        pacer: UdpDupPacer,
        udp_dispatch: UdpDispatchMap,
        protocol: NodeProtocol,
        game_dup: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            tcp_conn,
            udp_conn,
            pacer,
            udp_dispatch,
            protocol,
            game_dup: game_dup && protocol == NodeProtocol::Gacl,
        })
    }

    pub fn game_dup_enabled(&self) -> bool {
        self.game_dup
    }

    /// Shared duplicate-send pacer of this tunnel's UDP lane (used by the health sampler).
    pub fn pacer(&self) -> &UdpDupPacer {
        &self.pacer
    }

    /// Whether inbound UDP frames carry monotonic dedup sequence numbers (`GACL` only).
    pub fn should_dedup_rx(&self) -> bool {
        self.protocol == NodeProtocol::Gacl
    }

    pub fn dispatch_map(&self) -> &UdpDispatchMap {
        &self.udp_dispatch
    }

    pub fn reserve_socks_flow(
        &self,
        cursor: &AtomicU16,
        tx: mpsc::Sender<UdpFrame>,
    ) -> Option<u16> {
        reserve_flow_id(
            cursor,
            &self.udp_dispatch,
            tx,
            SOCKS_FLOW_MIN,
            SOCKS_FLOW_MAX,
        )
    }

    pub fn reserve_tun_flow(&self, cursor: &AtomicU16, tx: mpsc::Sender<UdpFrame>) -> Option<u16> {
        reserve_flow_id(cursor, &self.udp_dispatch, tx, TUN_FLOW_MIN, TUN_FLOW_MAX)
    }

    pub fn release_flow(&self, flow_id: u16) {
        self.udp_dispatch.remove(&flow_id);
    }

    /// Opens a proxied TCP bidirectional stream on the bulk TCP QUIC lane and writes the
    /// protocol-appropriate open header (`GACL` or `Hysteria2 0x401`).
    pub async fn open_tcp(
        &self,
        target: &TargetAddr,
    ) -> Result<(quinn::SendStream, quinn::RecvStream)> {
        open_proxy_tcp_stream(&self.tcp_conn, self.protocol, target).await
    }

    /// Splices `stream` with an already-opened fast-open QUIC TCP stream, verifying dial status.
    pub async fn relay_tcp<S>(
        &self,
        stream: S,
        quic_send: quinn::SendStream,
        quic_recv: quinn::RecvStream,
        stats: Arc<ClientStats>,
        fail_reply: Option<&'static [u8]>,
    ) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        relay_fast_open(
            stream,
            quic_send,
            quic_recv,
            self.protocol,
            stats,
            fail_reply,
        )
        .await
    }

    /// Sends a proxied UDP datagram on the dedicated low-latency UDP QUIC lane, engaging adaptive
    /// `< 200B` dual-send on `GACL` or automatic datagram fragmentation on `Hysteria2`.
    /// Returns `false` if the underlying QUIC connection is closed.
    pub fn send_udp(
        &self,
        flow_id: u16,
        seq: u16,
        is_game_pkt: bool,
        dst: TargetAddr,
        payload: Bytes,
    ) -> bool {
        let use_game_dup = self.game_dup && is_game_pkt;
        match self.protocol {
            NodeProtocol::Gacl => {
                let flags = if use_game_dup { FLAG_GAME_MODE } else { 0 };
                let frame = UdpFrame {
                    flow_id,
                    seq,
                    flags,
                    addr: dst,
                    payload,
                };
                if let Ok(encoded) = frame.encode() {
                    if !send_datagram_checked(&self.udp_conn, encoded.clone()) {
                        return false;
                    }
                    if use_game_dup {
                        self.pacer.schedule_dup(encoded);
                    }
                }
                true
            }
            NodeProtocol::Hysteria2 => {
                let max_dgram = self.udp_conn.max_datagram_size().unwrap_or(1150);
                let addr_str = dst.to_host_port();
                let pkts = protocol::hy2::encode_hy2_udp_datagrams(
                    flow_id as u32,
                    seq,
                    &addr_str,
                    &payload,
                    max_dgram,
                );
                for pkt in pkts {
                    if !send_datagram_checked(&self.udp_conn, pkt) {
                        return false;
                    }
                }
                true
            }
        }
    }
}
