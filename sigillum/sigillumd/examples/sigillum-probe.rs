//! A test client for the VM gate (scripts/vm-sigillum-check.sh), never
//! installed on a real system.
//!
//! ```text
//! sigillum-probe ui [SOCKET]                 # a scripted trusted UI: presses the first device
//! sigillum-probe register RP FILE [SOCKET]   # register; saves cred id + public key to FILE
//! sigillum-probe assert RP FILE [SOCKET]     # sign in with FILE's credential; verifies the signature
//! ```
//!
//! `ui` must run as `_sigillum-ui`; the client as a uid the launch
//! registry names as the web broker app. Prints one line per result.

use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::time::Duration;

use p256::ecdsa::{signature::Verifier, DerSignature, VerifyingKey};
use sha2::{Digest, Sha256};
use sigillum_ctap::authdata::AuthData;
use sigillum_proto::client::{self, Assert, Attestation, Register, Reply, Request, Uv};
use sigillum_proto::ui::{self, Choice, FromUi, ToUi};
use sigillum_proto::{read_msg, write_msg};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).filter_map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()).collect()
}

fn ui_loop(sock: &str) -> ExitCode {
    let mut s = UnixStream::connect(sock).expect("ui socket");
    let mut w = s.try_clone().unwrap();
    println!("ui: connected");
    while let Ok(Some(m)) = read_msg::<ToUi>(&mut s, ui::MAX_MESSAGE) {
        println!("ui: {m:?}");
        if let ToUi::Show { key, devices, .. } = &m {
            if let Some(d) = devices.first() {
                let _ = write_msg(&mut w, &FromUi::Press { key: *key, choice: Choice::Device(d.id), activation_ok: true }, ui::MAX_MESSAGE);
            }
        }
    }
    println!("ui: closed");
    ExitCode::SUCCESS
}

fn ask(sock: &str, req: &Request) -> Reply {
    let mut s = UnixStream::connect(sock).expect("client socket");
    s.set_read_timeout(Some(Duration::from_secs(150))).unwrap();
    write_msg(&mut s, req, client::MAX_MESSAGE).unwrap();
    read_msg::<Reply>(&mut s, client::MAX_MESSAGE).unwrap().expect("a reply")
}

fn main() -> ExitCode {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let sock = |i: usize, d: &str| a.get(i).cloned().unwrap_or_else(|| d.to_string());
    match a.first().map(String::as_str) {
        Some("ui") => ui_loop(&sock(1, ui::SOCKET)),
        Some("register") if a.len() >= 3 => {
            let rp = &a[1];
            let r = ask(
                &sock(3, client::SOCKET),
                &Request::Register(Register {
                    ceremony: 1,
                    timeout_ms: 0,
                    rp_id: rp.clone(),
                    rp_name: rp.clone(),
                    client_data_hash: Sha256::digest(b"create").into(),
                    user_id: vec![1, 2, 3, 4],
                    user_name: "probe".into(),
                    user_display_name: "Probe".into(),
                    exclude: vec![],
                    resident_key: false,
                    uv: Uv::Discouraged,
                    attestation: Attestation::None,
                }),
            );
            let Reply::Registered { cred_id, auth_data, .. } = &r else {
                println!("register: {r:?}");
                return ExitCode::FAILURE;
            };
            let ad = AuthData::parse(auth_data).expect("authenticator data parses");
            let att = ad.attested.expect("attested credential data");
            let pk = att.public_key.p256().expect("a P-256 key");
            let ok_hash = ad.rp_id_hash == <[u8; 32]>::from(Sha256::digest(rp.as_bytes()));
            std::fs::write(&a[2], format!("{} {}\n", hex(cred_id), hex(&pk.to_sec1_bytes()))).unwrap();
            println!("register: ok cred={} flags={:#04x} rp_hash_ok={ok_hash} aaguid_zero={}", hex(cred_id), ad.flags, att.aaguid == [0; 16]);
            if ok_hash {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Some("assert") if a.len() >= 3 => {
            let rp = &a[1];
            let saved = std::fs::read_to_string(&a[2]).expect("credential file");
            let mut it = saved.split_whitespace();
            let cred = unhex(it.next().unwrap());
            let key = VerifyingKey::from_sec1_bytes(&unhex(it.next().unwrap())).expect("saved key");
            let cdh: [u8; 32] = Sha256::digest(b"get").into();
            let r = ask(
                &sock(3, client::SOCKET),
                &Request::Assert(Assert { ceremony: 2, timeout_ms: 0, rp_id: rp.clone(), client_data_hash: cdh, allow: vec![cred.clone()], uv: Uv::Discouraged }),
            );
            let Reply::Asserted { cred_id, auth_data, signature, .. } = &r else {
                println!("assert: {r:?}");
                return ExitCode::FAILURE;
            };
            let mut signed = auth_data.clone();
            signed.extend_from_slice(&cdh);
            let verified = DerSignature::try_from(&signature[..]).ok().is_some_and(|s| key.verify(&signed, &s).is_ok());
            let ad = AuthData::parse(auth_data).expect("authenticator data parses");
            println!("assert: cred_match={} signature_ok={verified} flags={:#04x} count={}", *cred_id == cred, ad.flags, ad.sign_count);
            if verified && *cred_id == cred {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        _ => {
            eprintln!("usage: sigillum-probe ui [SOCKET] | register RP FILE [SOCKET] | assert RP FILE [SOCKET]");
            ExitCode::from(2)
        }
    }
}
