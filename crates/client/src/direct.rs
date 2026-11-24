//! DIRECT outbound sockets that never loop back into our own TUN.
//!
//! With TUN mode on, the half-default routes send everything into the virtual NIC,
//! including our own "direct" connections. Like sing-box/Clash `auto-detect-interface`,
//! direct sockets are pinned to the physical interface with `IP_UNICAST_IF` (Windows),
//! so no per-prefix routing table entries are needed for split routing.

use protocol::frame::TargetAddr;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio::net::{TcpSocket, TcpStream, UdpSocket};

#[derive(Debug, Default)]
pub struct DirectDialer {
    /// Physical IPv4 interface index to pin direct sockets to; 0 = unpinned (TUN off).
    if4: AtomicU32,
}

impl DirectDialer {
    pub fn set_interface(&self, if_index: Option<u32>) {
        self.if4.store(if_index.unwrap_or(0), Ordering::Relaxed);
    }

    pub fn interface(&self) -> Option<u32> {
        match self.if4.load(Ordering::Relaxed) {
            0 => None,
            i => Some(i),
        }
    }

    /// Connect to one address, pinned to the physical NIC when TUN is active.
    pub async fn connect_tcp(&self, addr: SocketAddr) -> io::Result<TcpStream> {
        let sock = if addr.is_ipv4() {
            TcpSocket::new_v4()?
        } else {
            TcpSocket::new_v6()?
        };
        if addr.is_ipv4() && !addr.ip().is_loopback() {
            if let Some(idx) = self.interface() {
                pin_v4(&sock, idx)?;
            }
        }
        let stream = sock.connect(addr).await?;
        let _ = stream.set_nodelay(true);
        Ok(stream)
    }

    /// Resolve (OS resolver) and connect within `total`, trying each address in turn.
    pub async fn connect_target(&self, target: &TargetAddr, total: Duration) -> io::Result<TcpStream> {
        let deadline = Instant::now() + total;
        let addrs: Vec<SocketAddr> = match target {
            TargetAddr::Ip(a) => vec![*a],
            TargetAddr::Domain(h, p) => {
                tokio::time::timeout(total, tokio::net::lookup_host((h.as_str(), *p)))
                    .await
                    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "dns timed out"))??
                    .collect()
            }
        };
        let mut last_err = None;
        for (i, addr) in addrs.iter().enumerate() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let left = (addrs.len() - i) as u32;
            let budget = if left > 1 {
                (remaining / left).max(Duration::from_secs(2)).min(remaining)
            } else {
                remaining
            };
            match tokio::time::timeout(budget, self.connect_tcp(*addr)).await {
                Ok(Ok(s)) => return Ok(s),
                Ok(Err(e)) => last_err = Some(e),
                Err(_) => {
                    last_err = Some(io::Error::new(io::ErrorKind::TimedOut, format!("{addr} timed out")))
                }
            }
        }
        Err(last_err.unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no address")))
    }

    /// Unconnected IPv4 UDP socket for direct traffic, pinned like `connect_tcp`.
    pub fn bind_udp_v4(&self) -> io::Result<UdpSocket> {
        let std_sock = std::net::UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))?;
        std_sock.set_nonblocking(true)?;
        if let Some(idx) = self.interface() {
            pin_v4(&std_sock, idx)?;
        }
        UdpSocket::from_std(std_sock)
    }
}

#[cfg(windows)]
fn pin_v4<S: std::os::windows::io::AsRawSocket>(sock: &S, if_index: u32) -> io::Result<()> {
    win::set_unicast_if_v4(sock.as_raw_socket(), if_index)
}

#[cfg(not(windows))]
fn pin_v4<S>(_sock: &S, _if_index: u32) -> io::Result<()> {
    Ok(())
}

/// Interface the OS would use to reach the public internet, queried BEFORE the TUN
/// half-default routes are installed (afterwards it would answer with the TUN itself).
pub fn detect_physical_interface_v4() -> Option<u32> {
    #[cfg(windows)]
    {
        win::best_interface_v4(Ipv4Addr::new(223, 5, 5, 5))
    }
    #[cfg(not(windows))]
    {
        None
    }
}

#[cfg(windows)]
mod win {
    use std::io;
    use std::net::Ipv4Addr;

    const IPPROTO_IP: i32 = 0;
    const IP_UNICAST_IF: i32 = 31;

    #[link(name = "ws2_32")]
    unsafe extern "system" {
        fn setsockopt(s: usize, level: i32, optname: i32, optval: *const u8, optlen: i32) -> i32;
        fn WSAGetLastError() -> i32;
    }

    #[link(name = "iphlpapi")]
    unsafe extern "system" {
        fn GetBestInterface(dw_dest_addr: u32, pdw_best_if_index: *mut u32) -> u32;
    }

    pub fn set_unicast_if_v4(sock: u64, if_index: u32) -> io::Result<()> {
        // For IPv4, IP_UNICAST_IF takes the interface index in NETWORK byte order.
        let value = if_index.to_be();
        // SAFETY: valid socket handle owned by the caller; optval points to 4 live bytes.
        let rc = unsafe {
            setsockopt(
                sock as usize,
                IPPROTO_IP,
                IP_UNICAST_IF,
                &value as *const u32 as *const u8,
                std::mem::size_of::<u32>() as i32,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            // SAFETY: plain thread-local error query.
            Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }))
        }
    }

    pub fn best_interface_v4(dest: Ipv4Addr) -> Option<u32> {
        let mut index = 0u32;
        // dwDestAddr is an IPAddr (network byte order in memory).
        // SAFETY: `index` is a valid out-pointer for the duration of the call.
        let rc = unsafe { GetBestInterface(u32::from_ne_bytes(dest.octets()), &mut index) };
        (rc == 0 && index != 0).then_some(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pinned_sockets_still_work() {
        let dialer = DirectDialer::default();
        if let Some(idx) = detect_physical_interface_v4() {
            // Pinning to the physical NIC must still allow sending/binding.
            dialer.set_interface(Some(idx));
        }
        let udp = dialer.bind_udp_v4().unwrap();
        assert!(udp.local_addr().unwrap().is_ipv4());

        // Unpinned dial to a loopback listener (pinning to a physical NIC would
        // legitimately prevent reaching loopback, so test that path unpinned).
        dialer.set_interface(None);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let s = dialer
            .connect_target(&TargetAddr::Ip(addr), Duration::from_secs(2))
            .await;
        assert!(s.is_ok());
    }
}
