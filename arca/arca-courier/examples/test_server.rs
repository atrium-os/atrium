//! A TLS server for the jailed courier's VM gate, run on the machine that
//! runs the VM (the guest reaches it as 10.0.2.2 through slirp):
//!
//!   courier-test-server HOSTS CA_OUT PORT_OUT LOG
//!
//! HOSTS (comma separated) are the certificate's names, signed by a
//! throwaway CA written to CA_OUT. The port goes to PORT_OUT; each request
//! head, one line per request (`<request line> token=<bool> cookie=<value|->`),
//! goes to LOG.
//! A fifth argument is the listen address for the Mail sign-in fixture
//! (use 127.0.0.1:0 on the host). In this mode the log contains stage
//! markers only. The fixture checks PKCE request fields and a redirect cookie.
//! With the final argument `webauthn-probe`, the page calls credentials.get
//! and displays its outcome. That diagnostic flow cannot issue an OAuth code.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;

// Local provider state lasts for a single gate. The cookie selects a single
// authorization and the code is removed on the first redemption attempt.
#[derive(Default)]
struct Provider {
    webauthn_probe: bool,
    registration: bool,
    registration_challenges: std::collections::HashMap<String, String>,
    registered: std::collections::HashMap<String, Enrollment>,
    enrollment: Option<Enrollment>,
    challenges: std::collections::HashMap<String, String>,
    sessions: std::collections::HashMap<String, (String, String)>,
    codes: std::collections::HashMap<String, String>,
}

fn nonce() -> String {
    use ring::rand::SecureRandom;
    let mut bytes = [0; 24];
    ring::rand::SystemRandom::new().fill(&mut bytes).unwrap();
    arcad::oauth::b64url(&bytes)
}

fn field<'a>(form: &'a str, key: &str) -> Option<&'a str> {
    let mut fields = form.split('&').filter_map(|kv| kv.split_once('=')).filter(|(k, _)| *k == key);
    let (_, value) = fields.next()?;
    fields.next().is_none().then_some(value)
}

const CLIENT: &str = "11111111-1111-4111-8111-111111111111";
const REDIRECT: &str = "https%3A%2F%2Flogin.microsoftonline.com%2Fcommon%2Foauth2%2Fnativeclient";

// The software authenticator hands only these public enrollment values to the fixture.
struct Enrollment { id: Vec<u8>, key: Vec<u8>, counter: u32 }

impl Enrollment {
    fn read(path: &str) -> Self {
        let input = std::fs::read_to_string(path).unwrap();
        let fields: Vec<_> = input.split_whitespace().collect();
        assert_eq!(fields.len(), 3);
        fn hex(s: &str) -> Vec<u8> {
            assert!(s.is_ascii() && s.len() % 2 == 0);
            (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i+2], 16).unwrap()).collect()
        }
        Self { id: hex(fields[0]), key: hex(fields[1]), counter: fields[2].parse().unwrap() }
    }

    fn register(body: &str, challenge: &str) -> Option<Self> {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        use sigillum_ctap::{authdata::AuthData, cose::{alg, CoseKey}};
        let mut fields = url::form_urlencoded::parse(body.as_bytes());
        let (name, data) = fields.next()?;
        if name != "registration" || fields.next().is_some() { return None; }
        let credential: serde_json::Value = serde_json::from_str(&data).ok()?;
        let bytes = |k: &str| URL_SAFE_NO_PAD.decode(credential[k].as_str()?).ok();
        let client: serde_json::Value = serde_json::from_slice(&bytes("clientDataJSON")?).ok()?;
        if credential["type"] != "public-key" || client["type"] != "webauthn.create"
            || client["origin"] != "https://login.microsoftonline.com" || client["crossOrigin"] != false
            || client["challenge"] != challenge { return None; }
        let object = sigillum_cbor::decode(&bytes("attestationObject")?).ok()?;
        if object.get_text("fmt")?.as_text()? != "none" || !object.get_text("attStmt")?.as_map()?.is_empty() { return None; }
        let auth = AuthData::parse(object.get_text("authData")?.as_bytes()?).ok()?;
        let attested = auth.attested?;
        if auth.rp_id_hash.as_slice() != ring::digest::digest(&ring::digest::SHA256, b"login.microsoftonline.com").as_ref()
            || auth.flags & 5 != 5 || attested.cred_id != bytes("rawId")? { return None; }
        let CoseKey::P256 { alg: alg::ES256, x, y } = attested.public_key else { return None };
        let key = [&[4u8][..], &x, &y].concat();
        Some(Self { id: attested.cred_id, key, counter: auth.sign_count })
    }

    fn verify(&mut self, body: &str, challenge: &str) -> Option<()> {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        let mut fields = url::form_urlencoded::parse(body.as_bytes());
        let (name, data) = fields.next()?;
        if name != "assertion" || fields.next().is_some() { return None; }
        let v: serde_json::Value = serde_json::from_str(&data).ok()?;
        let bytes = |k: &str| URL_SAFE_NO_PAD.decode(v[k].as_str()?).ok();
        if v["type"] != "public-key" || bytes("rawId")? != self.id || bytes("userHandle")? != b"fixture-user" { return None; }
        let client = bytes("clientDataJSON")?;
        let cd: serde_json::Value = serde_json::from_slice(&client).ok()?;
        if cd["type"] != "webauthn.get" || cd["origin"] != "https://login.microsoftonline.com"
            || cd["crossOrigin"] != false || cd["challenge"] != challenge { return None; }
        let mut signed = bytes("authenticatorData")?;
        let sha = |b| ring::digest::digest(&ring::digest::SHA256, b);
        if signed.len() != 37 || &signed[..32] != sha(b"login.microsoftonline.com").as_ref() || signed[32] & 5 != 5 { return None; }
        let counter = u32::from_be_bytes(signed[33..37].try_into().ok()?);
        if counter <= self.counter { return None; }
        signed.extend_from_slice(sha(&client).as_ref());
        ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_ASN1, &self.key)
            .verify(&signed, &bytes("signature")?).ok()?;
        self.counter = counter;
        Some(())
    }
}

impl Provider {
    fn callback(&mut self, first: &str, cookie: &str, body: &str) -> (&'static str, String) {
        let mut parts = first.split_whitespace();
        let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        let flow = cookie.split("; ").find_map(|v| v.strip_prefix("flow=")).unwrap_or("");
        let response = |status, headers: &str, body: &str| format!("HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\n\r\n{body}", body.len());
        if method == "GET" {
            if let Some(q) = path.strip_prefix("/common/oauth2/v2.0/authorize?") {
                if let (Some(state), Some(challenge)) = (field(q, "state"), field(q, "code_challenge")) {
                    let safe = |v: &str| v.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'));
                    if self.sessions.len() < 64 && field(q, "client_id") == Some(CLIENT) && field(q, "redirect_uri") == Some(REDIRECT)
                        && field(q, "code_challenge_method") == Some("S256") && challenge.len() == 43 && safe(challenge) && state.len() == 22 && safe(state) {
                        let flow = nonce();
                        self.sessions.insert(flow.clone(), (state.into(), challenge.into()));
                        return ("authorized", response("302 Found", &format!("Set-Cookie: flow={flow}; Secure; HttpOnly; Path=/\r\nLocation: /first\r\n"), ""));
                    }
                }
            }
            if self.sessions.contains_key(flow) {
                if self.registration && path == "/atrium-fixture/account.js" {
                    let challenge = self.registration_challenges.entry(flow.into()).or_insert_with(nonce);
                    let script = include_str!("../../../test-assets/navigator-i3/register.js").replace("__CREATE__", challenge);
                    let challenge = self.challenges.entry(flow.into()).or_insert_with(nonce);
                    return ("registration-challenges-issued", response("200 OK", "Content-Type: text/javascript\r\n", &script.replace("__GET__", challenge)));
                }
                if self.enrollment.is_some() && path == "/atrium-fixture/account.js" {
                    let challenge = self.challenges.entry(flow.into()).or_insert_with(nonce);
                    let js = include_str!("../../../test-assets/navigator-i3/sign-in.js").replace("__CHALLENGE__", challenge);
                    return ("challenge-issued", response("200 OK", "Content-Type: text/javascript\r\n", &js));
                }
                let resource = match path {
                    "/first" => Some(("first-screen-cookie-ok", "text/html", include_str!("../../../test-assets/navigator-i0/mail/first.html"))),
                    "/atrium-fixture/account.css" => Some(("stylesheet", "text/css", include_str!("../../../test-assets/navigator-i0/mail/account.css"))),
                    "/atrium-fixture/account.js" if self.webauthn_probe => Some(("webauthn-script", "text/javascript", include_str!("../../../test-assets/navigator-i3/get.js"))),
                    "/atrium-fixture/account.js" => Some(("script", "text/javascript", include_str!("../../../test-assets/navigator-i0/mail/account.js"))),
                    _ => None,
                };
                if let Some((marker, mime, body)) = resource {
                    return (marker, response("200 OK", &format!("Content-Type: {mime}\r\n"), body));
                }
                if !self.registration && !self.webauthn_probe && self.enrollment.is_none() && path == "/atrium-fixture/continue" && self.codes.len() < 64 {
                    let (state, challenge) = self.sessions.remove(flow).unwrap();
                    let code = nonce();
                    self.codes.insert(code.clone(), challenge);
                    return ("callback-issued", response("302 Found", &format!("Location: {}?code={code}&state={state}\r\n", arcad::oauth::MICROSOFT.redirect), ""));
                }
            }
        } else if method == "POST" && path == "/atrium-fixture/continue" {
            if self.registration && self.sessions.contains_key(flow) && body.starts_with("registration=") {
                if let Some(key) = self.registration_challenges.remove(flow)
                    .and_then(|challenge| Enrollment::register(body, &challenge)) {
                    self.registered.insert(flow.to_owned(), key);
                    return ("registration-verified", response("204 No Content", "", ""));
                }
                return ("registration-refused", response("403 Forbidden", "", ""));
            }
            let enrollment = if self.registration { self.registered.get_mut(flow) } else { self.enrollment.as_mut() };
            if let (Some(enrollment), Some(challenge)) = (enrollment, self.challenges.remove(flow)) {
                if self.codes.len() < 64 && enrollment.verify(body, &challenge).is_some() {
                    let (state, verifier_hash) = self.sessions.remove(flow).unwrap();
                    let code = nonce();
                    self.codes.insert(code.clone(), verifier_hash);
                    return ("assertion-verified", response("302 Found", &format!("Location: {}?code={code}&state={state}\r\n", arcad::oauth::MICROSOFT.redirect), ""));
                }
            }
        } else if method == "POST" && path == arcad::oauth::MICROSOFT.token_path {
            let expected = field(body, "code").and_then(|c| self.codes.remove(c));
            let actual = field(body, "code_verifier").filter(|v| v.len() == 43).map(|v|
                arcad::oauth::b64url(ring::digest::digest(&ring::digest::SHA256, v.as_bytes()).as_ref()));
            let headers = "Content-Type: application/json\r\nConnection: close\r\n";
            if expected.is_some() && expected == actual && field(body, "client_id") == Some(CLIENT) &&
                field(body, "redirect_uri") == Some(REDIRECT) && field(body, "grant_type") == Some("authorization_code") {
                return ("pkce-redeemed", response("200 OK", headers, r#"{"access_token":"demo-access","refresh_token":"demo-refresh","token_type":"Bearer","expires_in":3600}"#));
            }
            return ("invalid-grant", response("400 Bad Request", headers, r#"{"error":"invalid_grant"}"#));
        }
        ("refused", response("403 Forbidden", "", ""))
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (hosts, ca_out, port_out, log) = (&a[1], &a[2], &a[3], a[4].clone());
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca.distinguished_name.push(rcgen::DnType::CommonName, "Atrium courier test CA");
    ca.key_usages.push(rcgen::KeyUsagePurpose::KeyCertSign);
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let ca_cert = ca.self_signed(&ca_key).unwrap();
    std::fs::write(ca_out, ca_cert.pem()).unwrap();
    let k = rcgen::KeyPair::generate().unwrap();
    let names: Vec<String> = hosts.split(',').map(String::from).collect();
    let mut leaf = rcgen::CertificateParams::new(names).unwrap();
    leaf.use_authority_key_identifier_extension = true;
    let c = leaf.signed_by(&k, &ca_cert, &ca_key).unwrap();
    use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    let cfg = Arc::new(
        rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![CertificateDer::from(c.der().to_vec())], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(k.serialize_der())))
            .unwrap(),
    );
    // Optional guest-local address for the Mail button gate. The only signing
    // authority in that courier jail is this throwaway fixture CA.
    let mail_address = a.get(5);
    let mail = mail_address.is_some();
    let webauthn_probe = a.get(6).is_some_and(|mode| mode == "webauthn-probe");
    let enrollment = a.get(6).filter(|mode| *mode == "webauthn-sign-in").map(|_| Enrollment::read(&a[7]));
    let registration = a.get(6).is_some_and(|mode| mode == "webauthn-register");
    let callback = registration || enrollment.is_some() || webauthn_probe || a.get(6).is_some_and(|mode| mode == "mail-callback");
    let provider = Arc::new(std::sync::Mutex::new(Provider { webauthn_probe, registration, enrollment, ..Provider::default() }));
    let continuation = a.get(6).is_some_and(|mode| mode == "mail-continue");
    let inbox = a.get(6).is_some_and(|mode| mode == "inbox");
    let l = TcpListener::bind(mail_address.map(String::as_str).unwrap_or("127.0.0.1:0")).unwrap();
    std::fs::write(port_out, l.local_addr().unwrap().port().to_string()).unwrap();
    for s in l.incoming().flatten() {
        let provider = provider.clone();
        let (cfg, log) = (cfg.clone(), log.clone());
        std::thread::spawn(move || {
            let Ok(conn) = rustls::ServerConnection::new(cfg) else { return };
            let mut t = rustls::StreamOwned::new(conn, s);
            loop {
                let mut b = Vec::new();
                let mut one = [0u8; 1];
                while !b.ends_with(b"\r\n\r\n") {
                    if t.read(&mut one).unwrap_or(0) == 0 {
                        return;
                    }
                    if b.len() >= 16384 { return; }
                    b.push(one[0]);
                }
                let req = String::from_utf8_lossy(&b).to_string();
                let token = req.contains("Authorization: Bearer hunter2-token\r\n");
                let first = req.lines().next().unwrap_or("").to_string();
                let cookie = req.lines().find_map(|l| l.strip_prefix("Cookie: ")).unwrap_or("-").to_string();
                if callback {
                    let length = req.lines().filter_map(|l| l.split_once(':')).find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                        .map(|(_, v)| v.trim().parse::<usize>()).transpose();
                    let Ok(length) = length else { return };
                    if length.unwrap_or(0) > 8192 { return; }
                    let mut body = vec![0; length.unwrap_or(0)];
                    if t.read_exact(&mut body).is_err() { return; }
                    let (marker, reply) = provider.lock().unwrap().callback(&first, &cookie, std::str::from_utf8(&body).unwrap_or(""));
                    let mut file = std::fs::OpenOptions::new().append(true).create(true).open(&log).unwrap();
                    writeln!(file, "{marker}").unwrap();
                    if t.write_all(reply.as_bytes()).and_then(|_| t.flush()).is_err() || (first.starts_with("POST ") && marker != "registration-verified") { break; }
                    continue;
                }
                if inbox {
                    let headers = "Set-Cookie: inbox_session=local; Secure; HttpOnly; SameSite=Strict; Path=/\r\n";
                    let (status, mime, extra, body, marker) = match first.split_whitespace().collect::<Vec<_>>().as_slice() {
                        ["GET", "/inbox", _] => (200, "text/html", headers, include_str!("../../../test-assets/navigator-i0/inbox/index.html"), "document"),
                        ["GET", "/inbox.js", _] => (200, "text/javascript", "", include_str!("../../../test-assets/navigator-i0/inbox/inbox.js"), "script"),
                        ["GET", "/inbox.css", _] => (200, "text/css", "", include_str!("../../../test-assets/navigator-i0/inbox/inbox.css"), "stylesheet"),
                        ["GET", "/api/inbox", _] if cookie.split("; ").any(|s| s == "inbox_session=local") =>
                            (200, "application/json", "", r#"{"subject":"School field trip"}"#, "authenticated inbox"),
                        _ => (403, "text/plain", "", "Forbidden", "refused"),
                    };
                    let mut file = std::fs::OpenOptions::new().append(true).create(true).open(&log).unwrap();
                    writeln!(file, "{marker}").unwrap();
                    let reply = format!("HTTP/1.1 {status} {}\r\nContent-Type: {mime}\r\n{extra}Content-Length: {}\r\n\r\n{body}", if status == 200 { "OK" } else { "Forbidden" }, body.len());
                    if t.write_all(reply.as_bytes()).and_then(|_| t.flush()).is_err() { break; }
                    continue;
                }
                if mail {
                    let target = first.strip_prefix("GET ").and_then(|s| s.split_whitespace().next()).unwrap_or("");
                    let authorization = target.strip_prefix("/common/oauth2/v2.0/authorize?").is_some_and(|q| {
                        let has = |key: &str, value: &str| q.split('&').any(|kv| kv.split_once('=') == Some((key, value)));
                        has("client_id", "11111111-1111-4111-8111-111111111111") && has("code_challenge_method", "S256")
                            && q.split('&').any(|kv| kv.strip_prefix("code_challenge=").is_some_and(|v| v.len() == 43))
                            && q.split('&').any(|kv| kv.strip_prefix("state=").is_some_and(|v| v.len() == 22))
                    });
                    let (marker, reply) = if authorization {
                        ("authorized", "HTTP/1.1 302 Found\r\nSet-Cookie: flow=dev; Secure; HttpOnly; Path=/\r\nLocation: /first\r\nContent-Length: 0\r\n\r\n".to_string())
                    } else if continuation && first.starts_with("GET ") && cookie.split("; ").any(|v| v == "flow=dev") {
                        let resource = match target {
                            "/first" => Some(("first-screen-cookie-ok", "text/html", include_str!("../../../test-assets/navigator-i0/mail/first.html"))),
                            "/atrium-fixture/account.css" => Some(("stylesheet", "text/css", include_str!("../../../test-assets/navigator-i0/mail/account.css"))),
                            "/atrium-fixture/account.js" => Some(("script", "text/javascript", include_str!("../../../test-assets/navigator-i0/mail/account.js"))),
                            "/atrium-fixture/continue" => Some(("continued", "application/json", r#"{"message":"Demo account confirmed"}"#)),
                            _ => None,
                        };
                        match resource {
                            Some((marker, mime, body)) => (marker, format!("HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\n\r\n{body}", body.len())),
                            None => ("refused", "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n".into()),
                        }
                    } else if target == "/first" && cookie.split("; ").any(|v| v == "flow=dev") {
                        let html = "<!doctype html><label id=prompt for=account></label><input id=account><script>document.getElementById('prompt').textContent='Enter your account'</script>";
                        ("first-screen-cookie-ok", format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\r\n{html}", html.len()))
                    } else {
                        ("refused", "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n".to_string())
                    };
                    let mut log = std::fs::OpenOptions::new().append(true).create(true).open(&log).unwrap();
                    writeln!(log, "{marker}").unwrap();
                    if t.write_all(reply.as_bytes()).and_then(|_| t.flush()).is_err() { break; }
                    continue;
                }
                if let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).open(&log) {
                    let _ = writeln!(f, "{first} token={token} cookie={cookie}");
                }
                let resp = if req.starts_with("GET /redirect") {
                    "HTTP/1.1 302 Found\r\nLocation: https://evil.example/\r\nContent-Length: 0\r\n\r\n".to_string()
                } else {
                    let echo = if token { "echo hunter2-token" } else { "no token" };
                    format!("HTTP/1.1 200 OK\r\nSet-Cookie: sid=vm-cookie; Path=/; Secure; HttpOnly\r\nContent-Length: {}\r\n\r\n{echo}", echo.len())
                };
                if t.write_all(resp.as_bytes()).is_err() {
                    return;
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webauthn_probe_requires_its_cookie_and_cannot_issue_a_callback() {
        let mut provider = Provider { webauthn_probe: true, ..Provider::default() };
        provider.sessions.insert("fixture".into(), ("state".into(), "challenge".into()));
        let js = "GET /atrium-fixture/account.js HTTP/1.1";
        assert_eq!(provider.callback(js, "", "").0, "refused");
        assert_eq!(provider.callback(js, "flow=fixture", "").0, "webauthn-script");
        assert_eq!(provider.callback("GET /atrium-fixture/continue HTTP/1.1", "flow=fixture", "").0, "refused");
        assert!(provider.codes.is_empty());
    }

    #[test]
    fn passkey_continuation_requires_the_flow_and_a_verified_assertion() {
        let mut provider = Provider { enrollment: Some(Enrollment { id: vec![1], key: vec![2], counter: 0 }), ..Provider::default() };
        provider.sessions.insert("fixture".into(), ("state".into(), "pkce".into()));
        let cookie = "flow=fixture";
        assert_eq!(provider.callback("GET /atrium-fixture/account.js HTTP/1.1", cookie, "").0, "challenge-issued");
        let post = "POST /atrium-fixture/continue HTTP/1.1";
        assert_eq!(provider.callback(post, "flow=other", "assertion=bad").0, "refused");
        assert!(provider.challenges.contains_key("fixture"));
        assert_eq!(provider.callback("GET /atrium-fixture/continue HTTP/1.1", cookie, "").0, "refused");
        assert_eq!(provider.callback(post, cookie, "assertion=bad").0, "refused");
        assert!(!provider.challenges.contains_key("fixture"));
        assert!(provider.codes.is_empty());
    }

    #[test]
    fn registration_requires_its_own_flow_and_never_issues_a_code() {
        let mut provider = Provider { registration: true, ..Provider::default() };
        provider.sessions.insert("fixture".into(), ("state".into(), "pkce".into()));
        let cookie = "flow=fixture";
        assert_eq!(provider.callback("GET /atrium-fixture/account.js HTTP/1.1", cookie, "").0, "registration-challenges-issued");
        assert_ne!(provider.registration_challenges["fixture"], provider.challenges["fixture"]);
        let post = "POST /atrium-fixture/continue HTTP/1.1";
        assert_eq!(provider.callback(post, "flow=other", "registration=bad").0, "refused");
        assert!(provider.registration_challenges.contains_key("fixture"));
        assert_eq!(provider.callback(post, cookie, "registration=bad").0, "registration-refused");
        assert!(!provider.registration_challenges.contains_key("fixture"));
        assert!(provider.registered.is_empty());
        assert!(provider.codes.is_empty());
        assert_eq!(provider.callback("GET /atrium-fixture/continue HTTP/1.1", cookie, "").0, "refused");
    }

    #[test]
    fn provider_requires_the_matching_verifier_and_consumes_each_code_once() {
        let verifier = "v".repeat(43);
        let challenge = arcad::oauth::b64url(ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes()).as_ref());
        let mut provider = Provider::default();
        let request = format!("POST {} HTTP/1.1", arcad::oauth::MICROSOFT.token_path);
        let form = format!("grant_type=authorization_code&client_id={CLIENT}&redirect_uri={REDIRECT}&code=test&code_verifier={verifier}");
        provider.codes.insert("test".into(), challenge.clone());
        assert_eq!(provider.callback(&request, "", &form.replace(&verifier, &"x".repeat(43))).0, "invalid-grant");
        provider.codes.insert("test".into(), challenge);
        assert_eq!(provider.callback(&request, "", &form).0, "pkce-redeemed");
        assert_eq!(provider.callback(&request, "", &form).0, "invalid-grant");
    }
}
