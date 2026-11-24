//! atrium-ssh-agent: Arca's SSH agent (arca.md §9.1). See the crate docs.
//!
//! Environment: `ARCA_SOCKET` (arcad's), `SSH_AGENT_ROOT` (default
//! `/atrium/sockets/ssh-agent`), `APP_REGISTRY` (the launch registry).

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arca_ssh_agent::arca::Arca;
use arca_ssh_agent::control::{parse_admin, parse_person, Admin, Person};
use arca_ssh_agent::wire::{self, Key};
use arca_ssh_agent::{folder, hex, public_line, valid_owner, AGENT, CONTROL};

#[derive(Clone, PartialEq, Eq)]
struct Open {
    persona: String,
    uid: u32,
}

struct Agent {
    root: PathBuf,
    registry: String,
    arca: Arca,
    open: Mutex<HashMap<(String, String), Open>>,
}

fn main() -> ExitCode {
    let root = PathBuf::from(std::env::var("SSH_AGENT_ROOT").unwrap_or_else(|_| arca_ssh_agent::SOCKET_ROOT.into()));
    let a = Arc::new(Agent {
        arca: Arca { socket: std::env::var("ARCA_SOCKET").unwrap_or_else(|_| "/atrium/sockets/arca/arcad.sock".into()).into() },
        registry: std::env::var("APP_REGISTRY").unwrap_or_else(|_| portcullis_peer::DEFAULT_REGISTRY.into()),
        open: Mutex::new(HashMap::new()),
        root,
    });
    let _ = fs::DirBuilder::new().recursive(true).mode(0o755).create(&a.root);
    let _ = fs::set_permissions(&a.root, fs::Permissions::from_mode(0o755));
    // Workspaces open before a restart come back from the state file.
    for line in fs::read_to_string(a.state()).unwrap_or_default().lines() {
        if let Some(Admin::Open { owner, ws, persona, uid }) = parse_admin(line) {
            if let Err(e) = open(&a, &owner, &ws, &persona, uid) {
                eprintln!("ssh-agent: reopening {owner}/{ws}: {e}");
            }
        }
    }
    let ctl = a.root.join(CONTROL);
    let _ = fs::remove_file(&ctl);
    let l = match UnixListener::bind(&ctl) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("ssh-agent: bind {}: {e}", ctl.display());
            return ExitCode::FAILURE;
        }
    };
    let _ = fs::set_permissions(&ctl, fs::Permissions::from_mode(0o666));
    eprintln!("ssh-agent: control on {}", ctl.display());
    for c in l.incoming().flatten() {
        let a = a.clone();
        std::thread::spawn(move || control(&a, c));
    }
    ExitCode::SUCCESS
}

impl Agent {
    fn state(&self) -> PathBuf {
        self.root.join("state")
    }

    fn save(&self, m: &HashMap<(String, String), Open>) {
        let mut s = String::new();
        for ((o, w), e) in m {
            s.push_str(&format!("open {o} {w} {} {}\n", e.persona, e.uid));
        }
        let tmp = self.root.join("state.tmp");
        if fs::write(&tmp, s).and_then(|_| fs::rename(&tmp, self.state())).is_err() {
            eprintln!("ssh-agent: cannot save {}", self.state().display());
        }
    }
}

fn sock_path(a: &Agent, owner: &str, ws: &str) -> PathBuf {
    a.root.join(folder(owner, ws)).join(AGENT)
}

/// Open (or refresh) a workspace's socket.
fn open(a: &Arc<Agent>, owner: &str, ws: &str, persona: &str, uid: u32) -> Result<PathBuf, String> {
    let key = (owner.to_string(), ws.to_string());
    let entry = Open { persona: persona.into(), uid };
    let path = sock_path(a, owner, ws);
    let mut m = a.open.lock().unwrap_or_else(|p| p.into_inner());
    let listening = m.contains_key(&key) && path.exists();
    m.insert(key.clone(), entry);
    a.save(&m);
    drop(m);
    if listening {
        return Ok(path);
    }
    let dir = path.parent().expect("a folder");
    fs::DirBuilder::new().recursive(true).mode(0o755).create(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let _ = fs::remove_file(&path);
    let l = UnixListener::bind(&path).map_err(|e| format!("bind {}: {e}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).map_err(|e| e.to_string())?;
    let a = a.clone();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            // Closed: the path is gone, so nothing new arrives; end here.
            let Some(e) = a.open.lock().unwrap_or_else(|p| p.into_inner()).get(&key).cloned() else { break };
            let a = a.clone();
            let key = key.clone();
            std::thread::spawn(move || serve(&a, &key.0, &key.1, &e, c));
        }
    });
    eprintln!("ssh-agent: {owner}/{ws} ({persona}, uid {uid}) open at {}", path.display());
    Ok(path)
}

fn close(a: &Agent, owner: &str, ws: &str) {
    let mut m = a.open.lock().unwrap_or_else(|p| p.into_inner());
    m.remove(&(owner.to_string(), ws.to_string()));
    a.save(&m);
    let _ = fs::remove_file(sock_path(a, owner, ws));
}

/// One workspace connection: the ssh-agent protocol until it closes.
fn serve(a: &Agent, owner: &str, ws: &str, e: &Open, mut c: UnixStream) {
    let _ = c.set_read_timeout(Some(Duration::from_secs(300)));
    let _ = c.set_write_timeout(Some(Duration::from_secs(10)));
    // ★ The workspace's own uid only. Its jail sees only this socket, and
    // inside its jail any uid is the same workspace; a uid from elsewhere
    // (a process given the path some other way) gets nothing.
    match portcullis_peer::uid_gid(c.as_raw_fd()) {
        Ok((uid, _)) if uid == e.uid => {}
        Ok((uid, _)) => {
            eprintln!("ssh-agent: {owner}/{ws}: refused uid {uid}");
            return;
        }
        Err(_) => return,
    }
    loop {
        let mut len = [0u8; 4];
        if c.read_exact(&mut len).is_err() {
            return;
        }
        let n = u32::from_be_bytes(len) as usize;
        if n == 0 || n > wire::MAX_MSG {
            return;
        }
        let mut req = vec![0u8; n];
        if c.read_exact(&mut req).is_err() {
            return;
        }
        let r = wire::answer(&req, &mut Ws { a, owner, ws, e });
        if c.write_all(&wire::frame(&r)).is_err() {
            return;
        }
    }
}

/// One workspace's view of Arca, for [`wire::answer`].
struct Ws<'a> {
    a: &'a Agent,
    owner: &'a str,
    ws: &'a str,
    e: &'a Open,
}

impl wire::Ask for Ws<'_> {
    fn keys(&mut self) -> Option<Vec<Key>> {
        let (owner, ws) = (self.owner, self.ws);
        self.a.arca.keys(owner, &self.e.persona, ws)
            .map_err(|err| eprintln!("ssh-agent: {owner}/{ws}: keys: {err}"))
            .ok()
    }

    fn sign(&mut self, k: &Key, d: &[u8]) -> Option<[u8; 64]> {
        let (owner, ws) = (self.owner, self.ws);
        match self.a.arca.sign(owner, &self.e.persona, ws, &k.id, d) {
            Ok(s) => {
                eprintln!("ssh-agent: {owner}/{ws}: signed with {} ({})", &hex(&k.id)[..8], k.comment);
                Some(s)
            }
            Err(err) => {
                eprintln!("ssh-agent: {owner}/{ws}: sign: {err}");
                None
            }
        }
    }

    fn token(&mut self, host: &str) -> Option<(String, Vec<u8>)> {
        let (owner, ws) = (self.owner, self.ws);
        match self.a.arca.token_get(owner, &self.e.persona, ws, host) {
            Ok(t) => {
                eprintln!("ssh-agent: {owner}/{ws}: gave its {host} token");
                Some(t)
            }
            Err(err) => {
                eprintln!("ssh-agent: {owner}/{ws}: token for {host}: {err}");
                None
            }
        }
    }
}

/// Who is on the control socket: root (portcullisd), or a person: a uid
/// with a passwd entry below the app range that no launch registered.
enum Caller {
    Root,
    Person(String),
}

fn caller(a: &Agent, c: &UnixStream) -> Option<Caller> {
    let (uid, _) = portcullis_peer::uid_gid(c.as_raw_fd()).ok()?;
    if uid == 0 {
        return Some(Caller::Root);
    }
    if uid < 1000 || uid >= portcullis_peer::APP_UID_BASE {
        return None;
    }
    // No registry yet (nothing launched since boot): no app uids. Any
    // other read failure fails closed.
    let reg = match portcullis_peer::AppRegistry::load(&a.registry) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => portcullis_peer::AppRegistry::default(),
        Err(_) => return None,
    };
    if reg.resolve(uid).is_some() {
        return None;
    }
    let name = portcullis_peer::username(uid)?;
    valid_owner(&name).then_some(Caller::Person(name))
}

fn control(a: &Arc<Agent>, c: UnixStream) {
    let _ = c.set_read_timeout(Some(Duration::from_secs(10)));
    let who = caller(a, &c);
    let mut line = String::new();
    let mut rd = BufReader::new(match c.try_clone() {
        Ok(c) => c,
        Err(_) => return,
    });
    if rd.read_line(&mut line).is_err() || line.len() > 1024 {
        return;
    }
    let out = match who {
        None => Err("not a person or portcullisd".to_string()),
        Some(Caller::Root) => match parse_admin(&line) {
            Some(Admin::List { owner, persona }) => person_request(a, &owner, Person::List { persona }, ""),
            Some(Admin::Grant { owner, persona, id, ws, on }) => {
                person_request(a, &owner, Person::Grant { persona, id, ws, on }, "")
            }
            Some(Admin::Open { owner, ws, persona, uid }) => open(a, &owner, &ws, &persona, uid).map(|p| vec![p.display().to_string()]),
            Some(Admin::Close { owner, ws }) => {
                close(a, &owner, &ws);
                Ok(vec![])
            }
            Some(Admin::Forget { owner, ws, persona }) => {
                close(a, &owner, &ws);
                a.arca.forget(&owner, &persona, &ws).map(|n| vec![format!("{n}")]).map_err(|e| e.to_string())
            }
            None => Err("bad request".into()),
        },
        Some(Caller::Person(user)) => {
            // A token comes on the line after its request.
            let mut token = String::new();
            if matches!(parse_person(&line), Some(Person::Token { .. }))
                && (rd.by_ref().take(arcad::proto::ssh_op::TOKEN_MAX as u64 + 2).read_line(&mut token).is_err() || !token.ends_with('\n'))
            {
                token.clear();
            }
            let r = person(a, &user, &line, token.trim_end_matches(['\r', '\n']));
            // Not kept in this heap longer than it takes.
            unsafe { token.as_bytes_mut().fill(0) };
            r
        }
    };
    let mut w = &c;
    let text = match out {
        Ok(lines) => std::iter::once("ok".to_string()).chain(lines).collect::<Vec<_>>().join("\n"),
        Err(e) => format!("error {e}"),
    };
    let _ = writeln!(w, "{text}");
}

fn person(a: &Agent, user: &str, line: &str, token: &str) -> Result<Vec<String>, String> {
    person_request(a, user, parse_person(line).ok_or("bad request")?, token)
}

fn person_request(a: &Agent, user: &str, req: Person, token: &str) -> Result<Vec<String>, String> {
    let e = |e: arca_ssh_agent::arca::Error| e.to_string();
    match req {
        Person::New { persona, comment, touch, p256 } => {
            let k = a.arca.mint(user, &persona, &comment, touch, p256).map_err(e)?;
            eprintln!("ssh-agent: {user} made {} key {} in {persona}", if p256 { "P-256" } else { "Ed25519" }, &hex(&k.id)[..8]);
            Ok(vec![format!("{} {}", hex(&k.id), public_line(&k))])
        }
        Person::List { persona } => Ok(a.arca.list(user, &persona).map_err(e)?
            .into_iter()
            .map(|l| if l.token {
                format!("{} token {} granted={}", hex(&l.key.id), l.key.comment, l.grants.join(","))
            } else {
                format!("{} {} granted={}", hex(&l.key.id), public_line(&l.key), l.grants.join(","))
            })
            .collect()),
        Person::Grant { persona, id, ws, on } => {
            a.arca.grant(user, &persona, &id, &ws, on).map_err(e)?;
            eprintln!("ssh-agent: {user} {} key {} {} {ws}", if on { "granted" } else { "revoked" }, &hex(&id)[..8], if on { "to" } else { "from" });
            Ok(vec![])
        }
        Person::Token { persona, host, username } => {
            if token.is_empty() || token.chars().any(char::is_control) {
                return Err("the token goes on the next line".into());
            }
            let id = a.arca.token_add(user, &persona, &host, &username, token.as_bytes()).map_err(e)?;
            eprintln!("ssh-agent: {user} stored a {host} token {} in {persona}", &hex(&id)[..8]);
            Ok(vec![hex(&id)])
        }
        Person::Delete { persona, id } => {
            a.arca.delete(user, &persona, &id).map_err(e)?;
            Ok(vec![])
        }
    }
}

