//! "Connect to server" (network-shares.md §5.2, §6.3): the address, the
//! server's host key for the user to accept, then the password in Arca's
//! own field. The form is a small state machine; the app carries out what
//! it asks for (`want`) and reports back. Nothing here sees a password:
//! the field is Arca's, and what Scrinium holds is a handle.

/// Where a share is: `user@host[:port][/path]`, with or without `sftp://`,
/// or scp-style `user@host:path`. No path is the user's home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    pub user: String,
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl Address {
    pub fn text(&self) -> String {
        let port = if self.port == 22 { String::new() } else { format!(":{}", self.port) };
        let path = if self.path == "." { String::new() } else if self.path.starts_with('/') { self.path.clone() } else { format!("/~/{}", self.path) };
        format!("{}@{}{port}{path}", self.user, self.host)
    }
}

fn host_ok(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && (h.parse::<std::net::Ipv4Addr>().is_ok()
            || h.split('.').all(|l| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')))
}

pub fn parse_address(s: &str) -> Result<Address, String> {
    let s = s.trim();
    let s = s.strip_prefix("sftp://").unwrap_or(s);
    let (user, rest) = s.split_once('@').ok_or("Type it as name@server, for example alice@nas.local")?;
    if user.is_empty() || user.len() > 64 || user.starts_with('-') || !user.bytes().all(|b| b.is_ascii_graphic() && b != b':' && b != b'/') {
        return Err("That user name can't be used".into());
    }
    // host[:port][/path] or host:path (scp style: a colon not followed by a port).
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, String::new()),
    };
    let (host, port, mut path) = match hostport.split_once(':') {
        Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => {
            (h, p.parse::<u16>().ok().filter(|&p| p != 0).ok_or("That port can't be used")?, path)
        }
        Some((h, p)) if path.is_empty() => (h, 22, p.to_string()),
        Some((h, p)) => (h, 22, format!("{p}{path}")),
        None => (hostport, 22, path),
    };
    let host = host.to_ascii_lowercase();
    if !host_ok(&host) {
        return Err("That server name can't be used".into());
    }
    if let Some(rel) = path.strip_prefix("/~/") {
        path = rel.to_string();
    } else if path == "/~" || path.is_empty() {
        path = ".".into();
    }
    if path.len() > 1024 || path.starts_with('-') || path.chars().any(|c| c.is_control()) {
        return Err("That folder can't be used".into());
    }
    Ok(Address { user: user.into(), host, port, path })
}

/// OpenSSH's fingerprint of a key digest, the way `--learn` prints it.
pub fn fingerprint(key: &[u8; 32]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::from("SHA256:");
    let mut acc = 0u32;
    let mut bits = 0;
    for &b in key {
        acc = (acc << 8) | b as u32;
        bits += 8;
        while bits >= 6 {
            bits -= 6;
            out.push(A[((acc >> bits) & 63) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(A[((acc << (6 - bits)) & 63) as usize] as char);
    }
    out
}

/// The digest in an OpenSSH fingerprint (`SHA256:` and 43 base64).
pub fn key_of(fp: &str) -> Option<[u8; 32]> {
    let b = fp.strip_prefix("SHA256:")?;
    if b.len() != 43 {
        return None;
    }
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(32);
    let (mut acc, mut bits) = (0u32, 0);
    for c in b.bytes() {
        acc = (acc << 6) | A.iter().position(|&x| x == c)? as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    if acc != 0 {
        return None;
    }
    out.try_into().ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Typing the address.
    Address,
    /// Asking the server for its key.
    Checking(Address),
    /// The key, for the user to accept: never pre-accepted.
    Key { addr: Address, fingerprint: String },
    /// Arca's password field; `filled` once something is in it.
    Password { addr: Address, fingerprint: String, filled: bool },
    Connecting(Address),
}

/// What the app should do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Want {
    /// Ask dispensatord for the server's fingerprint.
    Learn(Address),
    /// Save the field's handle in Arca for this share, then connect.
    Connect { addr: Address, fingerprint: String, remember: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Form {
    pub step: Step,
    /// Shown under the address, or on the key sheet.
    pub error: Option<String>,
    pub remember: bool,
    pub want: Option<Want>,
}

impl Default for Form {
    fn default() -> Self {
        Form { step: Step::Address, error: None, remember: true, want: None }
    }
}

impl Form {
    /// Next on the address: check it and ask for the key.
    pub fn submit_address(&mut self, text: &str) {
        if self.step != Step::Address {
            return;
        }
        match parse_address(text) {
            Ok(a) => {
                self.error = None;
                self.step = Step::Checking(a.clone());
                self.want = Some(Want::Learn(a));
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// The server's key came back (or why not).
    pub fn learned(&mut self, r: Result<String, String>) {
        let Step::Checking(addr) = self.step.clone() else { return };
        match r {
            Ok(fingerprint) => {
                self.error = None;
                self.step = Step::Key { addr, fingerprint };
            }
            Err(e) => {
                self.error = Some(format!("Couldn't reach {}: {e}", addr.host));
                self.step = Step::Address;
            }
        }
    }

    /// The user checked the key and trusts it.
    pub fn trust(&mut self) {
        if let Step::Key { addr, fingerprint } = self.step.clone() {
            self.step = Step::Password { addr, fingerprint, filled: false };
        }
    }

    /// Arca's field says it has (or no longer has) something in it.
    pub fn field_filled(&mut self, f: bool) {
        if let Step::Password { filled, .. } = &mut self.step {
            *filled = f;
        }
    }

    pub fn can_connect(&self) -> bool {
        matches!(self.step, Step::Password { filled: true, .. })
    }

    pub fn connect(&mut self) {
        if !self.can_connect() {
            return;
        }
        let Step::Password { addr, fingerprint, .. } = self.step.clone() else { return };
        self.step = Step::Connecting(addr.clone());
        self.want = Some(Want::Connect { addr, fingerprint, remember: self.remember });
    }

    /// Connecting failed: back to the password, with why.
    pub fn failed(&mut self, why: String) {
        // The field's handle was redeemed: start again from the address.
        if matches!(self.step, Step::Connecting(_)) {
            self.error = Some(why);
            self.step = Step::Address;
        }
    }

    pub fn take_want(&mut self) -> Option<Want> {
        self.want.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(user: &str, host: &str, port: u16, path: &str) -> Address {
        Address { user: user.into(), host: host.into(), port, path: path.into() }
    }

    #[test]
    fn addresses() {
        assert_eq!(parse_address("alice@nas.local").unwrap(), a("alice", "nas.local", 22, "."));
        assert_eq!(parse_address("sftp://alice@NAS.local:2222/srv/photos").unwrap(), a("alice", "nas.local", 2222, "/srv/photos"));
        assert_eq!(parse_address("alice@nas.local:photos").unwrap(), a("alice", "nas.local", 22, "photos"));
        assert_eq!(parse_address("alice@nas.local:/srv").unwrap(), a("alice", "nas.local", 22, "/srv"));
        assert_eq!(parse_address("alice@192.168.1.5/~/docs").unwrap(), a("alice", "192.168.1.5", 22, "docs"));
        for bad in ["nas.local", "@nas", "alice@", "alice@nas:0", "alice@nas:99999", "alice@-oProxy", "-x@nas", "alice@nas/\u{7}"] {
            assert!(parse_address(bad).is_err(), "{bad}");
        }
        for t in ["alice@nas.local", "alice@nas.local:2222/srv/photos", "alice@nas.local/~/docs"] {
            assert_eq!(parse_address(&parse_address(t).unwrap().text()).unwrap(), parse_address(t).unwrap(), "{t}");
        }
    }

    #[test]
    fn fingerprints_match_openssh() {
        // A digest whose OpenSSH fingerprint is known (the VM's host key).
        let fp = "SHA256:WmXuVRg10pJRVYDpsFPc/bGwkJRcX4mhTs6Zz3tuWEQ";
        let mut key = [0u8; 32];
        // Decode by brute re-encoding: encoding must be a bijection on 32 bytes.
        let alpha = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut acc = 0u32;
        let mut bits = 0;
        let mut n = 0;
        for c in fp[7..].bytes() {
            acc = (acc << 6) | alpha.iter().position(|&x| x == c).unwrap() as u32;
            bits += 6;
            if bits >= 8 && n < 32 {
                bits -= 8;
                key[n] = (acc >> bits) as u8;
                n += 1;
            }
        }
        assert_eq!(fingerprint(&key), fp);
        assert_eq!(key_of(fp), Some(key));
        assert_eq!(key_of("SHA256:short"), None);
    }

    #[test]
    fn the_form_goes_address_key_password_connect() {
        let mut f = Form::default();
        f.submit_address("nas.local");
        assert!(f.error.is_some() && f.want.is_none());
        f.submit_address("alice@nas.local:/srv");
        assert_eq!(f.take_want(), Some(Want::Learn(a("alice", "nas.local", 22, "/srv"))));
        f.learned(Ok("SHA256:x".into()));
        assert!(matches!(f.step, Step::Key { .. }));
        f.connect();
        assert!(f.want.is_none(), "no connect before the key is trusted and the field filled");
        f.trust();
        f.connect();
        assert!(f.want.is_none(), "an empty field connects nothing");
        f.field_filled(true);
        f.remember = false;
        f.connect();
        assert_eq!(
            f.take_want(),
            Some(Want::Connect { addr: a("alice", "nas.local", 22, "/srv"), fingerprint: "SHA256:x".into(), remember: false })
        );
        f.failed("Wrong password".into());
        assert_eq!((f.step.clone(), f.error.as_deref()), (Step::Address, Some("Wrong password")));
        // An unreachable server goes back to the address.
        f.submit_address("alice@nas.local");
        f.learned(Err("no route".into()));
        assert_eq!(f.step, Step::Address);
        assert!(f.error.as_deref().unwrap().contains("nas.local"));
    }
}
