//! Opening files in apps (docs/spec/opening-files.md §6.2): each app's
//! document channel, the checks on the descriptor the file manager sends,
//! and spending the press with frescod.
//!
//! Every app whose manifest has `[[opens]]` gets a directory of its own,
//! `ROOT/<app id>/`, holding `opens.sock`, which its jail sees at
//! `/atrium/sockets/opens/` ([`atrium_open::SOCKET`]). The app connects
//! once, at start; portcullisd takes the connection only from the app's
//! own per-app uid (the launch registry), keeps one per app, and sends
//! each file opened in it there. A file opened before the app has
//! connected (it is starting) waits, for [`PENDING_FOR`] at most.
#![allow(unsafe_code)]

use std::collections::HashMap;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Where the channels' directories are on the host.
pub const ROOT: &str = "/atrium/sockets/opens";
/// Overrides [`ROOT`] (a gate that runs its own portcullisd).
pub const ROOT_ENV: &str = "PORTCULLISD_OPENS_ROOT";
/// frescod's activation socket.
pub const ACTIVATION: &str = "/atrium/sockets/fresco-activation/fresco-activation.sock";
pub const ACTIVATION_ENV: &str = "PORTCULLISD_FRESCO_ACTIVATION";
/// How long a file waits for its app to connect.
pub const PENDING_FOR: Duration = Duration::from_secs(60);
/// Files waiting for one app, at most.
pub const MAX_PENDING: usize = 16;

struct Pending {
    at: Instant,
    /// Whose it is: only that person's instance of the app gets it.
    owner: String,
    name: String,
    mime: String,
    write: bool,
    /// A file, or (`None`) the link in `name` (opening-links.md §5).
    fd: Option<OwnedFd>,
    /// A link the person sent from another persona: its name
    /// (personas.md §6).
    from: Option<String>,
}

#[derive(Default)]
struct Slot {
    listening: bool,
    /// The connected instance, and the person it runs for.
    conn: Option<(u64, UnixStream, String)>,
    pending: Vec<Pending>,
}

/// What became of a file handed to [`Hub::deliver`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// On the app's channel now.
    Sent,
    /// Waiting for the app to connect (start it if it isn't running).
    Queued,
}

/// The document channel of `app`'s instance in `persona` (personas.md §3):
/// `<id>` for Personal, `<id>@<persona>` for a named persona, as the
/// instance's jail root is named. The hub is keyed by it, so a Work
/// instance's channel is never the Personal one's.
pub fn channel_key(app: &str, persona: Option<&str>) -> String {
    match persona {
        None => app.to_string(),
        Some(p) => format!("{app}@{p}"),
    }
}

pub struct Hub {
    root: PathBuf,
    registry: String,
    apps: Mutex<HashMap<String, Slot>>,
    next: std::sync::atomic::AtomicU64,
}

impl Hub {
    pub fn new(root: impl Into<PathBuf>, registry: impl Into<String>) -> Arc<Hub> {
        Arc::new(Hub { root: root.into(), registry: registry.into(), apps: Mutex::new(HashMap::new()), next: Default::default() })
    }

    /// portcullisd's one hub.
    pub fn global() -> &'static Arc<Hub> {
        static H: OnceLock<Arc<Hub>> = OnceLock::new();
        H.get_or_init(|| {
            let root = std::env::var(ROOT_ENV).unwrap_or_else(|_| ROOT.into());
            Hub::new(root, portcullis_peer::DEFAULT_REGISTRY)
        })
    }

    /// `app`'s channel directory (what its jail mounts).
    pub fn dir(&self, app: &str) -> PathBuf {
        self.root.join(app)
    }

    /// Make `app`'s channel and serve it (once; later calls just answer
    /// the directory).
    pub fn listen(self: &Arc<Self>, app: &str) -> io::Result<PathBuf> {
        if app.is_empty() || app.contains(['/', '\0']) || app.starts_with('.') {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("app id {app:?}")));
        }
        let dir = self.dir(app);
        let mut apps = self.apps.lock().unwrap();
        let slot = apps.entry(app.to_string()).or_default();
        if slot.listening {
            return Ok(dir);
        }
        std::fs::create_dir_all(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))?;
        let sock = dir.join("opens.sock");
        let _ = std::fs::remove_file(&sock);
        let l = UnixListener::bind(&sock)?;
        // The peer uid below is the gate; the jail's mount is the reach.
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o666))?;
        slot.listening = true;
        drop(apps);
        let (hub, app) = (self.clone(), app.to_string());
        std::thread::Builder::new().name(format!("opens-{app}")).spawn(move || {
            for s in l.incoming().flatten() {
                hub.accepted(&app, s);
            }
        })?;
        Ok(dir)
    }

    fn accepted(self: &Arc<Self>, app: &str, s: UnixStream) {
        let uid = portcullis_peer::uid_gid(s.as_raw_fd()).map(|(u, _)| u);
        let reg = portcullis_peer::AppRegistry::load(&self.registry).unwrap_or_default();
        // `app` is the channel's key: the uid must be that very instance.
        let owner = match uid.as_ref().ok().and_then(|u| reg.resolve(*u).filter(|_| reg.document_slot(*u).is_none()).map(|r| (r, reg.persona(*u).flatten()))) {
            Some(((o, a), p)) if channel_key(a, p) == app => o.to_string(),
            _ => {
                eprintln!("portcullisd: {app}'s document channel: refused uid {uid:?}, not {app}");
                return;
            }
        };
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let Ok(mut watch) = s.try_clone() else { return };
        {
            let mut apps = self.apps.lock().unwrap();
            let slot = apps.entry(app.to_string()).or_default();
            slot.conn = Some((id, s, owner));
            Self::flush(app, slot);
        }
        eprintln!("portcullisd: {app} connected its document channel");
        // Anything the app sends is ignored; its end closing ends the channel.
        let (hub, app) = (self.clone(), app.to_string());
        let _ = std::thread::Builder::new().name(format!("opens-{app}-conn")).spawn(move || {
            let mut b = [0u8; 256];
            while matches!(watch.read(&mut b), Ok(n) if n > 0) {}
            let mut apps = hub.apps.lock().unwrap();
            if let Some(slot) = apps.get_mut(&app) {
                if slot.conn.as_ref().is_some_and(|(i, _, _)| *i == id) {
                    slot.conn = None;
                }
            }
        });
    }

    /// Send what waits, oldest first; drop what waited too long.
    fn flush(app: &str, slot: &mut Slot) {
        slot.pending.retain(|p| p.at.elapsed() < PENDING_FOR);
        while !slot.pending.is_empty() {
            let Some((_, c, owner)) = &slot.conn else { return };
            let p = &slot.pending[0];
            // ★ Another person's instance: never theirs to have.
            if &p.owner != owner {
                eprintln!("portcullisd: {app}: “{}” was {}'s, and {app} runs for {owner}: dropped", p.name, p.owner);
                slot.pending.remove(0);
                continue;
            }
            let sent = match &p.fd {
                Some(fd) => atrium_open::send(c, &p.name, &p.mime, p.write, fd.as_raw_fd()),
                None => match &p.from {
                    None => atrium_open::send_link(c, &p.name),
                    Some(f) => atrium_open::send_crossed_link(c, &p.name, f),
                },
            };
            match sent {
                Ok(()) => {
                    match p.fd {
                        Some(_) => eprintln!("portcullisd: {app}: “{}” ({}) handed over{}", p.name, p.mime, if p.write { " (write)" } else { "" }),
                        None => eprintln!("portcullisd: {app}: a {} link handed over", p.mime),
                    }
                    slot.pending.remove(0);
                }
                Err(e) => {
                    eprintln!("portcullisd: {app}'s document channel: {e}");
                    slot.conn = None;
                }
            }
        }
    }

    /// Hand `fd` to `owner`'s `app`, now or once it connects. Refused while
    /// the app runs for someone else.
    pub fn deliver(&self, owner: &str, app: &str, name: &str, mime: &str, write: bool, fd: OwnedFd) -> io::Result<Delivery> {
        self.push(owner, app, name, mime, write, Some(fd), None)
    }

    /// Hand `url` (a link of `scheme`) to `owner`'s `app`, as [`Hub::deliver`].
    pub fn deliver_link(&self, owner: &str, app: &str, scheme: &str, url: &str) -> io::Result<Delivery> {
        self.deliver_link_from(owner, app, scheme, url, None)
    }

    /// [`Hub::deliver_link`], for a link the person sent from persona
    /// `from` (`personal` for Personal): the app is told so, and a Work
    /// app opens it as a stranger would (personas.md §6).
    pub fn deliver_link_from(&self, owner: &str, app: &str, scheme: &str, url: &str, from: Option<&str>) -> io::Result<Delivery> {
        let ok = match from {
            None => atrium_open::encode_link(url).is_some(),
            Some(f) => atrium_open::encode_crossed_link(url, f).is_some(),
        };
        if !ok {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a link to pass on"));
        }
        self.push(owner, app, url, scheme, false, None, from.map(String::from))
    }

    #[allow(clippy::too_many_arguments)]
    fn push(&self, owner: &str, app: &str, name: &str, mime: &str, write: bool, fd: Option<OwnedFd>, from: Option<String>) -> io::Result<Delivery> {
        let mut apps = self.apps.lock().unwrap();
        let slot = apps.entry(app.to_string()).or_default();
        if let Some((_, _, o)) = &slot.conn {
            if o != owner {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("{app} is open for another person")));
            }
        }
        slot.pending.retain(|p| p.at.elapsed() < PENDING_FOR);
        if slot.pending.len() >= MAX_PENDING {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, format!("{MAX_PENDING} files or links are already waiting for {app}")));
        }
        slot.pending.push(Pending { at: Instant::now(), owner: owner.into(), name: name.into(), mime: mime.into(), write, fd, from });
        Self::flush(app, slot);
        Ok(if slot.pending.is_empty() { Delivery::Sent } else { Delivery::Queued })
    }

    /// Whether `app` has its channel open now.
    pub fn connected(&self, app: &str) -> bool {
        self.apps.lock().unwrap().get(app).is_some_and(|s| s.conn.is_some())
    }
}

/// Check the descriptor the file manager sent (§6.2): a regular file, open
/// for writing when `write` is asked.
pub fn check(fd: &OwnedFd, write: bool) -> Result<(), String> {
    let f = std::fs::File::from(fd.try_clone().map_err(|e| e.to_string())?);
    let m = f.metadata().map_err(|e| format!("fstat: {e}"))?;
    if !m.file_type().is_file() {
        return Err("not a regular file".into());
    }
    // SAFETY: F_GETFL on a descriptor we hold.
    let fl = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if fl < 0 {
        return Err(format!("fcntl: {}", io::Error::last_os_error()));
    }
    let acc = fl & libc::O_ACCMODE;
    if write && acc != libc::O_RDWR {
        return Err("asked to open for changes, but the file isn't open for writing".into());
    }
    if acc == libc::O_WRONLY {
        return Err("open for writing only".into());
    }
    Ok(())
}

/// Check an attachment's descriptor (mail.md §8.2): a regular file
/// sealed against writing, growing and shrinking, so what the app gets is
/// what the person pressed Open on; its first bytes, for its type.
pub fn check_sealed(fd: &OwnedFd) -> Result<Vec<u8>, String> {
    check(fd, false)?;
    // SAFETY: F_GET_SEALS on a descriptor we hold.
    let seals = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GET_SEALS) };
    if seals < 0 {
        return Err(format!("not a sealed memory file ({})", io::Error::last_os_error()));
    }
    let want = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK;
    if seals & want != want {
        return Err("not sealed against changes".into());
    }
    use std::os::unix::fs::FileExt;
    let f = std::fs::File::from(fd.try_clone().map_err(|e| e.to_string())?);
    let mut head = vec![0u8; atrium_types::HEAD];
    let mut got = 0;
    while got < head.len() {
        match f.read_at(&mut head[got..], got as u64) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("read: {e}")),
        }
    }
    head.truncate(got);
    Ok(head)
}

/// Narrow `fd` to what an app may do with an opened file (§6.2): read,
/// seek, fstat; and, for an edit, write, ftruncate, fsync. Nothing else
/// (no fchmod, fchown, fexecve, executable mappings).
pub fn narrow(fd: &OwnedFd, write: bool) -> io::Result<()> {
    #[cfg(target_os = "freebsd")]
    {
        use libc::{cap_rights_limit, cap_rights_t, CAP_FSTAT, CAP_FSYNC, CAP_FTRUNCATE, CAP_READ, CAP_RIGHTS_VERSION, CAP_SEEK, CAP_WRITE};
        // SAFETY: __cap_rights_init fills a cap_rights_t we own from a
        // zero-terminated list; cap_rights_limit only reads it.
        unsafe {
            let mut r: cap_rights_t = std::mem::zeroed();
            if write {
                libc::__cap_rights_init(CAP_RIGHTS_VERSION, &mut r, CAP_READ, CAP_SEEK, CAP_FSTAT, CAP_WRITE, CAP_FTRUNCATE, CAP_FSYNC, 0u64);
            } else {
                libc::__cap_rights_init(CAP_RIGHTS_VERSION, &mut r, CAP_READ, CAP_SEEK, CAP_FSTAT, 0u64);
            }
            if cap_rights_limit(fd.as_raw_fd(), &r) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    #[cfg(not(target_os = "freebsd"))]
    let _ = (fd, write);
    Ok(())
}

/// Spend `token`, a press in a window of `presenter_uid`'s, with frescod
/// on purpose OPEN. `Err` says why not, for the file manager to show.
pub fn spend_press(sock: &Path, token: [u8; 16], presenter_uid: u32) -> Result<(), String> {
    spend_press_for(sock, token, presenter_uid, fresco_protocol::activation::purpose::OPEN)
}

/// Ask frescod to bring forward `uid`'s top window: the app a link was
/// just handed to (opening-links.md §5). frescod allows it once per
/// `LINK` press this daemon spent in the last two minutes.
pub fn raise(sock: &Path, uid: u32) -> Result<u32, String> {
    use fresco_protocol::activation::{op, refusal, Raise, RaiseResult};
    let ask = || -> io::Result<RaiseResult> {
        let bad = |e: String| io::Error::new(io::ErrorKind::InvalidData, e);
        let mut conn = aqueduct::Connection::connect(sock)?;
        let payload = postcard::to_stdvec(&Raise { uid }).map_err(|e| bad(e.to_string()))?;
        conn.send_message(aqueduct::classes::CLASS_DISPLAY, op::RAISE, aqueduct::flag::RESPONSE_EXPECTED, &payload)?;
        conn.set_read_timeout(Some(Duration::from_millis(500)))?;
        let m = conn.recv_message()?;
        if m.op != op::RAISE || m.flags & aqueduct::flag::IS_RESPONSE == 0 {
            return Err(bad("unexpected reply".into()));
        }
        postcard::from_bytes(&m.payload).map_err(|e| bad(e.to_string()))
    };
    match ask() {
        Ok(RaiseResult::Ok { window_id }) => Ok(window_id),
        Ok(RaiseResult::Refused { reason: refusal::UNKNOWN }) => Err("it has no window on screen".into()),
        Ok(RaiseResult::Refused { reason }) => Err(format!("frescod refused ({reason})")),
        Err(e) => Err(format!("frescod: {e}")),
    }
}

/// [`spend_press`] for another purpose (an app's console: `CONSOLE`).
pub fn spend_press_for(sock: &Path, token: [u8; 16], presenter_uid: u32, purpose_: u8) -> Result<(), String> {
    use fresco_protocol::activation::{op, refusal, Consume, ConsumeResult};
    if token == fresco_protocol::activation::NONE {
        return Err("no press came with it".into());
    }
    let ask = || -> io::Result<ConsumeResult> {
        let bad = |e: String| io::Error::new(io::ErrorKind::InvalidData, e);
        let mut conn = aqueduct::Connection::connect(sock)?;
        let payload = postcard::to_stdvec(&Consume { token, presenter_uid, purpose: purpose_ }).map_err(|e| bad(e.to_string()))?;
        conn.send_message(aqueduct::classes::CLASS_DISPLAY, op::CONSUME, aqueduct::flag::RESPONSE_EXPECTED, &payload)?;
        conn.set_read_timeout(Some(Duration::from_millis(500)))?;
        let m = conn.recv_message()?;
        if m.op != op::CONSUME || m.flags & aqueduct::flag::IS_RESPONSE == 0 {
            return Err(bad("unexpected reply".into()));
        }
        postcard::from_bytes(&m.payload).map_err(|e| bad(e.to_string()))
    };
    match ask() {
        Ok(ConsumeResult::Ok { .. }) => Ok(()),
        Ok(ConsumeResult::Refused { reason }) => Err(match reason {
            refusal::EXPIRED | refusal::UNKNOWN => "the press was too long ago; open it again",
            refusal::NOT_FOCUSED => "the window lost focus; open it again",
            refusal::WRONG_UID => "the press wasn't in the asking app's window",
            refusal::WRONG_KIND => "that press can't do this",
            refusal::OBSCURED => "the window was covered or had just moved; press again",
            _ => "frescod refused the press",
        }
        .into()),
        Err(e) => Err(format!("frescod's activation socket: {e}")),
    }
}

/// frescod's activation socket, as configured.
pub fn activation_socket() -> PathBuf {
    std::env::var_os(ACTIVATION_ENV).map(PathBuf::from).unwrap_or_else(|| ACTIVATION.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("opens-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn only_the_app_itself_gets_its_files() {
        let d = tmp("hub");
        let me = unsafe { libc::getuid() };
        let reg = d.join("registry");
        std::fs::write(&reg, format!("{me} alice org.test.player\n")).unwrap();
        let hub = Hub::new(d.join("opens"), reg.to_str().unwrap());
        let dir = hub.listen("org.test.player").unwrap();
        assert_eq!(hub.listen("org.test.player").unwrap(), dir, "once");
        assert!(hub.listen("../etc").is_err());
        std::fs::write(d.join("a.flac"), b"fLaC").unwrap();
        let fd = OwnedFd::from(File::open(d.join("a.flac")).unwrap());
        // Before the app is up: it waits.
        assert_eq!(hub.deliver("alice", "org.test.player", "a.flac", "audio/flac", false, fd).unwrap(), Delivery::Queued);
        assert!(!hub.connected("org.test.player"));
        let mut ch = atrium_open::Channel::connect_to(&dir.join("opens.sock")).unwrap();
        let o = ch.recv(Some(Duration::from_secs(2))).unwrap().expect("the waiting file");
        assert_eq!((o.name.as_str(), o.mime.as_str(), o.write), ("a.flac", "audio/flac", false));
        // Running: straight there.
        let fd = OwnedFd::from(File::open(d.join("a.flac")).unwrap());
        assert_eq!(hub.deliver("alice", "org.test.player", "b.flac", "audio/flac", false, fd).unwrap(), Delivery::Sent);
        assert_eq!(ch.recv(Some(Duration::from_secs(2))).unwrap().unwrap().name, "b.flac");
        // Another app's channel: this uid isn't that app, so it's refused.
        let other = hub.listen("org.test.other").unwrap();
        let fd = OwnedFd::from(File::open(d.join("a.flac")).unwrap());
        hub.deliver("alice", "org.test.other", "c.flac", "audio/flac", false, fd).unwrap();
        let mut wrong = atrium_open::Channel::connect_to(&other.join("opens.sock")).unwrap();
        assert!(matches!(wrong.recv(Some(Duration::from_secs(2))), Err(_)), "closed without a file");
        // Someone else's: refused while alice's runs.
        let fd = OwnedFd::from(File::open(d.join("a.flac")).unwrap());
        assert!(hub.deliver("bob", "org.test.player", "d.flac", "audio/flac", false, fd).is_err(), "not bob's player");
        // A link, on the same channel, no descriptor.
        assert_eq!(hub.deliver_link("alice", "org.test.player", "mailto", "mailto:a@b").unwrap(), Delivery::Sent);
        match ch.recv_any(Some(Duration::from_secs(2))).unwrap() {
            Some(atrium_open::Message::Link(u)) => assert_eq!(u, "mailto:a@b"),
            other => panic!("{other:?}"),
        }
        assert!(hub.deliver_link("bob", "org.test.player", "mailto", "mailto:a@b").is_err(), "not bob's");
        // The app goes: files wait again.
        drop(ch);
        std::thread::sleep(Duration::from_millis(200));
        assert!(!hub.connected("org.test.player"));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_work_instance_has_a_channel_of_its_own() {
        let d = tmp("persona");
        let me = unsafe { libc::getuid() };
        let reg = d.join("registry");
        // This uid is alice's Work player: the Personal channel isn't its.
        std::fs::write(&reg, format!("{me} alice org.test.player persona=work\n")).unwrap();
        let hub = Hub::new(d.join("opens"), reg.to_str().unwrap());
        assert_eq!(channel_key("org.test.player", Some("work")), "org.test.player@work");
        assert_eq!(channel_key("org.test.player", None), "org.test.player");
        let personal = hub.listen("org.test.player").unwrap();
        let work = hub.listen(&channel_key("org.test.player", Some("work"))).unwrap();
        assert_ne!(personal, work);
        hub.deliver_link("alice", "org.test.player", "https", "https://personal.example/").unwrap();
        let mut wrong = atrium_open::Channel::connect_to(&personal.join("opens.sock")).unwrap();
        assert!(matches!(wrong.recv_any(Some(Duration::from_secs(2))), Err(_)), "Personal's link never reaches Work");
        assert_eq!(hub.deliver_link("alice", "org.test.player@work", "https", "https://work.example/").unwrap(), Delivery::Queued);
        let mut ch = atrium_open::Channel::connect_to(&work.join("opens.sock")).unwrap();
        match ch.recv_any(Some(Duration::from_secs(2))).unwrap() {
            Some(atrium_open::Message::Link(u)) => assert_eq!(u, "https://work.example/"),
            other => panic!("{other:?}"),
        }
        // A link the person sent from Personal says so (personas.md §6).
        assert_eq!(hub.deliver_link_from("alice", "org.test.player@work", "https", "https://x.example/", Some("personal")).unwrap(), Delivery::Sent);
        match ch.recv_any(Some(Duration::from_secs(2))).unwrap() {
            Some(atrium_open::Message::Crossed { url, from }) => assert_eq!((url.as_str(), from.as_str()), ("https://x.example/", "personal")),
            other => panic!("{other:?}"),
        }
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_descriptor_is_a_regular_file_opened_as_claimed() {
        let d = tmp("check");
        std::fs::write(d.join("f"), b"x").unwrap();
        let ro = OwnedFd::from(File::open(d.join("f")).unwrap());
        assert!(check(&ro, false).is_ok());
        assert!(check(&ro, true).is_err(), "write asked of a read-only descriptor");
        let rw = OwnedFd::from(std::fs::OpenOptions::new().read(true).write(true).open(d.join("f")).unwrap());
        assert!(check(&rw, true).is_ok());
        let dir = OwnedFd::from(File::open(&d).unwrap());
        assert!(check(&dir, false).is_err(), "a folder");
        let (a, _b) = UnixStream::pair().unwrap();
        assert!(check(&OwnedFd::from(a), false).is_err(), "a socket");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn no_press_no_open() {
        assert!(spend_press(Path::new("/nonexistent"), [0; 16], 1).unwrap_err().contains("no press"));
        assert!(spend_press(Path::new("/nonexistent"), [1; 16], 1).unwrap_err().contains("activation socket"));
    }
}
