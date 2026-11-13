//! The daemon around [`Broker`] (sigillum.md §7): three sockets, one owner.
//!
//! - **Clients** (`/atrium/sockets/sigillum/sigillumd.sock`, mode 0666):
//!   anyone may connect; the label comes from the kernel's peer uid through
//!   the [`Registry`], once, at accept.
//! - **Drivers** (`/atrium/sockets/sigillum-dev/drivers.sock`): only peer
//!   uid `_sigillum-dev`. Each driver jail sees this one directory,
//!   read-only, connects once and enters Capsicum. A driver's first record must be `Hello`, and only
//!   one `Hello` per connection; anything else drops it.
//! - **The trusted UI** (`/atrium/sockets/sigillum-ui/ui.sock`): only peer
//!   uid `_sigillum-ui`, one connection at a time. With no UI connected a
//!   ceremony can't show its sheet and ends at once (`NotAllowed`); when the
//!   UI goes away every ceremony ends.
//!
//! Each connection has a reader thread that decodes capped messages and
//! sends them to one channel; the thread running [`Daemon::run`] owns the
//! broker and every write. A peer that sends a malformed or oversized
//! message is dropped. Writes have a timeout, so a client that stops
//! reading can't stall the broker; it is dropped too.

use std::collections::BTreeMap;
use std::io;
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sigillum_proto::client::{self, Request};
use sigillum_proto::driver::{self, DeviceClass, FromDriver};
use sigillum_proto::ui::{self, DeviceView, FromUi, Sheet, ToUi};
use sigillum_proto::{read_msg, write_msg};

use crate::broker::{Action, Broker, Config, ConnId, DeviceId, Key};
use crate::label::{Label, Registry};

pub const MAX_CLIENTS: usize = 64;
pub const MAX_DRIVERS: usize = 8;
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const TICK: Duration = Duration::from_millis(250);

/// Checks a passcode typed on a verify sheet. S4 wires ostiarius; until
/// then [`NoPasscode`] refuses every one, so `VerifyUser` succeeds only
/// with a method the broker verifies itself.
pub trait Passcode: Send {
    fn check(&mut self, user: &str, passcode: &[u8]) -> bool;
}

pub struct NoPasscode;

impl Passcode for NoPasscode {
    fn check(&mut self, _: &str, _: &[u8]) -> bool {
        false
    }
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub client_socket: PathBuf,
    pub driver_socket: PathBuf,
    pub ui_socket: PathBuf,
    /// `_sigillum-dev` (65029): the only uid served on the driver socket.
    pub dev_uid: u32,
    /// `_sigillum-ui` (65030): the only uid served on the UI socket.
    pub ui_uid: u32,
    pub broker: Config,
}

impl Settings {
    pub fn standard(broker: Config) -> Settings {
        Settings {
            client_socket: client::SOCKET.into(),
            driver_socket: driver::SOCKET.into(),
            ui_socket: ui::SOCKET.into(),
            dev_uid: 65029,
            ui_uid: 65030,
            broker,
        }
    }
}

enum Ev {
    Client(UnixStream, Label, u32),
    ClientReq(ConnId, Request),
    ClientGone(ConnId),
    Driver(UnixStream, u32),
    DriverRec(DeviceId, FromDriver),
    DriverGone(DeviceId),
    Ui(UnixStream, u32),
    UiMsg(u64, FromUi),
    UiGone(u64),
}

struct ClientConn {
    stream: UnixStream,
    label: Label,
}

struct DriverConn {
    stream: UnixStream,
    hello: bool,
}

pub struct Daemon {
    broker: Broker,
    tx: Sender<Ev>,
    rx: Receiver<Ev>,
    clients: BTreeMap<ConnId, ClientConn>,
    drivers: BTreeMap<DeviceId, DriverConn>,
    ui: Option<(u64, UnixStream)>,
    next: u64,
    started: Instant,
    passcode: Box<dyn Passcode>,
    platform: Box<dyn crate::platform::Platform>,
    /// The verify ceremony waiting for a passcode, and its user.
    asked_passcode: Option<(Key, String)>,
    pub settings: Settings,
}

fn bind(path: &PathBuf, mode: u32) -> io::Result<UnixListener> {
    let _ = std::fs::remove_file(path);
    let l = UnixListener::bind(path)?;
    // Who may connect is decided per connection by peer uid; the mode only
    // has to let them reach it.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(l)
}

fn peer_uid(s: &UnixStream) -> u32 {
    portcullis_peer::uid_gid(s.as_raw_fd()).map(|(u, _)| u).unwrap_or(u32::MAX)
}

/// One reader thread: decode capped messages until EOF or garbage.
fn reader<T: serde::de::DeserializeOwned + Send + 'static>(
    mut s: UnixStream,
    max: usize,
    tx: Sender<Ev>,
    msg: impl Fn(T) -> Ev + Send + 'static,
    gone: Ev,
) {
    std::thread::spawn(move || {
        while let Ok(Some(m)) = read_msg::<T>(&mut s, max) {
            if tx.send(msg(m)).is_err() {
                return;
            }
        }
        let _ = tx.send(gone);
    });
}

impl Daemon {
    /// Bind the three sockets and start their accept threads.
    pub fn bind(settings: Settings, registry: Arc<dyn Registry + Send + Sync>, passcode: Box<dyn Passcode>) -> io::Result<Daemon> {
        let (tx, rx) = mpsc::channel();
        let clients = bind(&settings.client_socket, 0o666)?;
        let drivers = bind(&settings.driver_socket, 0o666)?;
        let uis = bind(&settings.ui_socket, 0o666)?;
        {
            let tx = tx.clone();
            std::thread::spawn(move || {
                for s in clients.incoming().flatten() {
                    let uid = peer_uid(&s);
                    let label = if uid == u32::MAX { Label::Unknown } else { registry.label(uid) };
                    if tx.send(Ev::Client(s, label, uid)).is_err() {
                        return;
                    }
                }
            });
        }
        for (l, ui) in [(drivers, false), (uis, true)] {
            let tx = tx.clone();
            std::thread::spawn(move || {
                for s in l.incoming().flatten() {
                    let uid = peer_uid(&s);
                    let ev = if ui { Ev::Ui(s, uid) } else { Ev::Driver(s, uid) };
                    if tx.send(ev).is_err() {
                        return;
                    }
                }
            });
        }
        Ok(Daemon {
            broker: Broker::new(settings.broker.clone()),
            tx,
            rx,
            clients: BTreeMap::new(),
            drivers: BTreeMap::new(),
            ui: None,
            next: 1,
            started: Instant::now(),
            passcode,
            platform: Box::new(crate::platform::NoPlatform),
            asked_passcode: None,
            settings,
        })
    }

    /// Serve Arca as the platform authenticator (§9.2).
    pub fn with_platform(mut self, p: Box<dyn crate::platform::Platform>) -> Daemon {
        self.platform = p;
        self
    }

    fn now(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    fn id(&mut self) -> u64 {
        let n = self.next;
        self.next += 1;
        n
    }

    /// Serve until every accept thread is gone (in practice, forever).
    pub fn run(mut self) {
        loop {
            let actions = match self.rx.recv_timeout(TICK) {
                Ok(ev) => self.event(ev),
                Err(RecvTimeoutError::Timeout) => Vec::new(),
                Err(RecvTimeoutError::Disconnected) => return,
            };
            let now = self.now();
            let mut all = actions;
            all.extend(self.broker.tick(now));
            self.act(all);
        }
    }

    fn audit(line: &str) {
        eprintln!("sigillumd: {line}");
    }

    fn event(&mut self, ev: Ev) -> Vec<Action> {
        match ev {
            Ev::Client(s, label, uid) => {
                if self.clients.len() >= MAX_CLIENTS {
                    let _ = s.shutdown(Shutdown::Both);
                    return Vec::new();
                }
                let conn = self.id();
                let Ok(r) = s.try_clone() else { return Vec::new() };
                let _ = s.set_write_timeout(Some(WRITE_TIMEOUT));
                Self::audit(&format!("client conn={conn} uid={uid} label={}", label_kind(&label)));
                // The label is fixed at accept, never re-read from anything
                // the peer sends.
                self.clients.insert(conn, ClientConn { stream: s, label });
                reader::<Request>(r, client::MAX_MESSAGE, self.tx.clone(), move |m| Ev::ClientReq(conn, m), Ev::ClientGone(conn));
                Vec::new()
            }
            Ev::ClientReq(conn, req) => {
                let now = self.now();
                let Some(c) = self.clients.get(&conn) else { return Vec::new() };
                self.broker.request(conn, &c.label, req, now)
            }
            Ev::ClientGone(conn) => self.drop_client(conn),
            Ev::Driver(s, uid) => {
                if uid != self.settings.dev_uid || self.drivers.len() >= MAX_DRIVERS {
                    Self::audit(&format!("driver socket: refused uid={uid}"));
                    let _ = s.shutdown(Shutdown::Both);
                    return Vec::new();
                }
                let device = self.id();
                let Ok(r) = s.try_clone() else { return Vec::new() };
                let _ = s.set_write_timeout(Some(WRITE_TIMEOUT));
                self.drivers.insert(device, DriverConn { stream: s, hello: false });
                reader::<FromDriver>(r, driver::MAX_MESSAGE, self.tx.clone(), move |m| Ev::DriverRec(device, m), Ev::DriverGone(device));
                Vec::new()
            }
            Ev::DriverRec(device, rec) => {
                let Some(d) = self.drivers.get_mut(&device) else { return Vec::new() };
                let hello = matches!(rec, FromDriver::Hello { .. });
                if hello == d.hello {
                    // A second Hello, or work before the first: not a driver
                    // we understand.
                    Self::audit(&format!("device {device}: protocol violation, dropped"));
                    return self.drop_driver(device);
                }
                if hello {
                    d.hello = true;
                }
                self.broker.driver(device, rec)
            }
            Ev::DriverGone(device) => self.drop_driver(device),
            Ev::Ui(s, uid) => {
                if uid != self.settings.ui_uid || self.ui.is_some() {
                    Self::audit(&format!("ui socket: refused uid={uid}"));
                    let _ = s.shutdown(Shutdown::Both);
                    return Vec::new();
                }
                let id = self.id();
                let Ok(r) = s.try_clone() else { return Vec::new() };
                let _ = s.set_write_timeout(Some(WRITE_TIMEOUT));
                self.ui = Some((id, s));
                reader::<FromUi>(r, ui::MAX_MESSAGE, self.tx.clone(), move |m| Ev::UiMsg(id, m), Ev::UiGone(id));
                Self::audit("trusted UI connected");
                Vec::new()
            }
            Ev::UiMsg(id, m) => {
                if self.ui.as_ref().map(|(i, _)| *i) != Some(id) {
                    return Vec::new();
                }
                match m {
                    FromUi::Press { key, choice, activation_ok } => self.broker.press(key, choice, activation_ok),
                    FromUi::Pin { key, pin } => self.broker.pin(key, pin),
                    FromUi::Passcode { key, passcode } => match self.asked_passcode.take() {
                        Some((k, user)) if k == key => {
                            let ok = self.passcode.check(&user, &passcode.0);
                            self.broker.passcode_result(key, ok)
                        }
                        other => {
                            self.asked_passcode = other;
                            Vec::new()
                        }
                    },
                }
            }
            Ev::UiGone(id) => {
                if self.ui.as_ref().map(|(i, _)| *i) != Some(id) {
                    return Vec::new();
                }
                self.drop_ui()
            }
        }
    }

    fn drop_client(&mut self, conn: ConnId) -> Vec<Action> {
        if let Some(c) = self.clients.remove(&conn) {
            let _ = c.stream.shutdown(Shutdown::Both);
        }
        self.broker.disconnected(conn)
    }

    fn drop_driver(&mut self, device: DeviceId) -> Vec<Action> {
        if let Some(d) = self.drivers.remove(&device) {
            let _ = d.stream.shutdown(Shutdown::Both);
        }
        self.broker.device_gone(device)
    }

    fn drop_ui(&mut self) -> Vec<Action> {
        if let Some((_, s)) = self.ui.take() {
            let _ = s.shutdown(Shutdown::Both);
        }
        self.asked_passcode = None;
        Self::audit("trusted UI gone");
        self.broker.abort_all("trusted UI gone")
    }

    fn to_ui(&mut self, m: &ToUi) -> Result<(), ()> {
        let Some((_, s)) = self.ui.as_mut() else { return Err(()) };
        write_msg(s, m, ui::MAX_MESSAGE).map_err(|_| ())
    }

    fn devices_for(&self, sheet: &Sheet) -> Vec<DeviceView> {
        let fido = matches!(sheet, Sheet::Register { .. } | Sheet::Assert { .. });
        self.broker
            .devices()
            .filter(|(_, d)| !fido || d.class == DeviceClass::Fido)
            .map(|(id, d)| DeviceView { id: *id, name: d.name.clone() })
            .collect()
    }

    /// Carry out actions; an action that fails (no UI, a dead peer) may
    /// produce more, which run in the same pass.
    fn act(&mut self, actions: Vec<Action>) {
        let mut queue: std::collections::VecDeque<Action> = actions.into();
        while let Some(a) = queue.pop_front() {
            let more = match a {
                Action::Audit(line) => {
                    Self::audit(&line);
                    Vec::new()
                }
                Action::Reply { conn, reply } => {
                    let failed = match self.clients.get_mut(&conn) {
                        Some(c) => write_msg(&mut c.stream, &reply, client::MAX_MESSAGE).is_err(),
                        None => false,
                    };
                    if failed {
                        self.drop_client(conn)
                    } else {
                        Vec::new()
                    }
                }
                Action::Sheet { key, sheet } => {
                    let devices = self.devices_for(&sheet);
                    if self.ui.is_none() {
                        self.broker.abort(key, "no trusted UI")
                    } else if self.to_ui(&ToUi::Show { key, sheet, devices }).is_err() {
                        self.drop_ui()
                    } else {
                        Vec::new()
                    }
                }
                Action::AskPin { key, device, retries } => self.ui_or_drop(ToUi::AskPin { key, device, retries }),
                Action::Touch { key, device } => self.ui_or_drop(ToUi::Touch { key, device }),
                Action::CloseSheet { key } => {
                    if self.asked_passcode.as_ref().is_some_and(|(k, _)| *k == key) {
                        self.asked_passcode = None;
                    }
                    self.ui_or_drop(ToUi::Close { key })
                }
                Action::CheckPasscode { key, user } => {
                    self.asked_passcode = Some((key, user));
                    self.ui_or_drop(ToUi::AskPasscode { key })
                }
                Action::ListPasskeys { key, user, rp_id } => {
                    let list = self.platform.list(&user, &rp_id);
                    self.broker.passkeys(key, list)
                }
                Action::PlatformCreate { key, user, rp_id, user_handle, name, display_name, uv } => {
                    let r = self.platform.create(&user, &rp_id, &user_handle, &name, &display_name, uv);
                    self.broker.platform_result(key, r)
                }
                Action::PlatformAssert { key, user, rp_id, id, uv, client_data_hash } => {
                    let r = self.platform.assert(&user, &rp_id, &id, uv, &client_data_hash);
                    self.broker.platform_result(key, r)
                }
                Action::ToDriver { device, msg } => {
                    let failed = match self.drivers.get_mut(&device) {
                        Some(d) => write_msg(&mut d.stream, &msg, driver::MAX_MESSAGE).is_err(),
                        None => true,
                    };
                    if failed {
                        self.drop_driver(device)
                    } else {
                        Vec::new()
                    }
                }
            };
            queue.extend(more);
        }
    }

    fn ui_or_drop(&mut self, m: ToUi) -> Vec<Action> {
        if self.ui.is_none() {
            return Vec::new();
        }
        if self.to_ui(&m).is_err() {
            return self.drop_ui();
        }
        Vec::new()
    }
}

fn label_kind(l: &Label) -> String {
    match l {
        Label::App { app, .. } => format!("app:{}", app.app_id),
        Label::Arca => "arca".into(),
        Label::Ostiarius => "ostiarius".into(),
        Label::Unknown => "unknown".into(),
    }
}
