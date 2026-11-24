//! The EAP peer state machine (RFC 3748 §2, RFC 4137 in spirit): one
//! conversation, packets in, packets out, no I/O. aura-keys carries the
//! packets over EAPOL and takes the MSK at the end.
//!
//! The order is fixed: the server's certificate is checked in the TLS
//! handshake, and no credential is written until the handshake finished.
//! A server that claims success must also have proved the password
//! (MSCHAPv2's authenticator response) where the method allows it.

use std::io::{Read, Write};
use std::sync::Arc;

use rustls::sign::CertifiedKey;
use rustls::ClientConnection;
use zeroize::Zeroizing;

use crate::eap::{self, typ};
use crate::tls::{self, Frames, In, Seen, Trust};
use crate::{mschapv2, peap, ttls};

pub use crate::profile::Method;

pub struct Config {
    pub method: Method,
    /// Sent in the clear (`anonymous@realm` by default for tunnelled methods).
    pub outer_identity: String,
    /// Sent inside the tunnel.
    pub identity: String,
    pub password: Zeroizing<String>,
    pub trust: Trust,
    /// EAP-TLS only.
    pub client: Option<Arc<CertifiedKey>>,
}

impl Config {
    /// The settings for a join from the stored profile.
    /// `system`: the system's root certificates, used when the profile
    /// asks for them (with a domain, so not any server they ever signed).
    pub fn from_profile(p: &crate::profile::Profile, system: &[rustls::pki_types::CertificateDer<'static>]) -> Config {
        let roots = |certs: &mut dyn Iterator<Item = rustls::pki_types::CertificateDer<'static>>| {
            let mut roots = rustls::RootCertStore::empty();
            for c in certs {
                let _ = roots.add(c);
            }
            Trust::Roots { roots: Arc::new(roots), domain: p.domain.clone() }
        };
        let trust = match (&p.pin, p.ca.is_empty()) {
            (Some(top), _) => Trust::Pinned { top: top.clone().into(), domain: p.domain.clone() },
            (None, false) => roots(&mut p.ca.iter().map(|c| c.clone().into())),
            (None, true) if p.system_roots && !p.domain.is_empty() => roots(&mut system.iter().cloned()),
            (None, true) => Trust::Unset,
        };
        Config {
            method: p.method,
            outer_identity: p.outer_identity(),
            identity: p.identity.clone(),
            password: p.password.clone(),
            trust,
            client: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Why {
    /// A method packet we could not read.
    Malformed(&'static str),
    /// EAP-Failure before any credential was sent.
    Refused,
    /// The credentials were refused.
    Rejected,
    /// The server would not run the configured method.
    Method,
    /// The server's certificate did not pass.
    Untrusted(String),
    Tls(String),
    /// The server did not prove it knows the password.
    ServerProof,
    /// EAP-Success before the method finished.
    EarlySuccess,
    /// PEAP's crypto-binding did not check out.
    Binding,
}

impl Why {
    /// A word for logs and reply lines (no spaces, nothing secret).
    pub fn code(&self) -> &'static str {
        match self {
            Why::Malformed(_) => "malformed",
            Why::Refused => "refused",
            Why::Rejected => "rejected",
            Why::Method => "method",
            Why::Untrusted(_) => "untrusted",
            Why::Tls(_) => "tls",
            Why::ServerProof => "server-proof",
            Why::EarlySuccess => "early-success",
            Why::Binding => "binding",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Out {
    /// An EAP response to send.
    Send(Vec<u8>),
    /// Authenticated; the MSK (PMK = its first 32 bytes).
    Done(Zeroizing<[u8; 64]>),
    Failed(Why),
    /// The trust is unset: the join stops here, before any password,
    /// and the user decides about this certificate.
    NeedTrust(Seen),
    /// Not for us or not readable; the conversation goes on.
    Dropped(&'static str),
}

struct Mschap {
    ident: u8,
    auth: [u8; 16],
    peer: [u8; 16],
    nt: [u8; 24],
}

pub struct Peer {
    cfg: Config,
    frames: Frames,
    conn: Option<ClientConnection>,
    verifier: Option<Arc<tls::Verifier>>,
    last: Option<(u8, Vec<u8>)>,
    started: bool,
    phase2: bool,
    sent_credentials: bool,
    proved: bool,
    mschap: Option<Mschap>,
    isk: Zeroizing<[u8; 32]>,
    msk: Option<Zeroizing<[u8; 64]>>,
    ended: bool,
}

fn random16() -> [u8; 16] {
    let mut b = [0u8; 16];
    rustls::crypto::ring::default_provider().secure_random.fill(&mut b).expect("system randomness");
    b
}

fn export<const N: usize>(conn: &ClientConnection, label: &[u8]) -> Result<Zeroizing<[u8; N]>, Why> {
    let mut k = Zeroizing::new([0u8; N]);
    conn.export_keying_material(&mut k[..], label, None).map_err(|e| Why::Tls(e.to_string()))?;
    Ok(k)
}

impl Peer {
    pub fn new(cfg: Config) -> Self {
        Peer {
            cfg,
            frames: Frames::default(),
            conn: None,
            verifier: None,
            last: None,
            started: false,
            phase2: false,
            sent_credentials: false,
            proved: false,
            mschap: None,
            isk: Zeroizing::new([0; 32]),
            msk: None,
            ended: false,
        }
    }

    fn end(&mut self, o: Out) -> Out {
        self.ended = true;
        self.conn = None;
        o
    }

    fn fail(&mut self, w: Why) -> Out {
        self.end(Out::Failed(w))
    }

    fn send(&mut self, id: u8, t: u8, data: &[u8]) -> Out {
        let p = eap::response(id, t, data);
        self.last = Some((id, p.clone()));
        Out::Send(p)
    }

    /// One EAP packet from the authenticator.
    pub fn handle(&mut self, pkt: &[u8]) -> Out {
        if self.ended {
            return Out::Dropped("conversation over");
        }
        let Ok(p) = eap::parse(pkt) else {
            return Out::Dropped("malformed EAP packet");
        };
        match p.code {
            eap::SUCCESS => match self.msk.take() {
                Some(m) => self.end(Out::Done(m)),
                None => self.fail(Why::EarlySuccess),
            },
            eap::FAILURE => self.fail(if self.sent_credentials { Why::Rejected } else { Why::Refused }),
            eap::REQUEST => {
                if let Some((id, r)) = &self.last {
                    if *id == p.id {
                        return Out::Send(r.clone());
                    }
                }
                let t = p.typ.unwrap();
                match t {
                    typ::IDENTITY => {
                        let id = self.cfg.outer_identity.clone();
                        self.send(p.id, typ::IDENTITY, id.as_bytes())
                    }
                    typ::NOTIFICATION => self.send(p.id, typ::NOTIFICATION, &[]),
                    t if t == self.cfg.method.eap_type() => {
                        self.started = true;
                        self.method(p.id, p.data)
                    }
                    _ if !self.started => {
                        let want = self.cfg.method.eap_type();
                        self.send(p.id, typ::NAK, &[want])
                    }
                    _ => self.fail(Why::Method),
                }
            }
            _ => Out::Dropped("a response"),
        }
    }

    fn method(&mut self, id: u8, data: &[u8]) -> Out {
        if self.conn.is_none() && !self.phase2 && data.first().is_some_and(|f| f & tls::START == 0) {
            // A retransmission from an earlier conversation the AP still
            // holds (a lost deauth): ours starts with the S flag.
            return Out::Dropped("TLS before start");
        }
        let input = match self.frames.take(data) {
            Ok(i) => i,
            Err(_) => return self.fail(Why::Malformed("TLS framing")),
        };
        let t = self.cfg.method.eap_type();
        match input {
            In::Start => {
                if self.sent_credentials || self.phase2 {
                    return self.fail(Why::Malformed("a second start"));
                }
                // The AP began again (our EAPOL-Start crossed its first
                // request): a new handshake, nothing was sent yet.
                self.conn = None;
                self.verifier = None;
                let client = if self.cfg.method == Method::Tls { self.cfg.client.clone() } else { None };
                let (mut conn, v) = match tls::client(self.cfg.trust.clone(), client) {
                    Ok(c) => c,
                    Err(e) => return self.fail(Why::Tls(e.to_string())),
                };
                let mut out = Vec::new();
                while conn.wants_write() {
                    conn.write_tls(&mut out).expect("writing to a Vec");
                }
                self.conn = Some(conn);
                self.verifier = Some(v);
                self.frames.queue(out);
                let f = self.frames.next(0);
                self.send(id, t, &f)
            }
            In::Partial | In::Ack => {
                let f = self.frames.next(0);
                self.send(id, t, &f)
            }
            In::Message(m) => match self.tls_message(id, &m) {
                Ok(()) => {
                    let f = self.frames.next(0);
                    self.send(id, t, &f)
                }
                Err(o) => self.end(o),
            },
        }
    }

    fn tls_message(&mut self, id: u8, m: &[u8]) -> Result<(), Out> {
        let mut conn = self.conn.take().ok_or(Out::Failed(Why::Malformed("TLS before start")))?;
        let mut rd = m;
        while !rd.is_empty() {
            let r = conn.read_tls(&mut rd).map_err(|e| e.to_string()).and_then(|_| conn.process_new_packets().map(|_| ()).map_err(|e| e.to_string()));
            if let Err(e) = r {
                if let Some(seen) = self.verifier.as_ref().and_then(|v| v.seen.lock().unwrap().take()) {
                    return Err(Out::NeedTrust(seen));
                }
                if e.contains("certificate") || e.contains("server certificate") {
                    return Err(Out::Failed(Why::Untrusted(e)));
                }
                return Err(Out::Failed(Why::Tls(e)));
            }
        }
        let mut plain = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match conn.reader().read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    plain.extend_from_slice(&buf[..n]);
                    if plain.len() > tls::REASSEMBLY_MAX {
                        return Err(Out::Failed(Why::Malformed("too much tunnel data")));
                    }
                }
                Err(_) => break,
            }
        }
        if !conn.is_handshaking() {
            let reply = self.phase2(&conn, id, &plain).map_err(Out::Failed)?;
            if !reply.is_empty() {
                conn.writer().write_all(&reply).expect("writing to the tunnel");
            }
        } else if !plain.is_empty() {
            return Err(Out::Failed(Why::Malformed("data before the handshake finished")));
        }
        let mut out = Vec::new();
        while conn.wants_write() {
            conn.write_tls(&mut out).expect("writing to a Vec");
        }
        self.frames.queue(out);
        self.conn = Some(conn);
        Ok(())
    }

    /// Inside the finished tunnel: what to write back.
    fn phase2(&mut self, conn: &ClientConnection, id: u8, plain: &[u8]) -> Result<Zeroizing<Vec<u8>>, Why> {
        let first = !self.phase2;
        self.phase2 = true;
        match self.cfg.method {
            Method::Tls => {
                if !plain.is_empty() {
                    return Err(Why::Malformed("data in an EAP-TLS tunnel"));
                }
                if first {
                    self.msk = Some(export(conn, b"client EAP encryption")?);
                }
                Ok(Zeroizing::new(Vec::new()))
            }
            Method::TtlsPap | Method::TtlsMschapv2 if first => self.ttls_credentials(conn, plain),
            Method::TtlsPap | Method::TtlsMschapv2 => self.ttls_reply(conn, plain),
            Method::Peap if plain.is_empty() => Ok(Zeroizing::new(Vec::new())),
            Method::Peap => self.peap(conn, id, plain),
        }
    }

    fn ttls_credentials(&mut self, conn: &ClientConnection, plain: &[u8]) -> Result<Zeroizing<Vec<u8>>, Why> {
        if !plain.is_empty() {
            return Err(Why::Malformed("server data before phase 2"));
        }
        let mut out = Vec::new();
        ttls::put(&mut out, 0, ttls::USER_NAME, self.cfg.identity.as_bytes());
        if self.cfg.method == Method::TtlsPap {
            ttls::put(&mut out, 0, ttls::USER_PASSWORD, &ttls::padded_password(&self.cfg.password));
            // PAP has no server proof beyond the certificate.
            self.msk = Some(export(conn, b"ttls keying material")?);
        } else {
            let ch: Zeroizing<[u8; 17]> = export(conn, b"ttls challenge")?;
            let auth: [u8; 16] = ch[..16].try_into().unwrap();
            let peer = random16();
            let nt = mschapv2::nt_response(&auth, &peer, &self.cfg.identity, &self.cfg.password);
            let mut r = Vec::with_capacity(50);
            r.extend_from_slice(&[ch[16], 0]);
            r.extend_from_slice(&peer);
            r.extend_from_slice(&[0; 8]);
            r.extend_from_slice(&nt);
            ttls::put(&mut out, ttls::MICROSOFT, ttls::MS_CHAP_CHALLENGE, &auth);
            ttls::put(&mut out, ttls::MICROSOFT, ttls::MS_CHAP2_RESPONSE, &r);
            self.mschap = Some(Mschap { ident: ch[16], auth, peer, nt });
        }
        self.sent_credentials = true;
        Ok(Zeroizing::new(out))
    }

    fn ttls_reply(&mut self, conn: &ClientConnection, plain: &[u8]) -> Result<Zeroizing<Vec<u8>>, Why> {
        let avps = ttls::parse(plain).ok_or(Why::Malformed("AVPs"))?;
        for a in avps {
            match (a.vendor, a.code) {
                (ttls::MICROSOFT, ttls::MS_CHAP2_SUCCESS) => {
                    let m = self.mschap.as_ref().ok_or(Why::Malformed("success without a challenge"))?;
                    let want = mschapv2::authenticator_response(&self.cfg.password, &m.nt, &m.peer, &m.auth, &self.cfg.identity);
                    if a.data.first() != Some(&m.ident) || !mschapv2::check_success(&a.data[1..], &want) {
                        return Err(Why::ServerProof);
                    }
                    self.msk = Some(export(conn, b"ttls keying material")?);
                }
                (ttls::MICROSOFT, ttls::MS_CHAP_ERROR) => return Err(Why::Rejected),
                _ if a.mandatory => return Err(Why::Malformed("an unknown mandatory AVP")),
                _ => {}
            }
        }
        Ok(Zeroizing::new(Vec::new()))
    }

    fn peap(&mut self, conn: &ClientConnection, outer_id: u8, plain: &[u8]) -> Result<Zeroizing<Vec<u8>>, Why> {
        // PEAPv0 drops the inner header, except on the extensions
        // (type 33) and on a bare inner Success or Failure.
        let full = plain.len() >= 4 && u16::from_be_bytes([plain[2], plain[3]]) as usize == plain.len();
        if full && plain.len() == 4 && plain[0] == eap::FAILURE {
            return Err(Why::Rejected);
        }
        let (id, t, data, full) = if full && plain.len() >= 5 && plain[0] == eap::REQUEST && plain[4] == typ::TLV {
            (plain[1], typ::TLV, &plain[5..], true)
        } else {
            (outer_id, plain[0], &plain[1..], false)
        };
        let (rt, body) = match t {
            typ::IDENTITY => (typ::IDENTITY, self.cfg.identity.as_bytes().to_vec()),
            typ::MSCHAPV2 => (typ::MSCHAPV2, self.peap_mschapv2(data)?),
            typ::TLV => (typ::TLV, self.peap_tlv(conn, data)?),
            _ => (typ::NAK, vec![typ::MSCHAPV2]),
        };
        let out = if full {
            eap::response(id, rt, &body)
        } else {
            let mut v = vec![rt];
            v.extend_from_slice(&body);
            v
        };
        Ok(Zeroizing::new(out))
    }

    fn peap_mschapv2(&mut self, d: &[u8]) -> Result<Vec<u8>, Why> {
        let (&op, _) = d.split_first().ok_or(Why::Malformed("MSCHAPv2"))?;
        match op {
            1 => {
                if d.len() < 21 || d[4] != 16 {
                    return Err(Why::Malformed("MSCHAPv2 challenge"));
                }
                let auth: [u8; 16] = d[5..21].try_into().unwrap();
                let peer = random16();
                let nt = mschapv2::nt_response(&auth, &peer, &self.cfg.identity, &self.cfg.password);
                let name = self.cfg.identity.as_bytes();
                let mut r = vec![2, d[1]];
                r.extend_from_slice(&((4 + 1 + 49 + name.len()) as u16).to_be_bytes());
                r.push(49);
                r.extend_from_slice(&peer);
                r.extend_from_slice(&[0; 8]);
                r.extend_from_slice(&nt);
                r.push(0);
                r.extend_from_slice(name);
                self.mschap = Some(Mschap { ident: d[1], auth, peer, nt });
                self.sent_credentials = true;
                Ok(r)
            }
            3 => {
                let m = self.mschap.as_ref().ok_or(Why::Malformed("success without a challenge"))?;
                let want = mschapv2::authenticator_response(&self.cfg.password, &m.nt, &m.peer, &m.auth, &self.cfg.identity);
                if d.len() < 4 || !mschapv2::check_success(&d[4..], &want) {
                    return Err(Why::ServerProof);
                }
                self.isk = mschapv2::session_key(&self.cfg.password, &m.nt);
                self.proved = true;
                Ok(vec![3])
            }
            4 => Err(Why::Rejected),
            _ => Err(Why::Malformed("MSCHAPv2 op")),
        }
    }

    fn peap_tlv(&mut self, conn: &ClientConnection, d: &[u8]) -> Result<Vec<u8>, Why> {
        let tlvs = peap::parse(d).ok_or(Why::Malformed("TLVs"))?;
        let mut result = None;
        let mut binding = None;
        for t in &tlvs {
            match t.typ {
                peap::RESULT if t.value.len() == 2 => result = Some(u16::from_be_bytes([t.value[0], t.value[1]])),
                peap::CRYPTO_BINDING => binding = Some(t),
                _ if t.mandatory => return Err(Why::Malformed("an unknown mandatory TLV")),
                _ => {}
            }
        }
        match result {
            Some(peap::SUCCESS) if self.proved => {}
            Some(peap::SUCCESS) => return Err(Why::ServerProof),
            Some(_) => return Err(Why::Rejected),
            None => return Err(Why::Malformed("no Result TLV")),
        }
        let tk: Zeroizing<[u8; 64]> = export(conn, b"client EAP encryption")?;
        let mut out = peap::result(peap::SUCCESS).to_vec();
        match binding {
            Some(b) => {
                let (ipmk, cmk) = peap::compound_keys(&tk[..], &self.isk);
                out.extend_from_slice(&peap::binding_response(b, &cmk).ok_or(Why::Binding)?);
                self.msk = Some(peap::bound_msk(&ipmk));
            }
            None => self.msk = Some(tk),
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(method: Method) -> Peer {
        Peer::new(Config {
            method,
            outer_identity: "anonymous@corp.example".into(),
            identity: "alice".into(),
            password: Zeroizing::new("pw".into()),
            trust: Trust::Unset,
            client: None,
        })
    }

    #[test]
    fn outer_exchange() {
        let mut p = peer(Method::TtlsPap);
        let Out::Send(r) = p.handle(&[1, 1, 0, 5, 1]) else { panic!() };
        assert_eq!(r, eap::response(1, typ::IDENTITY, b"anonymous@corp.example"));
        assert_eq!(p.handle(&[1, 1, 0, 5, 1]), Out::Send(r), "a repeated request gets the same answer");
        assert_eq!(p.handle(&[1, 2, 0, 6, typ::PEAP, 0x21]), Out::Send(eap::response(2, typ::NAK, &[typ::TTLS])));
        assert_eq!(p.handle(&[9]), Out::Dropped("malformed EAP packet"));
        let Out::Send(hello) = p.handle(&[1, 3, 0, 6, typ::TTLS, tls::START]) else { panic!() };
        assert_eq!(hello[4], typ::TTLS);
        assert_eq!(p.handle(&[1, 4, 0, 6, typ::PEAP, 0x21]), Out::Failed(Why::Method), "no switching once started");
        let mut q = peer(Method::Peap);
        assert_eq!(q.handle(&[1, 9, 0, 8, typ::PEAP, 0, 22, 3]), Out::Dropped("TLS before start"), "a stale record before our start");
        assert!(matches!(q.handle(&[1, 10, 0, 6, typ::PEAP, tls::START]), Out::Send(_)));
    }

    #[test]
    fn success_only_at_the_end() {
        let mut p = peer(Method::Peap);
        assert_eq!(p.handle(&[3, 1, 0, 4]), Out::Failed(Why::EarlySuccess));
        assert_eq!(p.handle(&[3, 1, 0, 4]), Out::Dropped("conversation over"));
        let mut p = peer(Method::Peap);
        assert_eq!(p.handle(&[4, 1, 0, 4]), Out::Failed(Why::Refused));
    }

    #[test]
    fn partial_tls_record_waits() {
        // Garbage instead of a ServerHello: a TLS error, not a trust question.
        let mut p = peer(Method::Peap);
        let _ = p.handle(&[1, 3, 0, 6, typ::PEAP, tls::START]);
        let o = p.handle(&[1, 4, 0, 10, typ::PEAP, 0, 22, 3, 3, 0]);
        assert!(matches!(o, Out::Send(_) | Out::Failed(Why::Tls(_))), "{o:?}");
    }
}
