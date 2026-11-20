//! I1 counter qualification. Only the interpreter fixture currently accepts this URL.
use crate::{upstream, Config, Upstream};
use limen::{client::Parent, proto::ParentEvent};
use navigator_control::Ticket;
use std::{
    os::unix::net::UnixStream,
    path::Path,
    time::{Duration, Instant},
};

pub const COUNTER: &str = "https://widget.example/counter.html";

pub fn bounds(r: [u32; 4]) -> Result<[f32; 4], String> {
    let [x, y, w, h] = r;
    if w == 0 || h == 0 || x.saturating_add(w) > 65535 || y.saturating_add(h) > 65535 {
        return Err("widget has no visible placement".into());
    }
    Ok(r.map(|v| v as f32))
}

pub struct Widget {
    pub document: Ticket,
    parent: Parent,
    embed: u64,
}

impl Widget {
    pub fn start(
        socket: &Path,
        window: [u8; 16],
        document: Ticket,
        rect: [u32; 4],
        cfg: std::sync::Arc<Config>,
    ) -> Result<Self, String> {
        let rect = bounds(rect)?;
        let mut parent = Parent::at(socket).map_err(|e| format!("Limen: {e:?}"))?;
        let embed = parent
            .request_island(window, 1)
            .map_err(|e| format!("engine launch: {e:?}"))?;
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err("no fetch channel for the widget".into());
            }
            match parent.poll(left).map_err(|e| format!("engine: {e:?}"))? {
                Some(ParentEvent::IslandFetch { embed: id }) if id == embed => break,
                Some(ParentEvent::ChildLost { .. }) => {
                    return Err("widget stopped at launch".into())
                }
                _ => {}
            }
        }
        let channel = parent
            .take_island_fetch(embed)
            .ok_or("missing widget fetch socket")?;
        parent
            .place(embed, Some(rect))
            .map_err(|e| format!("place: {e:?}"))?;
        std::thread::spawn(move || {
            if let Err(e) = fetch(channel, &cfg) {
                eprintln!("navigatord: widget fetch: {e}");
            }
        });
        Ok(Self {
            document,
            parent,
            embed,
        })
    }

    pub fn place(&mut self, rect: [u32; 4]) -> Result<(), String> {
        self.parent
            .place(self.embed, Some(bounds(rect)?))
            .map_err(|e| format!("place widget: {e:?}"))
    }
}

fn fetch(socket: UnixStream, cfg: &Config) -> Result<(), String> {
    use nuntius::{Deliver, Event, Method, Request};
    let timeout = Some(Duration::from_secs(5));
    socket
        .set_read_timeout(timeout)
        .and_then(|_| socket.set_write_timeout(timeout))
        .map_err(|e| e.to_string())?;
    let (mut input, mut output) = nuntius::server::channel(socket).map_err(|e| e.to_string())?;
    let Some(Request::Open(open)) = input.recv().map_err(|e| e.to_string())? else {
        return Err("expected a document GET".into());
    };
    let id = open.id;
    if open.url != COUNTER
        || open.method != Method::Get
        || open.body.is_some()
        || open.range.is_some()
        || open.if_range.is_some()
        || open.deliver != Deliver::Stream
    {
        return Err("request outside the widget grant".into());
    }
    let Upstream::Fetchd(mut fetcher, _worker) = upstream(cfg, &crate::instance())? else {
        return Err("widget requires the plain fetch worker".into());
    };
    let body = fetcher.widget_document(COUNTER)?;
    let length = body.len() as u64;
    if length == 0 || length > open.limits.max_bytes.min(65536) {
        return Err("widget document size".into());
    }
    eprintln!("navigatord: widget GET {COUNTER} bytes={length}");
    let mut send = |event| output.send(event).map_err(|e| e.to_string());
    send(Event::Head {
        id,
        status: 200,
        final_url: COUNTER.into(),
        redirects: 0,
        content_type: Some("text/html".into()),
        length: Some(length),
        accept_ranges: false,
        validator: None,
    })?;
    for (i, bytes) in body.chunks(nuntius::DATA_MAX).enumerate() {
        send(Event::Data {
            id,
            offset: (i * nuntius::DATA_MAX) as u64,
            bytes: bytes.to_vec(),
        })?;
    }
    send(Event::Done { id, bytes: length })
}
