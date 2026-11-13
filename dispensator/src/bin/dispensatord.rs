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
//! ```
//!
//! The media socket takes one line, `list` or `eject PART`, from root or the
//! active session's user, and answers `ok ...` or `error ...`.

use std::path::PathBuf;
use std::process::ExitCode;

use dispensator::plan::Planner;
use dispensator::rules::Rules;

const RULES_DIR: &str = "/usr/local/etc/atrium/devices.d";
const JAILD_SOCKET: &str = "/var/run/atrium/jaild.sock";
const STATE_FILE: &str = "/var/run/atrium/dispensator.state";
const MEDIA_SOCKET: &str = "/atrium/sockets/dispensatord/media.sock";

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
            "--eject" => a.media_cmd = Some(format!("eject {}", val()?)),
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
            eprintln!("usage: atrium-dispensatord [--rules DIR] [--jaild-socket PATH] [--state FILE] | --check [--rules DIR] | --plan FILE [--devinfo FILE] [--rules DIR]");
            return ExitCode::from(2);
        }
    };
    if let Some(cmd) = &a.media_cmd {
        return media_client(&a.media_socket, cmd);
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
        return ExitCode::SUCCESS;
    }
    run::serve(rules, a.jaild, a.state, a.seat, a.media_socket)
}

/// One request to the daemon's media socket; prints its answer.
fn media_client(sock: &std::path::Path, cmd: &str) -> ExitCode {
    use std::io::Write;
    let mut s = match std::os::unix::net::UnixStream::connect(sock) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("atrium-dispensatord: {}: {e}", sock.display());
            return ExitCode::FAILURE;
        }
    };
    let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(30)));
    if writeln!(s, "{cmd}").is_err() {
        return ExitCode::FAILURE;
    }
    let mut out = String::new();
    let _ = std::io::Read::read_to_string(&mut s, &mut out);
    print!("{out}");
    if out.starts_with("ok") { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}


#[cfg(not(target_os = "freebsd"))]
mod run {
    use super::*;
    pub fn serve(_: Rules, _: String, _: PathBuf, _: PathBuf, _: PathBuf) -> ExitCode {
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
    use std::os::unix::net::UnixListener;
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
    }

    pub fn serve(rules: Rules, jaild: String, state: PathBuf, seat: PathBuf, media_socket: PathBuf) -> ExitCode {
        let reshow = rules.services.values().map(|s| s.reshow_secs).min().unwrap_or(2);
        let reshow = Duration::from_secs(reshow.into());
        eprintln!(
            "atrium-dispensatord: {} rules, {} drivers, {} services",
            rules.rules.len(),
            rules.drivers.len(),
            rules.services.len()
        );
        let media = if rules.drivers.values().any(|d| d.fuse_mount.is_some()) { media_listener(&media_socket) } else { None };
        let mut a = Agent { jaild, state, planner: Planner::new(rules), drivers: BTreeMap::new(), show_errors: BTreeMap::new(), media: Media::default(), session: Session::None, seat };
        a.session = a.read_session();
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
            a.serve(&w, media.as_ref(), reshow);
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

    /// The media socket: in a directory of its own, so it can be mounted
    /// into the session; any local user may connect, the daemon checks who.
    fn media_listener(path: &Path) -> Option<UnixListener> {
        use std::os::unix::fs::PermissionsExt;
        let dir = path.parent()?;
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::remove_file(path);
        match UnixListener::bind(path) {
            Ok(l) => {
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666));
                let _ = l.set_nonblocking(true);
                Some(l)
            }
            Err(e) => {
                eprintln!("atrium-dispensatord: media socket {}: {e}", path.display());
                None
            }
        }
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
                let acts = self.media.sync(&self.session);
                self.act(acts);
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

        /// One request on the media socket.
        fn media_request(&mut self, l: &UnixListener) {
            use std::io::{BufRead, BufReader, Write};
            let Ok((mut c, _)) = l.accept() else { return };
            let _ = c.set_nonblocking(false);
            let _ = c.set_read_timeout(Some(Duration::from_secs(2)));
            let (mut uid, mut gid) = (u32::MAX, u32::MAX);
            // SAFETY: a connected socket and two out-parameters.
            if unsafe { libc::getpeereid(c.as_raw_fd(), &mut uid, &mut gid) } != 0 {
                return;
            }
            let mut line = String::new();
            if BufReader::new(std::io::Read::take(&c, 256)).read_line(&mut line).is_err() {
                return;
            }
            let allowed = uid == 0 || matches!(&self.session, Session::Active { uid: u, .. } if *u == uid);
            let reply = if !allowed {
                "error only the active session's user may do that".to_string()
            } else {
                match line.trim().split_once(' ').unwrap_or((line.trim(), "")) {
                    ("list", "") => {
                        let mut r = String::from("ok");
                        for d in self.media.served() {
                            r.push_str(&format!("\n{d} {}", self.mountpoint(d).unwrap_or_default()));
                        }
                        r
                    }
                    ("eject", dev) => self.eject(dev),
                    _ => "error list | eject PART".into(),
                }
            };
            let _ = writeln!(c, "{reply}");
        }

        fn mountpoint(&self, device: &str) -> Option<String> {
            let d = self.planner.rules().drivers.get(self.media.driver_of(device)?)?;
            dispensator::rules::fill(d.fuse_mount.as_deref()?, &[("part", Some(device.to_string()))])
        }

        /// Unmount without force first: a file still open fails the eject
        /// instead of losing what the program has not written yet. Then the
        /// server stops (its final writes are done) and stays down until the
        /// stick is unplugged.
        fn eject(&mut self, device: &str) -> String {
            let Some(mnt) = self.mountpoint(device) else { return format!("error {device} is not served") };
            if let Ok(c) = std::ffi::CString::new(mnt.as_str()) {
                // SAFETY: a valid NUL-terminated path.
                if unsafe { libc::unmount(c.as_ptr(), 0) } != 0 {
                    let e = std::io::Error::last_os_error();
                    if e.raw_os_error() == Some(libc::EBUSY) {
                        return format!("error {mnt} is busy: close the files open on it first");
                    }
                    if e.raw_os_error() != Some(libc::EINVAL) {
                        return format!("error unmount {mnt}: {e}");
                    }
                }
            }
            let (_, acts) = self.media.eject(device, &self.session);
            self.act(acts);
            eprintln!("atrium-dispensatord: {device} ejected");
            format!("ok {device} ejected; it can be unplugged")
        }

        fn serve(&mut self, w: &DeviceWatcher, media: Option<&UnixListener>, reshow: Duration) {
            let mut last = Instant::now();
            loop {
                let mut p = [
                    libc::pollfd { fd: w.as_raw_fd(), events: libc::POLLIN, revents: 0 },
                    libc::pollfd { fd: media.map_or(-1, |l| l.as_raw_fd()), events: libc::POLLIN, revents: 0 },
                ];
                // SAFETY: two valid pollfds (a negative fd is ignored).
                let r = unsafe { libc::poll(p.as_mut_ptr(), 2, reshow.as_millis() as libc::c_int) };
                if r < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                    return;
                }
                if r > 0 && p[1].revents != 0 {
                    if let Some(l) = media {
                        self.media_request(l);
                    }
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
                    self.follow_session();
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
                }
            }
            if changed {
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
            let sent = Client::connect(&self.jaild).and_then(|mut c| c.send(&Request::CreateJail(Self::request(&p))));
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
            let rm = Request::RemoveJail { jid: None, name: Some(name.clone()) };
            let _ = Client::connect(&self.jaild).and_then(|mut c| c.send(&rm));
            Self::unmount_all(&path, &mounts);
            if had {
                eprintln!("atrium-dispensatord: {name} removed");
            }
        }
    }
}
