//! "Connect to server" (network-shares.md §5.2, §6.3): the address, the
//! server's host key for the user to accept, then the password in Arca's
//! own field. The form is a small state machine; the app carries out what
//! it asks for (`want`) and reports back. Nothing here sees a password:
//! the field is Arca's, and what Scrinium holds is a handle.

/// A share's protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proto {
    Sftp,
    /// WebDAV over HTTPS (S2).
    Webdav,
    /// SMB 3.1.1, encrypted, with a password (S3).
    Smb,
}

impl Proto {
    /// As Arca's wire writes it.
    pub fn byte(self) -> u8 {
        match self {
            Proto::Sftp => 1,
            Proto::Webdav => 2,
            Proto::Smb => 3,
        }
    }

    pub fn from_byte(b: u8) -> Option<Proto> {
        match b {
            1 => Some(Proto::Sftp),
            2 => Some(Proto::Webdav),
            3 => Some(Proto::Smb),
            _ => None,
        }
    }

    pub fn media(self) -> atrium_media::ShareProto {
        match self {
            Proto::Sftp => atrium_media::ShareProto::Sftp,
            Proto::Webdav => atrium_media::ShareProto::Webdav,
            Proto::Smb => atrium_media::ShareProto::Smb,
        }
    }
}

/// What a WebDAV share's identity is when the trust store vouches for its
/// name (dispensatord's word for it); Arca pins zeros.
pub const TRUSTED: &str = "trusted";

/// Where a share is: `user@host[:port][/path]`, with or without `sftp://`,
/// or scp-style `user@host:path` (no path is the user's home); or
/// `https://user@host[:port]/path` for WebDAV (no path is `/`); or
/// `smb://[domain;]user@host[:port]/share[/folder]` for SMB (the user is
/// kept as `domain\user`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    pub proto: Proto,
    pub user: String,
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl Address {
    pub fn text(&self) -> String {
        if self.proto == Proto::Smb {
            let port = if self.port == 445 { String::new() } else { format!(":{}", self.port) };
            return format!("smb://{}@{}{port}{}", self.user.replacen('\\', ";", 1), self.host, self.path);
        }
        if self.proto == Proto::Webdav {
            let port = if self.port == 443 { String::new() } else { format!(":{}", self.port) };
            return format!("https://{}@{}{port}{}", self.user, self.host, self.path);
        }
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

fn user_ok(user: &str) -> bool {
    !user.is_empty() && user.len() <= 64 && !user.starts_with('-') && user.bytes().all(|b| b.is_ascii_graphic() && b != b':' && b != b'/')
}

/// `https://user@host[:port][/path]`.
fn parse_webdav(rest: &str) -> Result<Address, String> {
    let (user, rest) = rest.split_once('@').ok_or("Type it as https://name@server/folder, for example https://alice@nas.local/dav")?;
    if !user_ok(user) {
        return Err("That user name can't be used".into());
    }
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, "/".to_string()),
    };
    let (host, port) = match hostport.split_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().ok().filter(|&p| p != 0).ok_or("That port can't be used")?),
        None => (hostport, 443),
    };
    let host = host.to_ascii_lowercase();
    if !host_ok(&host) {
        return Err("That server name can't be used".into());
    }
    if path.len() > 1024 || path.chars().any(|c| c.is_control() || c == '?' || c == '#') || path.split('/').any(|seg| seg == "..") {
        return Err("That folder can't be used".into());
    }
    Ok(Address { proto: Proto::Webdav, user: user.into(), host, port, path })
}

/// A share name or folder name SMB can use (capsa-share's smb_name).
fn smb_component(n: &str) -> bool {
    !n.is_empty() && n.len() <= 255 && n != "." && n != ".." && !n.contains(['\\', ':', '*', '?', '"', '<', '>', '|']) && !n.chars().any(|c| c.is_control())
}

/// `smb://[domain;]user@host[:port]/share[/folder]`.
fn parse_smb(rest: &str) -> Result<Address, String> {
    const HOW: &str = "Type it as smb://name@server/share, for example smb://alice@nas.local/photos";
    let (user, rest) = rest.split_once('@').ok_or(HOW)?;
    let user = match user.split_once(';') {
        Some((d, u)) if !d.is_empty() => format!("{d}\\{u}"),
        _ => user.to_string(),
    };
    let bare = user.rsplit('\\').next().unwrap_or("");
    if !user_ok(&user) || bare.is_empty() || user.matches('\\').count() > 1 {
        return Err("That user name can't be used".into());
    }
    let (hostport, path) = rest.split_once('/').map(|(h, p)| (h, format!("/{p}"))).ok_or(HOW)?;
    let (host, port) = match hostport.split_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().ok().filter(|&p| p != 0).ok_or("That port can't be used")?),
        None => (hostport, 445),
    };
    let host = host.to_ascii_lowercase();
    if !host_ok(&host) {
        return Err("That server name can't be used".into());
    }
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return Err(HOW.into());
    }
    if path.len() > 1024 || !parts.iter().all(|p| smb_component(p)) {
        return Err("That share or folder can't be used".into());
    }
    Ok(Address { proto: Proto::Smb, user, host, port, path: format!("/{}", parts.join("/")) })
}

pub fn parse_address(s: &str) -> Result<Address, String> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("smb://") {
        return parse_smb(rest);
    }
    if s.starts_with("\\\\") {
        return Err("Type it as smb://name@server/share, with your name on it".into());
    }
    if let Some(rest) = s.strip_prefix("https://").or_else(|| s.strip_prefix("davs://")) {
        return parse_webdav(rest);
    }
    if s.starts_with("http://") || s.starts_with("dav://") {
        return Err("Only https:// addresses: Scrinium doesn't send passwords unencrypted".into());
    }
    let s = s.strip_prefix("sftp://").unwrap_or(s);
    let (user, rest) = s.split_once('@').ok_or("Type it as name@server, for example alice@nas.local")?;
    if !user_ok(user) {
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
    Ok(Address { proto: Proto::Sftp, user: user.into(), host, port, path })
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

/// The key Arca pins for what the user accepted: a fingerprint's digest,
/// or zeros for [`TRUSTED`].
pub fn key_for(fp: &str) -> Option<[u8; 32]> {
    if fp == TRUSTED {
        Some([0; 32])
    } else {
        key_of(fp)
    }
}

/// What dispensatord is told a saved share's identity is.
pub fn identity_of(proto: Proto, key: &[u8; 32]) -> String {
    if proto == Proto::Webdav && key == &[0; 32] {
        TRUSTED.into()
    } else {
        fingerprint(key)
    }
}

/// What an SMB server said when asked (dispensatord's learn answer).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SmbInfo {
    /// AES-128-GCM or AES-256-GCM.
    pub cipher: String,
    /// Of the server's GUID: what the share is pinned to.
    pub fingerprint: String,
    pub guid: String,
}

pub fn parse_smb_info(s: &str) -> Option<SmbInfo> {
    let mut lines = s.lines().map(str::trim);
    let cipher = lines.next()?.strip_prefix("smb 3.1.1 ")?.to_string();
    let mut i = SmbInfo { cipher, ..Default::default() };
    for l in lines {
        if key_of(l).is_some() {
            i.fingerprint = l.into();
        } else if let Some(g) = l.strip_prefix("guid ") {
            i.guid = g.into();
        }
    }
    (!i.fingerprint.is_empty()).then_some(i)
}

/// What a WebDAV server's certificate says (dispensatord's learn answer).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cert {
    /// `None`: the trust store vouches for the name; else why not.
    pub untrusted: Option<String>,
    pub fingerprint: String,
    pub subject: String,
    pub issuer: String,
    pub expires: String,
}

pub fn parse_cert(s: &str) -> Option<Cert> {
    let mut lines = s.lines();
    let first = lines.next()?.trim();
    let untrusted = if first == "trusted" { None } else { Some(first.strip_prefix("untrusted")?.trim().to_string()) };
    let mut c = Cert { untrusted, ..Default::default() };
    for l in lines {
        if key_of(l.trim()).is_some() {
            c.fingerprint = l.trim().into();
        } else if let Some(v) = l.strip_prefix("subject ") {
            c.subject = v.into();
        } else if let Some(v) = l.strip_prefix("issuer ") {
            c.issuer = v.into();
        } else if let Some(v) = l.strip_prefix("expires ") {
            c.expires = v.into();
        }
    }
    (!c.fingerprint.is_empty()).then_some(c)
}

/// Why a certificate isn't trusted, for people: rustls's words, shortened.
pub fn untrusted_text(why: &str) -> String {
    let w = why.to_ascii_lowercase();
    // A self-signed certificate is UnknownIssuer, or CaUsedAsEndEntity when
    // it calls itself a CA (openssl req -x509 does, and many NASes).
    if w.contains("unknownissuer") || w.contains("unknown issuer") || w.contains("causedasendentity") {
        "It isn't signed by an authority Atrium trusts (a self-signed server, for example).".into()
    } else if w.contains("expired") {
        "Its certificate has expired.".into()
    } else if w.contains("notvalidforname") || w.contains("not valid for name") {
        "Its certificate is for another name.".into()
    } else if w.contains("trust store") {
        "Atrium has no trust store to check it with.".into()
    } else {
        "Its certificate can't be checked.".into()
    }
}

/// A saved server, asked again before connecting (§5.1, the table in §5):
/// is it still the key the user accepted?
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recheck {
    Same,
    /// A different key: a hard stop, never a click-through.
    Changed { now: String },
    /// The answer wasn't a fingerprint at all.
    Odd,
}

pub fn recheck(proto: Proto, saved: &[u8; 32], learned: &str) -> Recheck {
    if proto == Proto::Smb {
        let Some(i) = parse_smb_info(learned) else { return Recheck::Odd };
        return match key_of(&i.fingerprint) {
            Some(k) if &k == saved => Recheck::Same,
            Some(_) => Recheck::Changed { now: format!("{} (server ID {})", i.fingerprint, i.guid) },
            None => Recheck::Odd,
        };
    }
    if proto == Proto::Webdav {
        let Some(c) = parse_cert(learned) else { return Recheck::Odd };
        if saved == &[0; 32] {
            // Saved as trusted: it must still be.
            return match c.untrusted {
                None => Recheck::Same,
                Some(why) => Recheck::Changed { now: format!("{} {}", c.fingerprint, untrusted_text(&why)) },
            };
        }
        return match key_of(&c.fingerprint) {
            Some(k) if &k == saved => Recheck::Same,
            Some(_) => Recheck::Changed { now: c.fingerprint },
            None => Recheck::Odd,
        };
    }
    match key_of(learned) {
        Some(k) if &k == saved => Recheck::Same,
        Some(_) => Recheck::Changed { now: learned.to_string() },
        None => Recheck::Odd,
    }
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
    Connect { addr: Address, fingerprint: String, remember: bool, at_login: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Form {
    pub step: Step,
    /// Shown under the address, or on the key sheet.
    pub error: Option<String>,
    pub remember: bool,
    /// Connect it again at each login (D8): only for a remembered share.
    pub at_login: bool,
    pub want: Option<Want>,
    /// Connect was pressed before the field's handle came. Arca's field
    /// hands one over only when it loses focus (or on Enter or Tab), and
    /// the press on Connect is what takes its focus, so the handle comes
    /// just after the press: connect then, or give up after a moment.
    pub armed: Option<std::time::Instant>,
    /// A WebDAV server's certificate, as learned.
    pub cert: Option<Cert>,
    /// What an SMB server said, as learned.
    pub smb: Option<SmbInfo>,
}

/// How long a press on Connect waits for the field's handle.
pub const ARMED_WAIT: std::time::Duration = std::time::Duration::from_millis(1500);

impl Default for Form {
    fn default() -> Self {
        Form { step: Step::Address, error: None, remember: true, at_login: false, want: None, armed: None, cert: None, smb: None }
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
            Ok(learned) if addr.proto == Proto::Smb => match parse_smb_info(&learned) {
                Some(i) => {
                    self.error = None;
                    self.step = Step::Key { addr, fingerprint: i.fingerprint.clone() };
                    self.smb = Some(i);
                }
                None => {
                    self.error = Some(format!("{} didn't answer as an SMB server", addr.host));
                    self.step = Step::Address;
                }
            },
            Ok(learned) if addr.proto == Proto::Webdav => {
                self.error = None;
                match parse_cert(&learned) {
                    // The trust store vouches for the name: no key to check.
                    Some(c) if c.untrusted.is_none() => {
                        self.step = Step::Password { addr, fingerprint: TRUSTED.into(), filled: false };
                        self.cert = Some(c);
                    }
                    Some(c) => {
                        self.step = Step::Key { addr, fingerprint: c.fingerprint.clone() };
                        self.cert = Some(c);
                    }
                    None => {
                        self.error = Some(format!("{} didn't show a certificate", addr.host));
                        self.step = Step::Address;
                    }
                }
            }
            Ok(fingerprint) => {
                self.error = None;
                self.step = Step::Key { addr, fingerprint };
            }
            Err(e) => {
                // dispensatord's word for a server below the floor (§5.3).
                self.error = Some(match e.strip_prefix("refused: ") {
                    Some(why) => format!("Not connecting to {}: {why}", addr.host),
                    None => format!("Couldn't reach {}: {e}", addr.host),
                });
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
        if f && self.armed.take().is_some() {
            self.connect();
        }
    }

    pub fn can_connect(&self) -> bool {
        matches!(self.step, Step::Password { filled: true, .. })
    }

    /// Connect is offered on the password step; the field's handle may
    /// still be on its way (see `armed`).
    pub fn can_press_connect(&self) -> bool {
        matches!(self.step, Step::Password { .. }) && self.armed.is_none()
    }

    pub fn connect(&mut self) {
        self.connect_at(std::time::Instant::now());
    }

    pub fn connect_at(&mut self, now: std::time::Instant) {
        if !self.can_connect() {
            if matches!(self.step, Step::Password { .. }) {
                self.armed = Some(now);
                self.error = None;
            }
            return;
        }
        self.armed = None;
        self.error = None;
        let Step::Password { addr, fingerprint, .. } = self.step.clone() else { return };
        self.step = Step::Connecting(addr.clone());
        self.want = Some(Want::Connect { addr, fingerprint, remember: self.remember, at_login: self.remember && self.at_login });
    }

    /// Connecting failed: back to the password, with why.
    pub fn failed(&mut self, why: String) {
        // The field's handle was redeemed: start again from the address.
        if matches!(self.step, Step::Connecting(_)) {
            self.error = Some(why);
            self.step = Step::Address;
        }
    }

    /// No handle came after a press on Connect: the field was empty.
    pub fn tick(&mut self, now: std::time::Instant) -> bool {
        match self.armed {
            Some(t) if now.duration_since(t) >= ARMED_WAIT => {
                self.armed = None;
                self.error = Some("Type the password in Arca's field first".into());
                true
            }
            _ => false,
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
        Address { proto: Proto::Sftp, user: user.into(), host: host.into(), port, path: path.into() }
    }

    #[test]
    fn addresses() {
        let smb = |user: &str, host: &str, port: u16, path: &str| Address { proto: Proto::Smb, user: user.into(), host: host.into(), port, path: path.into() };
        assert_eq!(parse_address("smb://alice@NAS.local/photos").unwrap(), smb("alice", "nas.local", 445, "/photos"));
        assert_eq!(parse_address("smb://HOME;alice@nas.local:4450/files/2026/").unwrap(), smb("HOME\\alice", "nas.local", 4450, "/files/2026"));
        assert_eq!(smb("HOME\\alice", "nas.local", 4450, "/files").text(), "smb://HOME;alice@nas.local:4450/files");
        for bad in ["smb://nas.local/photos", "smb://alice@nas.local", "smb://alice@nas.local/", "smb://alice@nas.local/a:b", "smb://alice@nas.local/x/../y", "smb://a\\b\\c@nas.local/x", "\\\\nas\\photos"] {
            assert!(parse_address(bad).is_err(), "{bad}");
        }
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
    fn a_saved_server_is_rechecked() {
        let k = [7u8; 32];
        assert_eq!(recheck(Proto::Sftp, &k, &fingerprint(&k)), Recheck::Same);
        let other = fingerprint(&[8u8; 32]);
        assert_eq!(recheck(Proto::Sftp, &k, &other), Recheck::Changed { now: other.clone() });
        assert_eq!(recheck(Proto::Sftp, &k, "no key"), Recheck::Odd);
        // WebDAV: a pinned key, or a name the trust store must still vouch for.
        let pinned = format!("untrusted invalid peer certificate: UnknownIssuer\n{}\nsubject my-nas\nissuer my-nas\nexpires 261214", fingerprint(&k));
        assert_eq!(recheck(Proto::Webdav, &k, &pinned), Recheck::Same);
        assert!(matches!(recheck(Proto::Webdav, &[9; 32], &pinned), Recheck::Changed { .. }));
        assert!(matches!(recheck(Proto::Webdav, &[0; 32], &pinned), Recheck::Changed { now } if now.contains("authority")));
        let trusted = format!("trusted\n{}\nsubject nas.lan", fingerprint(&k));
        assert_eq!(recheck(Proto::Webdav, &[0; 32], &trusted), Recheck::Same);
        assert_eq!(recheck(Proto::Webdav, &[0; 32], "nothing"), Recheck::Odd);
    }

    #[test]
    fn webdav_addresses() {
        let a = parse_address("https://alice@NAS.lan/dav/My Files").unwrap();
        assert_eq!((a.proto, a.host.as_str(), a.port, a.path.as_str()), (Proto::Webdav, "nas.lan", 443, "/dav/My Files"));
        assert_eq!(a.text(), "https://alice@nas.lan/dav/My Files");
        let a = parse_address("davs://alice@10.0.2.2:8444").unwrap();
        assert_eq!((a.port, a.path.as_str()), (8444, "/"));
        assert_eq!(a.text(), "https://alice@10.0.2.2:8444/");
        assert!(parse_address("http://alice@nas.lan/dav").unwrap_err().contains("https"));
        assert!(parse_address("https://nas.lan/dav").is_err(), "a name to log in with");
        assert!(parse_address("https://alice@nas.lan/a/../b").is_err());
        assert!(parse_address("https://alice@nas.lan/a?b").is_err());
    }

    #[test]
    fn a_trusted_webdav_server_goes_straight_to_the_password() {
        let mut f = Form::default();
        f.submit_address("https://alice@nas.lan/dav");
        f.take_want();
        f.learned(Ok(format!("trusted\n{}\nsubject nas.lan\nissuer R11\nexpires 270101", fingerprint(&[3; 32]))));
        assert!(matches!(&f.step, Step::Password { fingerprint, .. } if fingerprint == TRUSTED));
        assert_eq!(f.cert.as_ref().unwrap().issuer, "R11");
        f.field_filled(true);
        f.connect();
        assert!(matches!(f.take_want(), Some(Want::Connect { fingerprint, .. }) if fingerprint == TRUSTED));
        assert_eq!(key_for(TRUSTED), Some([0; 32]));
        assert_eq!(identity_of(Proto::Webdav, &[0; 32]), TRUSTED);
        assert_eq!(identity_of(Proto::Sftp, &[0; 32]), fingerprint(&[0; 32]));
        // Self-signed: its key, for the user to check.
        let mut f = Form::default();
        f.submit_address("https://alice@my-nas/dav");
        f.take_want();
        f.learned(Ok(format!("untrusted invalid peer certificate: UnknownIssuer\n{}\nsubject my-nas", fingerprint(&[4; 32]))));
        assert!(matches!(&f.step, Step::Key { fingerprint: fp, .. } if fp == &fingerprint(&[4; 32])));
        assert!(untrusted_text(f.cert.as_ref().unwrap().untrusted.as_deref().unwrap()).contains("authority"));
        assert!(untrusted_text("invalid peer certificate: Other(OtherError(CaUsedAsEndEntity))").contains("self-signed"));
    }

    #[test]
    fn an_smb_server_is_shown_then_pinned_by_its_id() {
        let fp = fingerprint(&[5; 32]);
        let said = format!("smb 3.1.1 AES-128-GCM\n{fp}\nguid 00006d76-0000-0000-0000-000000000000");
        let mut f = Form::default();
        f.submit_address("smb://alice@nas.lan/files");
        assert!(matches!(f.take_want(), Some(Want::Learn(a)) if a.proto == Proto::Smb));
        f.learned(Ok(said.clone()));
        assert!(matches!(&f.step, Step::Key { fingerprint: k, .. } if k == &fp));
        assert_eq!(f.smb.as_ref().unwrap().cipher, "AES-128-GCM");
        f.trust();
        assert!(matches!(f.step, Step::Password { .. }));
        assert_eq!(recheck(Proto::Smb, &[5; 32], &said), Recheck::Same);
        assert!(matches!(recheck(Proto::Smb, &[6; 32], &said), Recheck::Changed { now } if now.contains("00006d76")));
        assert_eq!(recheck(Proto::Smb, &[5; 32], &fp), Recheck::Odd);
        // A server below the floor: why, and no key to accept.
        let mut f = Form::default();
        f.submit_address("smb://alice@old-nas/files");
        f.learned(Err("refused: the server won't encrypt (no AES-GCM)".into()));
        assert_eq!(f.step, Step::Address);
        assert_eq!(f.error.as_deref(), Some("Not connecting to old-nas: the server won't encrypt (no AES-GCM)"));
    }

    #[test]
    fn a_press_before_the_handle_connects_when_it_comes() {
        let mut f = Form::default();
        f.submit_address("alice@nas.local:/srv");
        f.learned(Ok("SHA256:x".into()));
        f.take_want();
        f.trust();
        // The press takes the field's focus; its handle comes after.
        f.connect();
        assert!(f.want.is_none() && !f.can_press_connect());
        f.field_filled(true);
        assert!(matches!(f.take_want(), Some(Want::Connect { .. })));
        assert!(matches!(f.step, Step::Connecting(_)) && f.armed.is_none());
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
        let t0 = std::time::Instant::now();
        f.connect_at(t0);
        assert!(f.want.is_none(), "an empty field connects nothing");
        assert!(!f.tick(t0) && f.armed.is_some(), "the press waits a moment for the handle");
        assert!(f.tick(t0 + super::ARMED_WAIT) && f.error.is_some() && f.armed.is_none(), "then says why");
        f.field_filled(true);
        f.remember = false;
        f.connect();
        assert_eq!(
            f.take_want(),
            Some(Want::Connect { addr: a("alice", "nas.local", 22, "/srv"), fingerprint: "SHA256:x".into(), remember: false, at_login: false })
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
