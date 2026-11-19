//! The VPN page (vpn.md §7.3): the tunnels (connect, disconnect, the block
//! switch, the whole computer or chosen personas, remove), which personas
//! use each, and a new tunnel: a WireGuard file through the system picker,
//! or a key made in Arca for a server whose details the person types.
//! cryptad knows Curia by `vpn-configure`; the personas' routes are the
//! person's policy, changed through portcullisd with a press. `main.rs`
//! runs [`work`] for each [`Want`] and hands back what it [`Got`]; the
//! drawing is [`CuriaView::vpn`]. What it does is logged as `curia: vpn: …`
//! (the VM check reads it).

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use crypta_ctl::client::Client;
use crypta_ctl::{CreateWireGuard, Created, Details, Grant, ImportWireGuard, Net, Scope, SetProfile, State};
use pergola::geom::Rect;
use pergola::layout::FlexStyle;
use pergola::node::Node;
use pergola::text_edit::TextEdit;
use pergola::theme::{type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::widgets::{phosphor as ph, ScrollView};
use pergola::{Color, Mutable};

use crate::apps::persona_name;
use crate::view::{button, em, icon, rect, text, CuriaView, Look};

/// Where the system picker is drawn (over the whole window).
pub const PICK_SLOT: pergola::slot::SlotKey = 0x5650_4e31;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Want {
    Refresh,
    Connect(String),
    Disconnect(String),
    Set(SetProfile),
    Remove(String),
    Create(CreateWireGuard),
    Import(ImportWireGuard),
    /// Persona `persona` goes through `vpn` (None: direct), with the press.
    PersonaVpn { persona: String, vpn: Option<String>, activation: Option<[u8; 16]> },
    /// The system picker for a `.conf` (main.rs opens it: it holds limend).
    Pick,
}

#[derive(Clone, Debug)]
pub enum Got {
    Tunnels(Result<Vec<Details>, String>),
    /// The person's named personas, and (persona, tunnel) for each routed one.
    Personas(Result<(Vec<String>, Vec<(String, String)>), String>),
    /// A change: what to say, or why it failed.
    Done(Result<String, String>),
    /// A new tunnel, and its public key for the server's admin.
    Created(Result<Created, String>),
    /// The picker gave a file (its name and bytes), or why not.
    Picked(Result<(String, Vec<u8>), String>),
}

/// The new-tunnel form's fields, in order.
pub const FIELDS: [&str; 6] = ["Name, like work", "Server, like 203.0.113.5:51820", "The server's public key", "Your address in the tunnel, like 10.7.0.2/32", "Networks it carries (0.0.0.0/0: everything)", "DNS servers in the tunnel (optional)"];

#[derive(Clone, Debug, Default)]
pub struct VpnPage {
    /// None until crypta-ctl answers.
    pub tunnels: Option<Result<Vec<Details>, String>>,
    pub personas: Option<Result<(Vec<String>, Vec<(String, String)>), String>>,
    /// A change on its way (nothing else is offered meanwhile).
    pub busy: bool,
    /// The last change's outcome: (failed, words).
    pub note: Option<(bool, String)>,
    /// Remove pressed once: press again to remove.
    pub confirm_remove: Option<String>,
    /// The new-tunnel form is open.
    pub creating: bool,
    /// A tunnel just made or imported: its key, to give the server.
    pub made: Option<Created>,
    /// The picker is up.
    pub picking: bool,
    wants: Vec<Want>,
}

fn list<T>(s: &str, what: &str, f: impl Fn(&str) -> Option<T>) -> Result<Vec<T>, String> {
    s.split([',', ' ']).filter(|x| !x.is_empty()).map(|x| f(x).ok_or_else(|| format!("{x:?} is not {what}"))).collect()
}

/// The new-tunnel form as a request.
pub fn parse_create(f: &[String; 6]) -> Result<CreateWireGuard, String> {
    let name = f[0].trim().to_ascii_lowercase();
    let endpoint = f[1].trim().parse().map_err(|_| "the server is an IPv4 address and port, like 203.0.113.5:51820".to_string())?;
    let nets = |s: &str, what: &str| list(s, what, Net::parse);
    let allowed = if f[4].trim().is_empty() { vec![Net::parse("0.0.0.0/0").unwrap()] } else { nets(&f[4], "a network like 10.0.0.0/8")? };
    let c = CreateWireGuard {
        name,
        peer: f[2].trim().to_string(),
        endpoint,
        addrs: nets(&f[3], "an address like 10.7.0.2/32")?,
        allowed,
        dns: list(&f[5], "an IPv4 address", |a| a.parse().ok())?,
        keepalive: Some(25),
    };
    c.check().map_err(|e| e.1)?;
    Ok(c)
}

/// A tunnel name from a picked file's name: `Acme VPN.conf` is `acme-vpn`.
pub fn name_from_file(file: &str) -> String {
    let stem = file.rsplit_once('.').map(|(s, _)| s).unwrap_or(file);
    let mut n: String = stem.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    while n.contains("--") {
        n = n.replace("--", "-");
    }
    let n: String = n.trim_matches('-').chars().take(32).collect();
    let n = n.trim_end_matches('-').to_string();
    if crypta_ctl::valid_name(&n) {
        n
    } else {
        "vpn".into()
    }
}

/// A tunnel's second line: how it is and what it carries.
pub fn describe(d: &Details) -> String {
    let p = &d.profile;
    let state = match p.tunnel.state {
        State::Up => "Connected",
        State::Connecting if p.block => "Connecting: its traffic waits",
        State::Connecting => "Connecting",
        State::Down => "Not connected",
    };
    let carries = if p.full { "all traffic".to_string() } else { format!("{} {}", d.routes, if d.routes == 1 { "network" } else { "networks" }) };
    let whom = match p.tunnel.scope {
        Scope::Machine => "the whole computer",
        Scope::Personas => "chosen personas",
    };
    format!("{state} \u{b7} {carries} \u{b7} for {whom}")
}

/// Its third line: the server, our address, our key's start.
pub fn detail_line(d: &Details) -> String {
    let addrs = d.addrs.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ");
    format!("Server {} \u{b7} address {addrs} \u{b7} key {}…", d.endpoint, &d.public[..8.min(d.public.len())])
}

impl VpnPage {
    pub fn refresh(&mut self) {
        if !self.wants.contains(&Want::Refresh) {
            self.wants.push(Want::Refresh);
        }
    }

    pub fn take_wants(&mut self) -> Vec<Want> {
        std::mem::take(&mut self.wants)
    }

    fn change(&mut self, w: Want) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.note = None;
        self.wants.push(w);
    }

    pub fn connect(&mut self, name: &str, on: bool) {
        self.change(if on { Want::Connect(name.into()) } else { Want::Disconnect(name.into()) });
    }

    pub fn set_block(&mut self, name: &str, on: bool) {
        self.change(Want::Set(SetProfile { name: name.into(), block: Some(on), personas: None }));
    }

    pub fn set_scope(&mut self, name: &str, personas: bool) {
        self.change(Want::Set(SetProfile { name: name.into(), block: None, personas: Some(personas) }));
    }

    /// The first press asks; the second removes.
    pub fn remove(&mut self, name: &str) {
        if self.confirm_remove.as_deref() == Some(name) {
            self.confirm_remove = None;
            self.change(Want::Remove(name.into()));
        } else {
            self.confirm_remove = Some(name.into());
        }
    }

    /// The tunnel persona `p` uses, if any.
    pub fn route_of(&self, p: &str) -> Option<&str> {
        match &self.personas {
            Some(Ok((_, r))) => r.iter().find(|(n, _)| n == p).map(|(_, v)| v.as_str()),
            _ => None,
        }
    }

    /// A press on a persona under a tunnel: on it if it was not, off if it was.
    pub fn toggle_persona(&mut self, persona: &str, tunnel: &str, activation: Option<[u8; 16]>) {
        let vpn = if self.route_of(persona) == Some(tunnel) { None } else { Some(tunnel.to_string()) };
        self.change(Want::PersonaVpn { persona: persona.into(), vpn, activation });
    }

    pub fn create(&mut self, fields: &[String; 6]) {
        match parse_create(fields) {
            Ok(c) => self.change(Want::Create(c)),
            Err(e) => self.note = Some((true, e)),
        }
    }

    pub fn pick(&mut self) {
        if !self.busy && !self.picking {
            self.picking = true;
            self.note = None;
            self.wants.push(Want::Pick);
        }
    }

    pub fn apply(&mut self, g: Got) {
        match g {
            Got::Tunnels(r) => self.tunnels = Some(r),
            Got::Personas(r) => self.personas = Some(r),
            Got::Done(r) => {
                self.busy = false;
                self.note = Some(match r {
                    Ok(m) => (false, m),
                    Err(m) => (true, m),
                });
                self.refresh();
            }
            Got::Created(r) => {
                self.busy = false;
                match r {
                    Ok(c) => {
                        let ignored = if c.ignored.is_empty() { String::new() } else { format!(" (left out of the file: {})", c.ignored.join(", ")) };
                        self.note = Some((false, format!("{} added{ignored}", c.name)));
                        self.creating = false;
                        self.made = Some(c);
                    }
                    Err(e) => self.note = Some((true, format!("Not added: {e}"))),
                }
                self.refresh();
            }
            Got::Picked(r) => {
                self.picking = false;
                match r {
                    Ok((file, conf)) => {
                        let name = name_from_file(&file);
                        self.change(Want::Import(ImportWireGuard { name, conf }));
                    }
                    Err(e) if e.is_empty() => {}
                    Err(e) => self.note = Some((true, e)),
                }
            }
        }
    }
}

fn sock_dir() -> PathBuf {
    std::env::var_os("CURIA_SOCKETS").map(PathBuf::from).unwrap_or_else(|| "/atrium/sockets".into())
}

/// portcullisd: the person's personas, and the routes of those with a VPN.
fn personas() -> Result<(Vec<String>, Vec<(String, String)>), String> {
    use portcullis_ipc::{round_trip, Request, Response, PROTO_VERSION};
    let ask = |req: &Request| -> Result<Response, String> {
        let path = portcullis_ipc::resolve_socket_path().ok_or("portcullisd is not running")?;
        let mut s = std::os::unix::net::UnixStream::connect(path).map_err(|e| format!("portcullisd: {e}"))?;
        s.set_read_timeout(Some(std::time::Duration::from_secs(10))).map_err(|e| e.to_string())?;
        match round_trip(&mut s, &Request::Hello { version: PROTO_VERSION }) {
            Ok(Response::Hello { version }) if version == PROTO_VERSION => {}
            Ok(other) => return Err(format!("portcullisd: {other:?}")),
            Err(e) => return Err(format!("portcullisd is not answering ({e})")),
        }
        round_trip(&mut s, req).map_err(|e| format!("portcullisd is not answering ({e})"))
    };
    let named = match ask(&Request::Crossings)? {
        Response::Crossings { personas, .. } => personas.into_iter().map(|(n, _)| n).collect(),
        Response::Error { message } => return Err(message),
        other => return Err(format!("unexpected reply: {other:?}")),
    };
    let routes = match ask(&Request::PersonaVpns)? {
        Response::PersonaVpns { routes } => routes,
        Response::Error { message } => return Err(message),
        other => return Err(format!("unexpected reply: {other:?}")),
    };
    Ok((named, routes))
}

fn set_persona_vpn(persona: &str, vpn: Option<&str>, activation: [u8; 16]) -> Result<(), String> {
    use portcullis_ipc::{round_trip, Request, Response, PROTO_VERSION};
    let path = portcullis_ipc::resolve_socket_path().ok_or("portcullisd is not running")?;
    let mut s = std::os::unix::net::UnixStream::connect(path).map_err(|e| format!("portcullisd: {e}"))?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(10))).map_err(|e| e.to_string())?;
    match round_trip(&mut s, &Request::Hello { version: PROTO_VERSION }) {
        Ok(Response::Hello { version }) if version == PROTO_VERSION => {}
        _ => return Err("portcullisd is not answering".into()),
    }
    match round_trip(&mut s, &Request::SetPersonaVpn { name: persona.into(), vpn: vpn.map(String::from), activation }) {
        Ok(Response::Ok) => Ok(()),
        Ok(Response::Error { message }) => Err(message),
        Ok(other) => Err(format!("unexpected reply: {other:?}")),
        Err(e) => Err(format!("portcullisd is not answering ({e})")),
    }
}

/// One ask, on a thread of its own.
pub fn work(w: Want, tx: Sender<Got>) {
    std::thread::spawn(move || {
        let connect = || Client::connect(&Client::path(&sock_dir(), Grant::Configure)).map_err(|_| "the VPN service is not running".to_string());
        let say = |r: Result<String, String>| {
            match &r {
                Ok(m) => eprintln!("curia: vpn: {m}"),
                Err(e) => eprintln!("curia: vpn: failed: {e}"),
            }
            let _ = tx.send(Got::Done(r));
        };
        let fail = |what: String| move |e: crypta_ctl::Fail| format!("{what}: {}", e.1);
        match w {
            Want::Refresh => {
                let r = connect().and_then(|mut c| {
                    let names = c.list().map_err(|e| e.1)?;
                    names.iter().map(|p| c.details(&p.tunnel.name).map_err(|e| e.1)).collect::<Result<Vec<_>, _>>()
                });
                let _ = tx.send(Got::Tunnels(r));
                let _ = tx.send(Got::Personas(personas()));
            }
            Want::Connect(n) => say(connect().and_then(|mut c| c.connect_tunnel(&n).map_err(fail(format!("connect {n}")))).map(|_| format!("{n} connecting"))),
            Want::Disconnect(n) => say(connect().and_then(|mut c| c.disconnect_tunnel(&n).map_err(fail(format!("disconnect {n}")))).map(|_| format!("{n} disconnected"))),
            Want::Set(s) => {
                let what = match (s.block, s.personas) {
                    (Some(true), _) => format!("{} blocks traffic when not connected", s.name),
                    (Some(false), _) => format!("{} lets traffic go direct when not connected", s.name),
                    (_, Some(true)) => format!("{} is for chosen personas", s.name),
                    _ => format!("{} is for the whole computer", s.name),
                };
                say(connect().and_then(|mut c| c.set(&s).map_err(fail(s.name.clone()))).map(|_| format!("{what}; from its next connect")))
            }
            Want::Remove(n) => say(connect().and_then(|mut c| c.remove(&n).map_err(fail(format!("remove {n}")))).map(|_| format!("removed {n}"))),
            Want::Create(cw) => {
                let r = connect().and_then(|mut c| c.create(&cw).map_err(|e| e.1));
                match &r {
                    Ok(c) => eprintln!("curia: vpn: created {} (key {})", c.name, c.public),
                    Err(e) => eprintln!("curia: vpn: create {} failed: {e}", cw.name),
                }
                let _ = tx.send(Got::Created(r));
            }
            Want::Import(i) => {
                let name = i.name.clone();
                let r = connect().and_then(|mut c| c.import(&i).map_err(|e| e.1));
                match &r {
                    Ok(c) => eprintln!("curia: vpn: imported {} (key {})", c.name, c.public),
                    Err(e) => eprintln!("curia: vpn: import {name} failed: {e}"),
                }
                let _ = tx.send(Got::Created(r));
            }
            Want::PersonaVpn { persona, vpn, activation } => {
                let who = persona_name(&persona);
                let r = match activation {
                    None => Err("press the button itself".to_string()),
                    Some(a) => set_persona_vpn(&persona, vpn.as_deref(), a),
                };
                say(r.map(|_| match &vpn {
                    Some(v) => format!("{who} goes through {v}"),
                    None => format!("{who} goes direct"),
                }));
            }
            Want::Pick => {}
        }
    });
}

pub fn with_vpn(page: &Mutable<VpnPage>, f: impl FnOnce(&mut VpnPage)) {
    let mut p = page.get_cloned();
    f(&mut p);
    page.set(p);
}

/// The row buttons' slots, left to right from the row's left edge.

impl CuriaView {
    /// The VPN page. Each tunnel is a card: its name, how it is, its
    /// server; Connect, the block switch, whom it is for and Remove in fixed
    /// slots; for chosen personas, a press per persona.
    pub(crate) fn vpn(&self, ctx: &mut Ctx, x0: f32, pw: f32, h: f32) {
        let p = self.vpn.get_cloned();
        let t = ctx.theme;
        let y = em(20.0);
        text(ctx, x0, y + em(16.0), "VPN", type_size::XL, Weight::Semibold, t.text_primary());
        let top = y + em(48.0);
        let page = self.vpn.clone();
        let picking = p.picking;
        let fields: [Mutable<TextEdit>; 6] = self.vpn_fields.clone();
        let content = pergola::view::Draw(move |ctx: &mut Ctx| {
            let t = ctx.theme;
            let g = ctx.push(Node::stack(pergola::geom::Axis::Vertical, Rect::new(0.0, 0.0, pw, 0.0), 0.0));
            ctx.set_flex(g, FlexStyle { free: true, ..FlexStyle::default() });
            let mut y = 0.0;
            match (&p.note, p.busy) {
                (_, true) => {
                    text(ctx, 0.0, y + em(12.0), "Working…", type_size::SM, Weight::Regular, t.text_tertiary());
                }
                (Some((failed, m)), _) => {
                    text(ctx, 0.0, y + em(12.0), m, type_size::SM, Weight::Regular, if *failed { t.danger_fg() } else { t.text_secondary() });
                }
                _ => {}
            }
            y += em(28.0);
            if let Some(c) = &p.made {
                let mh = em(76.0);
                rect(ctx, 0.0, y, pw, mh, t.bg_surface(), 8.0);
                text(ctx, em(16.0), y + em(22.0), &format!("Give the server's admin {}'s public key:", c.name), type_size::SM, Weight::Regular, t.text_secondary());
                text(ctx, em(16.0), y + em(48.0), &c.public, type_size::MD, Weight::Medium, t.text_primary());
                let pg = page.clone();
                button(ctx, pw - em(100.0), y + em(34.0), "Done", Look::Ghost, true, move || with_vpn(&pg, |a| a.made = None));
                y += mh + em(12.0);
            }
            let list = match &p.tunnels {
                None => {
                    text(ctx, 0.0, y + em(10.0), "Asking for your tunnels…", type_size::SM, Weight::Regular, t.text_tertiary());
                    Vec::new()
                }
                Some(Err(why)) => {
                    // curia.md §10: say so, offer nothing, and keep asking.
                    text(ctx, 0.0, y + em(10.0), &format!("VPN settings are not available: {why}"), type_size::SM, Weight::Regular, t.text_secondary());
                    y += em(28.0);
                    if let Some(Node::Stack { rect, .. }) = ctx.tree.get_mut(g) {
                        rect.size.h = y;
                    }
                    ctx.pop();
                    return;
                }
                Some(Ok(v)) if v.is_empty() => {
                    text(ctx, 0.0, y + em(10.0), "No tunnels yet. Add one below.", type_size::SM, Weight::Regular, t.text_secondary());
                    Vec::new()
                }
                Some(Ok(v)) => v.clone(),
            };
            if list.is_empty() {
                y += em(28.0);
            }
            let named: Vec<String> = match &p.personas {
                Some(Ok((n, _))) => n.clone(),
                _ => Vec::new(),
            };
            for d in &list {
                let name = d.profile.tunnel.name.clone();
                let chosen = d.profile.tunnel.scope == Scope::Personas;
                let rh = em(if chosen { 156.0 } else { 116.0 });
                rect(ctx, 0.0, y, pw, rh, t.bg_surface(), 8.0);
                let up = d.profile.tunnel.state != State::Down;
                let colour = match d.profile.tunnel.state {
                    State::Up => t.success_fg(),
                    State::Connecting => t.warning_fg(),
                    State::Down => t.text_tertiary(),
                };
                icon(ctx, em(16.0), y + em(14.0), ph::LOCK_SIMPLE, em(20.0), colour);
                text(ctx, em(48.0), y + em(24.0), &name, type_size::MD, Weight::Medium, t.text_primary());
                text(ctx, em(48.0), y + em(44.0), &describe(d), type_size::SM, Weight::Regular, t.text_secondary());
                text(ctx, em(48.0), y + em(62.0), &detail_line(d), type_size::XS, Weight::Regular, t.text_tertiary());
                let by = y + em(74.0);
                let enabled = !p.busy;
                let (pg, n) = (page.clone(), name.clone());
                let mut bx = em(16.0);
                bx += button(ctx, bx, by, if up { "Disconnect" } else { "Connect" }, if up { Look::Secondary } else { Look::Primary }, enabled, move || with_vpn(&pg, |a| a.connect(&n, !up)));
                let (pg, n, block) = (page.clone(), name.clone(), d.profile.block);
                bx += em(8.0);
                bx += button(ctx, bx, by, if block { "Block when off: on" } else { "Block when off: off" }, Look::Secondary, enabled, move || with_vpn(&pg, |a| a.set_block(&n, !block)));
                let (pg, n) = (page.clone(), name.clone());
                bx += em(8.0);
                bx += button(ctx, bx, by, if chosen { "For: chosen personas" } else { "For: whole computer" }, Look::Secondary, enabled, move || with_vpn(&pg, |a| a.set_scope(&n, !chosen)));
                let pg = page.clone();
                let confirm = p.confirm_remove.as_deref() == Some(name.as_str());
                let (n2, rx) = (name.clone(), bx + em(8.0));
                button(ctx, rx, by, if confirm { "Remove" } else { "Remove…" }, if confirm { Look::Danger } else { Look::Secondary }, enabled, move || with_vpn(&pg, |a| a.remove(&n2)));
                if chosen {
                    let py = y + em(116.0);
                    text(ctx, em(16.0), py + em(14.0), "Personas:", type_size::SM, Weight::Regular, t.text_secondary());
                    let mut px = em(96.0);
                    if named.is_empty() {
                        text(ctx, px, py + em(14.0), "none yet: add one on the Personas page", type_size::SM, Weight::Regular, t.text_tertiary());
                    }
                    for persona in &named {
                        let on = p.route_of(persona) == Some(name.as_str());
                        let label = format!("{}{}", persona_name(persona), if on { " \u{2713}" } else { "" });
                        let (pg, pn, n) = (page.clone(), persona.clone(), name.clone());
                        let w = button(ctx, px, py, &label, if on { Look::Primary } else { Look::Secondary }, enabled, move || {
                            let token = pergola::clipboard::current_activation();
                            with_vpn(&pg, |a| a.toggle_persona(&pn, &n, token))
                        });
                        px += w + em(8.0);
                    }
                }
                y += rh + em(8.0);
            }

            // A new tunnel: a file, or a key made here.
            y += em(12.0);
            text(ctx, 0.0, y + em(14.0), "Add a tunnel", type_size::LG, Weight::Semibold, t.text_primary());
            y += em(36.0);
            let pg = page.clone();
            let w = button(ctx, 0.0, y, if p.picking { "Choosing a file…" } else { "Import a WireGuard file…" }, Look::Secondary, !p.busy && !p.picking, move || with_vpn(&pg, |a| a.pick()));
            let pg = page.clone();
            button(ctx, w + em(12.0), y, "Make a new key…", Look::Secondary, !p.busy, move || with_vpn(&pg, |a| a.creating = !a.creating));
            y += em(44.0);
            if p.creating {
                for (i, f) in fields.iter().enumerate() {
                    pergola::widgets::TextField::new(f.clone()).placeholder(FIELDS[i]).at(0.0, y).width(em(420.0)).render(ctx);
                    y += em(40.0);
                }
                let (pg, fs) = (page.clone(), fields.clone());
                button(ctx, 0.0, y, "Add tunnel", Look::Primary, !p.busy, move || {
                    let v: [String; 6] = std::array::from_fn(|i| fs[i].get_cloned().text().to_string());
                    with_vpn(&pg, |a| a.create(&v));
                });
                text(ctx, em(120.0), y + em(15.0), "The key is made in Arca and never leaves it.", type_size::XS, Weight::Regular, t.text_tertiary());
                y += em(44.0);
            }
            y += em(16.0);
            if let Some(Node::Stack { rect, .. }) = ctx.tree.get_mut(g) {
                rect.size.h = y;
            }
            ctx.pop();
        });
        ScrollView::new(self.vpn_scroll.clone(), Rect::new(x0, top, pw, (h - top - em(8.0)).max(em(60.0))), content)
            .key(98)
            .backdrop(t.bg_canvas())
            .emit(ctx);
        if picking {
            let (w, h) = self.size.get();
            let id = ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: Color::TRANSPARENT, radius: 0.0 });
            ctx.slot(id, PICK_SLOT);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crypta_ctl::{Kind, Profile, Tunnel};

    const KEY: &str = "xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=";

    fn details(name: &str, state: State, scope: Scope) -> Details {
        Details {
            profile: Profile { tunnel: Tunnel { name: name.into(), kind: Kind::WireGuard, state, scope }, block: true, full: true },
            public: KEY.into(),
            endpoint: "203.0.113.5:51820".parse().unwrap(),
            addrs: vec![Net::parse("10.7.0.2/32").unwrap()],
            routes: 1,
            dns: vec![],
            rx: 0,
            tx: 0,
            starts: 0,
        }
    }

    #[test]
    fn the_form_parses_and_checks() {
        let mut f: [String; 6] = ["Acme".into(), "203.0.113.5:51820".into(), KEY.into(), "10.7.0.2/32".into(), "".into(), "10.7.0.1".into()];
        let c = parse_create(&f).unwrap();
        assert_eq!((c.name.as_str(), c.allowed[0].prefix, c.dns.len()), ("acme", 0, 1), "empty networks: everything");
        f[4] = "10.0.0.0/8, 192.168.50.0/24".into();
        assert_eq!(parse_create(&f).unwrap().allowed.len(), 2);
        f[1] = "vpn.example.com:51820".into();
        assert!(parse_create(&f).unwrap_err().contains("IPv4 address and port"));
        f[1] = "203.0.113.5:51820".into();
        f[2] = "nope".into();
        assert!(parse_create(&f).unwrap_err().contains("key"));
    }

    #[test]
    fn file_names_become_tunnel_names() {
        assert_eq!(name_from_file("Acme VPN.conf"), "acme-vpn");
        assert_eq!(name_from_file("wg0.conf"), "wg0");
        assert_eq!(name_from_file("...conf"), "vpn");
        assert_eq!(name_from_file(&format!("{}.conf", "a".repeat(40))).len(), 32);
    }

    #[test]
    fn changes_one_at_a_time_and_remove_asks() {
        let mut p = VpnPage::default();
        p.remove("acme");
        assert!(p.take_wants().is_empty());
        p.remove("acme");
        assert_eq!(p.take_wants(), vec![Want::Remove("acme".into())]);
        p.connect("acme", true);
        assert!(p.take_wants().is_empty(), "busy");
        p.apply(Got::Done(Ok("removed acme".into())));
        assert_eq!(p.take_wants(), vec![Want::Refresh]);
        p.apply(Got::Personas(Ok((vec!["work".into(), "school".into()], vec![("work".into(), "acme".into())]))));
        p.toggle_persona("work", "acme", Some([1; 16]));
        assert_eq!(p.take_wants(), vec![Want::PersonaVpn { persona: "work".into(), vpn: None, activation: Some([1; 16]) }], "on: off");
        p.apply(Got::Done(Ok("Work goes direct".into())));
        p.take_wants();
        p.toggle_persona("school", "acme", Some([2; 16]));
        assert_eq!(p.take_wants(), vec![Want::PersonaVpn { persona: "school".into(), vpn: Some("acme".into()), activation: Some([2; 16]) }]);
        p.apply(Got::Done(Err("Lyra is open in school: close it first".into())));
        assert_eq!(p.note, Some((true, "Lyra is open in school: close it first".into())));
    }

    #[test]
    fn a_pick_imports_and_a_new_key_is_shown() {
        let mut p = VpnPage::default();
        p.pick();
        assert_eq!(p.take_wants(), vec![Want::Pick]);
        p.apply(Got::Picked(Ok(("Acme.conf".into(), b"[Interface]".to_vec()))));
        assert_eq!(p.take_wants(), vec![Want::Import(ImportWireGuard { name: "acme".into(), conf: b"[Interface]".to_vec() })]);
        p.apply(Got::Created(Ok(Created { name: "acme".into(), public: KEY.into(), ignored: vec!["PostUp".into()] })));
        assert_eq!(p.made.as_ref().unwrap().public, KEY);
        assert_eq!(p.note, Some((false, "acme added (left out of the file: PostUp)".into())));
        p.pick();
        p.take_wants();
        p.apply(Got::Picked(Err(String::new())));
        assert!(!p.picking && p.note.is_none(), "a cancel says nothing");
    }

    #[test]
    fn words() {
        let d = details("acme", State::Connecting, Scope::Personas);
        assert_eq!(describe(&d), "Connecting: its traffic waits \u{b7} all traffic \u{b7} for chosen personas");
        assert_eq!(detail_line(&d), "Server 203.0.113.5:51820 \u{b7} address 10.7.0.2/32 \u{b7} key xTIBA5rb…");
        let d = details("home", State::Up, Scope::Machine);
        assert!(describe(&d).starts_with("Connected"));
    }
}
