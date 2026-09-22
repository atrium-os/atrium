//! The host identity a jailed app sees.
//!
//! ★★★ NEVER THE REAL MACHINE'S (portcullis.md §9.1c). A jail created without
//! host parameters reports hostid 0 and an all-zero host UUID — identical on
//! every machine, which breaks anything that keys a licence to the host
//! (FlexLM's `lmhostid` and kin) — and passing the real values through would
//! hand every app a stable machine fingerprint to correlate with every other
//! app. Decided: every app gets a SYNTHETIC identity, and no capability exposes
//! the real one.
//!
//! The synthetic identity is `HMAC-SHA256(machine secret, app id)`:
//!
//!   - **stable** — the same app on the same machine gets the same hostid and
//!     UUID across launches and reboots, so a licence activated against it
//!     keeps working;
//!   - **machine-bound** — the secret is per machine, so a node-locked licence
//!     is still node-locked;
//!   - **per app** — two apps cannot compare identities to learn they share a
//!     machine;
//!   - **unrelated to the real machine** — nothing is derived from real
//!     hardware or the host's own hostid.
//!
//! The secret is 32 random bytes in a root-only file, created on first use.

use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use sha2::{Digest, Sha256};

/// Where the machine secret lives. Root-only; see [`load_or_create_secret`].
pub const SECRET_PATH: &str = "/var/db/atrium/host-identity.key";

/// Domain label: a derivation for any other purpose must never collide with
/// this one, even from the same secret and input.
const DOMAIN: &[u8] = b"atrium-host-identity/v1\0";
/// The MAC has its own label, so it shares no bits with the hostid/UUID —
/// knowing one tells an app nothing about the other.
const MAC_DOMAIN: &[u8] = b"atrium-host-mac/v1\0";

/// What a jail is told about its host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostIdentity {
    /// `host.hostid` — what `gethostid(3)` returns inside. Never 0.
    pub hostid: u32,
    /// `host.hostuuid` — RFC 9562 layout, version 8 (vendor-specific).
    pub hostuuid: String,
    /// The MAC of the app's own interface when it has network (network.md
    /// §0): locally administered, unicast (`x2:…`), so it can never collide
    /// with a vendor-assigned address — and never the real NIC's.
    pub mac: String,
}

/// Derive the identity for `app_id` from the machine secret. Pure.
pub fn derive(secret: &[u8; 32], app_id: &str) -> HostIdentity {
    let mac = hmac_sha256(secret, &[DOMAIN, app_id.as_bytes()].concat());
    let mut u = [0u8; 16];
    u.copy_from_slice(&mac[..16]);
    u[6] = (u[6] & 0x0f) | 0x80; // version 8: custom
    u[8] = (u[8] & 0x3f) | 0x80; // RFC variant
    let hex: String = u.iter().map(|b| format!("{b:02x}")).collect();
    let hostuuid = format!("{}-{}-{}-{}-{}",
        &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32]);
    // ★ Never 0: hostid 0 is what an un-configured jail reports, and licence
    // managers commonly treat it as "no hostid".
    let raw = u32::from_be_bytes([mac[16], mac[17], mac[18], mac[19]]);
    let hostid = if raw == 0 { 1 } else { raw };
    let m = hmac_sha256(secret, &[MAC_DOMAIN, app_id.as_bytes()].concat());
    // Locally administered (bit 1 set), unicast (bit 0 clear).
    let b0 = (m[0] & 0xfc) | 0x02;
    let mac = format!("{b0:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[1], m[2], m[3], m[4], m[5]);
    HostIdentity { hostid, hostuuid, mac }
}

/// HMAC-SHA256 (RFC 2104) over `sha2`, so the one hash crate the tree already
/// audits is the only one involved.
fn hmac_sha256(key: &[u8; 32], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    k[..32].copy_from_slice(key);
    let ipad: Vec<u8> = k.iter().map(|b| b ^ 0x36).collect();
    let opad: Vec<u8> = k.iter().map(|b| b ^ 0x5c).collect();
    let inner = Sha256::new().chain_update(&ipad).chain_update(msg).finalize();
    Sha256::new().chain_update(&opad).chain_update(inner).finalize().into()
}

/// Load the machine secret, creating it on first use.
///
/// ★ Refused, not repaired, if the file exists but is not exactly 32 bytes,
/// owned by root, mode 0600. A secret that other users could read would let
/// them compute every app's identity; one that has been truncated or replaced
/// would silently change every app's identity and invalidate every licence
/// bound to it — both are for a human to look at, not for this to paper over.
pub fn load_or_create_secret(path: &Path) -> io::Result<[u8; 32]> {
    match std::fs::File::open(path) {
        Ok(mut f) => {
            let md = f.metadata()?;
            if md.len() != 32 || md.uid() != 0 || md.permissions().mode() & 0o077 != 0 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!(
                    "{} must be 32 bytes, owned by root, mode 0600 (is {} bytes, uid {}, mode {:o})",
                    path.display(), md.len(), md.uid(), md.permissions().mode() & 0o777)));
            }
            let mut s = [0u8; 32];
            f.read_exact(&mut s)?;
            Ok(s)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if let Some(dir) = path.parent() { std::fs::create_dir_all(dir)?; }
            let mut s = [0u8; 32];
            fill_random(&mut s);
            // create_new: two first-launches racing must not both write — the
            // loser re-reads the winner's secret.
            match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path) {
                Ok(mut f) => { f.write_all(&s)?; f.sync_all()?; Ok(s) }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => load_or_create_secret(path),
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

/// The identity for `app_id` on this machine.
pub fn for_app(app_id: &str) -> io::Result<HostIdentity> {
    Ok(derive(&load_or_create_secret(Path::new(SECRET_PATH))?, app_id))
}

fn fill_random(buf: &mut [u8]) {
    // SAFETY: arc4random_buf fills exactly `len` bytes and cannot fail.
    unsafe { libc::arc4random_buf(buf.as_mut_ptr().cast(), buf.len()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S1: [u8; 32] = [7u8; 32];
    const S2: [u8; 32] = [9u8; 32];

    /// Stable: a licence activated against it keeps working.
    #[test]
    fn same_app_same_machine_same_identity() {
        assert_eq!(derive(&S1, "org.x.cad"), derive(&S1, "org.x.cad"));
    }

    /// Per app: two apps cannot compare identities to learn they share a host.
    #[test]
    fn different_apps_get_unrelated_identities() {
        let (a, b) = (derive(&S1, "org.x.cad"), derive(&S1, "org.y.eda"));
        assert_ne!(a.hostid, b.hostid);
        assert_ne!(a.hostuuid, b.hostuuid);
    }

    /// Machine-bound: another machine's secret gives another identity.
    #[test]
    fn different_machines_get_different_identities() {
        assert_ne!(derive(&S1, "org.x.cad"), derive(&S2, "org.x.cad"));
    }

    /// A well-formed version-8 UUID, and never the zero values an
    /// unconfigured jail reports.
    #[test]
    fn the_identity_is_well_formed_and_never_zero() {
        let id = derive(&S1, "org.x.cad");
        assert_ne!(id.hostid, 0);
        let u = &id.hostuuid;
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "8", "version nibble: {u}");
        assert!(matches!(&u[19..20], "8" | "9" | "a" | "b"), "variant: {u}");
        assert_ne!(u, "00000000-0000-0000-0000-000000000000");
    }

    /// The hand-built HMAC against an independent implementation:
    /// `printf hello | openssl dgst -sha256 -mac HMAC -macopt hexkey:07…07`.
    #[test]
    fn hmac_matches_openssl() {
        let got = hmac_sha256(&S1, b"hello");
        let hex: String = got.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, OPENSSL_HMAC_07_HELLO);
    }
    const OPENSSL_HMAC_07_HELLO: &str = "290af183d08286ae740dfed386724985dc666de6350a8df2e8520307ae2503ed";

    /// The MAC: locally administered unicast, stable, per app, per machine.
    #[test]
    fn the_mac_is_locally_administered_unicast_and_per_app() {
        let a = derive(&S1, "org.x.cad");
        let first = u8::from_str_radix(&a.mac[0..2], 16).unwrap();
        assert_eq!(first & 0x01, 0, "multicast bit set: {}", a.mac);
        assert_eq!(first & 0x02, 0x02, "not locally administered: {}", a.mac);
        assert_eq!(a.mac.len(), 17);
        assert_eq!(a.mac, derive(&S1, "org.x.cad").mac);
        assert_ne!(a.mac, derive(&S1, "org.y.eda").mac);
        assert_ne!(a.mac, derive(&S2, "org.x.cad").mac);
    }

    #[test]
    fn a_secret_that_is_not_root_only_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("k");
        std::fs::write(&p, [1u8; 32]).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_or_create_secret(&p).is_err());
    }
}
