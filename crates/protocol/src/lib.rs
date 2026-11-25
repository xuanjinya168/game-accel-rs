pub mod auth;
pub mod dedup;
pub mod frame;
pub mod hy2;
pub mod io;
pub mod pacer;
pub mod share;
pub mod sniff;

pub const ALPN_PROTOCOL: &[u8] = b"h3";
pub const DEFAULT_INITIAL_MTU: u16 = 1200;

/// TUN interface MTU. Kept well below the QUIC datagram payload limit at
/// `DEFAULT_INITIAL_MTU` (~1160B minus a 12–24B `UdpFrame` header), so a full-size
/// UDP packet from the TUN never exceeds `max_datagram_size` and gets dropped
/// before (or without) path MTU discovery raising the limit.
pub const TUN_MTU: u16 = 1120;

/// QUIC keep-alive interval: frequent enough to keep NAT mappings warm and to feed
/// loss detection so a dead path is noticed quickly.
pub const KEEP_ALIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(4);

/// QUIC idle timeout: how long a black-holed path takes to be declared dead and
/// trigger a reconnect (was 60s, far too long for a game session).
pub const MAX_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Outgoing QUIC datagram queue. Kept small on purpose: under congestion a stale
/// game packet is worthless, and quinn drops the oldest queued datagrams once the
/// buffer is full, which bounds queuing delay instead of building seconds of bloat.
pub const DATAGRAM_SEND_BUFFER: usize = 128 * 1024;

/// Incoming QUIC datagram queue (application read side).
pub const DATAGRAM_RECV_BUFFER: usize = 1024 * 1024;

/// Current Unix timestamp in seconds (monotonic-safe fallback to 0).
pub fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
