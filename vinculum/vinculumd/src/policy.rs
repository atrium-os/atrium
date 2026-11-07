//! Pairing and bond policy (docs/spec/bluetooth.md §7), as pure functions
//! so each rule is tested on its own. The link rules (16-byte keys, Secure
//! Connections only, SMP timeouts) are enforced lower down, in
//! `vinculum_smp` and `vinculum_host`; these are the ones that need the
//! bond database or the user.

use vinculum_keys::Bond;

/// After the user says "pair" on a device, the controller stays pairable
/// this long (§7).
pub const PAIRABLE_WINDOW_MS: u64 = 60_000;

/// Whether pairing is allowed now: only while the pairing UI is open, or
/// for [`PAIRABLE_WINDOW_MS`] after the user asked to pair a device.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Pairable {
    until: Option<u64>,
    ui_open: bool,
}

// `ui`, `close` and `deadline` are driven by the bluetooth-control socket.
#[allow(dead_code)]
impl Pairable {
    /// The user asked to pair a device.
    pub fn asked(&mut self, now_ms: u64) {
        self.until = Some(now_ms + PAIRABLE_WINDOW_MS);
    }
    pub fn ui(&mut self, open: bool) {
        self.ui_open = open;
    }
    pub fn close(&mut self) {
        self.until = None;
        self.ui_open = false;
    }
    pub fn is_open(&self, now_ms: u64) -> bool {
        self.ui_open || self.until.is_some_and(|u| now_ms < u)
    }
    pub fn deadline(&self) -> Option<u64> {
        if self.ui_open {
            None
        } else {
            self.until
        }
    }
}

/// What to do with a new bond, given the stored one for the same device.
#[derive(Debug, PartialEq, Eq)]
pub enum NewBond {
    /// No stored bond: keep the new one.
    Add,
    /// At least as strong as the stored one: replace it.
    Replace,
    /// Weaker than the stored one: refuse the pairing and keep the old
    /// bond; the user is told why (BLURtooth-class downgrades).
    Refuse(&'static str),
}

/// No key downgrade (§7): Secure Connections ≥ legacy, authenticated ≥
/// unauthenticated, and the same key size.
pub fn new_bond(stored: Option<&Bond>, new: &Bond) -> NewBond {
    let Some(old) = stored else { return NewBond::Add };
    if old.secure_connections && !new.secure_connections {
        return NewBond::Refuse("the new key is legacy; the stored one is Secure Connections");
    }
    if old.authenticated && !new.authenticated {
        return NewBond::Refuse("the new key is unauthenticated (Just Works); the stored one is authenticated");
    }
    if new.key_size < old.key_size {
        return NewBond::Refuse("the new key is shorter than the stored one");
    }
    NewBond::Replace
}

/// Whether a bond may carry keyboard reports (§7): only an authenticated,
/// Secure Connections key. A Just Works keyboard gets mouse and consumer
/// reports until it is paired again with passkey or numeric comparison.
pub fn keyboard_allowed(b: &Bond) -> bool {
    b.authenticated && b.secure_connections && b.key_size == 16
}

/// A Security Request from the peripheral: encrypt with the stored bond
/// if there is one; pair only while pairable; otherwise ignore it (a
/// stranger cannot start pairing on its own, §7).
#[derive(Debug, PartialEq, Eq)]
pub enum OnSecurityRequest {
    Encrypt,
    Pair,
    Ignore,
}

pub fn on_security_request(bonded: bool, pairable: bool) -> OnSecurityRequest {
    match (bonded, pairable) {
        (true, _) => OnSecurityRequest::Encrypt,
        (false, true) => OnSecurityRequest::Pair,
        (false, false) => OnSecurityRequest::Ignore,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vinculum_keys::Address;

    fn bond(sc: bool, auth: bool, size: u8) -> Bond {
        Bond {
            address: Address { kind: 1, bytes: [1; 6] },
            identity: None,
            ltk: [0; 16],
            key_size: size,
            secure_connections: sc,
            authenticated: auth,
            peer_irk: None,
            paired_by: 0,
            name: String::new(),
        }
    }

    #[test]
    fn pairable_window() {
        let mut p = Pairable::default();
        assert!(!p.is_open(0));
        p.asked(1000);
        assert!(p.is_open(1000 + PAIRABLE_WINDOW_MS - 1));
        assert!(!p.is_open(1000 + PAIRABLE_WINDOW_MS));
        p.ui(true);
        assert!(p.is_open(u64::MAX) && p.deadline().is_none());
        p.close();
        assert!(!p.is_open(0));
    }

    #[test]
    fn no_downgrade() {
        let strong = bond(true, true, 16);
        let jw = bond(true, false, 16);
        assert_eq!(new_bond(None, &jw), NewBond::Add);
        assert_eq!(new_bond(Some(&jw), &strong), NewBond::Replace, "upgrade");
        assert_eq!(new_bond(Some(&strong), &strong), NewBond::Replace, "same strength");
        assert!(matches!(new_bond(Some(&strong), &jw), NewBond::Refuse(_)));
        assert!(matches!(new_bond(Some(&strong), &bond(false, true, 16)), NewBond::Refuse(_)));
        assert!(matches!(new_bond(Some(&strong), &bond(true, true, 7)), NewBond::Refuse(_)));
    }

    #[test]
    fn keyboards_need_authenticated_keys() {
        assert!(keyboard_allowed(&bond(true, true, 16)));
        assert!(!keyboard_allowed(&bond(true, false, 16)));
        assert!(!keyboard_allowed(&bond(false, true, 16)));
    }

    #[test]
    fn strangers_cannot_start_pairing() {
        assert_eq!(on_security_request(false, false), OnSecurityRequest::Ignore);
        assert_eq!(on_security_request(false, true), OnSecurityRequest::Pair);
        assert_eq!(on_security_request(true, false), OnSecurityRequest::Encrypt);
    }
}
