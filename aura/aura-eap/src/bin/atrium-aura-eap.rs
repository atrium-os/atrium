//! atrium-aura-eap: one 802.1X join's EAP conversation (wifi.md §7.4).
//!
//! aura-keys starts it per join with a socket as stdin and nothing else.
//! It enters capability mode before reading a byte: from then on it can
//! reach nothing but that socket. It reads the profile, then EAP packets,
//! and writes back responses and, at the end, the MSK, a failure, or the
//! certificate the user has to decide about (aura_eap::helper).

use std::io::{Read, Write};
use std::os::fd::FromRawFd;

use aura_eap::helper::{self, Reader};
use aura_eap::profile::Profile;
use aura_eap::{Config, Out, Peer};

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
    let mut sock = unsafe { std::fs::File::from_raw_fd(0) };
    let mut rd = Reader::default();
    let mut peer: Option<Peer> = None;
    let mut buf = [0u8; 8192];
    loop {
        let n = match sock.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        rd.push(&buf[..n]);
        loop {
            let (tag, data) = match rd.next() {
                Ok(Some(f)) => f,
                Ok(None) => break,
                Err(()) => {
                    let _ = helper::write(&mut sock, helper::FAILED, b"bad-frame");
                    return;
                }
            };
            let out = match (tag, &mut peer) {
                (helper::CONFIG, None) => {
                    let Some(p) = Profile::decode(&data) else {
                        let _ = helper::write(&mut sock, helper::FAILED, b"bad-profile");
                        return;
                    };
                    peer = Some(Peer::new(Config::from_profile(&p)));
                    continue;
                }
                (helper::PACKET, Some(p)) => p.handle(&data),
                _ => {
                    let _ = helper::write(&mut sock, helper::FAILED, b"bad-frame");
                    return;
                }
            };
            let end = match out {
                Out::Send(r) => helper::write(&mut sock, helper::SEND, &r).is_err(),
                Out::Dropped(why) => helper::write(&mut sock, helper::DROPPED, why.as_bytes()).is_err(),
                Out::Done(msk) => {
                    let _ = helper::write(&mut sock, helper::DONE, &msk[..]);
                    true
                }
                Out::Failed(why) => {
                    eprintln!("atrium-aura-eap: failed: {why:?}");
                    let _ = helper::write(&mut sock, helper::FAILED, why.code().as_bytes());
                    true
                }
                Out::NeedTrust(seen) => {
                    let chain: Vec<Vec<u8>> = seen.chain.iter().map(|c| c.to_vec()).collect();
                    let _ = helper::write(&mut sock, helper::TRUST, &helper::encode_seen(&seen.names, &chain));
                    true
                }
            };
            if end {
                let _ = sock.flush();
                return;
            }
        }
    }
}
