//! Give the ordinary fetcher one endpoint on the host's local resolver.
//! The kernel checks the descriptor's rights even before the child enters
//! capability mode. No routing or firewall grant accompanies the socket.

#[cfg(target_os = "freebsd")]
use std::os::fd::AsRawFd;
use std::{io, os::fd::OwnedFd};

pub fn socket() -> io::Result<Option<OwnedFd>> {
    #[cfg(target_os = "freebsd")]
    {
        let config = match std::fs::read_to_string("/etc/resolv.conf") {
            Ok(config) => config,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let Some(address) = loopback_resolver(&config) else {
            return Ok(None);
        };
        return connect(address).map(Some);
    }
    #[cfg(not(target_os = "freebsd"))]
    Ok(None)
}

#[cfg(any(test, target_os = "freebsd"))]
fn loopback_resolver(config: &str) -> Option<std::net::SocketAddr> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    let first = config
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            if words.next() != Some("nameserver") {
                return None;
            }
            words.next()?.split('%').next()?.parse::<IpAddr>().ok()
        })
        .next()?;
    matches!(
        first,
        IpAddr::V4(Ipv4Addr::LOCALHOST) | IpAddr::V6(Ipv6Addr::LOCALHOST)
    )
    .then_some((first, 53).into())
}

#[cfg(target_os = "freebsd")]
fn connect(address: std::net::SocketAddr) -> io::Result<OwnedFd> {
    let socket = std::net::UdpSocket::bind(if address.is_ipv4() {
        "127.0.0.1:0"
    } else {
        "[::1]:0"
    })?;
    socket.connect(address)?;
    socket.set_read_timeout(Some(std::time::Duration::from_millis(250)))?;
    // SAFETY: cap_rights_init initializes this storage from the terminated
    // rights list. Omit connect, bind, setsockopt, and descriptor mutation.
    unsafe {
        let mut rights = std::mem::MaybeUninit::<libc::cap_rights_t>::uninit();
        libc::__cap_rights_init(
            libc::CAP_RIGHTS_VERSION,
            rights.as_mut_ptr(),
            libc::CAP_RECV,
            libc::CAP_SEND,
            libc::CAP_FSTAT,
            0u64,
        );
        if libc::cap_rights_limit(socket.as_raw_fd(), rights.as_ptr()) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(socket.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_primary_local_resolver_gets_host_access() {
        assert_eq!(
            loopback_resolver("# local-unbound\nnameserver 127.0.0.1"),
            Some("127.0.0.1:53".parse().unwrap())
        );
        assert_eq!(
            loopback_resolver("nameserver ::1"),
            Some("[::1]:53".parse().unwrap())
        );
        assert_eq!(
            loopback_resolver("nameserver 10.0.0.1\nnameserver 127.0.0.1"),
            None
        );
        assert_eq!(loopback_resolver("nameserver 127.0.0.2"), None);
        assert_eq!(loopback_resolver(""), None);
    }

    #[cfg(target_os = "freebsd")]
    #[test]
    fn socket_can_exchange_only_with_the_connected_resolver() {
        use std::net::UdpSocket;
        let listener = UdpSocket::bind("127.0.0.1:0").unwrap();
        listener
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let other = UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let client = UdpSocket::from(connect(listener.local_addr().unwrap()).unwrap());
        assert_eq!(
            client.connect(other).unwrap_err().raw_os_error(),
            Some(libc::ENOTCAPABLE)
        );
        assert_eq!(
            client.send_to(b"wrong", other).unwrap_err().raw_os_error(),
            Some(libc::ENOTCAPABLE)
        );
        assert_eq!(
            client.set_broadcast(true).unwrap_err().raw_os_error(),
            Some(libc::ENOTCAPABLE)
        );
        client.send(b"query").unwrap();
        let mut buf = [0; 16];
        let (n, peer) = listener.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"query");
        listener.send_to(b"answer", peer).unwrap();
        let n = client.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"answer");
    }
}
