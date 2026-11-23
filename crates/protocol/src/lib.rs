pub mod auth;
pub mod dedup;
pub mod frame;
pub mod io;
pub mod pacer;
pub mod share;

pub const ALPN_PROTOCOL: &[u8] = b"h3";
pub const DEFAULT_INITIAL_MTU: u16 = 1200;
