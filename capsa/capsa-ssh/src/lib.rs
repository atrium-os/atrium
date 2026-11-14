//! A small SSH-2 client, enough for SFTP (network-shares.md §5.1).
//!
//! What it does: version exchange, curve25519-sha256 key exchange with
//! OpenSSH's strict KEX (the Terrapin fix), a host key that must match the
//! caller's pin before anything else happens, AES-GCM packets, password or
//! Ed25519 public-key user authentication (the signature comes from the
//! caller, i.e. from Arca, so the private key never reaches this process),
//! one session channel running a subsystem, and server-initiated re-keying.
//! What it doesn't: agents, forwarding, shells, compression, SHA-1, DSA,
//! CBC or CTR modes.

pub mod crypto;
pub mod wire;

use std::io::{Read, Write};

use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crypto::PacketCipher;
use wire::{Reader, Writer};

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// The peer broke the protocol, or sent what we don't accept.
    Protocol(&'static str),
    /// The host key is unacceptable or doesn't prove itself.
    HostKey(&'static str),
    /// The host key is fine but isn't the pinned one; its fingerprint.
    HostKeyMismatch { got: String },
    /// No algorithm in common for this slot.
    NoCommon(&'static str),
    /// The server refused our credentials; the methods it would take.
    AuthFailed(Vec<String>),
    /// The server ended the connection: reason code and its text.
    Disconnected(u32, String),
    /// The server refused to open the channel or start the subsystem.
    ChannelRefused,
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "i/o: {e}"),
            Error::Protocol(s) => write!(f, "protocol: {s}"),
            Error::HostKey(s) => write!(f, "host key: {s}"),
            Error::HostKeyMismatch { got } => write!(f, "host key changed: server now shows {got}"),
            Error::NoCommon(s) => write!(f, "no {s} in common with the server"),
            Error::AuthFailed(m) => write!(f, "authentication refused (server takes: {})", m.join(",")),
            Error::Disconnected(c, t) => write!(f, "server disconnected ({c}): {t}"),
            Error::ChannelRefused => write!(f, "server refused the channel"),
        }
    }
}

impl std::error::Error for Error {}

pub mod msg {
    pub const DISCONNECT: u8 = 1;
    pub const IGNORE: u8 = 2;
    pub const UNIMPLEMENTED: u8 = 3;
    pub const DEBUG: u8 = 4;
    pub const SERVICE_REQUEST: u8 = 5;
    pub const SERVICE_ACCEPT: u8 = 6;
    pub const EXT_INFO: u8 = 7;
    pub const KEXINIT: u8 = 20;
    pub const NEWKEYS: u8 = 21;
    pub const KEX_ECDH_INIT: u8 = 30;
    pub const KEX_ECDH_REPLY: u8 = 31;
    pub const USERAUTH_REQUEST: u8 = 50;
    pub const USERAUTH_FAILURE: u8 = 51;
    pub const USERAUTH_SUCCESS: u8 = 52;
    pub const USERAUTH_BANNER: u8 = 53;
    pub const USERAUTH_PK_OK: u8 = 60;
    pub const USERAUTH_INFO_REQUEST: u8 = 60;
    pub const USERAUTH_INFO_RESPONSE: u8 = 61;
    pub const GLOBAL_REQUEST: u8 = 80;
    pub const REQUEST_FAILURE: u8 = 82;
    pub const CHANNEL_OPEN: u8 = 90;
    pub const CHANNEL_OPEN_CONFIRMATION: u8 = 91;
    pub const CHANNEL_OPEN_FAILURE: u8 = 92;
    pub const CHANNEL_WINDOW_ADJUST: u8 = 93;
    pub const CHANNEL_DATA: u8 = 94;
    pub const CHANNEL_EXTENDED_DATA: u8 = 95;
    pub const CHANNEL_EOF: u8 = 96;
    pub const CHANNEL_CLOSE: u8 = 97;
    pub const CHANNEL_REQUEST: u8 = 98;
    pub const CHANNEL_SUCCESS: u8 = 99;
    pub const CHANNEL_FAILURE: u8 = 100;
}

const CLIENT_VERSION: &str = "SSH-2.0-Atrium_capsa-ssh_0.1";
const KEX_ALGS: &[&str] = &["curve25519-sha256", "curve25519-sha256@libssh.org"];
const HOSTKEY_ALGS: &[&str] = &["ssh-ed25519", "ecdsa-sha2-nistp256", "rsa-sha2-512", "rsa-sha2-256"];
const CIPHERS: &[&str] = &["aes256-gcm@openssh.com", "aes128-gcm@openssh.com"];
/// Never used with an AEAD cipher, but the slot must name something.
const MACS: &[&str] = &["hmac-sha2-256-etm@openssh.com"];
const STRICT_C: &str = "kex-strict-c-v00@openssh.com";
const STRICT_S: &str = "kex-strict-s-v00@openssh.com";

/// The largest packet we accept (RFC 4253 asks for at least 35000).
const MAX_PACKET: usize = 256 * 1024;
/// Our receive window and the largest data packet we ask for.
const WINDOW: u32 = 2 * 1024 * 1024;
const MAX_DATA: u32 = 32 * 1024;

/// What the caller pins the server to.
#[derive(Clone, Debug)]
pub enum HostKeyPolicy {
    /// Only this fingerprint ("SHA256:...") is accepted.
    Pinned(String),
    /// First contact: accept any acceptable key and report it, so the user
    /// can be shown the fingerprint before anything secret is sent. A
    /// connection made this way never authenticates.
    Learn,
}

/// How the user proves who they are.
pub enum Auth<'a> {
    Password(&'a [u8]),
    /// An Ed25519 public key and a signer for the data the server must see
    /// signed (Arca's SIGN; the key never enters this process).
    Ed25519 { public: [u8; 32], sign: &'a mut dyn FnMut(&[u8]) -> Result<[u8; 64], String> },
}

struct Dir {
    cipher: Option<PacketCipher>,
    seq: u32,
}

/// An SSH connection, authenticated or not.
pub struct Client<S: Read + Write> {
    s: S,
    rx: Dir,
    tx: Dir,
    v_s: Vec<u8>,
    session_id: Vec<u8>,
    host_key: Vec<u8>,
    strict: bool,
    /// Bytes read from the socket and not yet used.
    inbuf: Vec<u8>,
    chan: Option<Channel>,
}

struct Channel {
    remote: u32,
    /// How much more we may send.
    send_window: u64,
    send_max: u32,
    /// How much more the server may send before we adjust.
    recv_window: u32,
    data: Vec<u8>,
    eof: bool,
}

impl<S: Read + Write> Client<S> {
    /// Version exchange and the first key exchange. With `Pinned`, a host
    /// key that isn't the pinned one ends here with `HostKeyMismatch`.
    pub fn connect(mut s: S, policy: &HostKeyPolicy) -> Result<Self, Error> {
        s.write_all(format!("{CLIENT_VERSION}\r\n").as_bytes())?;
        let mut inbuf = Vec::new();
        let v_s = read_version(&mut s, &mut inbuf)?;
        let mut c = Client {
            s,
            rx: Dir { cipher: None, seq: 0 },
            tx: Dir { cipher: None, seq: 0 },
            v_s,
            session_id: Vec::new(),
            host_key: Vec::new(),
            strict: false,
            inbuf,
            chan: None,
        };
        c.kex(None, policy)?;
        Ok(c)
    }

    /// The server's host key fingerprint, as checked.
    pub fn host_fingerprint(&self) -> String {
        crypto::fingerprint(&self.host_key)
    }

    fn send(&mut self, payload: &[u8]) -> Result<(), Error> {
        let block = 16usize;
        let aead = self.tx.cipher.is_some();
        // With AEAD the length isn't part of the padded block.
        let base = if aead { 1 + payload.len() } else { 5 + payload.len() };
        let mut pad = block - base % block;
        if pad < 4 {
            pad += block;
        }
        let plen = 1 + payload.len() + pad;
        let mut body = Vec::with_capacity(plen + 16);
        body.push(pad as u8);
        body.extend_from_slice(payload);
        let mut padding = vec![0u8; pad];
        getrandom::getrandom(&mut padding).map_err(|_| Error::Protocol("no randomness"))?;
        body.extend_from_slice(&padding);
        let len = (plen as u32).to_be_bytes();
        let mut out = Vec::with_capacity(4 + plen + 16);
        out.extend_from_slice(&len);
        if let Some(c) = self.tx.cipher.as_mut() {
            let tag = c.seal(&len, &mut body);
            out.extend_from_slice(&body);
            out.extend_from_slice(&tag);
        } else {
            out.extend_from_slice(&body);
        }
        self.s.write_all(&out)?;
        self.tx.seq = self.tx.seq.wrapping_add(1);
        Ok(())
    }

    fn fill(&mut self, n: usize) -> Result<(), Error> {
        let mut tmp = [0u8; 16384];
        while self.inbuf.len() < n {
            let k = self.s.read(&mut tmp)?;
            if k == 0 {
                return Err(Error::Io(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "server closed the connection")));
            }
            self.inbuf.extend_from_slice(&tmp[..k]);
        }
        Ok(())
    }

    /// One packet's payload, decrypted and checked.
    fn recv_raw(&mut self) -> Result<Vec<u8>, Error> {
        self.fill(4)?;
        let len = [self.inbuf[0], self.inbuf[1], self.inbuf[2], self.inbuf[3]];
        let plen = u32::from_be_bytes(len) as usize;
        if !(5..=MAX_PACKET).contains(&plen) {
            return Err(Error::Protocol("packet length out of range"));
        }
        let aead = self.rx.cipher.is_some();
        if aead && plen % 16 != 0 {
            return Err(Error::Protocol("packet length not a multiple of the block"));
        }
        let tag = if aead { 16 } else { 0 };
        self.fill(4 + plen + tag)?;
        let mut body: Vec<u8> = self.inbuf[4..4 + plen].to_vec();
        if let Some(c) = self.rx.cipher.as_mut() {
            let t = self.inbuf[4 + plen..4 + plen + 16].to_vec();
            c.open(&len, &mut body, &t)?;
        }
        self.inbuf.drain(..4 + plen + tag);
        self.rx.seq = self.rx.seq.wrapping_add(1);
        let pad = body[0] as usize;
        if pad < 4 || pad + 1 >= plen {
            return Err(Error::Protocol("bad padding length"));
        }
        let payload = body[1..plen - pad].to_vec();
        if payload.is_empty() {
            return Err(Error::Protocol("empty packet"));
        }
        Ok(payload)
    }

    /// The next packet that isn't transport noise; handles re-keying.
    fn recv(&mut self) -> Result<Vec<u8>, Error> {
        loop {
            let p = self.recv_raw()?;
            match p[0] {
                msg::IGNORE | msg::DEBUG | msg::UNIMPLEMENTED => continue,
                msg::DISCONNECT => {
                    let mut r = Reader::new(&p[1..]);
                    let code = r.u32().unwrap_or(0);
                    let text = r.utf8().unwrap_or("").chars().take(200).collect();
                    return Err(Error::Disconnected(code, text));
                }
                msg::KEXINIT => {
                    // The server re-keys; the host key must stay the same.
                    let pin = HostKeyPolicy::Pinned(crypto::fingerprint(&self.host_key));
                    self.kex(Some(p), &pin)?;
                    continue;
                }
                msg::GLOBAL_REQUEST => {
                    let mut r = Reader::new(&p[1..]);
                    let _name = r.string()?;
                    if r.bool()? {
                        self.send(&[msg::REQUEST_FAILURE])?;
                    }
                    continue;
                }
                msg::EXT_INFO => continue,
                _ => return Ok(p),
            }
        }
    }

    fn kexinit_payload() -> Result<Vec<u8>, Error> {
        let mut cookie = [0u8; 16];
        getrandom::getrandom(&mut cookie).map_err(|_| Error::Protocol("no randomness"))?;
        let mut kex: Vec<&str> = KEX_ALGS.to_vec();
        kex.push(STRICT_C);
        let mut w = Writer::new(msg::KEXINIT);
        w.raw(&cookie)
            .string(kex.join(",").as_bytes())
            .string(HOSTKEY_ALGS.join(",").as_bytes())
            .string(CIPHERS.join(",").as_bytes())
            .string(CIPHERS.join(",").as_bytes())
            .string(MACS.join(",").as_bytes())
            .string(MACS.join(",").as_bytes())
            .string(b"none")
            .string(b"none")
            .string(b"")
            .string(b"")
            .bool(false)
            .u32(0);
        Ok(w.0)
    }

    /// A key exchange: the first one (server KEXINIT not yet read) or a
    /// re-key the server started (`their` is its KEXINIT).
    fn kex(&mut self, their: Option<Vec<u8>>, policy: &HostKeyPolicy) -> Result<(), Error> {
        let first = self.session_id.is_empty();
        let i_c = Self::kexinit_payload()?;
        self.send(&i_c)?;
        let i_s = match their {
            Some(p) => p,
            None => {
                let p = self.recv_raw()?;
                if p[0] != msg::KEXINIT {
                    // Strict KEX (and RFC 4253 for the first one): nothing
                    // may come before the server's KEXINIT.
                    return Err(Error::Protocol("message before the server's KEXINIT"));
                }
                p
            }
        };
        let mut r = Reader::new(&i_s[17..]);
        let kex_s = r.name_list()?;
        let hk_s = r.name_list()?;
        let enc_cs = r.name_list()?;
        let enc_sc = r.name_list()?;
        let _mac_cs = r.name_list()?;
        let _mac_sc = r.name_list()?;
        let comp_cs = r.name_list()?;
        let comp_sc = r.name_list()?;
        let _ = (r.name_list()?, r.name_list()?);
        let follows = r.bool()?;
        if first {
            self.strict = kex_s.contains(&STRICT_S);
        }
        let pick = |ours: &[&'static str], theirs: &[&str], what: &'static str| -> Result<&'static str, Error> {
            ours.iter().copied().find(|a| theirs.contains(a)).ok_or(Error::NoCommon(what))
        };
        let kex_alg = pick(KEX_ALGS, &kex_s, "key exchange")?;
        let hk_alg = pick(HOSTKEY_ALGS, &hk_s, "host key algorithm")?;
        let c_cs = pick(CIPHERS, &enc_cs, "cipher")?;
        let c_sc = pick(CIPHERS, &enc_sc, "cipher")?;
        if !comp_cs.contains(&"none") || !comp_sc.contains(&"none") {
            return Err(Error::NoCommon("compression"));
        }
        if follows && kex_s.first() != Some(&kex_alg) {
            // A guessed packet for another method follows; drop it.
            let _ = self.recv_raw()?;
        }
        let _ = kex_alg;

        let mut seed = Zeroizing::new([0u8; 32]);
        getrandom::getrandom(&mut seed[..]).map_err(|_| Error::Protocol("no randomness"))?;
        let secret = x25519_dalek::StaticSecret::from(*seed);
        let q_c = x25519_dalek::PublicKey::from(&secret);
        let mut w = Writer::new(msg::KEX_ECDH_INIT);
        w.string(q_c.as_bytes());
        self.send(&w.0)?;
        let reply = self.recv_raw()?;
        if reply[0] != msg::KEX_ECDH_REPLY {
            return Err(Error::Protocol("expected KEX_ECDH_REPLY"));
        }
        let mut r = Reader::new(&reply[1..]);
        let k_s = r.string()?.to_vec();
        let q_s: [u8; 32] = r.string()?.try_into().map_err(|_| Error::Protocol("server key share is not 32 bytes"))?;
        let sig = r.string()?.to_vec();
        if !r.done() {
            return Err(Error::Protocol("trailing bytes in KEX_ECDH_REPLY"));
        }
        let shared = secret.diffie_hellman(&x25519_dalek::PublicKey::from(q_s));
        if !shared.was_contributory() {
            return Err(Error::Protocol("degenerate key share"));
        }
        let k = Zeroizing::new(shared.as_bytes().to_vec());

        let mut hw = Writer::default();
        hw.string(CLIENT_VERSION.as_bytes())
            .string(&self.v_s)
            .string(&i_c)
            .string(&i_s)
            .string(&k_s)
            .string(q_c.as_bytes())
            .string(&q_s)
            .mpint(&k);
        let h = Sha256::digest(&hw.0).to_vec();

        // The host key: acceptable, proves itself, and is the pinned one.
        crypto::verify_host_sig(hk_alg, &k_s, &h, &sig)?;
        let fp = crypto::fingerprint(&k_s);
        if let HostKeyPolicy::Pinned(want) = policy {
            if &fp != want {
                return Err(Error::HostKeyMismatch { got: fp });
            }
        }
        if first {
            self.session_id = h.clone();
            self.host_key = k_s;
        }

        self.send(&[msg::NEWKEYS])?;
        let nk = self.recv_raw()?;
        if nk != [msg::NEWKEYS] {
            return Err(Error::Protocol("expected NEWKEYS"));
        }
        let sid = self.session_id.clone();
        let mk = |l: u8, n: usize| crypto::derive(&k, &h, l, &sid, n);
        let tx = PacketCipher::new(c_cs, &mk(b'C', PacketCipher::key_len(c_cs)), &mk(b'A', 12))?;
        let rx = PacketCipher::new(c_sc, &mk(b'D', PacketCipher::key_len(c_sc)), &mk(b'B', 12))?;
        self.tx.cipher = Some(tx);
        self.rx.cipher = Some(rx);
        if self.strict {
            self.tx.seq = 0;
            self.rx.seq = 0;
        }
        Ok(())
    }

    /// User authentication, then the session channel with `subsystem`.
    pub fn login(&mut self, user: &str, auth: Auth<'_>, subsystem: &str) -> Result<(), Error> {
        let mut w = Writer::new(msg::SERVICE_REQUEST);
        w.string(b"ssh-userauth");
        self.send(&w.0)?;
        let p = self.recv()?;
        if p[0] != msg::SERVICE_ACCEPT {
            return Err(Error::Protocol("ssh-userauth not accepted"));
        }
        let password: Option<&[u8]> = match &auth {
            Auth::Password(pw) => Some(pw),
            _ => None,
        };
        let mut w = Writer::new(msg::USERAUTH_REQUEST);
        w.string(user.as_bytes()).string(b"ssh-connection");
        match auth {
            Auth::Password(pw) => {
                w.string(b"password").bool(false).string(pw);
            }
            Auth::Ed25519 { public, sign } => {
                let mut blob = Writer::default();
                blob.string(b"ssh-ed25519").string(&public);
                let mut data = Writer::default();
                data.string(&self.session_id)
                    .byte(msg::USERAUTH_REQUEST)
                    .string(user.as_bytes())
                    .string(b"ssh-connection")
                    .string(b"publickey")
                    .bool(true)
                    .string(b"ssh-ed25519")
                    .string(&blob.0);
                let s = sign(&data.0).map_err(|_| Error::AuthFailed(vec!["(the signer refused)".into()]))?;
                let mut sig = Writer::default();
                sig.string(b"ssh-ed25519").string(&s);
                w.string(b"publickey").bool(true).string(b"ssh-ed25519").string(&blob.0).string(&sig.0);
            }
        }
        let req = Zeroizing::new(std::mem::take(&mut w.0));
        self.send(&req)?;
        let mut kbd_tried = false;
        loop {
            let p = self.recv()?;
            match p[0] {
                msg::USERAUTH_SUCCESS => break,
                msg::USERAUTH_BANNER => continue,
                msg::USERAUTH_FAILURE => {
                    let mut r = Reader::new(&p[1..]);
                    let methods: Vec<String> = r.name_list()?.into_iter().map(String::from).collect();
                    // Many servers (PAM) take a password only through
                    // keyboard-interactive: try that once, answering a
                    // single hidden prompt with the same password.
                    if let (Some(_), false) = (password, kbd_tried) {
                        if methods.iter().any(|m| m == "keyboard-interactive") {
                            kbd_tried = true;
                            let mut w = Writer::new(msg::USERAUTH_REQUEST);
                            w.string(user.as_bytes()).string(b"ssh-connection").string(b"keyboard-interactive").string(b"").string(b"");
                            self.send(&w.0)?;
                            continue;
                        }
                    }
                    return Err(Error::AuthFailed(methods));
                }
                msg::USERAUTH_INFO_REQUEST if kbd_tried => {
                    let mut r = Reader::new(&p[1..]);
                    let (_name, _instr, _lang) = (r.string()?, r.string()?, r.string()?);
                    let n = r.u32()?;
                    let mut prompts = Vec::new();
                    for _ in 0..n.min(8) {
                        prompts.push((r.string()?, r.bool()?));
                    }
                    let mut w = Writer::new(msg::USERAUTH_INFO_RESPONSE);
                    match (n, prompts.first()) {
                        (0, _) => {
                            w.u32(0);
                        }
                        (1, Some((_, false))) => {
                            w.u32(1).string(password.unwrap_or_default());
                        }
                        // Anything else (several prompts, an echoed one)
                        // is not a password prompt: answer nothing.
                        _ => {
                            w.u32(n);
                            for _ in 0..n {
                                w.string(b"");
                            }
                        }
                    }
                    let resp = Zeroizing::new(std::mem::take(&mut w.0));
                    self.send(&resp)?;
                }
                _ => return Err(Error::Protocol("unexpected message during authentication")),
            }
        }
        // The session channel and its subsystem.
        let mut w = Writer::new(msg::CHANNEL_OPEN);
        w.string(b"session").u32(0).u32(WINDOW).u32(MAX_DATA);
        self.send(&w.0)?;
        let p = self.recv()?;
        let (remote, window, max) = match p[0] {
            msg::CHANNEL_OPEN_CONFIRMATION => {
                let mut r = Reader::new(&p[1..]);
                if r.u32()? != 0 {
                    return Err(Error::Protocol("confirmation for another channel"));
                }
                (r.u32()?, r.u32()?, r.u32()?)
            }
            msg::CHANNEL_OPEN_FAILURE => return Err(Error::ChannelRefused),
            _ => return Err(Error::Protocol("unexpected reply to CHANNEL_OPEN")),
        };
        self.chan = Some(Channel { remote, send_window: u64::from(window), send_max: max.clamp(1, MAX_DATA), recv_window: WINDOW, data: Vec::new(), eof: false });
        let mut w = Writer::new(msg::CHANNEL_REQUEST);
        w.u32(remote).string(b"subsystem").bool(true).string(subsystem.as_bytes());
        self.send(&w.0)?;
        loop {
            let p = self.recv()?;
            match p[0] {
                msg::CHANNEL_SUCCESS => return Ok(()),
                msg::CHANNEL_FAILURE => return Err(Error::ChannelRefused),
                msg::CHANNEL_WINDOW_ADJUST => self.on_channel(&p)?,
                _ => return Err(Error::Protocol("unexpected reply to the subsystem request")),
            }
        }
    }

    fn on_channel(&mut self, p: &[u8]) -> Result<(), Error> {
        let mut r = Reader::new(&p[1..]);
        let id = r.u32()?;
        if id != 0 {
            return Err(Error::Protocol("message for a channel we didn't open"));
        }
        let ch = self.chan.as_mut().ok_or(Error::Protocol("channel message before the channel"))?;
        match p[0] {
            msg::CHANNEL_WINDOW_ADJUST => {
                ch.send_window = ch.send_window.saturating_add(u64::from(r.u32()?)).min(u64::from(u32::MAX));
            }
            msg::CHANNEL_DATA => {
                let d = r.string()?;
                if d.len() as u64 > u64::from(ch.recv_window) {
                    return Err(Error::Protocol("server sent past our window"));
                }
                ch.recv_window -= d.len() as u32;
                if ch.data.len() + d.len() > 2 * WINDOW as usize {
                    return Err(Error::Protocol("channel buffer overflow"));
                }
                ch.data.extend_from_slice(d);
            }
            msg::CHANNEL_EXTENDED_DATA => {
                let _code = r.u32()?;
                let d = r.string()?;
                ch.recv_window = ch.recv_window.saturating_sub(d.len() as u32);
            }
            msg::CHANNEL_EOF => ch.eof = true,
            msg::CHANNEL_CLOSE => {
                ch.eof = true;
            }
            msg::CHANNEL_REQUEST => {
                let _name = r.string()?;
                if r.bool()? {
                    let mut w = Writer::new(msg::CHANNEL_FAILURE);
                    w.u32(ch.remote);
                    self.send(&w.0)?;
                }
            }
            _ => return Err(Error::Protocol("unexpected channel message")),
        }
        Ok(())
    }

    /// Writes to the subsystem, waiting for window as needed.
    pub fn write_data(&mut self, mut d: &[u8]) -> Result<(), Error> {
        while !d.is_empty() {
            let (remote, n) = {
                let ch = self.chan.as_ref().ok_or(Error::Protocol("no channel"))?;
                if ch.eof {
                    return Err(Error::Protocol("the subsystem ended"));
                }
                (ch.remote, (ch.send_max as usize).min(ch.send_window as usize).min(d.len()))
            };
            if n == 0 {
                let p = self.recv()?;
                self.on_channel(&p)?;
                continue;
            }
            let mut w = Writer::new(msg::CHANNEL_DATA);
            w.u32(remote).string(&d[..n]);
            self.send(&w.0)?;
            self.chan.as_mut().unwrap().send_window -= n as u64;
            d = &d[n..];
        }
        Ok(())
    }

    /// Exactly `n` bytes from the subsystem.
    pub fn read_exact_data(&mut self, n: usize) -> Result<Vec<u8>, Error> {
        loop {
            let ch = self.chan.as_mut().ok_or(Error::Protocol("no channel"))?;
            if ch.data.len() >= n {
                let out: Vec<u8> = ch.data.drain(..n).collect();
                // Give the window back once half of it is used.
                if ch.recv_window < WINDOW / 2 {
                    let add = WINDOW - ch.recv_window;
                    ch.recv_window = WINDOW;
                    let mut w = Writer::new(msg::CHANNEL_WINDOW_ADJUST);
                    w.u32(ch.remote).u32(add);
                    self.send(&w.0)?;
                }
                return Ok(out);
            }
            if ch.eof {
                return Err(Error::Protocol("the subsystem ended"));
            }
            let p = self.recv()?;
            self.on_channel(&p)?;
        }
    }

    /// Ends the connection politely.
    pub fn disconnect(mut self) {
        let mut w = Writer::new(msg::DISCONNECT);
        w.u32(11).string(b"bye").string(b"");
        let _ = self.send(&w.0);
    }
}

/// The server's identification line (RFC 4253 §4.2), skipping any lines it
/// sends before it. Bytes after it stay in `inbuf`.
fn read_version<S: Read>(s: &mut S, inbuf: &mut Vec<u8>) -> Result<Vec<u8>, Error> {
    let mut lines = 0;
    let mut tmp = [0u8; 1024];
    loop {
        if let Some(i) = inbuf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = inbuf.drain(..=i).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.starts_with(b"SSH-") {
                if !(line.starts_with(b"SSH-2.0-") || line.starts_with(b"SSH-1.99-")) {
                    return Err(Error::Protocol("server does not speak SSH-2"));
                }
                if line.len() > 253 {
                    return Err(Error::Protocol("identification line too long"));
                }
                return Ok(line);
            }
            lines += 1;
            if lines > 32 || line.len() > 1024 {
                return Err(Error::Protocol("no identification line"));
            }
            continue;
        }
        if inbuf.len() > 4096 {
            return Err(Error::Protocol("no identification line"));
        }
        let n = s.read(&mut tmp)?;
        if n == 0 {
            return Err(Error::Io(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "closed before identification")));
        }
        inbuf.extend_from_slice(&tmp[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_line_after_a_banner() {
        let mut src: &[u8] = b"hello\r\nSSH-2.0-OpenSSH_9.9\r\n\x00\x00";
        let mut inbuf = Vec::new();
        assert_eq!(read_version(&mut src, &mut inbuf).unwrap(), b"SSH-2.0-OpenSSH_9.9");
        assert_eq!(inbuf, b"\x00\x00");
        let mut src: &[u8] = b"SSH-1.5-old\r\n";
        assert!(read_version(&mut src, &mut Vec::new()).is_err());
    }
}
