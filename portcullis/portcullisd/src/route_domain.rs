//! A persona's route domain (vpn.md §5.4, V2): the routing table its apps'
//! jails live in, from atrium-net's root-only control socket.
//!
//! A persona whose policy names a `vpn` gets that tunnel's domain. atrium-net
//! hands out one FIB per domain name; with the tunnel down the table is empty,
//! so the persona's apps reach nothing rather than leaving by the plain
//! network.

pub use atrium_netconf::route_domain::fib_for;

/// The route domain for `person`'s `persona` (None or "personal": the
/// Personal persona): `Ok(None)` when its policy names no `vpn`, the FIB when
/// it does, and an error, which refuses the launch, when atrium-net gives
/// none. Never the host's own table instead.
pub fn for_persona(person: &str, persona: Option<&str>) -> Result<Option<u32>, String> {
    let Some(p) = persona.filter(|p| *p != portcullis_policy::PERSONAL) else { return Ok(None) };
    let policy = portcullis_policy::Policy::load(&portcullis_policy::Policy::user_path(person))
        .map_err(|e| format!("policy for {p}: {e}"))?;
    let vpn = policy.personas.get(p).ok_or_else(|| format!("unknown persona {p}"))?.vpn.as_deref();
    match vpn {
        None => Ok(None),
        Some(v) => fib_for(v).map(Some).map_err(|e| format!("{p}'s VPN {v}: {e}")),
    }
}
