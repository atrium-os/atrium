//! arca-courier: one endpoint's courier (arca.md §5).
//!
//! arca-courierd runs it in its own jail under a courier uid, with the
//! client's end of a socketpair as stdin and stdout, after writing the
//! uid's label to the courier registry. The courier reads its endpoint
//! from that registry by its own uid; the client can't name another one. Requests and replies
//! are `u32 LE length | bytes` frames of [`arca_courier::schema`].
//!
//! Environment (defaults in brackets):
//!   ARCA_SOCKET             [/atrium/sockets/arca/arcad.sock]
//!   ARCA_COURIER_REGISTRY   [/var/run/atrium/courier-registry]
//!   ARCA_COURIER_CA         [/etc/ssl/cert.pem] the trust store (PEM)
//!   ATRIUM_COURIER_ADDRS    the addresses arca-courierd resolved for the
//!                           label's host, comma separated; its jail's pf
//!                           anchor passes only these. Unset: resolve.
//!   ARCA_COURIER_GRANT_PRIVATE_NETWORK=1   tests only: admit loopback/LAN
//!                           (ATRIUM_COURIER_GRANT_PRIVATE_NETWORK in a jail)
//!   ARCA_COURIER_CONNECT    tests only: host:port to connect to instead
//!                           of resolving (the TLS name is still the label's)

use std::process::ExitCode;

use arca_courier::send::{read_frame, write_frame, Courier, Endpoint};
use arca_courier::vault::Arcad;
use arca_store::Recipient;
use navigator_fetch::{addr, Connect, Direct};

/// Most requests one courier carries before it exits (arca.md §5.2).
const MAX_REQUESTS: usize = 1000;

fn env(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.to_string())
}

/// `ATRIUM_COURIER_ADDRS` or `ARCA_COURIER_CONNECT`: every lookup answers
/// these addresses (the courier asks only for its label's host). The
/// address policy still judges each one.
struct Fixed(Vec<std::net::SocketAddr>);

impl Connect for Fixed {
    fn resolve(&self, _: &str, _: u16) -> std::io::Result<Vec<std::net::SocketAddr>> {
        Ok(self.0.clone())
    }
    fn connect_addr(&self, a: std::net::SocketAddr) -> std::io::Result<std::net::TcpStream> {
        std::net::TcpStream::connect_timeout(&a, navigator_fetch::TIMEOUT)
    }
}

fn main() -> ExitCode {
    if let Err(e) = arca_store::secret::harden_process() {
        eprintln!("arca-courier: harden: {e}");
        return ExitCode::FAILURE;
    }
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let reg = env("ARCA_COURIER_REGISTRY", arcad::courier::DEFAULT_REGISTRY);
    let Some(label) = arcad::courier::lookup(&reg, uid) else {
        eprintln!("arca-courier: uid {uid} has no courier label");
        return ExitCode::FAILURE;
    };
    let Recipient::Endpoint { host, port, .. } = label.endpoint else { return ExitCode::FAILURE };
    let pem = match std::fs::read(env("ARCA_COURIER_CA", "/etc/ssl/cert.pem")) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("arca-courier: trust store: {e}");
            return ExitCode::FAILURE;
        }
    };
    let tls = match navigator_fetch::tls_config(&pem) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("arca-courier: {e}");
            return ExitCode::FAILURE;
        }
    };
    // (jaild passes only ATRIUM_* into a jail: arca-courierd's name for it.)
    let grant = |k: &str| std::env::var(k).as_deref() == Ok("1");
    let policy = addr::Policy {
        grant_private_network: grant("ARCA_COURIER_GRANT_PRIVATE_NETWORK") || grant("ATRIUM_COURIER_GRANT_PRIVATE_NETWORK"),
    };
    let fixed = match (std::env::var("ARCA_COURIER_CONNECT"), std::env::var("ATRIUM_COURIER_ADDRS")) {
        (Ok(a), _) => a.parse().ok().map(|a| Fixed(vec![a])),
        (_, Ok(list)) => {
            let addrs: Option<Vec<std::net::SocketAddr>> =
                list.split(',').map(|a| a.parse::<std::net::IpAddr>().ok().map(|ip| (ip, port).into())).collect();
            match addrs {
                Some(a) if !a.is_empty() => Some(Fixed(a)),
                _ => {
                    eprintln!("arca-courier: ATRIUM_COURIER_ADDRS is not a list of addresses");
                    return ExitCode::FAILURE;
                }
            }
        }
        _ => None,
    };
    let net: &(dyn Connect + Sync) = match &fixed {
        Some(f) => f,
        None => &Direct,
    };
    let mut courier = Courier::new(Endpoint { host, port }, net, tls, policy);
    let mut vault = Arcad::new(env("ARCA_SOCKET", "/atrium/sockets/arca/arcad.sock"));
    let (mut input, mut output) = (std::io::stdin().lock(), std::io::stdout().lock());
    for _ in 0..MAX_REQUESTS {
        let frame = match read_frame(&mut input) {
            Ok(Some(f)) => f,
            Ok(None) => return ExitCode::SUCCESS,
            Err(_) => return ExitCode::FAILURE,
        };
        let reply = courier.handle(&frame, &mut vault);
        if write_frame(&mut output, &reply.encode()).is_err() {
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}
