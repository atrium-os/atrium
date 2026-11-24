//! I1 counter qualification. Only the interpreter fixture currently accepts this URL.
use crate::{upstream, Config, Upstream};
use limen::{client::Parent, proto::ParentEvent};
use navigator_control::{Ticket, YOUTUBE_ENTRY, YOUTUBE_ENTRY_LIMIT};
use std::{
    os::unix::net::UnixStream,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub const COUNTER: &str = "https://widget.example/counter.html";

pub const VIDEO: &str = "https://widget.example/navigator-i4/local-player.html";
const VIDEO_RESOURCES: &[(&str, &str)] = &[
    (VIDEO, "text/html"),
    ("https://widget.example/navigator-i4/local-player.js", "text/javascript"),
    ("https://widget.example/media/red-blue.webm", "video/webm"),
    ("https://widget.example/media/tone-opus.webm", "audio/webm"),
];
const COUNTER_RESOURCES: &[(&str, &str)] = &[(COUNTER, "text/html")];

pub fn enabled_source(base: &str, src: &str) -> Option<&'static str> {
    let url = url::Url::parse(base).ok()?.join(src).ok()?;
    [(COUNTER, "NAVIGATORD_I1_COUNTER"), (VIDEO, "NAVIGATORD_I4_VIDEO"), (YOUTUBE_ENTRY, "NAVIGATORD_I4_YOUTUBE")].into_iter()
        .find(|(exact, setting)| url.as_str() == *exact && crate::settings::get(setting).as_deref() == Some(*exact))
        .map(|(exact, _)| exact)
}

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
    video: Option<crate::widget_video::Video>,
    pub answers: std::sync::mpsc::SyncSender<limen::web_island::UnloadAnswer>,
}

impl Widget {
    pub fn start(
        socket: &Path,
        source: &'static str,
        window: [u8; 16],
        document: Ticket,
        press: navigator_control::Press,
        rect: [u32; 4],
        cfg: Arc<Config>,
        attached: impl FnOnce() + Send + 'static,
        media_ready: impl FnOnce() + Send + 'static,
        unload: impl Fn(u32, bool) + Send + 'static,
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
        let playback = Arc::new(Mutex::new(None));
        let video = if matches!(source, VIDEO | YOUTUBE_ENTRY) {
            use std::os::fd::AsRawFd;
            let append_budget = if source == YOUTUBE_ENTRY {
                navigator_control::youtube::ASSET_BUDGET
            } else {
                65536
            };
            let (video, worker) = crate::widget_video::Video::start(socket, window, rect, append_budget, playback.clone())?;
            navigator_control::send_fd(&channel, worker.as_raw_fd()).map_err(|e| e.to_string())?;
            Some(video)
        } else { None };
        let parent = Arc::new(Mutex::new(Some(parent)));
        let watched = parent.clone();
        let (answers, received) = std::sync::mpsc::sync_channel::<limen::web_island::UnloadAnswer>(1);
        // A slow document GET must not delay Stop or placement retirement.
        let (done, fetched) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = fetch(channel, &cfg, source, playback);
            if let Err(e) = &result { eprintln!("navigatord: widget fetch: {e}"); }
            let _ = done.send(result);
        });
        std::thread::spawn(move || {
            use navigator_control::WidgetFailure;
            use limen::web_island::{EndReason, Lifecycle, Report};
            let granted_origin = crate::origin(source);
            let mut reports = Lifecycle::default();
            let mut question = None;
            let mut first_title = true;
            let mut first_resize = true;
            let mut media_ready = Some(media_ready);
            let mut attached = Some(attached);
            let failure = loop {
                let mut slot = watched.lock().unwrap();
                let Some(connection) = slot.as_mut() else { return };
                let answer = received.try_recv().ok();
                if let Some((query, deadline)) = question {
                    let choice = if Instant::now() >= deadline { Some(false) } else {
                        answer.filter(|a| a.query == query).map(|a| a.leave)
                    };
                    if let Some(leave) = choice {
                        let reply = limen::web_island::UnloadAnswer { query, leave };
                        if !reports.answer(reply) || connection.send(embed, reply.encode()).is_err() {
                            break Some(WidgetFailure::Exit);
                        }
                        question = None;
                        unload(query, false);
                        eprintln!("navigatord: widget unload {query} leave={leave}");
                    }
                }
                match connection.poll(Duration::from_millis(40)) {
                    Ok(Some(ParentEvent::Attached { embed: id })) if id == embed => {
                        if let Some(report) = attached.take() { report(); }
                    }
                    Ok(Some(ParentEvent::Message { embed: id, bytes })) if id == embed => {
                        match reports.receive(&bytes) {
                            Some(Report::Started { origin }) if granted_origin.as_deref() == Some(origin) => {
                                eprintln!("navigatord: widget reported start for {origin}");
                            }
                            Some(Report::MediaReady) => {
                                if let Some(notify) = media_ready.take() { notify(); }
                            }
                            Some(Report::BeforeUnload { query }) => {
                                question = Some((query, Instant::now() + Duration::from_secs(30)));
                                unload(query, true);
                            }
                            Some(Report::Title(title)) if first_title => {
                                eprintln!("navigatord: initial widget title received ({} bytes)", title.len());
                                first_title = false;
                            }
                            Some(Report::Resize { width, height }) if first_resize => {
                                eprintln!("navigatord: widget requested {width}x{height}; parent controls placement");
                                first_resize = false;
                            }
                            Some(Report::Title(_) | Report::Resize { .. }) => {}
                            Some(Report::Ended(EndReason::Closed)) => {
                                eprintln!("navigatord: widget reported voluntary close");
                                break None;
                            }
                            _ => break Some(WidgetFailure::Exit),
                        }
                    }
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
            answers,
            video,
            embed,
        })
    }

    pub fn seek(&self, seek: limen::web_island::ChapterSeek) -> Result<(), String> {
        let mut slot = self.parent.lock().unwrap();
        slot.as_mut().ok_or("island retired")?.send(self.embed, seek.encode()).map_err(|e| format!("{e:?}"))
    }

    pub fn place(&mut self, rect: [u32; 4]) -> Result<(), String> {
        if let Some(p) = self.parent.lock().unwrap().as_mut() {
            let rect = bounds(rect)?;
            if let Some(video) = &self.video { video.place(rect)?; }
            p.place(self.embed, Some(rect))
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

fn fetch(socket: UnixStream, cfg: &Config, source: &str, playback: Arc<Mutex<Option<Instant>>>) -> Result<(), String> {
    use nuntius::{Deliver, Event, Method, Request};
    let timeout = Some(if source == YOUTUBE_ENTRY {
        navigator_control::youtube::PLAYER_WINDOW
    } else {
        Duration::from_secs(5)
    });
    socket
        .set_read_timeout(timeout)
        .and_then(|_| socket.set_write_timeout(timeout))
        .map_err(|e| e.to_string())?;
    let (mut input, mut output) = nuntius::server::channel(socket).map_err(|e| e.to_string())?;
    let mut upstream = if source == YOUTUBE_ENTRY { None } else { Some(upstream(cfg, &crate::instance())?) };
    let resources = match source {
        COUNTER => COUNTER_RESOURCES,
        VIDEO => VIDEO_RESOURCES,
        YOUTUBE_ENTRY => &[(YOUTUBE_ENTRY, "text/html")],
        _ => return Err("unknown widget source".into()),
    };
    let deadline = Instant::now() + navigator_control::youtube::PLAYER_WINDOW;
    let mut provider = None;
    use navigator_control::youtube_media as media;
    let mut delivery = None::<String>;
    let mut media_posts = 0;
    let mut posts = 0;
    let mut remaining = navigator_control::youtube::ASSET_BUDGET;
    let requests = if source == YOUTUBE_ENTRY {
        1 + navigator_control::youtube::ASSET_COUNT + media::MAX_REQUESTS
    } else {
        resources.len()
    };
    for step in 0..requests {
        let request = match input.recv() {
            Ok(r) => r,
            Err(e) if source == YOUTUBE_ENTRY && step > 0 && matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => break,
            Err(e) => return Err(e.to_string())
        };
        if request.is_none() && source == YOUTUBE_ENTRY && step > 0 { break; }
        let Some(Request::Open(open)) = request else {
            return Err("expected a document GET".into());
        };
        let player = source == YOUTUBE_ENTRY && step > 0 && open.method == Method::Post
            && navigator_control::youtube::player_url(&open.url);
        let stream = source == YOUTUBE_ENTRY
            && open.method == Method::Post
            && delivery.as_deref().is_some_and(|grant| media::permits(grant, &open.url));
        if source == YOUTUBE_ENTRY && !stream && media::endpoint(&open.url).is_some() {
            // A CDN retry must not retire the island or discard earlier media.
            output.send(Event::Error { id: open.id, code: nuntius::ErrorCode::Policy })
                .map_err(|e| e.to_string())?;
            continue;
        }
        if source == YOUTUBE_ENTRY && !stream && step - media_posts > navigator_control::youtube::ASSET_COUNT {
            return Err("startup request count exhausted".into());
        }
        let mut media_kind = None;
        let payload = if player {
            let body = open.body.as_ref().ok_or("missing player request")?;
            if body.content_type != "application/json" || posts >= navigator_control::youtube::PLAYER_REQUESTS { return Err("outside player grant".into()); }
            posts += 1;
            Some(navigator_control::youtube::player_body(&body.bytes)?)
        } else if stream {
            let body = open.body.as_ref().ok_or("missing delivery request")?;
            if body.bytes.is_empty()
                || body.bytes.len() > media::BODY
                || body.content_type != "application/octet-stream"
            {
                return Err("outside delivery grant".into());
            }
            let kind = media::request_kind(media_posts).ok_or("delivery session exhausted")?;
            if kind == 5 {
                let recent = playback.lock().unwrap().take();
                if !recent.is_some_and(|t| t.elapsed() < Duration::from_secs(5)) {
                    output.send(Event::Error { id: open.id, code: nuntius::ErrorCode::Policy })
                        .map_err(|e| e.to_string())?;
                    continue;
                }
                eprintln!("navigatord: renewed delivery after {media_posts} media requests");
            }
            media_kind = Some(kind);
            media_posts += 1;
            Some(body.bytes.clone())
        } else {None};
        if (player || stream) && Instant::now() >= deadline { return Err("player grant expired".into()); }
        let (url, mime) = if stream {
            (open.url.as_str(), media::MIME)
        } else if player {
            (open.url.as_str(), "application/json")
        } else if source == YOUTUBE_ENTRY && step > 0 {
            let mime = navigator_control::youtube::asset_mime(&open.url).ok_or("unknown provider asset")?;
            (open.url.as_str(), mime)
        } else { resources[step] };
        let id = open.id;
        if open.url != url
            || (!player && !stream && (open.method != Method::Get || open.body.is_some()))
            || open.range.is_some()
            || open.if_range.is_some()
            || open.deliver != Deliver::Stream
        {
            return Err("request outside the widget grant".into());
        }
        let (body, limit) = if source == YOUTUBE_ENTRY {
            let limit = if stream {
                media::REPLY
            } else if player {
                navigator_control::youtube::PLAYER_RESPONSE_LIMIT
            } else if step == 0 {
                YOUTUBE_ENTRY_LIMIT
            } else {
                navigator_control::youtube::asset_limit(url)
            };
            if step > 0 && remaining == 0 { return Err("provider startup budget exhausted".into()); }
            if provider.is_none() { provider = Some(youtube_worker()?); }
            let (socket, _worker) = provider.as_mut().ok_or("provider worker missing")?;
            let body = youtube_resource(socket, url, payload.as_deref(), media_kind, if step == 0 {limit} else {limit.min(remaining)})?;
            if step > 0 { remaining -= body.len(); }
            (body, limit)
        } else {
            let Some(Upstream::Fetchd(fetcher, _)) = &mut upstream else {
                return Err("widget requires the plain fetch worker".into());
            };
            (fetcher.widget_resource(url, mime)?, 65536)
        };
        let length = body.len() as u64;
        if length == 0 || length > open.limits.max_bytes.min(limit as u64) {
            return Err("widget document size".into());
        }
        let method = if player { "POST" } else { "GET" };
        if stream {
            eprintln!("navigatord: media POST bytes={length}");
        } else {
            eprintln!("navigatord: widget {method} {url} bytes={length}");
        }
        if player {
            delivery = media::issued(&body);
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body) {
                let status = value["playabilityStatus"]["status"].as_str().unwrap_or("missing");
                let status = match status {
                    "OK" | "LOGIN_REQUIRED" | "ERROR" | "UNPLAYABLE" | "CONTENT_CHECK_REQUIRED" => status,
                    _ => "other",
                };
                let formats = value["streamingData"]["adaptiveFormats"].as_array().map_or(0, Vec::len);
                eprintln!("navigatord: player playability={status} adaptive_formats={formats}");
            }
        }
        let mut send = |event| output.send(event).map_err(|e| e.to_string());
        send(Event::Head {
            id,
            status: 200,
            final_url: url.into(),
            redirects: 0,
            content_type: Some(mime.into()),
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
        send(Event::Done { id, bytes: length })?;
    }
    Ok(())
}

fn youtube_worker() -> Result<(UnixStream, crate::worker::Worker), String> {
    let spec = crate::worker::Spec::Jail("org.atrium.navigator.youtube-entry".into());
    let (reader, peer) = UnixStream::pair().map_err(|e| e.to_string())?;
    reader.set_read_timeout(Some(Duration::from_secs(12))).map_err(|e| e.to_string())?;
    let worker = crate::worker::Worker::start_on(&spec, &format!("y{}", crate::instance()), peer.into())
        .map_err(|e| e.to_string())?;
    Ok((reader, worker))
}

fn youtube_resource(
    reader: &mut UnixStream,
    url: &str,
    post: Option<&[u8]>,
    media_kind: Option<u8>,
    limit: usize,
) -> Result<Vec<u8>, String> {
    use std::io::{Read, Write};

    if let Some(body) = post {
        if let Some(kind) = media_kind {
            reader.write_all(&[kind])
                .and_then(|_| reader.write_all(&(url.len() as u16).to_be_bytes()))
                .and_then(|_| reader.write_all(url.as_bytes()))
                .map_err(|e| e.to_string())?;
        } else {
            reader.write_all(&[3]).map_err(|e| e.to_string())?;
        }
        reader.write_all(&(body.len() as u16).to_be_bytes())
            .and_then(|_|reader.write_all(body)).map_err(|e|e.to_string())?;
    } else if url == YOUTUBE_ENTRY {
        reader.write_all(&[1]).map_err(|e| e.to_string())?;
    } else {
        reader.write_all(&[2]).and_then(|_| reader.write_all(&(url.len() as u16).to_be_bytes()))
            .and_then(|_| reader.write_all(url.as_bytes())).map_err(|e| e.to_string())?;
    }
    let mut header = [0; 4];
    reader.read_exact(&mut header).map_err(|e| format!("entry worker: {e}"))?;
    let len = u32::from_be_bytes(header) as usize;
    if !(1..=limit).contains(&len) { return Err("invalid entry length".into()); }
    let mut body = vec![0; len];
    reader.read_exact(&mut body).map_err(|e| e.to_string())?;
    Ok(body)
}
