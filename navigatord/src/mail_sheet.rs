//! Mail supplies an Arca ticket. Only the document and its final URL go to the
//! page worker. Each connection to Limen outlives the window it owns.
use crate::Cookies;
use fresco_client::{Connection, Event};
use fresco_protocol::WindowHints;
use limen::client::{Child, Parent};
use limen::proto::{ChildEvent, ParentEvent};
use std::collections::HashMap;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use limen::sign_in::CredentialMessage;
use sigillum_webauthn::{DomError, Origin};

struct Sheet {
    origin: Option<Origin>,
    credential: Option<crate::credential_get::Pending>,
    last_request: u32,
    assertion_issued: bool,
    window: Connection,
    token: [u8; 16],
    page: Option<(Parent, u64)>,
    ticket: Option<[u8; 16]>,
    fixture_requests: Option<u8>,
    started: Instant,
    view: u64,
    cookies: Cookies,
}

impl Drop for Sheet {
    fn drop(&mut self) {
        self.credential.take();
        if let Ok(mut broker) = self.cookies.lock() {
            broker.close_mail_sheet(self.view);
        }
    }
}

// The sheet owns this control outside the page worker’s rectangle.
const CANCEL_X: f32 = 24.;
const CANCEL_Y: f32 = 448.;
const CANCEL_W: f32 = 146.;
const CANCEL_H: f32 = 40.;

fn cancel_requested(window: &mut Connection) -> std::io::Result<bool> {
    for _ in 0..32 {
        match window.poll_event()? {
            Some(Event::CloseRequested { .. } | Event::Key { hid_usage: 41, pressed: true, .. }) => return Ok(true),
            Some(Event::PointerButton { x, y, button: 1, pressed: true, .. })
                if (CANCEL_X..CANCEL_X + CANCEL_W).contains(&x) && (CANCEL_Y..CANCEL_Y + CANCEL_H).contains(&y) => return Ok(true),
            None => break,
            _ => {}
        }
    }
    Ok(false)
}

fn window(view: u64, cookies: Cookies) -> Result<Sheet, ()> {
    let mut window = Connection::connect_default().map_err(|_| ())?;
    window.set_read_timeout(Some(Duration::from_secs(3))).map_err(|_| ())?;
    let id = window.window_create(720, 540, "Microsoft sign-in", WindowHints { embed_pending: true, ..Default::default() }).map_err(|_| ())?;
    window.set_default_window(u16::try_from(id).map_err(|_| ())?);
    let font = window.font_open("system-sans").map_err(|_| ())?.font_id;
    let mut frame = window.frame().map_err(|_| ())?;
    frame.rect(fresco_protocol::RectParams {
        x: 0., y: 0., w: 720., h: 540., r: 1., g: 1., b: 1., a: 1., radius: 0.,
    }).map_err(|_| ())?;
    frame.rect(fresco_protocol::RectParams {
        x: CANCEL_X, y: CANCEL_Y, w: CANCEL_W, h: CANCEL_H, r: 0.92, g: 0.94, b: 0.95, a: 1., radius: 5.,
    }).map_err(|_| ())?;
    frame.text_run(font, 16., CANCEL_X + 15., CANCEL_Y + 24., [0.08, 0.12, 0.15, 1.], "Cancel sign-in").map_err(|_| ())?;
    frame.finish().map_err(|_| ())?;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(3) {
        if let Some(Event::EmbedToken { window_id, token }) = window.wait_event(Some(Duration::from_millis(100))).map_err(|_| ())? {
            if window_id == id { return Ok(Sheet { origin: None, credential: None, last_request: 0, assertion_issued: false, window, token, page: None, ticket: None, fixture_requests: None, started, view, cookies }); }
        }
    }
    Err(())
}

fn page(sheet: &mut Sheet, view: u64, ticket: [u8; 16], cookies: &Cookies, socket: &Path) -> Result<(), ()> {
    let result = {
        let mut broker = cookies.lock().map_err(|_| ())?;
        let url = broker.arca.claim_mail_sign_in(&ticket).map_err(|_| ())?;
        broker.mail_first_screen(view, &ticket, &url).map_err(|_| ())?
    };
    if result.status != 200 || result.body.is_empty() || result.body.len() > 2 * 1024 * 1024
        || !result.headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("content-type") && v.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case("text/html")) {
        return Err(());
    }
    let body = limen::fdpass::sealed_file("sign-in-document", &result.body).map_err(|_| ())?;
    let channel = UnixStream::connect(socket).map_err(|_| ())?;
    let mut parent = Parent::wrap(aqueduct::Connection::wrap(channel).map_err(|_| ())?);
    let worker = parent.request("mail-sign-in-page", sheet.token).map_err(|_| ())?;
    parent.place(worker, Some([24., 36., 640., 360.])).map_err(|_| ())?;
    if result.url == "https://login.microsoftonline.com/first"
        && crate::settings::get("NAVIGATORD_MAIL_CONTINUATION_FIXTURE").as_deref() == Some("1") {
        sheet.fixture_requests = Some(0);
    }
    sheet.origin = url::Url::parse(&result.url).ok().and_then(|url| Origin::parse(&url.origin().ascii_serialization())).filter(|o| o.is_secure());
    parent.send_fd(worker, result.url.into_bytes(), body.as_fd()).map_err(|_| ())?;
    sheet.ticket = Some(ticket);
    sheet.page = Some((parent, worker));
    Ok(())
}

fn fixture_resource(cookies: &Cookies, view: u64, ticket: &[u8; 16], url: &[u8]) -> Result<Option<std::os::fd::OwnedFd>, ()> {
    let url = std::str::from_utf8(url).map_err(|_| ())?;
    let mime = match url {
        "https://login.microsoftonline.com/atrium-fixture/account.js" => "text/javascript",
        "https://login.microsoftonline.com/atrium-fixture/account.css" => "text/css",
        "https://login.microsoftonline.com/atrium-fixture/continue" => "application/json",
        _ => return Err(()),
    };
    let ask = navigator_cookies::Fetch::subresource(url, "login.microsoftonline.com");
    let mut broker = cookies.lock().map_err(|_| ())?;
    let result = if mime == "application/json" { broker.mail_continuation(view) } else { broker.fetch_once(view, &ask) }.map_err(|_| ())?;
    if mime == "application/json" && result.status == 302 {
        let mut locations = result.headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("location"));
        let (_, url) = locations.next().ok_or(())?;
        if locations.next().is_some() { return Err(()); }
        broker.couriers.redeem_mail(ticket, url).map_err(|_| ())?;
        return Ok(None);
    }
    if result.status != 200 || result.body.is_empty() || result.body.len() > 65536
        || !result.headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("content-type")
            && v.split(';').next().is_some_and(|v| v.trim().eq_ignore_ascii_case(mime))) {
        return Err(());
    }
    limen::fdpass::sealed_file("sign-in-resource", &result.body).map(Some).map_err(|_| ())
}

fn serve(socket: &Path, cookies: &Cookies) -> Result<(), ()> {
    let mut child = Child::provider(socket, "mail-sign-in").map_err(|_| ())?;
    let mut sheets = HashMap::<u64, Sheet>::new();
    loop {
        let ev = child.poll(Duration::from_millis(100)).map_err(|_| ())?;
        match ev {
            Some(ChildEvent::Attach { embed, role, parent_app, .. }) if role == "mail-sign-in" && parent_app == "org.atrium.mail" && sheets.len() < 8 => {
                let sheet = window(embed, cookies.clone())?;
                child.ready(embed, sheet.token).map_err(|_| ())?;
                sheets.insert(embed, sheet);
            }
            Some(ChildEvent::Message { embed, bytes }) => {
                if let Some(sheet) = sheets.get_mut(&embed) {
                    let loaded = bytes.as_slice().try_into().ok()
                        .filter(|_| sheet.page.is_none())
                        .is_some_and(|ticket| page(sheet, embed, ticket, cookies, socket).is_ok());
                    if !loaded {
                        child.emit(embed, b"failed".to_vec()).map_err(|_| ())?;
                        sheets.remove(&embed);
                    }
                }
            }
            Some(ChildEvent::Detaching { embed }) => {
                sheets.remove(&embed);
            }
            _ => {}
        }
        let mut closed = Vec::new();
        let mut cancelled = Vec::new();
        let mut completed = Vec::new();
        for (&id, sheet) in &mut sheets {
            match cancel_requested(&mut sheet.window) {
                Ok(true) => { cancelled.push(id); continue; }
                Err(_) => { closed.push(id); continue; }
                Ok(false) => {}
            }
            if sheet.started.elapsed() >= Duration::from_secs(600) { closed.push(id); continue; }
            if let Some((parent, worker)) = &mut sheet.page {
                match parent.poll(Duration::ZERO) {
                    Ok(Some(ParentEvent::ChildLost { embed } | ParentEvent::Detached { embed })) if embed == *worker => closed.push(id),
                    Ok(Some(ParentEvent::Message { embed, bytes })) if embed == *worker && limen::sign_in::decode(&bytes).is_some() => {
                        match limen::sign_in::decode(&bytes) {
                            Some(CredentialMessage::Get { id: request, options }) if request > sheet.last_request && sheet.credential.is_none() => {
                                sheet.last_request = request;
                                sheet.assertion_issued = false;
                                let pending = sheet.origin.as_ref().ok_or(DomError::Security)
                                    .and_then(|origin| crate::credential_get::Pending::start(request, origin, options));
                                match pending {
                                    Ok(pending) => sheet.credential = Some(pending),
                                    Err(error) => {
                                        let reply = CredentialMessage::Error { id: request, name: error.name() };
                                        if !reply.encode().is_some_and(|wire| parent.send(embed, wire).is_ok()) { closed.push(id); }
                                    }
                                }
                            }
                            Some(CredentialMessage::Abort { id: request }) if sheet.credential.as_ref().is_some_and(|c| c.id == request) => {
                                sheet.credential.take();
                            }
                            Some(CredentialMessage::Abort { id: request }) if request == sheet.last_request => {
                                sheet.assertion_issued = false;
                            }
                            Some(CredentialMessage::SubmitFixture { body }) if sheet.fixture_requests.is_some() && std::mem::take(&mut sheet.assertion_issued) => {
                                let redeemed = sheet.ticket.as_ref().is_some_and(|ticket| {
                                    let Ok(mut broker) = cookies.lock() else { return false };
                                    let Ok(result) = broker.mail_assertion(sheet.view, body) else { return false };
                                    let mut locations = result.headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("location"));
                                    result.status == 302 && locations.next().is_some_and(|(_, url)| locations.next().is_none() && broker.couriers.redeem_mail(ticket, url).is_ok())
                                });
                                if redeemed { completed.push(id); } else { closed.push(id); }
                            }
                            _ => closed.push(id),
                        }
                    }
                    Ok(Some(ParentEvent::Message { embed, bytes })) if embed == *worker && bytes != b"frame" => {
                        let sent = sheet.fixture_requests.as_mut().filter(|count| **count < 16)
                            .is_some_and(|count| {
                                *count += 1;
                                match sheet.ticket.as_ref().and_then(|t| fixture_resource(cookies, sheet.view, t, &bytes).ok()) {
                                    Some(Some(fd)) => parent.send_fd(embed, bytes, fd.as_fd()).is_ok(),
                                    Some(None) => { completed.push(id); true }
                                    None => false,
                                }
                            });
                        if !sent { closed.push(id); }
                    }
                    Err(_) => closed.push(id),
                    _ => {}
                }
            }
            if let Some((request, result)) = sheet.credential.as_ref().and_then(|c| Some((c.id, c.poll()?))) {
                sheet.credential.take();
                if closed.contains(&id) { continue; }
                if let Some((parent, worker)) = &mut sheet.page {
                    sheet.assertion_issued = result.is_ok();
                    let reply = match &result {
                        Ok(json) => CredentialMessage::Assertion { id: request, json },
                        Err(error) => CredentialMessage::Error { id: request, name: error.name() },
                    };
                    if !reply.encode().is_some_and(|wire| parent.send(*worker, wire).is_ok()) { closed.push(id); }
                }
            }
        }
        for id in cancelled {
            child.emit(id, b"cancelled".to_vec()).map_err(|_| ())?;
            sheets.remove(&id);
        }
        for id in completed {
            child.emit(id, b"complete".to_vec()).map_err(|_| ())?;
            sheets.remove(&id);
        }
        for id in closed {
            child.emit(id, b"failed".to_vec()).map_err(|_| ())?;
            sheets.remove(&id);
        }
    }
}

pub fn start(socket: PathBuf, cookies: Cookies) {
    std::thread::spawn(move || loop {
        let _ = serve(&socket, &cookies);
        std::thread::sleep(Duration::from_secs(3));
    });
}
