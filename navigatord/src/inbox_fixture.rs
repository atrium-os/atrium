//! Opt-in I0 whole-page fixture opened by the signed Navigator app.
use fresco_client::{Connection, Event};
use fresco_protocol::WindowHints;
use limen::{client::Parent, proto::ParentEvent};
use navigator_cookies::{ArcadSocket, Broker, Fetch, Launcher, Why};
use std::{
    os::fd::AsFd,
    path::PathBuf,
    time::{Duration, Instant},
};

use navigator_control::INBOX_FIXTURE_URL as URL;
type Transport = Broker<ArcadSocket, Launcher>;

fn document(broker: &mut Transport, url: &str) -> Result<std::os::fd::OwnedFd, String> {
    let (mime, ask) = match url {
        URL => ("text/html", Fetch::navigation(URL, None, true)),
        "https://inbox.example/inbox.js" => {
            ("text/javascript", Fetch::subresource(url, "inbox.example"))
        }
        "https://inbox.example/inbox.css" => ("text/css", Fetch::subresource(url, "inbox.example")),
        "https://inbox.example/api/inbox" => {
            ("application/json", Fetch::subresource(url, "inbox.example"))
        }
        _ => return Err("resource outside fixture grant".into()),
    };
    let response = broker
        .fetch_once(1, &ask)
        .map_err(|e| format!("courier: {e:?}"))?;
    if response.status != 200
        || response.body.is_empty()
        || response.body.len() > 65536
        || !response.headers.iter().any(|(k, v)| {
            k.eq_ignore_ascii_case("content-type") && v.split(';').next() == Some(mime)
        })
    {
        return Err("unexpected status, MIME type or size".into());
    }
    limen::fdpass::sealed_file("inbox-resource", &response.body).map_err(|e| e.to_string())
}

fn run(socket: PathBuf, mut broker: Transport) -> Result<(), String> {
    let mut window = Connection::connect_default().map_err(|e| e.to_string())?;
    let id = window
        .window_create(
            640,
            360,
            "Navigator · inbox.example",
            WindowHints::default(),
        )
        .map_err(|e| e.to_string())?;
    window.set_default_window(u16::try_from(id).map_err(|e| e.to_string())?);
    let mut frame = window.frame().map_err(|e| e.to_string())?;
    frame
        .rect(fresco_protocol::RectParams {
            x: 0.,
            y: 0.,
            w: 640.,
            h: 360.,
            r: 1.,
            g: 1.,
            b: 1.,
            a: 1.,
            radius: 0.,
        })
        .map_err(|e| e.to_string())?;
    frame.finish().map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let token = loop {
        let wait = deadline.saturating_duration_since(Instant::now());
        if wait.is_zero() {
            return Err("no window token".into());
        }
        if let Some(Event::EmbedToken { window_id, token }) =
            window.wait_event(Some(wait)).map_err(|e| e.to_string())?
        {
            if window_id == id {
                break token;
            }
        }
    };
    let mut parent = Parent::at(socket).map_err(|e| format!("Limen: {e:?}"))?;
    broker.show(1, URL).map_err(|e| format!("{e:?}"))?;
    broker
        .set_cookied(1, Why::Reader)
        .map_err(|e| format!("{e:?}"))?;
    let doc = document(&mut broker, URL)?;
    let page = parent
        .request("navigator-inbox-page", token)
        .map_err(|e| format!("request: {e:?}"))?;
    parent
        .place(page, Some([0., 0., 640., 360.]))
        .map_err(|e| format!("place: {e:?}"))?;
    parent
        .send_fd(page, URL.as_bytes().to_vec(), doc.as_fd())
        .map_err(|e| format!("document: {e:?}"))?;
    let deadline = Instant::now() + Duration::from_secs(300);
    let mut requests = 0;
    while Instant::now() < deadline {
        if matches!(
            window.wait_event(Some(Duration::from_millis(1))),
            Ok(Some(Event::CloseRequested { .. })) | Err(_)
        ) {
            break;
        }
        match parent
            .poll(Duration::from_millis(50))
            .map_err(|e| format!("page: {e:?}"))?
        {
            Some(ParentEvent::Message { embed, bytes }) if embed == page && bytes != b"frame" => {
                requests += 1;
                if requests > 16 {
                    return Err("too many resource requests".into());
                }
                let url = std::str::from_utf8(&bytes).map_err(|_| "invalid URL")?;
                if url == URL {
                    return Err("unexpected document reload".into());
                }
                let fd = document(&mut broker, url)?;
                parent
                    .send_fd(page, bytes, fd.as_fd())
                    .map_err(|e| format!("resource: {e:?}"))?;
            }
            Some(ParentEvent::ChildLost { .. }) => return Err("The live page worker stopped".into()),
            Some(ParentEvent::Detached { .. }) => break,
            _ => {}
        }
    }
    Ok(())
}

pub fn start(socket: PathBuf, broker: Transport, failed: impl FnOnce(String) + Send + 'static) {
    std::thread::spawn(move || {
        if let Err(e) = run(socket, broker) {
            eprintln!("navigatord: inbox fixture: {e}");
            failed(e);
        }
    });
}
