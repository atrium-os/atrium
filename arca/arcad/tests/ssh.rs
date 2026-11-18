//! The SSH agent's side of arcad (arca.md §9.1): keys minted for a persona,
//! granted to workspaces by name; a workspace lists and signs with only its
//! own grants, in its own persona; only the agent service may ask.

use arca_store::codec::{Reader, Writer};
use arcad::label::Label;
use arcad::proto::{op, ssh_op, status};
use arcad::server::State;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};

fn body(f: impl FnOnce(&mut Writer)) -> Vec<u8> {
    let mut w = Writer::new();
    f(&mut w);
    w.buf
}

fn agent() -> Label {
    Label::Service("ssh-agent".into())
}

fn setup() -> (tempfile::TempDir, State) {
    let d = tempfile::tempdir().unwrap();
    let mut s = State::new(d.path().into()).unwrap();
    assert_eq!(s.handle(&Label::Vestibulum, op::V_CREATE, &body(|w| { w.str("alice"); w.raw(&[1; 32]); })).status, status::OK);
    (d, s)
}

fn mint(s: &mut State, persona: &str, comment: &str) -> ([u8; 16], [u8; 32]) {
    let r = s.handle(&agent(), ssh_op::SA_MINT, &body(|w| { w.str("alice"); w.str(persona); w.str(comment); }));
    assert_eq!(r.status, status::OK);
    let b = r.body.bytes();
    (b[..16].try_into().unwrap(), b[16..48].try_into().unwrap())
}

fn grant(s: &mut State, persona: &str, id: [u8; 16], ws: &str, on: bool) -> u8 {
    let o = if on { ssh_op::SA_GRANT } else { ssh_op::SA_REVOKE };
    s.handle(&agent(), o, &body(|w| { w.str("alice"); w.str(persona); w.raw(&id); w.str(ws); })).status
}

fn keys(s: &mut State, persona: &str, ws: &str) -> Vec<([u8; 16], [u8; 32], String)> {
    let r = s.handle(&agent(), ssh_op::SA_KEYS, &body(|w| { w.str("alice"); w.str(persona); w.str(ws); }));
    assert_eq!(r.status, status::OK);
    let mut rd = Reader::new(r.body.bytes());
    let n = rd.u32().unwrap();
    (0..n).map(|_| (rd.array().unwrap(), rd.array().unwrap(), rd.str(256).unwrap().to_string())).collect()
}

fn sign(s: &mut State, who: &Label, persona: &str, ws: &str, id: [u8; 16], msg: &[u8]) -> (u8, Vec<u8>) {
    let r = s.handle(who, ssh_op::SA_SIGN, &body(|w| { w.str("alice"); w.str(persona); w.str(ws); w.raw(&id); w.bytes(msg); }));
    (r.status, r.body.bytes().to_vec())
}

#[test]
fn a_workspace_signs_with_only_its_granted_keys() {
    let (_d, mut s) = setup();
    let (id, pk) = mint(&mut s, "main", "alice@web");
    let (other, _) = mint(&mut s, "main", "alice@lib");
    assert!(keys(&mut s, "main", "web").is_empty(), "nothing granted yet");
    assert_eq!(sign(&mut s, &agent(), "main", "web", id, b"m").0, status::DENIED);

    assert_eq!(grant(&mut s, "main", id, "web", true), status::OK);
    let k = keys(&mut s, "main", "web");
    assert_eq!(k, vec![(id, pk, "alice@web".to_string())]);
    let (st, sig) = sign(&mut s, &agent(), "main", "web", id, b"session-id+userauth");
    assert_eq!(st, status::OK);
    let vk = VerifyingKey::from_bytes(&pk).unwrap();
    vk.verify(b"session-id+userauth", &Signature::from_slice(&sig).unwrap()).unwrap();

    // Another workspace, the other key, another persona: refused.
    assert_eq!(sign(&mut s, &agent(), "main", "lib", id, b"m").0, status::DENIED);
    assert_eq!(sign(&mut s, &agent(), "main", "web", other, b"m").0, status::DENIED);
    assert_eq!(sign(&mut s, &agent(), "work", "web", id, b"m").0, status::DENIED);
    assert!(keys(&mut s, "work", "web").is_empty());

    // The list shows grants; a revoke ends them.
    let r = s.handle(&agent(), ssh_op::SA_LIST, &body(|w| { w.str("alice"); w.str("main"); }));
    assert_eq!(r.status, status::OK);
    assert_eq!(Reader::new(r.body.bytes()).u32().unwrap(), 2);
    assert_eq!(grant(&mut s, "main", id, "web", false), status::OK);
    assert_eq!(sign(&mut s, &agent(), "main", "web", id, b"m").0, status::DENIED);

    // Only the agent service: another service and an app get nothing.
    assert_eq!(grant(&mut s, "main", id, "web", true), status::OK);
    let not_agent = Label::Service("vinculumd".into());
    assert_ne!(sign(&mut s, &not_agent, "main", "web", id, b"m").0, status::OK);
    let app = Label::App { user: "alice".into(), publisher: [4; 32], app_id: "org.example.x".into() };
    assert_ne!(sign(&mut s, &app, "main", "web", id, b"m").0, status::OK);
    // An app's own SIGN can't name the agent's item either.
    assert_ne!(s.handle(&app, op::SIGN, &body(|w| { w.raw(&id); w.bytes(b"m"); })).status, status::OK);

    // A deleted workspace's grants go.
    let r = s.handle(&agent(), ssh_op::SA_FORGET, &body(|w| { w.str("alice"); w.str("main"); w.str("web"); }));
    assert_eq!((r.status, r.body.bytes().to_vec()), (status::OK, 1u32.to_le_bytes().to_vec()));
    assert!(keys(&mut s, "main", "web").is_empty());
    assert_eq!(grant(&mut s, "main", id, "web", true), status::OK);

    // Delete, and the lock.
    assert_eq!(s.handle(&agent(), ssh_op::SA_DELETE, &body(|w| { w.str("alice"); w.str("main"); w.raw(&other); })).status, status::OK);
    assert_eq!(s.handle(&Label::Vestibulum, op::V_LOCK, &body(|w| w.str("alice"))).status, status::OK);
    assert_eq!(sign(&mut s, &agent(), "main", "web", id, b"m").0, status::LOCKED);
}

fn token_add(s: &mut State, host: &str, user: &str, tok: &[u8]) -> (u8, Vec<u8>) {
    let r = s.handle(&agent(), ssh_op::SA_TOKEN_ADD, &body(|w| { w.str("alice"); w.str("main"); w.str(host); w.str(user); w.bytes(tok); }));
    (r.status, r.body.bytes().to_vec())
}

fn token_get(s: &mut State, who: &Label, persona: &str, ws: &str, host: &str) -> (u8, Option<(String, Vec<u8>)>) {
    let r = s.handle(who, ssh_op::SA_TOKEN_GET, &body(|w| { w.str("alice"); w.str(persona); w.str(ws); w.str(host); }));
    if r.status != status::OK {
        return (r.status, None);
    }
    let mut rd = Reader::new(r.body.bytes());
    let u = rd.str(128).unwrap().to_string();
    let t = rd.bytes(4096).unwrap().to_vec();
    rd.end().unwrap();
    (r.status, Some((u, t)))
}

#[test]
fn a_workspace_gets_only_its_granted_https_token() {
    let (_d, mut s) = setup();
    let (st, id) = token_add(&mut s, "github.com", "alice", b"ghp_one");
    assert_eq!(st, status::OK);
    let id: [u8; 16] = id.try_into().unwrap();
    assert_eq!(token_get(&mut s, &agent(), "main", "web", "github.com").0, status::DENIED, "not granted yet");

    assert_eq!(grant(&mut s, "main", id, "web", true), status::OK);
    assert_eq!(token_get(&mut s, &agent(), "main", "web", "github.com").1, Some(("alice".into(), b"ghp_one".to_vec())));
    // Another workspace, host, persona: nothing.
    assert_eq!(token_get(&mut s, &agent(), "main", "lib", "github.com").0, status::DENIED);
    assert_eq!(token_get(&mut s, &agent(), "main", "web", "gitlab.com").0, status::DENIED);
    assert_eq!(token_get(&mut s, &agent(), "work", "web", "github.com").0, status::DENIED);
    // Not a key: not listed for signing, can't sign.
    assert!(keys(&mut s, "main", "web").is_empty());
    assert_eq!(sign(&mut s, &agent(), "main", "web", id, b"m").0, status::DENIED);

    // Replacing keeps the grant.
    assert_eq!(token_add(&mut s, "github.com", "alice", b"ghp_two").0, status::OK);
    assert_eq!(token_get(&mut s, &agent(), "main", "web", "github.com").1, Some(("alice".into(), b"ghp_two".to_vec())));
    let r = s.handle(&agent(), ssh_op::SA_LIST, &body(|w| { w.str("alice"); w.str("main"); }));
    let mut rd = Reader::new(r.body.bytes());
    assert_eq!(rd.u32().unwrap(), 1);
    let _id: [u8; 16] = rd.array().unwrap();
    assert_eq!(rd.u16().unwrap(), 4, "ApiKey");
    assert_eq!(rd.array::<32>().unwrap(), [0; 32]);
    assert_eq!(rd.str(512).unwrap(), "github.com alice");
    assert_eq!(rd.str(4096).unwrap(), "web");

    // Bad hosts and usernames are refused; only the agent may ask.
    assert_eq!(token_add(&mut s, "GitHub.com", "alice", b"x").0, status::MALFORMED);
    assert_eq!(token_add(&mut s, "github.com:443", "alice", b"x").0, status::MALFORMED);
    assert_eq!(token_add(&mut s, "github.com", "a b", b"x").0, status::MALFORMED);
    assert_eq!(token_add(&mut s, "github.com", "alice", b"").0, status::MALFORMED);
    assert_ne!(token_get(&mut s, &Label::Service("vinculumd".into()), "main", "web", "github.com").0, status::OK);

    // A deleted workspace loses it.
    s.handle(&agent(), ssh_op::SA_FORGET, &body(|w| { w.str("alice"); w.str("main"); w.str("web"); }));
    assert_eq!(token_get(&mut s, &agent(), "main", "web", "github.com").0, status::DENIED);
}
