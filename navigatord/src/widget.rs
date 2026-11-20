//! I1 counter qualification. Only the interpreter fixture currently accepts this URL.
use crate::{upstream, Config, Upstream};
use limen::{client::Parent, proto::ParentEvent};
use navigator_control::Ticket;
use std::{
    os::unix::net::UnixStream,
    path::Path,
    sync::{Arc, Mutex},
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
    pub press: navigator_control::Press,
    parent: Arc<Mutex<Option<Parent>>>,
    embed: u64,
}

impl Widget {
    pub fn start(
        socket: &Path,
        window: [u8; 16],
        document: Ticket,
        press: navigator_control::Press,
        rect: [u32; 4],
        cfg: Arc<Config>,
        finished: impl FnOnce(Option<navigator_control::WidgetFailure>) + Send + 'static,
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
        let parent = Arc::new(Mutex::new(Some(parent)));
        let watched = parent.clone();
        // A slow document GET must not delay Stop or placement retirement.
        let (done, fetched) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = fetch(channel, &cfg);
            if let Err(e) = &result { eprintln!("navigatord: widget fetch: {e}"); }
            let _ = done.send(result);
        });
        std::thread::spawn(move || {
            use navigator_control::WidgetFailure;
            let failure = loop {
                let mut slot = watched.lock().unwrap();
                let Some(connection) = slot.as_mut() else { return };
                match connection.poll(Duration::from_millis(40)) {
                    Ok(Some(ParentEvent::IslandStopped { embed: id })) if id == embed => break None,
                    Ok(Some(ParentEvent::ChildLost { .. } | ParentEvent::Detached { .. })) | Err(_) => {
                        // Fetch closes the child channel on refusal. The exit and
                        // the fetch result may reach these two sockets in either order.
                        let refused = matches!(fetched.recv_timeout(Duration::from_millis(100)), Ok(Err(_)));
                        break Some(if refused { WidgetFailure::Fetch } else { WidgetFailure::Exit });
                    }
                    Ok(Some(ParentEvent::MessageFd { .. })) => { drop(connection.take_fd()); }
                    _ => {}
                }
                if let Ok(Err(_)) = fetched.try_recv() { break Some(WidgetFailure::Fetch); }
                drop(slot);
                std::thread::sleep(Duration::from_millis(5));
            };
            let retired = watched.lock().unwrap().take();
            if retired.is_some() {
                drop(retired);
                finished(failure);
            }
        });
        Ok(Self {
            document,
            press,
            parent,
            embed,
        })
    }

    pub fn place(&mut self, rect: [u32; 4]) -> Result<(), String> {
        if let Some(p) = self.parent.lock().unwrap().as_mut() {
            p.place(self.embed, Some(bounds(rect)?))
                .map_err(|e| format!("place widget: {e:?}"))?;
        }
        Ok(())
    }
}

impl Drop for Widget {
    fn drop(&mut self) {
        self.parent.lock().unwrap().take();
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
