//! The control edge to lyrad — Choragus sends, the RT engine applies.
//!
//! Choragus resolves *what* the state should be ([`policy::diff`] → [`Change`]s);
//! this turns each into the wire [`Ctl`] of `lyra-protocol` (Aqueduct class 5)
//! and ships it down lyrad's control socket. lyrad decides *how* — a zipper-free
//! gain ramp, a glitch-free re-route. Samples never touch this socket.

use crate::policy::Change;
use lyra_protocol::Ctl;
use std::io::Write;
use std::os::unix::net::UnixStream;

/// Map a policy [`Change`] to its wire [`Ctl`].
pub fn change_to_ctl(c: Change) -> Ctl {
    match c {
        Change::GainRamp { stream, to_db } => Ctl::SetGainDb { stream, db: to_db },
        Change::Reroute { stream, to_sink } => Ctl::Reroute { stream, sink: to_sink },
    }
}

/// Connect to lyrad's control socket and send `changes` in order.
pub fn send(socket: &str, changes: &[Change]) -> std::io::Result<()> {
    let mut s = UnixStream::connect(socket)?;
    for c in changes {
        s.write_all(&change_to_ctl(*c).encode())?;
    }
    s.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_map_to_the_wire() {
        assert_eq!(
            change_to_ctl(Change::GainRamp { stream: 2, to_db: -18.0 }),
            Ctl::SetGainDb { stream: 2, db: -18.0 }
        );
        assert_eq!(
            change_to_ctl(Change::Reroute { stream: 2, to_sink: 1 }),
            Ctl::Reroute { stream: 2, sink: 1 }
        );
    }
}
