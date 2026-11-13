//! A test client for the VM gate (scripts/vm-sigillum-check.sh), never
//! installed on a real system.
//!
//! ```text
//! sigillum-probe ui [SOCKET]                 # a scripted trusted UI: presses the first device
//! sigillum-probe ui-tty [N] [SOCKET]         # a real key's UI: the PIN typed at this terminal
//!                                            # (echo off, one try per ceremony); exits after N sheets
//! sigillum-probe register RP FILE [SOCKET]   # register; saves cred id + public key to FILE
//! sigillum-probe assert RP FILE [SOCKET]     # sign in with FILE's credential; verifies the signature
//! sigillum-probe webauthn ORIGIN RP [SOCKET] # navigator.credentials.create() then .get() through
//!                                            # sigillum-webauthn, checked as a relying party would
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
use sigillum_proto::driver::Pin;
use sigillum_proto::ui::{self, Choice, FromUi, ToUi};
use sigillum_proto::{read_msg, write_msg};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).filter_map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()).collect()
}

/// The scripted trusted UI: presses the first device on every sheet, and on
/// a verify sheet types the passcodes in `$SPROBE_PASSCODES` (comma
/// separated), one per ask, then cancels.
fn ui_loop(sock: &str) -> ExitCode {
    let mut passcodes: std::collections::VecDeque<String> =
        std::env::var("SPROBE_PASSCODES").unwrap_or_default().split(',').filter(|p| !p.is_empty()).map(String::from).collect();
    let scripted = !passcodes.is_empty();
    let mut s = UnixStream::connect(sock).expect("ui socket");
    let mut w = s.try_clone().unwrap();
    println!("ui: connected");
    while let Ok(Some(m)) = read_msg::<ToUi>(&mut s, ui::MAX_MESSAGE) {
        println!("ui: {m:?}");
        match &m {
            // A verify sheet (Arca's verify-each-use): "use your passcode".
            ToUi::Show { key, sheet: ui::Sheet::Verify { .. }, .. } if scripted => {
                let _ = write_msg(&mut w, &FromUi::Press { key: *key, choice: Choice::Passcode, activation_ok: true }, ui::MAX_MESSAGE);
            }
            ToUi::Show { key, devices, .. } => {
                if let Some(d) = devices.first() {
                    let _ = write_msg(&mut w, &FromUi::Press { key: *key, choice: Choice::Device(d.id), activation_ok: true }, ui::MAX_MESSAGE);
                }
            }
            // A key's PIN is never scripted: on a real key a wrong PIN burns
            // one of its retries. Cancel the ceremony and say so; the gate
            // stops there and a person types the PIN in a real sheet.
            ToUi::AskPin { key, retries, wrong, .. } => {
                println!("ui: PIN requested (retries={retries} wrong={wrong}): cancelled, no PIN sent");
                let _ = write_msg(&mut w, &FromUi::Press { key: *key, choice: Choice::Cancel, activation_ok: true }, ui::MAX_MESSAGE);
            }
            // Type the next scripted passcode; none left: cancel.
            ToUi::AskPasscode { key, .. } => {
                let msg = match passcodes.pop_front() {
                    Some(p) => FromUi::Passcode { key: *key, passcode: Pin(p.into_bytes().into()) },
                    None => FromUi::Press { key: *key, choice: Choice::Cancel, activation_ok: true },
                };
                let _ = write_msg(&mut w, &msg, ui::MAX_MESSAGE);
            }
            _ => {}
        }
    }
    println!("ui: closed");
    ExitCode::SUCCESS
}

/// Reads a PIN from fd 0, which must be a terminal, with echo off. One byte
/// per read(2) into a zeroized buffer: no stdio buffer keeps a copy. None on
/// an empty line, EOF, or a fd 0 that is not a terminal.
fn read_pin_tty(prompt: &str) -> Option<Pin> {
    use std::io::Write;
    if unsafe { libc::isatty(0) } != 1 {
        eprintln!("ui-tty: stdin is not a terminal; no PIN read");
        return None;
    }
    let mut old: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(0, &mut old) } != 0 {
        return None;
    }
    let mut quiet = old;
    quiet.c_lflag &= !(libc::ECHO | libc::ECHONL);
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    if unsafe { libc::tcsetattr(0, libc::TCSAFLUSH, &quiet) } != 0 {
        return None;
    }
    let mut buf = zeroize::Zeroizing::new(Vec::<u8>::with_capacity(64));
    let mut b = zeroize::Zeroizing::new([0u8; 1]);
    let mut ok = false;
    loop {
        let n = unsafe { libc::read(0, b.as_mut_ptr().cast(), 1) };
        if n != 1 {
            break;
        }
        if b[0] == b'\n' || b[0] == b'\r' {
            ok = true;
            break;
        }
        // CTAP2 PINs are at most 63 bytes; refuse anything longer.
        if buf.len() == 63 {
            buf.clear();
            ok = false;
            while unsafe { libc::read(0, b.as_mut_ptr().cast(), 1) } == 1 && b[0] != b'\n' {}
            break;
        }
        buf.push(b[0]);
    }
    unsafe { libc::tcsetattr(0, libc::TCSAFLUSH, &old) };
    eprintln!();
    if !ok || buf.len() < 4 {
        return None;
    }
    Some(Pin(buf))
}

/// The trusted UI for a real key, typed by the person at the terminal:
/// presses the first device, asks for the key's PIN on this terminal (echo
/// off) once per ceremony, and stops at the first wrong PIN, saying how many
/// tries are left. It never asks when 3 or fewer are left. Exits after
/// `ceremonies` sheets close.
fn ui_tty(sock: &str, ceremonies: usize) -> ExitCode {
    let mut s = UnixStream::connect(sock).expect("ui socket");
    let mut w = s.try_clone().unwrap();
    let mut closed = 0;
    eprintln!("ui-tty: connected; waiting for a ceremony");
    while let Ok(Some(m)) = read_msg::<ToUi>(&mut s, ui::MAX_MESSAGE) {
        match &m {
            ToUi::Show { key, sheet, devices } => {
                eprintln!("ui-tty: sheet {sheet:?}");
                if let Some(d) = devices.first() {
                    eprintln!("ui-tty: using {}", d.name);
                    let _ = write_msg(&mut w, &FromUi::Press { key: *key, choice: Choice::Device(d.id), activation_ok: true }, ui::MAX_MESSAGE);
                }
            }
            ToUi::AskPin { key, retries, wrong, .. } => {
                let cancel = FromUi::Press { key: *key, choice: Choice::Cancel, activation_ok: true };
                if *wrong {
                    eprintln!("ui-tty: WRONG PIN; {retries} tries left on the key. Stopping (no second try).");
                    let _ = write_msg(&mut w, &cancel, ui::MAX_MESSAGE);
                    return ExitCode::FAILURE;
                }
                if *retries <= 3 {
                    eprintln!("ui-tty: only {retries} PIN tries left on the key; not asking. Stopping.");
                    let _ = write_msg(&mut w, &cancel, ui::MAX_MESSAGE);
                    return ExitCode::FAILURE;
                }
                match read_pin_tty(&format!("Security key PIN ({retries} tries left; empty line cancels): ")) {
                    Some(pin) => {
                        let _ = write_msg(&mut w, &FromUi::Pin { key: *key, pin }, ui::MAX_MESSAGE);
                    }
                    None => {
                        eprintln!("ui-tty: no PIN given; cancelled");
                        let _ = write_msg(&mut w, &cancel, ui::MAX_MESSAGE);
                    }
                }
            }
            ToUi::Touch { .. } => eprintln!("ui-tty: >>> TOUCH THE KEY NOW (it is blinking) <<<"),
            ToUi::Close { .. } => {
                closed += 1;
                eprintln!("ui-tty: sheet closed ({closed}/{ceremonies})");
                if closed >= ceremonies {
                    return ExitCode::SUCCESS;
                }
            }
            _ => {}
        }
    }
    eprintln!("ui-tty: closed by sigillumd");
    ExitCode::FAILURE
}

fn ask(sock: &str, req: &Request) -> Reply {
    let mut s = UnixStream::connect(sock).expect("client socket");
    s.set_read_timeout(Some(Duration::from_secs(150))).unwrap();
    write_msg(&mut s, req, client::MAX_MESSAGE).unwrap();
    read_msg::<Reply>(&mut s, client::MAX_MESSAGE).unwrap().expect("a reply")
}

/// The page's side through `sigillum-webauthn`, and the relying party's
/// checks on what comes back.
fn webauthn(origin: &str, rp: &str, sock: &str) -> ExitCode {
    use sigillum_webauthn::{create, get, unb64url, CreationOptions, DomError, Origin, RequestOptions, Sigillumd};
    let connect = || {
        let s = UnixStream::connect(sock).expect("client socket");
        s.set_read_timeout(Some(Duration::from_secs(150))).unwrap();
        Sigillumd::from_stream(s)
    };
    let origin = Origin::parse(origin).expect("origin");
    let b = |v: &serde_json::Value, k: &str| unb64url(v[k].as_str().unwrap_or("")).unwrap_or_default();
    let o = CreationOptions::from_json(&format!(
        r#"{{"rp":{{"id":"{rp}","name":"Probe"}},"user":{{"id":"AQIDBA","name":"probe","displayName":"Probe"}},
            "challenge":"dm0tZ2F0ZS1yZWdpc3Rlcg","pubKeyCredParams":[{{"type":"public-key","alg":-7}}],
            "authenticatorSelection":{{"userVerification":"discouraged"}}}}"#
    ))
    .expect("options");
    let reg = match create(&mut connect(), &origin, None, &o) {
        Ok(r) => r,
        Err(e) => {
            println!("webauthn create: {}", e.name());
            return ExitCode::FAILURE;
        }
    };
    let v: serde_json::Value = serde_json::from_str(&reg.to_json()).unwrap();
    let r = &v["response"];
    let cd: serde_json::Value = serde_json::from_slice(&b(r, "clientDataJSON")).unwrap();
    let ad = b(r, "authenticatorData");
    let cred = b(&v, "rawId");
    let spki = b(r, "publicKey");
    let key = VerifyingKey::from_sec1_bytes(spki.get(26..).unwrap_or(&[])).expect("SPKI point");
    let ok_create = cd["type"] == "webauthn.create"
        && cd["origin"] == origin.serialize()
        && ad.get(..32) == Some(&Sha256::digest(rp.as_bytes())[..])
        && ad.get(32).is_some_and(|f| f & 0x41 == 0x41);
    println!("webauthn create: ok={ok_create} attachment={} cred={}", v["authenticatorAttachment"], hex(&cred));

    let o = RequestOptions::from_json(&format!(
        r#"{{"challenge":"dm0tZ2F0ZS1zaWduLWlu","rpId":"{rp}","userVerification":"discouraged","allowCredentials":[{{"type":"public-key","id":"{}"}}]}}"#,
        sigillum_webauthn::b64url(&cred)
    ))
    .expect("options");
    let a = match get(&mut connect(), &origin, None, &o) {
        Ok(a) => a,
        Err(e) => {
            println!("webauthn get: {}", e.name());
            return ExitCode::FAILURE;
        }
    };
    let v: serde_json::Value = serde_json::from_str(&a.to_json()).unwrap();
    let r = &v["response"];
    let cdj = b(r, "clientDataJSON");
    let cd: serde_json::Value = serde_json::from_slice(&cdj).unwrap();
    let mut signed = b(r, "authenticatorData");
    signed.extend_from_slice(&Sha256::digest(&cdj));
    let verified = DerSignature::try_from(&b(r, "signature")[..]).ok().is_some_and(|s| key.verify(&signed, &s).is_ok());
    let ok_get = verified && cd["type"] == "webauthn.get" && b(&v, "rawId") == cred;
    println!("webauthn get: ok={ok_get} signature_ok={verified}");

    // A foreign RP ID never reaches sigillumd.
    let evil = RequestOptions::from_json(r#"{"challenge":"AQID","rpId":"evil.example"}"#).unwrap();
    let refused = get(&mut connect(), &origin, None, &evil).err() == Some(DomError::Security);
    println!("webauthn foreign rp: security_error={refused}");
    if ok_create && ok_get && refused {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn main() -> ExitCode {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let sock = |i: usize, d: &str| a.get(i).cloned().unwrap_or_else(|| d.to_string());
    match a.first().map(String::as_str) {
        Some("ui") => ui_loop(&sock(1, ui::SOCKET)),
        Some("ui-tty") => ui_tty(&sock(2, ui::SOCKET), a.get(1).and_then(|n| n.parse().ok()).unwrap_or(2)),
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
        Some("webauthn") if a.len() >= 3 => webauthn(&a[1], &a[2], &sock(3, client::SOCKET)),
        _ => {
            eprintln!("usage: sigillum-probe ui [SOCKET] | register RP FILE [SOCKET] | assert RP FILE [SOCKET]");
            ExitCode::from(2)
        }
    }
}
