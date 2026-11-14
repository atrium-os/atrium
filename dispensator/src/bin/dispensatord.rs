//! atrium-dispensatord: gives each device to exactly one owner
//! (docs/spec/device-assignment.md §4).
//!
//! Reads `devices.d/*.toml`, the devices present (`devinfo -v`) and then
//! devd's events, and as root:
//!
//! - shows a granted node in a service's own devfs mount
//!   (`devfs -m <mount> rule apply path <node> unhide`), re-applied every
//!   `reshow_secs` because a restarted service gets a fresh mount;
//! - starts a driver's jail per device through jaild (`CreateJail` with
//!   `devfs_grant` = that node, plus `devfs_grants` for a device that is
//!   several, no network, the driver's sysfil profile), holding its
//!   procdesc; stops it when the device goes;
//! - serves a partition (a `disk` rule's FUSE driver) only while a human
//!   session is active and unlocked, as that human (media.rs); the session
//!   ejects one through the media socket;
//! - takes a USB interface from a kernel driver when a `usb` rule with
//!   `detach_kernel` names it (`usbconfig ... detach_kernel_driver`);
//! - leaves everything else hidden.
//!
//! It never reads a device byte: the kernel classifies, jailed drivers parse.
//! Took over portcullisd's identity-device agent (sigillum.md §5.1) on
//! 2026-11-13 with the same behaviour, now driven by rules.
//!
//! ```text
//! atrium-dispensatord [--rules DIR] [--jaild-socket PATH] [--state FILE]
//! atrium-dispensatord --check [--rules DIR]          validate, print, exit
//! atrium-dispensatord --plan FILE [--devinfo FILE]   print the ops for devd lines
//! atrium-dispensatord --media [--media-socket PATH]  list the partitions served
//! atrium-dispensatord --eject PART [--media-socket PATH]
//! atrium-dispensatord --media-watch [--media-socket PATH]   print events
//! atrium-dispensatord --devices [--devices-socket PATH]     the Devices list
//! atrium-dispensatord --grant PART PATH --jail NAME --uid U [--gid G] [--ro]
//! atrium-dispensatord --revoke JAIL      (root: a folder of a stick in an app's jail, §4.8)
//! atrium-dispensatord --attach-all       (the file manager, from its jail: every stick)
//! atrium-dispensatord --open PART PATH [--write]   (root: print the file, as handed over)
//! ```
//!
//! The media socket speaks Aqueduct class `media` (atrium-media, §6.6):
//! list, eject and a subscription to added/removed events, for root or the
//! active session's user. The devices socket speaks class `devices`
//! (atrium-devices, §6.10): every device, its owner, and why one has none,
//! for the same callers.

use std::path::PathBuf;
use std::process::ExitCode;

use dispensator::plan::Planner;
use dispensator::rules::Rules;

const RULES_DIR: &str = "/usr/local/etc/atrium/devices.d";
const JAILD_SOCKET: &str = "/var/run/atrium/jaild.sock";
const STATE_FILE: &str = "/var/run/atrium/dispensator.state";
const MEDIA_SOCKET: &str = "/atrium/sockets/removable-media/removable-media.sock";
const DEVICES_SOCKET: &str = "/atrium/sockets/device-list/device-list.sock";

struct Args {
    rules: PathBuf,
    jaild: String,
    state: PathBuf,
    check: bool,
    plan: Option<PathBuf>,
    devinfo: Option<PathBuf>,
    /// Refuse rule files not owned by root or writable by others. On by
    /// default for the daemon; `--check` and `--plan` take files as they are.
    strict: bool,
    seat: PathBuf,
    media_socket: PathBuf,
    devices_socket: PathBuf,
    devices_cmd: bool,
    grant: Option<atrium_media::Grant>,
    grant_jail: String,
    grant_uid: u32,
    grant_gid: Option<u32>,
    grant_ro: bool,
    open: Option<atrium_media::Open>,
    open_write: bool,
    /// Client: `list` or `eject PART`.
    media_cmd: Option<String>,
}

fn args() -> Result<Args, String> {
    let mut a = Args {
        rules: RULES_DIR.into(),
        jaild: JAILD_SOCKET.into(),
        state: STATE_FILE.into(),
        check: false,
        plan: None,
        devinfo: None,
        strict: true,
        seat: dispensator::media::SEAT_FILE.into(),
        media_socket: MEDIA_SOCKET.into(),
        devices_socket: DEVICES_SOCKET.into(),
        devices_cmd: false,
        grant: None,
        grant_jail: String::new(),
        grant_uid: 0,
        grant_gid: None,
        grant_ro: false,
        open: None,
        open_write: false,
        media_cmd: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        let mut val = || it.next().ok_or(format!("{x} needs a value"));
        match x.as_str() {
            "--rules" => a.rules = val()?.into(),
            "--jaild-socket" => a.jaild = val()?,
            "--state" => a.state = val()?.into(),
            "--check" => a.check = true,
            "--plan" => a.plan = Some(val()?.into()),
            "--devinfo" => a.devinfo = Some(val()?.into()),
            "--seat" => a.seat = val()?.into(),
            "--media-socket" => a.media_socket = val()?.into(),
            "--media" => a.media_cmd = Some("list".into()),
            "--media-watch" => a.media_cmd = Some("watch".into()),
            "--eject" => a.media_cmd = Some(format!("eject {}", val()?)),
            "--devices-socket" => a.devices_socket = val()?.into(),
            "--devices" => a.devices_cmd = true,
            "--grant" => {
                let part = val()?;
                let path = it.next().ok_or("--grant needs PART PATH")?;
                a.grant = Some(atrium_media::Grant { part, path, jail: String::new(), uid: 0, gid: 0, read_only: false });
            }
            "--jail" => a.grant_jail = val()?,
            "--uid" => a.grant_uid = val()?.parse().map_err(|_| "--uid: a number")?,
            "--gid" => a.grant_gid = Some(val()?.parse().map_err(|_| "--gid: a number")?),
            "--ro" => a.grant_ro = true,
            "--revoke" => a.media_cmd = Some(format!("revoke {}", val()?)),
            "--attach-all" => a.media_cmd = Some("attach-all".into()),
            // The picker's request (scrinium.md §4.3): PART PATH APP_UID.
            "--picked" => {
                let part = val()?;
                let path = it.next().ok_or("--picked needs PART PATH APP_UID")?;
                let uid = it.next().ok_or("--picked needs PART PATH APP_UID")?;
                a.media_cmd = Some(format!("picked {part} {uid} {path}"));
            }
            "--open" => {
                let part = val()?;
                let path = it.next().ok_or("--open needs PART PATH")?;
                a.open = Some(atrium_media::Open { part, path, write: false });
            }
            "--write" => a.open_write = true,
            "-h" | "--help" => return Err(String::new()),
            _ => return Err(format!("unknown argument {x}")),
        }
    }
    if a.check || a.plan.is_some() {
        a.strict = false;
    }
    Ok(a)
}

fn main() -> ExitCode {
    let a = match args() {
        Ok(a) => a,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("atrium-dispensatord: {e}");
            }
            eprintln!("usage: atrium-dispensatord [--rules DIR] [--jaild-socket PATH] [--state FILE] | --check [--rules DIR] | --plan FILE [--devinfo FILE] [--rules DIR] [--devices] | --devices | --media | --eject PART");
            return ExitCode::from(2);
        }
    };
    if let Some(g) = &a.grant {
        let g = atrium_media::Grant { jail: a.grant_jail.clone(), uid: a.grant_uid, gid: a.grant_gid.unwrap_or(a.grant_uid), read_only: a.grant_ro, ..g.clone() };
        return match atrium_media::client::grant(&a.media_socket, &g) {
            Ok(m) => {
                println!("ok {m}");
                ExitCode::SUCCESS
            }
            Err((st, m)) => {
                println!("error {m} (status {st})");
                ExitCode::FAILURE
            }
        };
    }
    if let Some(o) = &a.open {
        let o = atrium_media::Open { write: a.open_write, ..o.clone() };
        return match atrium_media::client::open(&a.media_socket, &o) {
            Ok(mut f) => {
                use std::io::Write;
                let mut out = std::io::stdout().lock();
                if o.write {
                    // Proves the descriptor writes: append a line, read it all.
                    use std::io::Seek;
                    let _ = f.seek(std::io::SeekFrom::End(0)).and_then(|_| f.write_all(b"written through the descriptor\n"));
                    let _ = f.rewind();
                }
                match std::io::copy(&mut f, &mut out) {
                    Ok(_) => ExitCode::SUCCESS,
                    Err(e) => {
                        eprintln!("error reading: {e}");
                        ExitCode::FAILURE
                    }
                }
            }
            Err((st, m)) => {
                println!("error {m} (status {st})");
                ExitCode::FAILURE
            }
        };
    }
    if let Some(cmd) = &a.media_cmd {
        return media_client(&a.media_socket, cmd);
    }
    if a.devices_cmd && a.plan.is_none() {
        return match atrium_devices::client::list(&a.devices_socket) {
            Ok(ds) => {
                println!("ok {} devices", ds.len());
                print_devices(&ds);
                ExitCode::SUCCESS
            }
            Err(e) => {
                println!("error {e}");
                ExitCode::FAILURE
            }
        };
    }
    let rules = match Rules::load_dir(&a.rules, a.strict) {
        Ok(r) => r,
        Err(e) => {
            // Fail closed: with no valid rules, no device goes to anyone.
            eprintln!("atrium-dispensatord: rules: {e}");
            return ExitCode::FAILURE;
        }
    };
    if a.check {
        for (r, o) in &rules.rules {
            println!(
                "rule {} class={} priority={} pages={:04x?} driver={:?} usb={:02x?} ids={:?} detach_kernel={} nodes={:?} owner={o:?}",
                r.name, r.class, r.priority, r.any_page, r.driver, r.usb_interface, r.usb_ids, r.detach_kernel, r.nodes
            );
        }
        for (n, d) in &rules.drivers {
            println!("driver {n} exec={} uid={} ruleset={} sysfil={} jail={}", d.exec, d.uid, d.devfs_ruleset, d.sysfil, d.jail);
        }
        for (n, s) in &rules.services {
            println!("service {n} devfs={} reshow={}s", s.devfs, s.reshow_secs);
        }
        return ExitCode::SUCCESS;
    }
    if let Some(p) = &a.plan {
        let mut pl = Planner::new(rules);
        let mut ops = Vec::new();
        if let Some(d) = &a.devinfo {
            match std::fs::read_to_string(d) {
                Ok(t) => ops.extend(pl.devinfo(&t)),
                Err(e) => {
                    eprintln!("atrium-dispensatord: {}: {e}", d.display());
                    return ExitCode::FAILURE;
                }
            }
        }
        match std::fs::read_to_string(p) {
            Ok(t) => t.lines().for_each(|l| ops.extend(pl.line(l))),
            Err(e) => {
                eprintln!("atrium-dispensatord: {}: {e}", p.display());
                return ExitCode::FAILURE;
            }
        }
        for o in ops {
            println!("{o:?}");
        }
        print!("{}", pl.state());
        if a.devices_cmd {
            print_devices(&pl.inventory(|_, _| false));
        }
        return ExitCode::SUCCESS;
    }
    run::serve(rules, a.jaild, a.state, a.seat, a.media_socket, a.devices_socket)
}

/// One line per device: name, bus, owner, then the details.
fn print_devices(ds: &[atrium_devices::Device]) {
    use atrium_devices::Owner;
    for d in ds {
        let o = match &d.owner {
            Owner::Driver { rule, driver, running } => format!("driver {driver} (rule {rule}){}", if *running { "" } else { ", not running" }),
            Owner::Service { rule, service, nodes } => format!("service {service} (rule {rule}) {}", nodes.join(",")),
            Owner::Kernel { driver, note: None } => format!("kernel {driver}"),
            Owner::Kernel { driver, note: Some(n) } => format!("kernel {driver}: {n}"),
            Owner::Nobody { why } => format!("nobody: {why}"),
        };
        println!("{} {} {o}{}", d.name, d.bus, if d.ids.is_empty() { String::new() } else { format!(" [{}]", d.ids) });
    }
}

/// One request to the daemon's media socket (Aqueduct `CLASS_MEDIA`);
/// prints the answer.
fn media_client(sock: &std::path::Path, cmd: &str) -> ExitCode {
    if cmd == "attach-all" {
        return match atrium_media::client::attach_all(sock) {
            Ok(m) => {
                println!("ok {m}");
                ExitCode::SUCCESS
            }
            Err((st, m)) => {
                println!("error {m} (status {st})");
                ExitCode::FAILURE
            }
        };
    }
    if let Some(rest) = cmd.strip_prefix("picked ") {
        let mut f = rest.splitn(3, ' ');
        let (Some(part), Some(uid), Some(path)) = (f.next(), f.next().and_then(|u| u.parse().ok()), f.next()) else {
            println!("error malformed");
            return ExitCode::FAILURE;
        };
        let p = atrium_media::Picked { part: part.into(), path: path.into(), app_uid: uid, read_only: std::env::var_os("PICKED_RO").is_some() };
        return match atrium_media::client::picked(sock, &p) {
            Ok(m) => {
                println!("ok {m}");
                ExitCode::SUCCESS
            }
            Err((st, m)) => {
                println!("error {m} (status {st})");
                ExitCode::FAILURE
            }
        };
    }
    match cmd.split_once(' ') {
        Some(("revoke", jail)) => match atrium_media::client::revoke(sock, jail) {
            Ok(m) => {
                println!("ok {m}");
                ExitCode::SUCCESS
            }
            Err((st, m)) => {
                println!("error {m} (status {st})");
                ExitCode::FAILURE
            }
        },
        Some(("eject", part)) => match atrium_media::client::eject(sock, part) {
            Ok(m) => {
                println!("ok {m}");
                ExitCode::SUCCESS
            }
            Err((st, m)) => {
                println!("error {m} (status {st})");
                ExitCode::FAILURE
            }
        },
        Some(_) | None if cmd == "watch" => {
            use std::io::Write;
            let mut ev = match atrium_media::client::Events::open(sock) {
                Ok(e) => e,
                Err(e) => {
                    println!("error {e}");
                    return ExitCode::FAILURE;
                }
            };
            loop {
                match ev.next() {
                    Ok(atrium_media::Event::Added(v)) => println!("added {} {}", v.part, v.mountpoint),
                    Ok(atrium_media::Event::Removed { part, ejected }) => println!("removed {part}{}", if ejected { " ejected" } else { "" }),
                    Err(_) => {
                        println!("end");
                        return ExitCode::SUCCESS;
                    }
                }
                let _ = std::io::stdout().flush();
            }
        }
        _ => match atrium_media::client::list(sock) {
            Ok(vs) => {
                println!("ok");
                for v in vs {
                    println!("{} {} {} {}{}", v.part, v.mountpoint, v.size_bytes, v.free_bytes, if v.read_only { " ro" } else { "" });
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                println!("error {e}");
                ExitCode::FAILURE
            }
        },
    }
}


#[cfg(not(target_os = "freebsd"))]
mod run {
    use super::*;
    pub fn serve(_: Rules, _: String, _: PathBuf, _: PathBuf, _: PathBuf, _: PathBuf) -> ExitCode {
        eprintln!("atrium-dispensatord: the daemon runs on FreeBSD only (--check and --plan work anywhere)");
        ExitCode::FAILURE
    }
}

#[cfg(target_os = "freebsd")]
#[allow(unsafe_code)] // poll(2) and adopting jaild's procdesc
mod run {
    use std::collections::BTreeMap;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::path::Path;
    use std::time::{Duration, Instant};

    use atrium_devevents::DeviceWatcher;
    use dispensator::jail::{plan, stop_names, JailPlan};
    use dispensator::media::{Act, Media, Session};
    use dispensator::plan::{Op, Vars};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::{mpsc, Arc, Mutex};

    use atrium_devices::server as dserver;
    use atrium_media::{server as mserver, status as mstatus, Volume};
    use jaild::client::Client;
    use jaild::protocol::{CreateJailRequest, ExecSpec, FuseServe, MountKind, MountSpec, NetworkConfig, Request, Response};

    use super::*;

    struct Agent {
        jaild: String,
        state: PathBuf,
        planner: Planner,
        /// (driver, device) -> the procdesc that keeps its jail alive.
        drivers: BTreeMap<(String, String), OwnedFd>,
        /// The last failure showing each (service, node), so a service that
        /// is down is logged once, not every reshow.
        show_errors: BTreeMap<(String, String), String>,
        media: Media,
        session: Session,
        seat: PathBuf,
        /// The media service: what the session is told, and eject requests
        /// from its connection threads (with a byte on `wake` for each).
        shared: Option<Arc<mserver::Shared>>,
        session_shared: Arc<Mutex<Session>>,
        ejects: Option<(mpsc::Receiver<EjectReq>, UnixStream)>,
        ejected_now: Vec<String>,
        /// Whole disks waiting for GEOM to make their partitions, if any.
        settling: Vec<(Instant, String)>,
        /// The Devices list service (§6.10).
        devices: Option<Arc<dserver::Shared>>,
        /// Each stick server's control socket, by partition (§4.8).
        controls: BTreeMap<String, UnixStream>,
        /// Folders of served sticks in app jails.
        grants: Vec<GrantRec>,
        next_grant: u64,
        /// File managers given every stick.
        standing: Vec<Standing>,
    }

    /// A grant: a FUSE mount at `dir` (the stick's server, rooted at the
    /// folder, owned by the app), nullfs'd into `jail` at `dest`.
    struct GrantRec {
        part: String,
        jail: String,
        dir: PathBuf,
        dest: String,
    }

    enum Req {
        Eject(String),
        Grant(atrium_media::Grant),
        Revoke(String),
        AttachAll(Standing),
    }

    /// The file manager's standing grant (§4.8): every served stick, whole,
    /// in `jail`, as each comes, while the jail runs and `user`'s session
    /// is the active one.
    #[derive(Clone, PartialEq)]
    struct Standing {
        jail: String,
        uid: u32,
        gid: u32,
        user: String,
    }

    /// The one jail `uid`'s processes run in (a per-app uid has one), by
    /// name: none, several, or the host itself is no answer.
    fn jail_of_uid(uid: u32) -> Option<String> {
        let o = std::process::Command::new("ps").args(["-x", "-U", &uid.to_string(), "-o", "jid="]).output().ok()?;
        let mut jids: Vec<u32> = String::from_utf8_lossy(&o.stdout).split_whitespace().filter_map(|j| j.parse().ok()).collect();
        jids.sort_unstable();
        jids.dedup();
        let [jid] = jids[..] else { return None };
        if jid == 0 {
            return None;
        }
        let o = std::process::Command::new("jls").args(["-j", &jid.to_string(), "name"]).output().ok()?;
        let name = String::from_utf8_lossy(&o.stdout).trim().to_string();
        (!name.is_empty()).then_some(name)
    }

    fn primary_gid(uid: u32) -> Option<u32> {
        // SAFETY: getpwuid's result is read at once, on this thread's own
        // call (the daemon's connection threads are the only callers and
        // only read pw_gid).
        unsafe {
            let p = libc::getpwuid(uid);
            (!p.is_null()).then(|| (*p).pw_gid)
        }
    }

    /// Does the installed manifest of `app_id` ask for the capability `cap`
    /// (`removable-files`, `file-picker`)? (portcullisd granted it at
    /// install: a restricted capability is installed only with policy
    /// approval.)
    fn has_cap(apps_dir: &str, app_id: &str, cap: &str) -> bool {
        if app_id.is_empty() || app_id.contains('/') || app_id.starts_with('.') {
            return false;
        }
        let Ok(t) = std::fs::read_to_string(Path::new(apps_dir).join(app_id).join("atrium.toml")) else { return false };
        let Ok(v) = t.parse::<toml::Table>() else { return false };
        v.get("capabilities").and_then(|c| c.get(cap)).and_then(|b| b.as_bool()) == Some(true)
    }

    fn has_removable_files(apps_dir: &str, app_id: &str) -> bool {
        has_cap(apps_dir, app_id, "removable-files")
    }

    type EjectReq = (Req, mpsc::Sender<(u8, String)>);

    /// Is a jail of that name running?
    fn jail_alive(name: &str) -> bool {
        jaild::ffi::jail_id_by_name(name).is_some()
    }

    /// `line` with `fd` attached (SCM_RIGHTS).
    fn send_with_fd(c: &UnixStream, line: &str, fd: i32) -> std::io::Result<()> {
        let mut iov = libc::iovec { iov_base: line.as_ptr() as *mut _, iov_len: line.len() };
        let mut cbuf = [0u64; 4];
        // SAFETY: a zeroed msghdr is valid; every pointer outlives the call;
        // the CMSG macros work on our own buffer.
        unsafe {
            let mut m: libc::msghdr = std::mem::zeroed();
            m.msg_iov = &mut iov;
            m.msg_iovlen = 1;
            m.msg_control = cbuf.as_mut_ptr().cast();
            m.msg_controllen = libc::CMSG_SPACE(4) as _;
            let h = libc::CMSG_FIRSTHDR(&m);
            (*h).cmsg_level = libc::SOL_SOCKET;
            (*h).cmsg_type = libc::SCM_RIGHTS;
            (*h).cmsg_len = libc::CMSG_LEN(4) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(h) as *mut i32, fd);
            if libc::sendmsg(c.as_raw_fd(), &m, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }

    fn unmount_force(dir: &Path) {
        if let Ok(c) = std::ffi::CString::new(dir.to_string_lossy().as_bytes()) {
            // SAFETY: a valid NUL-terminated path.
            unsafe { libc::unmount(c.as_ptr(), libc::MNT_FORCE) };
        }
    }

    /// The media service's view of the daemon, for its connection threads.
    struct MediaHost {
        session: Arc<Mutex<Session>>,
        registry: String,
        apps_dir: String,
        tx: Mutex<mpsc::Sender<EjectReq>>,
        wake: Mutex<UnixStream>,
    }

    /// Root, the session's user, or one of that user's session components
    /// (Forum, Curia: the launch registry marks them).
    fn session_allowed(session: &Mutex<Session>, registry: &str, uid: u32) -> bool {
        let Session::Active { user, uid: su, .. } = session.lock().unwrap().clone() else { return uid == 0 };
        if uid == 0 || uid == su {
            return true;
        }
        let Ok(reg) = portcullis_peer::AppRegistry::load(registry) else { return false };
        reg.is_session(uid) && reg.resolve(uid).is_some_and(|(owner, _)| owner == user)
    }

    struct DevicesHost {
        session: Arc<Mutex<Session>>,
        registry: String,
    }

    impl dserver::Host for DevicesHost {
        fn allowed(&self, uid: u32) -> bool {
            session_allowed(&self.session, &self.registry, uid)
        }
    }

    impl mserver::Host for MediaHost {
        /// The session's user and components, and the file manager (an app
        /// of that user holding `removable-files`): it lists the sticks it
        /// holds, hears them come and go, and ejects them (scrinium.md §3).
        fn allowed(&self, uid: u32) -> bool {
            session_allowed(&self.session, &self.registry, uid) || self.file_manager(uid).is_some()
        }

        fn eject(&self, part: &str) -> (u8, String) {
            self.ask(Req::Eject(part.to_string()))
        }

        fn grant(&self, g: &atrium_media::Grant) -> (u8, String) {
            self.ask(Req::Grant(g.clone()))
        }

        fn revoke(&self, jail: &str) -> (u8, String) {
            self.ask(Req::Revoke(jail.to_string()))
        }

        /// The asker must be an app of the session's user whose manifest
        /// holds `removable-files`, running in exactly one jail.
        fn attach_all(&self, uid: u32) -> (u8, String) {
            let Session::Active { user, .. } = self.session.lock().unwrap().clone() else { return (mstatus::NOT_ALLOWED, "no unlocked session".into()) };
            let Ok(reg) = portcullis_peer::AppRegistry::load(&self.registry) else { return (mstatus::NOT_ALLOWED, "not an app".into()) };
            let Some((owner, app)) = reg.resolve(uid) else { return (mstatus::NOT_ALLOWED, "not an app".into()) };
            if owner != user {
                return (mstatus::NOT_ALLOWED, "not the session's app".into());
            }
            if !has_removable_files(&self.apps_dir, app) {
                return (mstatus::NOT_ALLOWED, format!("{app} does not hold removable-files"));
            }
            let Some(jail) = jail_of_uid(uid) else { return (mstatus::NOT_FOUND, format!("{app} runs in no one jail")) };
            let gid = primary_gid(uid).unwrap_or(uid);
            self.ask(Req::AttachAll(Standing { jail, uid, gid, user }))
        }

        /// The system picker (an app of the session's user holding
        /// `file-picker`) puts the folder the user picked into the jail of
        /// the app it picked for: also an app of that user, in exactly one
        /// jail, not a session component. The grant is the one root makes
        /// (§4.8), at `/media/<part>` in that jail.
        fn picked(&self, uid: u32, p: &atrium_media::Picked) -> (u8, String) {
            let Session::Active { user, .. } = self.session.lock().unwrap().clone() else { return (mstatus::NOT_ALLOWED, "no unlocked session".into()) };
            let Ok(reg) = portcullis_peer::AppRegistry::load(&self.registry) else { return (mstatus::NOT_ALLOWED, "not an app".into()) };
            let Some((owner, app)) = reg.resolve(uid) else { return (mstatus::NOT_ALLOWED, "not an app".into()) };
            if owner != user || !has_cap(&self.apps_dir, app, "file-picker") {
                return (mstatus::NOT_ALLOWED, format!("{app} is not the session's picker"));
            }
            let Some((towner, target)) = reg.resolve(p.app_uid) else { return (mstatus::NOT_FOUND, format!("uid {} is not an app", p.app_uid)) };
            if towner != user || reg.is_session(p.app_uid) {
                return (mstatus::NOT_ALLOWED, format!("{target} is not an app of the session's user"));
            }
            let Some(jail) = jail_of_uid(p.app_uid) else { return (mstatus::NOT_FOUND, format!("{target} runs in no one jail")) };
            let gid = primary_gid(p.app_uid).unwrap_or(p.app_uid);
            eprintln!("atrium-dispensatord: {app} picked {}{} for {target}{}", p.part, p.path, if p.read_only { " (read-only)" } else { "" });
            self.ask(Req::Grant(atrium_media::Grant { part: p.part.clone(), path: p.path.clone(), jail, uid: p.app_uid, gid, read_only: p.read_only }))
        }

        /// Root's own open, on the session's mount: no link followed, no
        /// directory, nothing above the stick (no `..`, checked before).
        fn open(&self, mountpoint: &str, o: &atrium_media::Open) -> Result<std::fs::File, (u8, String)> {
            use std::os::unix::fs::OpenOptionsExt;
            let path = format!("{mountpoint}{}", o.path);
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(o.write)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&path)
                .map_err(|e| (if e.kind() == std::io::ErrorKind::NotFound { mstatus::NOT_FOUND } else { mstatus::FAILED }, format!("{}: {e}", o.path)))?;
            match f.metadata() {
                Ok(m) if m.is_file() => {}
                _ => return Err((mstatus::MALFORMED, format!("{} is not a file", o.path))),
            }
            // Blocking again for the app.
            // SAFETY: fcntl on our own open descriptor.
            unsafe { libc::fcntl(f.as_raw_fd(), libc::F_SETFL, 0) };
            eprintln!("atrium-dispensatord: open {}{} handed over{}", o.part, o.path, if o.write { " (write)" } else { "" });
            Ok(f)
        }
    }

    impl MediaHost {
        /// `uid`'s app id, when it is an app of the session's user holding
        /// `removable-files` (the file manager).
        fn file_manager(&self, uid: u32) -> Option<String> {
            let Session::Active { user, .. } = self.session.lock().unwrap().clone() else { return None };
            let reg = portcullis_peer::AppRegistry::load(&self.registry).ok()?;
            let (owner, app) = reg.resolve(uid)?;
            (owner == user && has_removable_files(&self.apps_dir, app)).then(|| app.to_string())
        }

        /// Hand `r` to the daemon's loop and wait for its answer.
        fn ask(&self, r: Req) -> (u8, String) {
            use std::io::Write;
            let (rtx, rrx) = mpsc::channel();
            if self.tx.lock().unwrap().send((r, rtx)).is_err() {
                return (mstatus::FAILED, "dispensatord is shutting down".into());
            }
            let _ = self.wake.lock().unwrap().write_all(&[1]);
            rrx.recv_timeout(Duration::from_secs(60)).unwrap_or((mstatus::FAILED, "no answer".into()))
        }
    }

    pub fn serve(rules: Rules, jaild: String, state: PathBuf, seat: PathBuf, media_socket: PathBuf, devices_socket: PathBuf) -> ExitCode {
        let reshow = rules.services.values().map(|s| s.reshow_secs).min().unwrap_or(2);
        let reshow = Duration::from_secs(reshow.into());
        eprintln!(
            "atrium-dispensatord: {} rules, {} drivers, {} services",
            rules.rules.len(),
            rules.drivers.len(),
            rules.services.len()
        );
        let wants_media = rules.drivers.values().any(|d| d.fuse_mount.is_some());
        let mut a = Agent {
            jaild,
            state,
            planner: Planner::new(rules),
            drivers: BTreeMap::new(),
            show_errors: BTreeMap::new(),
            media: Media::default(),
            session: Session::None,
            seat,
            shared: None,
            session_shared: Arc::new(Mutex::new(Session::None)),
            ejects: None,
            ejected_now: Vec::new(),
            settling: Vec::new(),
            devices: None,
            controls: BTreeMap::new(),
            grants: Vec::new(),
            next_grant: 0,
            standing: Vec::new(),
        };
        a.session = a.read_session();
        *a.session_shared.lock().unwrap() = a.session.clone();
        if wants_media {
            a.start_media_service(&media_socket);
        }
        a.start_devices_service(&devices_socket);
        eprintln!("atrium-dispensatord: session {:?}", a.session);
        loop {
            // Listen first, then enumerate, so nothing falls between.
            let w = match DeviceWatcher::open() {
                Ok(w) => w,
                Err(e) => {
                    eprintln!("atrium-dispensatord: devd: {e}; retrying in 5 s");
                    std::thread::sleep(Duration::from_secs(5));
                    continue;
                }
            };
            match std::process::Command::new("devinfo").arg("-v").output() {
                Ok(o) if o.status.success() => {
                    let ops = a.planner.devinfo(&String::from_utf8_lossy(&o.stdout));
                    a.run(ops);
                }
                other => eprintln!("atrium-dispensatord: devinfo -v: {other:?}"),
            }
            let ops = a.planner.usb_present(&usb_dumps());
            a.run(ops);
            let dev: Vec<String> = std::fs::read_dir("/dev").map(|d| d.filter_map(|e| e.ok()?.file_name().into_string().ok()).collect()).unwrap_or_default();
            let ops = a.planner.disks_present(dev.iter().map(String::as_str));
            a.run(ops);
            a.serve(&w, reshow);
            eprintln!("atrium-dispensatord: devd connection lost; reconnecting");
        }
    }

    /// Every USB device's descriptors as `usbconfig` prints them: USB
    /// interfaces no kernel driver claimed are not in `devinfo -v`.
    fn usb_dumps() -> Vec<(String, String)> {
        let run = |args: &[&str]| {
            std::process::Command::new("usbconfig").args(args).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        };
        let Some(list) = run(&[]) else { return Vec::new() };
        dispensator::plan::parse_usbconfig_list(&list)
            .into_iter()
            .filter_map(|u| run(&["-d", &u, "dump_device_desc", "dump_curr_config_desc"]).map(|t| (u, t)))
            .collect()
    }

    /// A service socket: in a directory of its own, so it can be mounted
    /// into Forum's jail; any local user may connect, the service checks who.
    fn socket_listener(path: &Path) -> Option<UnixListener> {
        use std::os::unix::fs::PermissionsExt;
        let dir = path.parent()?;
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::remove_file(path);
        match UnixListener::bind(path) {
            Ok(l) => {
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666));
                Some(l)
            }
            Err(e) => {
                eprintln!("atrium-dispensatord: socket {}: {e}", path.display());
                None
            }
        }
    }

    /// Size and free space of a mount, if it is a live fusefs mount.
    fn fuse_space(mnt: &str) -> Option<(u64, u64)> {
        let c = std::ffi::CString::new(mnt).ok()?;
        // SAFETY: a zeroed statfs is a valid out-parameter.
        let mut st: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: a valid path and out-parameter.
        if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
            return None;
        }
        // SAFETY: f_fstypename and f_mntonname are NUL-terminated by the kernel.
        let ty = unsafe { std::ffi::CStr::from_ptr(st.f_fstypename.as_ptr()) };
        let on = unsafe { std::ffi::CStr::from_ptr(st.f_mntonname.as_ptr()) };
        (ty.to_bytes() == b"fusefs" && on.to_bytes() == mnt.as_bytes())
            .then(|| (st.f_blocks * st.f_bsize, st.f_bavail.max(0) as u64 * st.f_bsize))
    }

    /// A user name to (uid, gid), from the password database.
    fn lookup_user(name: &str) -> Option<(u32, u32)> {
        let c = std::ffi::CString::new(name).ok()?;
        // SAFETY: c is a valid NUL-terminated name; the result points into
        // libc's static buffer and is read at once on this thread.
        let pw = unsafe { libc::getpwnam(c.as_ptr()) };
        if pw.is_null() {
            return None;
        }
        // SAFETY: non-null, as checked.
        unsafe { Some(((*pw).pw_uid, (*pw).pw_gid)) }
    }

    impl Agent {
        fn read_session(&self) -> Session {
            dispensator::media::read_session(&self.seat, lookup_user)
        }

        /// Re-read the session; on a change, make the media servers follow.
        fn follow_session(&mut self) {
            let s = self.read_session();
            if s != self.session {
                eprintln!("atrium-dispensatord: session {:?} -> {:?}", self.session, s);
                self.session = s;
                *self.session_shared.lock().unwrap() = self.session.clone();
                let acts = self.media.sync(&self.session);
                self.act(acts);
                self.publish();
            }
        }

        fn act(&mut self, acts: Vec<Act>) {
            for a in acts {
                match a {
                    Act::Start { driver, device, vars, nodes, label } => {
                        let inst: Vars = vars.iter().filter(|(k, _)| k.as_str() == "part").map(|(k, v)| (k.clone(), v.clone())).collect();
                        self.stop(&driver, &device, &inst);
                        self.start(&driver, &device, &vars, &nodes, &label);
                    }
                    Act::Stop { driver, device, vars } => self.stop(&driver, &device, &vars),
                }
            }
        }

        fn start_media_service(&mut self, sock: &Path) {
            let Some(l) = socket_listener(sock) else { return };
            let Ok((wake_rd, wake_wr)) = UnixStream::pair() else { return };
            let _ = wake_rd.set_nonblocking(true);
            let (tx, rx) = mpsc::channel();
            let host = MediaHost {
                session: self.session_shared.clone(),
                registry: portcullis_peer::DEFAULT_REGISTRY.into(),
                apps_dir: std::env::var("DISPENSATOR_APPS_DIR").unwrap_or_else(|_| "/var/lib/atrium/apps".into()),
                tx: Mutex::new(tx),
                wake: Mutex::new(wake_wr),
            };
            let shared = mserver::Shared::new(Box::new(host));
            let sh = shared.clone();
            std::thread::spawn(move || {
                for c in l.incoming() {
                    let Ok(c) = c else { continue };
                    let Ok((uid, _)) = portcullis_peer::uid_gid(c.as_raw_fd()) else { continue };
                    let sh = sh.clone();
                    std::thread::spawn(move || {
                        if let Ok(conn) = aqueduct::Connection::wrap(c) {
                            let _ = mserver::serve(conn, uid, sh);
                        }
                    });
                }
            });
            eprintln!("atrium-dispensatord: media service on {}", sock.display());
            self.shared = Some(shared);
            self.ejects = Some((rx, wake_rd));
        }

        fn start_devices_service(&mut self, sock: &Path) {
            let Some(l) = socket_listener(sock) else { return };
            let host = DevicesHost { session: self.session_shared.clone(), registry: portcullis_peer::DEFAULT_REGISTRY.into() };
            let shared = dserver::Shared::new(Box::new(host));
            let sh = shared.clone();
            std::thread::spawn(move || {
                for c in l.incoming() {
                    let Ok(c) = c else { continue };
                    let Ok((uid, _)) = portcullis_peer::uid_gid(c.as_raw_fd()) else { continue };
                    let sh = sh.clone();
                    std::thread::spawn(move || {
                        if let Ok(conn) = aqueduct::Connection::wrap(c) {
                            let _ = dserver::serve(conn, uid, sh);
                        }
                    });
                }
            });
            eprintln!("atrium-dispensatord: device list on {}", sock.display());
            self.devices = Some(shared);
        }

        /// Tell the device list what is present and who has it.
        fn publish_devices(&self) {
            let Some(shared) = &self.devices else { return };
            let served: Vec<&str> = self.media.served().collect();
            shared.update(self.planner.inventory(|driver, device| self.drivers.contains_key(&(driver.to_string(), device.to_string())) || served.contains(&device)));
        }

        /// Tell the media service what is served now.
        fn publish(&mut self) {
            let Some(shared) = self.shared.clone() else { return };
            let parts: Vec<String> = self.media.served().map(String::from).collect();
            let mut vols = Vec::new();
            for part in parts {
                let Some(mnt) = self.mountpoint(&part) else { continue };
                // A server that exited (not FAT, say) leaves no live mount.
                let Some((size_bytes, free_bytes)) = fuse_space(&mnt) else { continue };
                let read_only = self.media.driver_of(&part).and_then(|d| self.planner.rules().drivers.get(d)).is_some_and(|d| d.fuse_read_only);
                vols.push(Volume { part, mountpoint: mnt, size_bytes, free_bytes, read_only });
            }
            shared.update(vols, &std::mem::take(&mut self.ejected_now));
        }

        /// Eject requests the connection threads handed over.
        fn take_ejects(&mut self) {
            use std::io::Read;
            let mut reqs = Vec::new();
            if let Some((rx, wake)) = &mut self.ejects {
                let mut b = [0u8; 64];
                while matches!(wake.read(&mut b), Ok(n) if n > 0) {}
                while let Ok(r) = rx.try_recv() {
                    reqs.push(r);
                }
            }
            for (r, reply) in reqs {
                let a = match r {
                    Req::Eject(part) => self.eject(&part),
                    Req::Grant(g) => self.grant(&g),
                    Req::Revoke(jail) => {
                        self.standing.retain(|s| s.jail != jail);
                        let n = self.end_grants(|g| g.jail == jail);
                        (mstatus::OK, format!("{n} grants into {jail} ended"))
                    }
                    Req::AttachAll(s) => self.attach_all(s),
                };
                let _ = reply.send(a);
            }
        }

        /// Put a folder of a served stick into an app's jail (§4.8): a new
        /// FUSE mount of the stick's own server, rooted there, owned by the
        /// app, nullfs'd in by jaild.
        fn grant(&mut self, g: &atrium_media::Grant) -> (u8, String) {
            use std::io::BufRead;
            if !matches!(self.session, Session::Active { .. }) {
                return (mstatus::NOT_ALLOWED, "no unlocked session".into());
            }
            let Some(ctl) = self.controls.get(&g.part) else { return (mstatus::NOT_FOUND, format!("{} has no server taking grants", g.part)) };
            if !jail_alive(&g.jail) {
                return (mstatus::NOT_FOUND, format!("no jail {}", g.jail));
            }
            let dest = format!("/media/{}", g.part);
            if self.grants.iter().any(|x| x.jail == g.jail && x.dest == dest) {
                return (mstatus::BUSY, format!("{} already has {dest}", g.jail));
            }
            let read_only = g.read_only || self.media.driver_of(&g.part).and_then(|d| self.planner.rules().drivers.get(d)).is_some_and(|d| d.fuse_read_only);
            let top = Path::new("/media/.grants");
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::create_dir_all(top);
                let _ = std::fs::set_permissions(top, std::fs::Permissions::from_mode(0o711));
            }
            self.next_grant += 1;
            let dir = top.join(self.next_grant.to_string());
            let _ = std::fs::create_dir(&dir);
            let fail = |dir: &Path, m: String| {
                unmount_force(dir);
                let _ = std::fs::remove_dir(dir);
                eprintln!("atrium-dispensatord: grant {} {} -> {}: {m}", g.part, g.path, g.jail);
                (mstatus::FAILED, m)
            };
            let fuse = match std::fs::OpenOptions::new().read(true).write(true).open("/dev/fuse") {
                Ok(f) => f,
                Err(e) => return fail(&dir, format!("/dev/fuse: {e}")),
            };
            if let Err(e) = jaild::ffi::fusefs_mount(&dir.to_string_lossy(), fuse.as_raw_fd(), read_only, &format!("capsa-grant:{}", g.part)) {
                return fail(&dir, format!("mount fusefs: {e}"));
            }
            let line = format!("mount {} {} {} {}\n", g.uid, g.gid, u8::from(read_only), g.path);
            if let Err(e) = send_with_fd(ctl, &line, fuse.as_raw_fd()) {
                return fail(&dir, format!("the server: {e}"));
            }
            drop(fuse);
            let mut reply = String::new();
            let read = ctl.try_clone().and_then(|c| {
                c.set_read_timeout(Some(Duration::from_secs(10)))?;
                std::io::BufReader::new(c).read_line(&mut reply)
            });
            match read {
                Ok(_) if reply.trim_end() == "ok" => {}
                Ok(_) => return fail(&dir, reply.trim_end().trim_start_matches("error ").to_string()),
                Err(e) => return fail(&dir, format!("the server: {e}")),
            }
            let attach = Request::AttachMount(jaild::protocol::AttachMountRequest {
                jail_name: g.jail.clone(),
                source: dir.to_string_lossy().into_owned(),
                dest: dest.clone(),
                mount_kind: MountKind::RwNullfs,
                size_mb: None,
            });
            match Client::connect(&self.jaild).and_then(|mut c| c.send(&attach)) {
                Ok((Response::Ok, _)) => {}
                Ok((other, _)) => return fail(&dir, format!("jaild: {other:?}")),
                Err(e) => return fail(&dir, format!("jaild: {e}")),
            }
            eprintln!("atrium-dispensatord: grant {}{} -> {} at {dest} (uid {}{})", g.part, g.path, g.jail, g.uid, if read_only { ", ro" } else { "" });
            self.grants.push(GrantRec { part: g.part.clone(), jail: g.jail.clone(), dir, dest: dest.clone() });
            (mstatus::OK, dest)
        }

        /// A file manager asks for every stick: now and each that comes.
        fn attach_all(&mut self, s: Standing) -> (u8, String) {
            if !jail_alive(&s.jail) {
                return (mstatus::NOT_FOUND, format!("no jail {}", s.jail));
            }
            let jail = s.jail.clone();
            if !self.standing.contains(&s) {
                self.standing.push(s);
            }
            let n = self.sync_standing();
            let have = self.grants.iter().filter(|g| g.jail == jail).count();
            (mstatus::OK, format!("{have} sticks in {jail} ({n} new); more as they come"))
        }

        /// Give each standing file manager every served stick it lacks;
        /// forget those whose jail went or whose user's session is not the
        /// active one. Grants are made only while the session is unlocked
        /// (`grant` checks), so a lock ends them and an unlock brings them
        /// back to the same jail.
        fn sync_standing(&mut self) -> usize {
            let user = match &self.session {
                Session::Active { user, .. } => Some(user.clone()),
                _ => None,
            };
            let locked_same = |s: &Standing| matches!(&self.session, Session::Locked { user } if *user == s.user);
            let keep: Vec<Standing> = self.standing.iter().filter(|s| jail_alive(&s.jail) && (user.as_deref() == Some(s.user.as_str()) || locked_same(s))).cloned().collect();
            self.standing = keep;
            if user.is_none() {
                return 0;
            }
            let mut made = 0;
            let parts: Vec<String> = self.controls.keys().cloned().collect();
            for s in self.standing.clone() {
                for part in &parts {
                    let dest = format!("/media/{part}");
                    if self.grants.iter().any(|g| g.jail == s.jail && g.dest == dest) {
                        continue;
                    }
                    let g = atrium_media::Grant { part: part.clone(), path: "/".into(), jail: s.jail.clone(), uid: s.uid, gid: s.gid, read_only: false };
                    if self.grant(&g).0 == mstatus::OK {
                        made += 1;
                    }
                }
            }
            made
        }

        /// End the grants `f` picks: out of the jail, then the mount.
        fn end_grants(&mut self, f: impl Fn(&GrantRec) -> bool) -> usize {
            let (gone, keep): (Vec<GrantRec>, Vec<GrantRec>) = std::mem::take(&mut self.grants).into_iter().partition(|g| f(g));
            self.grants = keep;
            for g in &gone {
                if jail_alive(&g.jail) {
                    let d = Request::DetachMount(jaild::protocol::DetachMountRequest { jail_name: g.jail.clone(), dest: g.dest.clone(), force: true });
                    let _ = Client::connect(&self.jaild).and_then(|mut c| c.send(&d));
                }
                unmount_force(&g.dir);
                let _ = std::fs::remove_dir(&g.dir);
                eprintln!("atrium-dispensatord: grant {} -> {} ended", g.part, g.jail);
            }
            gone.len()
        }

        fn mountpoint(&self, device: &str) -> Option<String> {
            let d = self.planner.rules().drivers.get(self.media.driver_of(device)?)?;
            dispensator::rules::fill(d.fuse_mount.as_deref()?, &[("part", Some(device.to_string()))])
        }

        /// Unmount without force first: a file still open fails the eject
        /// instead of losing what the program has not written yet. Then the
        /// server stops (its final writes are done) and stays down until the
        /// stick is unplugged.
        fn eject(&mut self, device: &str) -> (u8, String) {
            let Some(mnt) = self.mountpoint(device) else { return (mstatus::NOT_FOUND, format!("{device} is not mounted")) };
            if let Ok(c) = std::ffi::CString::new(mnt.as_str()) {
                // SAFETY: a valid NUL-terminated path.
                if unsafe { libc::unmount(c.as_ptr(), 0) } != 0 {
                    let e = std::io::Error::last_os_error();
                    if e.raw_os_error() == Some(libc::EBUSY) {
                        return (mstatus::BUSY, format!("{mnt} is busy: close the files open on it first"));
                    }
                    if e.raw_os_error() != Some(libc::EINVAL) {
                        return (mstatus::FAILED, format!("unmount {mnt}: {e}"));
                    }
                }
            }
            self.end_grants(|g| g.part == device);
            let (_, acts) = self.media.eject(device, &self.session);
            self.act(acts);
            self.ejected_now.push(device.to_string());
            self.publish();
            eprintln!("atrium-dispensatord: {device} ejected");
            (mstatus::OK, format!("{device} ejected; it can be unplugged"))
        }

        fn serve(&mut self, w: &DeviceWatcher, reshow: Duration) {
            let mut last = Instant::now();
            loop {
                let mut p = [
                    libc::pollfd { fd: w.as_raw_fd(), events: libc::POLLIN, revents: 0 },
                    libc::pollfd { fd: self.ejects.as_ref().map_or(-1, |(_, w)| w.as_raw_fd()), events: libc::POLLIN, revents: 0 },
                ];
                // SAFETY: two valid pollfds (a negative fd is ignored).
                let wait = if self.settling.is_empty() { reshow } else { Duration::from_millis(500).min(reshow) };
                let r = unsafe { libc::poll(p.as_mut_ptr(), 2, wait.as_millis() as libc::c_int) };
                if r < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                    return;
                }
                if r > 0 && p[1].revents != 0 {
                    self.take_ejects();
                }
                let now = Instant::now();
                let due: Vec<String> = self.settling.iter().filter(|(t, _)| *t <= now).map(|(_, d)| d.clone()).collect();
                self.settling.retain(|(t, _)| *t > now);
                for d in due {
                    let ops = self.planner.settle(&d);
                    self.run(ops);
                }
                if r > 0 && p[0].revents != 0 {
                    match w.recv_line() {
                        Ok(line) => {
                            let ops = self.planner.line(&line);
                            self.run(ops);
                        }
                        Err(_) => return,
                    }
                }
                if last.elapsed() >= reshow {
                    last = Instant::now();
                    // A grant lasts as long as its app's jail (one shot).
                    self.end_grants(|g| !jail_alive(&g.jail));
                    self.follow_session();
                    self.sync_standing();
                    self.publish();
                    self.publish_devices();
                    let mut ops = Vec::new();
                    for s in self.planner.rules().services.keys() {
                        ops.extend(self.planner.service_nodes(s).map(|n| Op::Show { service: s.clone(), node: n.to_string() }));
                    }
                    self.run(ops);
                }
            }
        }

        fn run(&mut self, ops: Vec<Op>) {
            let changed = ops.iter().any(|o| !matches!(o, Op::Show { .. }));
            for op in ops {
                match op {
                    Op::Show { service, node } => self.show(&service, &node),
                    Op::StartDriver { driver, device, vars, nodes, label } if self.is_media(&driver) => {
                        let acts = self.media.add(&driver, &device, vars, nodes, label, &self.session);
                        self.act(acts);
                    }
                    Op::StopDriver { driver, device, .. } if self.media.has(&driver, &device) => {
                        let acts = self.media.remove(&driver, &device);
                        self.act(acts);
                    }
                    Op::StartDriver { driver, device, vars, nodes, label } => {
                        let inst: Vars = vars.iter().filter(|(k, _)| dispensator::rules::INSTANCE_VARS.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
                        self.stop(&driver, &device, &inst);
                        self.start(&driver, &device, &vars, &nodes, &label);
                    }
                    Op::DetachKernel { ugen, interface } => Self::detach_kernel(&ugen, interface),
                    Op::StopDriver { driver, device, vars } => self.stop(&driver, &device, &vars),
                    Op::Log(m) => eprintln!("atrium-dispensatord: {m}"),
                    Op::Settle { disk } => self.settling.push((Instant::now() + Duration::from_millis(1500), disk)),
                }
            }
            if changed {
                self.publish_devices();
                let tmp = self.state.with_extension("tmp");
                if std::fs::write(&tmp, self.planner.state()).is_ok() {
                    let _ = std::fs::rename(&tmp, &self.state);
                }
            }
        }

        fn is_media(&self, driver: &str) -> bool {
            self.planner.rules().drivers.get(driver).is_some_and(|d| d.fuse_mount.is_some())
        }

        fn show(&mut self, service: &str, node: &str) {
            let Some(s) = self.planner.rules().services.get(service) else { return };
            let dev = Path::new(&s.devfs).to_path_buf();
            let key = (service.to_string(), node.to_string());
            // Already visible there (a hidden node does not resolve), or no
            // host node at all (unplugged meanwhile): nothing to do.
            if dev.join(node).exists() || !Path::new("/dev").join(node).exists() {
                self.show_errors.remove(&key);
                return;
            }
            let out = std::process::Command::new("devfs")
                .args(["-m", &s.devfs, "rule", "apply", "path", node, "unhide"])
                .output();
            let err = match out {
                Ok(o) if o.status.success() => None,
                Ok(o) => Some(String::from_utf8_lossy(&o.stderr).trim().to_string()),
                Err(e) => Some(e.to_string()),
            };
            match err {
                None => {
                    self.show_errors.remove(&key);
                    eprintln!("atrium-dispensatord: {node} shown to {service}");
                }
                Some(e) => {
                    if self.show_errors.get(&key) != Some(&e) {
                        eprintln!("atrium-dispensatord: show {node} to {service}: {e} (is {service} running?)");
                        self.show_errors.insert(key, e);
                    }
                }
            }
        }

        fn request(p: &JailPlan) -> CreateJailRequest {
            let ro = |m: &str| MountSpec {
                source: m.to_string(),
                dest: m.trim_start_matches('/').to_string(),
                kind: MountKind::RoNullfs,
                size_mb: None,
                owner: None,
            };
            CreateJailRequest {
                rctl: None,
                name: p.name.clone(),
                path: p.path.clone(),
                children_max: 0,
                devfs_ruleset: p.devfs_ruleset,
                network: NetworkConfig::Disable,
                mounts: p.mounts_ro.iter().map(|m| ro(m)).collect(),
                exec: Some(ExecSpec { path: p.exec.clone(), argv: p.argv.clone(), env: vec![], uid: p.uid, gid: p.gid, stdio: false }),
                hostname: None,
                hostid: None,
                hostuuid: None,
                allow_mlock: false,
                syscall_profile: Some(p.sysfil.clone()),
                devfs_grant: p.devfs_grants.first().cloned(),
                devfs_grants: p.devfs_grants.iter().skip(1).cloned().collect(),
                fuse: p.fuse.as_ref().map(|(device, mountpoint, read_only)| FuseServe {
                    device: device.clone(),
                    mountpoint: mountpoint.clone(),
                    read_only: *read_only,
                    control: p.argv.windows(2).any(|w| w == ["--control-fd", "5"]),
                }),
            }
        }

        /// Unmount what an earlier run left: jaild's exec path does not
        /// record create-time mounts, and a driver jail ended by a restart
        /// of this daemon (closing its procdesc) leaves them.
        fn unmount_all(path: &str, mounts: &[String]) {
            for m in mounts {
                let target = Path::new(path).join(m.trim_start_matches('/'));
                if let Ok(c) = std::ffi::CString::new(target.to_string_lossy().as_bytes()) {
                    // SAFETY: a valid NUL-terminated path; failure (not
                    // mounted) is expected and ignored.
                    unsafe { libc::unmount(c.as_ptr(), 0) };
                }
            }
        }

        /// The planner checked `ugen` (`ugenB.A`); the interface is a number.
        fn detach_kernel(ugen: &str, interface: u8) {
            let out = std::process::Command::new("usbconfig")
                .args(["-d", ugen, "-i", &interface.to_string(), "detach_kernel_driver"])
                .output();
            match out {
                Ok(o) if o.status.success() => eprintln!("atrium-dispensatord: {ugen}:{interface}: kernel driver detached"),
                Ok(o) => eprintln!("atrium-dispensatord: {ugen}:{interface}: detach_kernel_driver: {}", String::from_utf8_lossy(&o.stderr).trim()),
                Err(e) => eprintln!("atrium-dispensatord: usbconfig: {e}"),
            }
        }

        fn start(&mut self, driver: &str, device: &str, vars: &Vars, nodes: &[String], label: &str) {
            let node = nodes.join(",");
            let Some(d) = self.planner.rules().drivers.get(driver) else { return };
            let Some(p) = plan(d, vars, nodes) else {
                eprintln!("atrium-dispensatord: {driver}: {device}: a template has no value");
                return;
            };
            Self::unmount_all(&p.path, &p.mounts_ro);
            // GEOM's CREATE can come a moment before devfs has the node.
            if let Some((dev, ..)) = &p.fuse {
                let node = Path::new("/dev").join(dev);
                for _ in 0..20 {
                    if node.exists() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            // A stick's server takes grants on fd 5, a socket to us (§4.8).
            let req = Self::request(&p);
            let sent = if req.fuse.as_ref().is_some_and(|f| f.control) {
                match UnixStream::pair() {
                    Ok((ours, theirs)) => {
                        use std::os::fd::AsFd;
                        let r = Client::connect(&self.jaild).and_then(|mut c| c.send_with_fds(&Request::CreateJail(req), &[theirs.as_fd()], 1));
                        if matches!(r, Ok((Response::JailCreated(_), _))) {
                            self.controls.insert(device.to_string(), ours);
                        }
                        r.map(|(resp, fds)| (resp, fds.first().copied()))
                    }
                    Err(e) => Err(e),
                }
            } else {
                Client::connect(&self.jaild).and_then(|mut c| c.send(&Request::CreateJail(req)))
            };
            match sent {
                Ok((Response::JailCreated(r), fd)) => {
                    eprintln!(
                        "atrium-dispensatord: {device} ({label}) -> {} via {node} (jid {}, pid {})",
                        p.name, r.jid, r.pid
                    );
                    if let Some(raw) = fd {
                        // SAFETY: jaild just passed us this procdesc and we own
                        // it; holding it keeps the driver's persist=0 jail alive.
                        self.drivers.insert((driver.to_string(), device.to_string()), unsafe { OwnedFd::from_raw_fd(raw) });
                    }
                }
                Ok((other, _)) => eprintln!("atrium-dispensatord: {}: jaild: {other:?}", p.name),
                Err(e) => eprintln!("atrium-dispensatord: {}: jaild: {e}", p.name),
            }
        }

        fn stop(&mut self, driver: &str, device: &str, vars: &Vars) {
            let Some(d) = self.planner.rules().drivers.get(driver) else { return };
            let Some((name, path)) = stop_names(d, vars) else { return };
            let mounts = d.mounts_ro.clone();
            let had = self.drivers.remove(&(driver.to_string(), device.to_string())).is_some();
            self.end_grants(|g| g.part == device);
            self.controls.remove(device);
            let rm = Request::RemoveJail { jid: None, name: Some(name.clone()) };
            let _ = Client::connect(&self.jaild).and_then(|mut c| c.send(&rm));
            Self::unmount_all(&path, &mounts);
            if had {
                eprintln!("atrium-dispensatord: {name} removed");
            }
        }
    }
}
