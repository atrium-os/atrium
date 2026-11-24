//! atrium-aura-eap: one 802.1X join's EAP conversation (wifi.md §7.4).
//!
//! aura-keys starts it per join with a socket as stdin and nothing else.
//! It enters capability mode before reading a byte: from then on it can
//! reach nothing but that socket. It reads the profile, then EAP packets,
//! and writes back responses and, at the end, the MSK, a failure, or the
//! certificate the user has to decide about (aura_eap::helper). For
//! EAP-TLS it asks aura-keys to sign with the key Arca keeps.

use std::collections::VecDeque;
use std::io::Read;
use std::os::fd::FromRawFd;
use std::sync::{Arc, Mutex};

use aura_eap::helper::{self, Reader};
use aura_eap::profile::{Method, Profile};
use aura_eap::remote::RemoteKey;
use aura_eap::{Config, Out, Peer};
use zeroize::Zeroizing;

type Frame = (u8, Zeroizing<Vec<u8>>);

struct Pipe {
    sock: std::fs::File,
    rd: Reader,
    /// Frames read while waiting for a signature.
    stash: VecDeque<Frame>,
}

impl Pipe {
    /// The next frame, blocking; `None` at the end or on garbage.
    fn read(&mut self) -> Option<Frame> {
        loop {
            match self.rd.next() {
                Ok(Some(f)) => return Some(f),
                Ok(None) => {}
                Err(()) => return None,
            }
            let mut buf = [0u8; 8192];
            match self.sock.read(&mut buf) {
                Ok(0) | Err(_) => return None,
                Ok(n) => self.rd.push(&buf[..n]),
            }
        }
    }

    fn next(&mut self) -> Option<Frame> {
        self.stash.pop_front().or_else(|| self.read())
    }

    fn write(&mut self, tag: u8, data: &[u8]) -> bool {
        helper::write(&mut self.sock, tag, data).is_ok()
    }

    fn sign(&mut self, digest: &[u8; 32]) -> Option<Vec<u8>> {
        if !self.write(helper::SIGN, digest) {
            return None;
        }
        loop {
            let (tag, d) = self.read()?;
            if tag == helper::SIGNED {
                return Some(d.to_vec());
            }
            if self.stash.len() >= 8 {
                return None;
            }
            self.stash.push_back((tag, d));
        }
    }
}

fn main() {
    #[cfg(target_os = "freebsd")]
    {
        // SAFETY: cap_enter takes no arguments and only changes this process.
        if unsafe { libc::cap_enter() } != 0 {
            eprintln!("atrium-aura-eap: cap_enter failed");
            std::process::exit(1);
        }
        eprintln!("atrium-aura-eap: in capability mode");
    }
    // SAFETY: fd 0 is the socket aura-keys gave us and nothing else owns it.
    let sock = unsafe { std::fs::File::from_raw_fd(0) };
    let pipe = Arc::new(Mutex::new(Pipe { sock, rd: Reader::default(), stash: VecDeque::new() }));
    let send = |tag: u8, d: &[u8]| pipe.lock().unwrap().write(tag, d);
    let mut peer: Option<Peer> = None;
    loop {
        let Some((tag, data)) = pipe.lock().unwrap().next() else { return };
        let out = match (tag, &mut peer) {
            (helper::CONFIG, None) => {
                let Some(p) = Profile::decode(&data) else {
                    send(helper::FAILED, b"bad-profile");
                    return;
                };
                let mut cfg = Config::from_profile(&p);
                if p.method == Method::Tls {
                    let (Some(_), false) = (p.key, p.cert.is_empty()) else {
                        send(helper::FAILED, b"no-client-certificate");
                        return;
                    };
                    let pp = pipe.clone();
                    let key = RemoteKey(Arc::new(move |d: &[u8; 32]| pp.lock().unwrap().sign(d)));
                    let chain = p.cert.iter().map(|c| c.clone().into()).collect();
                    cfg.client = Some(Arc::new(rustls::sign::CertifiedKey::new(chain, Arc::new(key))));
                }
                peer = Some(Peer::new(cfg));
                continue;
            }
            (helper::PACKET, Some(p)) => p.handle(&data),
            _ => {
                send(helper::FAILED, b"bad-frame");
                return;
            }
        };
        let end = match out {
            Out::Send(r) => !send(helper::SEND, &r),
            Out::Dropped(why) => !send(helper::DROPPED, why.as_bytes()),
            Out::Done(msk) => {
                send(helper::DONE, &msk[..]);
                true
            }
            Out::Failed(why) => {
                eprintln!("atrium-aura-eap: failed: {why:?}");
                send(helper::FAILED, why.code().as_bytes());
                true
            }
            Out::NeedTrust(seen) => {
                let chain: Vec<Vec<u8>> = seen.chain.iter().map(|c| c.to_vec()).collect();
                send(helper::TRUST, &helper::encode_seen(&seen.names, &chain));
                true
            }
        };
        if end {
            return;
        }
    }
}
