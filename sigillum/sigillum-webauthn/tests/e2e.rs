//! S3a's gate: options JSON in, through the real `sigillumd` on real
//! sockets, the real FIDO driver loop over a soft key and a scripted
//! trusted UI, out to a relying party that verifies the response the way a
//! server library would (clientDataJSON, RP ID hash, flags, signature).

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use p256::ecdsa::{signature::Verifier, DerSignature, VerifyingKey};
use sha2::{Digest, Sha256};
use sigillum_cbor::Value;
use sigillum_proto::client::{CeremonyId, Reply, Request};
use sigillum_proto::driver::Pin;
use sigillum_proto::ui::{self, Choice, FromUi, Sheet, ToUi};
use sigillum_proto::{read_msg, write_msg};
use sigillum_softkey::SoftKey;
use sigillum_webauthn::{create, get, unb64url, CreationOptions, DomError, Origin, RequestOptions, Sigillumd, Transport};
use sigillumd::broker::Config;
use sigillumd::daemon::{Daemon, Settings};
use sigillumd::label::{AppId, Label, Registry};
use zeroize::Zeroizing;

fn me() -> u32 {
    unsafe { libc::getuid() }
}

fn navigator() -> AppId {
    AppId { publisher: [0xAB; 32], app_id: "org.atrium.navigator".into() }
}

struct AllNavigator;
impl Registry for AllNavigator {
    fn label(&self, _: u32) -> Label {
        Label::App { user: "alice".into(), app: navigator(), rp_ids: vec![] }
    }
}

struct Rig {
    dir: PathBuf,
    settings: Settings,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// What the scripted UI presses.
#[derive(Clone, Copy, PartialEq)]
enum Press {
    Nothing,
    Key,
    /// Arca: "save on this device", or the first passkey listed.
    Arca,
}

/// arcad on a real socket, serving sigillumd's uid (this test's) as the
/// `sigillumd` service, with alice's store unlocked.
fn arcad(dir: &std::path::Path) -> PathBuf {
    let mut s = arcad::server::State::new(dir.join("arca")).unwrap();
    let mut body = vec![5, 0, 0, 0];
    body.extend_from_slice(b"alice");
    body.extend_from_slice(&[1; 32]);
    assert_eq!(s.handle(&arcad::label::Label::Vestibulum, arcad::proto::op::V_CREATE, &body).status, 0);
    let state = Arc::new(std::sync::Mutex::new(s));
    let path = dir.join("arcad.sock");
    let l = std::os::unix::net::UnixListener::bind(&path).unwrap();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let st = state.clone();
            std::thread::spawn(move || arcad::serve_conn(c, &arcad::label::Label::Service("sigillumd".into()), &st));
        }
    });
    path
}

/// A stand-in for ostiarius's passcode socket (`ostiarius::verify`'s wire:
/// a JSON line each way): alice's passcode is 2468.
fn ostiarius(dir: &std::path::Path) -> PathBuf {
    use std::io::{BufRead, BufReader, Write};
    let path = dir.join("verify.sock");
    let l = std::os::unix::net::UnixListener::bind(&path).unwrap();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let mut w = c.try_clone().unwrap();
            let mut line = String::new();
            if BufReader::new(c).read_line(&mut line).is_err() {
                continue;
            }
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            let ok = v["user"] == "alice" && v["passcode"] == "2468";
            let _ = w.write_all(format!("{{\"ok\":{ok},\"locked_secs\":0}}\n").as_bytes());
        }
    });
    path
}

/// The daemon, a soft key with PIN 4711, Arca on its socket, and a UI that
/// presses what `press` says on every sheet and types the PIN.
fn rig(press: Press) -> Rig {
    rig_typing(press, b"2468")
}

/// As [`rig`], the UI typing `typed` when asked for the passcode.
fn rig_typing(press: Press, typed: &'static [u8]) -> Rig {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!("sigillum-webauthn-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
    std::fs::create_dir_all(&dir).unwrap();
    let settings = Settings {
        client_socket: dir.join("c.sock"),
        driver_socket: dir.join("d.sock"),
        ui_socket: dir.join("u.sock"),
        dev_uid: me(),
        ui_uid: me(),
        broker: Config { web_broker: Some(navigator()), ceremony_ms: 10_000 },
    };
    let arca = sigillumd::platform::Arca(sigillumd::platform::Socket(arcad(&dir)));
    let passcode = sigillumd::daemon::Ostiarius(ostiarius(&dir));
    let d = Daemon::bind(settings.clone(), Arc::new(AllNavigator), Box::new(passcode)).unwrap().with_platform(Box::new(arca));
    std::thread::spawn(move || d.run());

    let mut s = UnixStream::connect(&settings.ui_socket).unwrap();
    let mut w = s.try_clone().unwrap();
    std::thread::spawn(move || {
        while let Ok(Some(m)) = read_msg::<ToUi>(&mut s, ui::MAX_MESSAGE) {
            let answer = match &m {
                ToUi::Show { key, devices, .. } if press == Press::Key => devices.first().map(|d| FromUi::Press { key: *key, choice: Choice::Device(d.id), activation_ok: true }),
                ToUi::Show { key, sheet: Sheet::Register { platform: true, .. }, .. } if press == Press::Arca => Some(FromUi::Press { key: *key, choice: Choice::Platform, activation_ok: true }),
                ToUi::Show { key, sheet: Sheet::Assert { passkeys, .. }, .. } if press == Press::Arca => passkeys.first().map(|p| FromUi::Press { key: *key, choice: Choice::Passkey(p.id), activation_ok: true }),
                ToUi::AskPasscode { key, .. } => Some(FromUi::Passcode { key: *key, passcode: Pin(Zeroizing::new(typed.to_vec())) }),
                ToUi::AskPin { key, .. } => Some(FromUi::Pin { key: *key, pin: Pin(Zeroizing::new(b"4711".to_vec())) }),
                _ => None,
            };
            if let Some(a) = answer {
                let _ = write_msg(&mut w, &a, ui::MAX_MESSAGE);
            }
        }
    });

    let path = settings.driver_socket.clone();
    std::thread::spawn(move || {
        let sock = UnixStream::connect(path).unwrap();
        let (driver, hello) = sigillum_fido::Driver::open(SoftKey::new(Some(b"4711")), 0x1050, 0x0407, "Soft Key").unwrap();
        let _ = sigillum_fido::serve(driver, hello, sock);
    });
    // Let the driver's Hello arrive before the first sheet.
    std::thread::sleep(Duration::from_millis(300));
    Rig { dir, settings }
}

fn broker(r: &Rig) -> Sigillumd {
    let s = UnixStream::connect(&r.settings.client_socket).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    Sigillumd::from_stream(s)
}

/// The relying party's side: what `@simplewebauthn/server` or webauthn-rs
/// would check, done by hand.
struct Rp {
    id: &'static str,
    origin: &'static str,
}

fn b(v: &serde_json::Value, k: &str) -> Vec<u8> {
    unb64url(v[k].as_str().unwrap_or_else(|| panic!("{k}"))).unwrap()
}

impl Rp {
    fn client_data(&self, resp: &serde_json::Value, ty: &str, challenge: &[u8]) -> Vec<u8> {
        let raw = b(resp, "clientDataJSON");
        let cd: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(cd["type"], ty);
        assert_eq!(unb64url(cd["challenge"].as_str().unwrap()).unwrap(), challenge);
        assert_eq!(cd["origin"], self.origin);
        assert_eq!(cd["crossOrigin"], false);
        raw
    }

    fn auth_data(&self, ad: &[u8], uv: bool) {
        assert_eq!(ad[..32], Sha256::digest(self.id.as_bytes())[..]);
        assert!(ad[32] & 0x01 != 0, "UP");
        assert_eq!(ad[32] & 0x04 != 0, uv, "UV");
    }

    /// Verify a registration; returns the credential id and its key.
    fn register(&self, json: &str, challenge: &[u8], platform: bool) -> (Vec<u8>, VerifyingKey) {
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(v["type"], "public-key");
        assert_eq!(v["id"], v["rawId"]);
        assert_eq!(v["authenticatorAttachment"], if platform { "platform" } else { "cross-platform" });
        let r = &v["response"];
        assert_eq!(r["transports"], serde_json::json!([if platform { "internal" } else { "usb" }]));
        assert_eq!(r["publicKeyAlgorithm"], -7);
        self.client_data(r, "webauthn.create", challenge);
        let obj = sigillum_cbor::decode(&b(r, "attestationObject")).unwrap();
        assert_eq!(obj.get_text("fmt").and_then(Value::as_text), Some("none"));
        let ad = obj.get_text("authData").and_then(Value::as_bytes).unwrap().to_vec();
        assert_eq!(ad, b(r, "authenticatorData"));
        self.auth_data(&ad, true);
        assert!(ad[32] & 0x40 != 0, "AT");
        assert_eq!(ad[37..53], [0; 16], "AAGUID zeroed under none");
        let n = u16::from_be_bytes([ad[53], ad[54]]) as usize;
        let cred_id = ad[55..55 + n].to_vec();
        assert_eq!(cred_id, b(&v, "rawId"));
        // The SPKI's point is the attested COSE key's.
        let spki = b(r, "publicKey");
        assert_eq!(spki.len(), 91);
        let key = VerifyingKey::from_sec1_bytes(&spki[26..]).unwrap();
        let cose = sigillum_cbor::decode_prefix(&ad[55 + n..], Default::default()).unwrap().0;
        assert_eq!(cose.get_int(-2).and_then(Value::as_bytes), Some(&spki[27..59]));
        (cred_id, key)
    }

    fn authenticate(&self, json: &str, challenge: &[u8], cred_id: &[u8], key: &VerifyingKey, uv: bool) -> Option<Vec<u8>> {
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(b(&v, "rawId"), cred_id);
        let r = &v["response"];
        let cdj = self.client_data(r, "webauthn.get", challenge);
        let ad = b(r, "authenticatorData");
        self.auth_data(&ad, uv);
        let mut signed = ad.clone();
        signed.extend_from_slice(&Sha256::digest(&cdj));
        let sig = DerSignature::try_from(&b(r, "signature")[..]).unwrap();
        key.verify(&signed, &sig).expect("ES256 signature over authData || hash(clientDataJSON)");
        r.get("userHandle").and_then(|h| h.as_str()).map(|h| unb64url(h).unwrap())
    }
}

const RP: Rp = Rp { id: "example.com", origin: "https://login.example.com" };

fn creation(extra: &str) -> CreationOptions {
    CreationOptions::from_json(&format!(
        r#"{{"rp":{{"id":"example.com","name":"Example"}},
            "user":{{"id":"AQIDBA","name":"alice@example.com","displayName":"Alice"}},
            "challenge":"cmVnaXN0ZXItY2hhbGxlbmdlLTAx",
            "pubKeyCredParams":[{{"type":"public-key","alg":-7}}],
            "authenticatorSelection":{{"residentKey":"required","userVerification":"required"}}{extra}}}"#
    ))
    .unwrap()
}

#[test]
fn register_and_sign_in_verified_by_the_relying_party() {
    let r = rig(Press::Key);
    let mut t = broker(&r);
    let origin = Origin::parse(RP.origin).unwrap();

    let o = creation("");
    let reg = create(&mut t, &origin, None, &o).unwrap();
    let (cred_id, key) = RP.register(&reg.to_json(), &o.challenge, false);

    // Sign in with the credential named...
    let o = RequestOptions::from_json(&format!(
        r#"{{"challenge":"c2lnbi1pbi1jaGFsbGVuZ2UtMDE","rpId":"example.com","userVerification":"required","allowCredentials":[{{"type":"public-key","id":"{}"}}]}}"#,
        sigillum_webauthn::b64url(&cred_id)
    ))
    .unwrap();
    let a = get(&mut t, &origin, None, &o).unwrap();
    RP.authenticate(&a.to_json(), &o.challenge, &cred_id, &key, true);

    // ...and discoverably: no list, the key finds it and names the user.
    let o = RequestOptions::from_json(r#"{"challenge":"ZGlzY292ZXJhYmxlLTAx","rpId":"example.com","userVerification":"required"}"#).unwrap();
    let a = get(&mut t, &origin, None, &o).unwrap();
    assert_eq!(RP.authenticate(&a.to_json(), &o.challenge, &cred_id, &key, true), Some(vec![1, 2, 3, 4]));

    // A credential the key holds, in excludeCredentials: InvalidStateError.
    let o = creation(&format!(r#","excludeCredentials":[{{"type":"public-key","id":"{}"}}]"#, sigillum_webauthn::b64url(&cred_id)));
    assert_eq!(create(&mut t, &origin, None, &o).unwrap_err(), DomError::InvalidState);
}

/// A transport that must not be reached.
struct Never;
impl Transport for Never {
    fn next_ceremony(&mut self) -> CeremonyId {
        1
    }
    fn ask(&mut self, req: &Request) -> std::io::Result<Reply> {
        panic!("reached sigillumd with {req:?}")
    }
}

#[test]
fn an_rp_id_not_the_origins_never_reaches_sigillumd() {
    let o = creation("");
    for origin in ["https://evil.com", "https://example.com.evil.com", "http://login.example.com"] {
        assert_eq!(create(&mut Never, &Origin::parse(origin).unwrap(), None, &o).unwrap_err(), DomError::Security, "{origin}");
    }
    let req = RequestOptions::from_json(r#"{"challenge":"AQID","rpId":"com"}"#).unwrap();
    assert_eq!(get(&mut Never, &Origin::parse(RP.origin).unwrap(), None, &req).unwrap_err(), DomError::Security);
}

/// A transport that answers with a reply someone tampered with.
struct Lying(Box<dyn FnMut(&Request) -> Reply>);
impl Transport for Lying {
    fn next_ceremony(&mut self) -> CeremonyId {
        5
    }
    fn ask(&mut self, req: &Request) -> std::io::Result<Reply> {
        Ok((self.0)(req))
    }
}

fn auth_data(rp: &str, fl: u8) -> Vec<u8> {
    let mut v = Sha256::digest(rp.as_bytes()).to_vec();
    v.push(fl);
    v.extend_from_slice(&[0, 0, 0, 1]);
    v
}

#[test]
fn replies_that_fail_the_checks_are_not_allowed() {
    let origin = Origin::parse(RP.origin).unwrap();
    let req = RequestOptions::from_json(r#"{"challenge":"AQID","rpId":"example.com","userVerification":"required","allowCredentials":[{"type":"public-key","id":"AAEC"}]}"#).unwrap();
    let cases: Vec<(&str, Reply)> = vec![
        ("other RP", Reply::Asserted { ceremony: 5, cred_id: vec![0, 1, 2], auth_data: auth_data("evil.com", 0x05), signature: vec![1], user_handle: None, authenticator: sigillum_proto::client::AuthenticatorClass::Usb }),
        ("no UP", Reply::Asserted { ceremony: 5, cred_id: vec![0, 1, 2], auth_data: auth_data("example.com", 0x04), signature: vec![1], user_handle: None, authenticator: sigillum_proto::client::AuthenticatorClass::Usb }),
        ("no UV when required", Reply::Asserted { ceremony: 5, cred_id: vec![0, 1, 2], auth_data: auth_data("example.com", 0x01), signature: vec![1], user_handle: None, authenticator: sigillum_proto::client::AuthenticatorClass::Usb }),
        ("credential not allowed", Reply::Asserted { ceremony: 5, cred_id: vec![9], auth_data: auth_data("example.com", 0x05), signature: vec![1], user_handle: None, authenticator: sigillum_proto::client::AuthenticatorClass::Usb }),
        ("another ceremony", Reply::Asserted { ceremony: 6, cred_id: vec![0, 1, 2], auth_data: auth_data("example.com", 0x05), signature: vec![1], user_handle: None, authenticator: sigillum_proto::client::AuthenticatorClass::Usb }),
    ];
    for (what, reply) in cases {
        let mut reply = Some(reply);
        let mut t = Lying(Box::new(move |_| reply.take().unwrap()));
        assert_eq!(get(&mut t, &origin, None, &req).unwrap_err(), DomError::NotAllowed, "{what}");
    }
    // The same reply, untampered, passes.
    let mut t = Lying(Box::new(|_| Reply::Asserted { ceremony: 5, cred_id: vec![0, 1, 2], auth_data: auth_data("example.com", 0x05), signature: vec![1], user_handle: None, authenticator: sigillum_proto::client::AuthenticatorClass::Usb }));
    assert!(get(&mut t, &origin, None, &req).is_ok());
}

#[test]
fn the_page_aborting_cancels_the_ceremony() {
    // A UI that shows the sheet but never presses.
    let r = rig(Press::Nothing);
    let mut t = broker(&r);
    let mut c = t.canceller().unwrap();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        c.cancel().unwrap();
    });
    let started = std::time::Instant::now();
    assert_eq!(create(&mut t, &Origin::parse(RP.origin).unwrap(), None, &creation("")).unwrap_err(), DomError::NotAllowed);
    assert!(started.elapsed() < Duration::from_secs(5), "ended by the cancel, not the timeout");
}

#[test]
fn an_arca_passkey_through_both_daemons_verified_by_the_relying_party() {
    let r = rig(Press::Arca);
    let mut t = broker(&r);
    let origin = Origin::parse(RP.origin).unwrap();

    // UV preferred: Arca asks the passcode (ostiarius checks it).
    let o = creation("").clone();
    let o = CreationOptions { uv: sigillum_webauthn::options::Uv::Preferred, ..o };
    let reg = create(&mut t, &origin, None, &o).unwrap();
    let (cred_id, key) = RP.register(&reg.to_json(), &o.challenge, true);
    assert_eq!(cred_id.len(), 16);

    let o = RequestOptions::from_json(r#"{"challenge":"YXJjYS1zaWduLWlu","rpId":"example.com","userVerification":"preferred"}"#).unwrap();
    let a = get(&mut t, &origin, None, &o).unwrap();
    assert_eq!(RP.authenticate(&a.to_json(), &o.challenge, &cred_id, &key, true), Some(vec![1, 2, 3, 4]));
    let v: serde_json::Value = serde_json::from_str(&a.to_json()).unwrap();
    assert_eq!(v["authenticatorAttachment"], "platform");

    // UV required: the passcode again, and the UV flag the RP sees.
    let o = RequestOptions::from_json(r#"{"challenge":"dXYtcmVxdWlyZWQ","rpId":"example.com","userVerification":"required"}"#).unwrap();
    let a = get(&mut t, &origin, None, &o).unwrap();
    RP.authenticate(&a.to_json(), &o.challenge, &cred_id, &key, true);
}

#[test]
fn a_wrong_passcode_signs_nothing() {
    let r = rig_typing(Press::Arca, b"0000");
    let mut t = broker(&r);
    let origin = Origin::parse(RP.origin).unwrap();
    // The sheet stays after each wrong passcode; the page gives up.
    let mut c = t.canceller().unwrap();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(600));
        c.cancel().unwrap();
    });
    assert_eq!(create(&mut t, &origin, None, &creation("")).unwrap_err(), DomError::NotAllowed);
}
