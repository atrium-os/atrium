//! Feature-only fixture for a trusted Fresco sheet, an encrypted test store
//! and a real Portcullis reader. Regular arca-ui does not start this provider.

use std::io;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use arca_store::{RootKey, Store};
use arcad::server::integration::{self, Prepared, FAILURE};
use fresco_client::{Connection, Event};
use fresco_protocol::WindowHints;
use limen::client::Child;
use limen::proto::{ChildEvent, EmbedId};
use pergola::theme::Semantic;

use crate::gate::Frescod;
use crate::view::{Font, Px, Rect};

const ROLE: &str = "smime-legacy-read";
const APP: &str = "org.atrium.mail";
const USER: &str = "maillegacyfixture";
const CERT: &[u8] = include_bytes!("../../../mail/mail-smime/tests/data/leaf.der");
const KEY: &[u8] = include_bytes!("../../../mail/mail-smime/tests/data/leaf.p8");
const CMS: &[u8] = include_bytes!("../../../mail/mail-smime/tests/data/env.der");
const OPEN: Rect = Rect {
    x: 24.,
    y: 228.,
    w: 210.,
    h: 44.,
};
const CANCEL: Rect = Rect {
    x: 250.,
    y: 228.,
    w: 120.,
    h: 44.,
};

fn closed() -> io::Error {
    io::Error::other(FAILURE)
}

fn open_via_arcad(uid: u32, mut still_approved: impl FnMut() -> bool) -> io::Result<bool> {
    use arcad::proto;
    let mut stream = UnixStream::connect("/atrium/sockets/arca/arcad.sock")?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.set_read_timeout(Some(Duration::from_secs(24)))?;
    proto::write_request(
        &mut stream,
        proto::op::U_SMIME_SYNTHETIC_OPEN,
        &uid.to_le_bytes(),
    )?;
    let mut reply = stream.try_clone()?;
    std::thread::scope(|scope| {
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        let _reader = std::thread::Builder::new().spawn_scoped(scope, move || {
            let _ = send.send(proto::read_reply(&mut reply));
        })?;
        loop {
            if !still_approved() {
                let _ = stream.shutdown(Shutdown::Both);
                return Err(closed());
            }
            match receive.recv_timeout(Duration::from_millis(20)) {
                Ok(result) => {
                    let (status, body) = result?;
                    if !still_approved() || body.as_ref() != [0] {
                        return Err(closed());
                    }
                    println!("LEGACY_JAILED_WORKER_EXIT=0");
                    return Ok(status == proto::status::OK);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => return Err(closed()),
            }
        }
    })
}

fn valid_parent(registry: &str, uid: u32, publisher: [u8; 32], persona: Option<&str>) -> bool {
    let Ok(registry) = portcullis_peer::AppRegistry::load(registry) else {
        return false;
    };
    registry.resolve(uid) == Some((USER, APP))
        && registry.publisher(uid) == Some(Some(publisher))
        && registry.persona(uid) == Some(persona)
}

fn draw(
    win: &mut Connection,
    font: Font,
    scale: f32,
    persona: &str,
    content: Option<&str>,
) -> io::Result<()> {
    let t = if win
        .appearance()
        .is_ok_and(|a| a.scheme == fresco_protocol::APPEARANCE_DARK)
    {
        Semantic::DARK
    } else {
        Semantic::LIGHT
    };
    let mut fr = win.frame()?;
    let mut px = Px {
        fr: &mut fr,
        k: scale,
    };
    px.rect(0., 0., 620., 350., t.bg_window(), 0.)?;
    px.text(
        font,
        21.,
        24.,
        50.,
        t.text_primary(),
        "Open legacy S/MIME message".into(),
    )?;
    px.text(
        font,
        15.,
        24.,
        88.,
        t.text_secondary(),
        format!("Local message in {persona}"),
    )?;
    match content {
        Some(text) => {
            for (line, text) in text.lines().take(10).enumerate() {
                let displayed = text.chars().take(65).collect();
                px.text(
                    font,
                    15.,
                    24.,
                    120. + 18. * line as f32,
                    t.text_primary(),
                    displayed,
                )?;
            }
        }
        None => {
            px.rect(OPEN.x, OPEN.y, OPEN.w, OPEN.h, t.accent_fg(), 6.)?;
            px.text(
                font,
                15.,
                40.,
                254.,
                t.text_on_accent(),
                "Open local message".into(),
            )?;
            px.rect(CANCEL.x, CANCEL.y, CANCEL.w, CANCEL.h, t.bg_elevated(), 6.)?;
            px.text(font, 15., 266., 254., t.text_primary(), "Cancel".into())?;
        }
    }
    fr.finish()
}

struct Sheet<'a> {
    child: Child,
    embed: EmbedId,
    uid: u32,
    persona: Option<String>,
    publisher: [u8; 32],
    registry: &'a str,
}

impl Sheet<'_> {
    fn serve(
        &mut self,
        store: Option<&Store>,
        fresco: &str,
        activation: &str,
    ) -> io::Result<&'static [u8]> {
        let persona = self.persona.as_deref();
        if !valid_parent(self.registry, self.uid, self.publisher, persona) {
            return Err(closed());
        }
        let mut win = Connection::connect(fresco)?;
        let id = win.window_create(
            620,
            350,
            "Arca legacy S/MIME",
            WindowHints {
                embed_pending: true,
                ..Default::default()
            },
        )?;
        win.set_default_window(id as u16);
        let font = Font::open(&mut win);
        let scale = win.window_scale(id);
        let shown_persona = persona.unwrap_or("Personal");
        draw(&mut win, font, scale, shown_persona, None)?;
        let until = Instant::now() + Duration::from_secs(5);
        let token = loop {
            if Instant::now() >= until {
                return Err(closed());
            }
            if let Some(Event::EmbedToken { window_id, token }) =
                win.wait_event(Some(Duration::from_millis(50)))?
            {
                if window_id == id {
                    break token;
                }
            }
        };
        self.child.ready(self.embed, token).map_err(|_| closed())?;
        println!("LEGACY_APPROVAL_SHEET_READY");
        let mut gate = Frescod::new(activation);
        let until = Instant::now() + Duration::from_secs(25);
        while Instant::now() < until {
            if matches!(self.child.poll(Duration::ZERO).map_err(|_| closed())?,
                Some(ChildEvent::Detaching { embed } | ChildEvent::Suspended { embed }) if embed == self.embed)
            {
                return Ok(b"cancelled");
            }
            let Some(event) = win.wait_event(Some(Duration::from_millis(50)))? else {
                continue;
            };
            let press = match event {
                Event::CloseRequested { .. }
                | Event::Key {
                    hid_usage: 0x29,
                    pressed: true,
                    ..
                } => return Ok(b"cancelled"),
                Event::PointerButton {
                    x,
                    y,
                    pressed: true,
                    ..
                } if CANCEL.contains(x, y) => return Ok(b"cancelled"),
                Event::PointerButton {
                    x,
                    y,
                    pressed: true,
                    activation,
                    ..
                } if OPEN.contains(x, y) => activation,
                Event::Key {
                    hid_usage: 0x28 | 0x58,
                    pressed: true,
                    activation,
                    ..
                } => activation,
                _ => continue,
            };
            if !gate
                .approve(press, unsafe { libc::geteuid() })
                .is_ok_and(|window| window == id)
            {
                continue;
            }
            let approved_at = Instant::now();
            let result = (|| {
                if !valid_parent(self.registry, self.uid, self.publisher, persona) {
                    return Err(closed());
                }
                let mut still_approved = || {
                    let still_attached = match self.child.poll(Duration::ZERO) {
                        Ok(Some(
                            ChildEvent::Detaching { embed } | ChildEvent::Suspended { embed },
                        )) => embed != self.embed,
                        Ok(_) => true,
                        Err(_) => false,
                    };
                    approved_at.elapsed() < Duration::from_secs(5)
                        && still_attached
                        && valid_parent(self.registry, self.uid, self.publisher, persona)
                };
                if let Some(store) = store {
                    integration::with_worker(|control| {
                        let prepared = Prepared::start(control, CERT, CMS)?;
                        println!("LEGACY_JAILED_WORKER_READY");
                        let digest = prepared.message_digest();
                        prepared.finish(store, persona, self.publisher, |approved| {
                            approved == digest && still_approved()
                        })
                    })?
                    .map(|text| text.as_str() == integration::SYNTHETIC_PREVIEW)
                } else if still_approved() {
                    open_via_arcad(self.uid, || {
                        still_approved()
                            && matches!(
                                win.wait_event(Some(Duration::ZERO)),
                                Ok(event) if !matches!(
                                    event,
                                    Some(Event::CloseRequested { .. } | Event::Key {
                                        hid_usage: 0x29, pressed: true, ..
                                    })
                                )
                            )
                    })
                } else {
                    Err(closed())
                }
            })();
            let outcome: &'static [u8] = match result {
                Ok(true) => {
                    draw(
                        &mut win,
                        font,
                        scale,
                        shown_persona,
                        Some(integration::SYNTHETIC_PREVIEW),
                    )?;
                    b"opened"
                }
                _ => {
                    draw(&mut win, font, scale, shown_persona, Some(FAILURE))?;
                    b"closed"
                }
            };
            self.child
                .emit(self.embed, outcome.to_vec())
                .map_err(|_| closed())?;
            return Ok(outcome);
        }
        Err(closed())
    }
}

fn publisher(hex: &str) -> io::Result<[u8; 32]> {
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(closed());
    }
    let mut key = [0; 32];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).map_err(|_| closed())?;
    }
    if key == [0; 32] {
        return Err(closed());
    }
    Ok(key)
}

pub fn run() -> io::Result<()> {
    arca_store::secret::harden_process()?;
    let args: Vec<_> = std::env::args().skip(1).collect();
    let [mode, dir, publisher_hex, nonce] = args.as_slice() else {
        return Err(closed());
    };
    let dir = Path::new(dir);
    if !dir.is_absolute()
        || nonce.len() != 48
        || std::fs::read_to_string(dir.join(".isolated-gate"))?.trim() != nonce
    {
        return Err(closed());
    }
    let publisher = publisher(publisher_hex)?;
    let root = RootKey::from_bytes(&[71; 32]).map_err(|_| closed())?;
    let store_path = dir.join("store");
    if mode == "prepare" {
        let mut store = Store::create(&store_path, USER, &root).map_err(|_| closed())?;
        integration::enroll(&mut store, Some("work"), publisher, CERT, KEY)?;
        println!("LEGACY_FIXTURE_STORED");
        return Ok(());
    }
    let store = match mode.as_str() {
        "serve" => {
            let mut store = Store::open(&store_path, USER).map_err(|_| closed())?;
            store.unlock(&root).map_err(|_| closed())?;
            Some(store)
        }
        "signed" => None,
        _ => return Err(closed()),
    };
    let fresco = std::env::var("FRESCO_SOCKET").map_err(|_| closed())?;
    let activation = std::env::var("FRESCO_ACTIVATION").map_err(|_| closed())?;
    let registry = std::env::var("ARCA_APP_REGISTRY").map_err(|_| closed())?;
    let mut child = Child::provider(std::env::var("LIMEN_SOCKET").map_err(|_| closed())?, ROLE)
        .map_err(|_| closed())?;
    println!("LEGACY_FIXTURE_PROVIDER_READY");
    let until = Instant::now() + Duration::from_secs(35);
    let mut attach = None;
    while Instant::now() < until {
        match child
            .poll(Duration::from_millis(100))
            .map_err(|_| closed())?
        {
            Some(ChildEvent::Attach {
                embed,
                role,
                parent_app,
                parent_uid,
            }) if role == ROLE && parent_app == APP => attach = Some((embed, parent_uid)),
            Some(ChildEvent::ParentPersona { embed, persona })
                if attach.is_some_and(|(id, _)| id == embed) =>
            {
                let (_, uid) = attach.unwrap();
                let mut sheet = Sheet {
                    child,
                    embed,
                    uid,
                    persona,
                    publisher,
                    registry: &registry,
                };
                let result = sheet.serve(store.as_ref(), &fresco, &activation)?;
                if result == b"cancelled" {
                    let _ = sheet.child.emit(embed, result.to_vec());
                }
                println!(
                    "LEGACY_FIXTURE_{}",
                    std::str::from_utf8(result).unwrap().to_ascii_uppercase()
                );
                // Let the parent observe the public outcome before dropping the window.
                std::thread::sleep(Duration::from_millis(500));
                return Ok(());
            }
            _ => {}
        }
    }
    Err(closed())
}
