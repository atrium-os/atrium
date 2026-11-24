//! Native player for the single-element local I4 fixture. The worker holds
//! only a private command socket. The broker owns the window and both slots.
use limen::{client::Parent, media_player::ToParent, proto::ParentEvent};
use navigator_control::video::{self, Reply, Request};
use navigator_media::mse::Media;
use std::{
    io,
    net::Shutdown,
    os::unix::net::UnixStream,
    path::Path,
    sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex},
    time::{Duration, Instant},
};

struct Slot {
    parent: Parent,
    embed: u64,
    island: [f32; 4],
    element: Option<[u16; 4]>,
}

impl Slot {
    fn place(&mut self) -> Result<(), String> {
        let rect = self.element.and_then(|[x, y, w, h]| {
            let [left, top, width, height] = self.island;
            let (x, y) = (f32::from(x), f32::from(y));
            let w = f32::from(w).min(width - x);
            let h = f32::from(h).min(height - y);
            (w > 0.0 && h > 0.0).then_some([left + x, top + y, w, h])
        });
        self.parent
            .place(self.embed, rect)
            .map_err(|e| format!("place video: {e:?}"))
    }
}

pub struct Video {
    socket: UnixStream,
    /// None while the player is still being launched, and again once
    /// retired (`retired` tells the two apart).
    slot: Arc<Mutex<Option<Slot>>>,
    /// The island's latest rectangle, for a player still being launched.
    island: Arc<Mutex<[f32; 4]>>,
    retired: Arc<AtomicBool>,
}

impl Video {
    pub fn start(
        path: &Path,
        window: [u8; 16],
        island: [f32; 4],
        append_budget: usize,
        playback: Arc<Mutex<Option<Instant>>>,
    ) -> Result<(Self, UnixStream), String> {
        // The engine's video socket is handed over at once, and the player
        // is launched beside the engine's own start rather than before it
        // (navigator-islands.md "I5 Start trace": the document fetch waited
        // for the player's jail). The engine's media requests queue on the
        // socket until the player is there.
        let (server, worker) = UnixStream::pair().map_err(|e| e.to_string())?;
        let socket = server.try_clone().map_err(|e| e.to_string())?;
        let slot = Arc::new(Mutex::new(None));
        let shared_island = Arc::new(Mutex::new(island));
        let retired = Arc::new(AtomicBool::new(false));
        let session = Session {
            playback,
            slot: slot.clone(),
            media: Media::new(),
            bytes: 0,
            append_budget,
            ended: false,
            traced: Default::default(),
        };
        let path = path.to_owned();
        let (launch_slot, launch_island, launch_retired) = (slot.clone(), shared_island.clone(), retired.clone());
        std::thread::spawn(move || {
            limen::start_trace("navigatord", "player-request", "");
            let launched = Parent::at(&path)
                .map_err(|e| format!("player connection: {e:?}"))
                .and_then(|mut parent| {
                    parent
                        .request_at("media-player", window, 2)
                        .map(|embed| (parent, embed))
                        .map_err(|e| format!("player launch: {e:?}"))
                });
            let (parent, embed) = match launched {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("navigatord: video: {e}");
                    launch_retired.store(true, Ordering::SeqCst);
                    let _ = server.shutdown(Shutdown::Both);
                    return;
                }
            };
            limen::start_trace("navigatord", "player-granted", &format!("embed={embed:016x}"));
            {
                let mut locked = launch_slot.lock().unwrap();
                if launch_retired.load(Ordering::SeqCst) {
                    return;
                }
                let island = *launch_island.lock().unwrap();
                *locked = Some(Slot { parent, embed, island, element: None });
            }
            if let Err(e) = session.serve(server) {
                eprintln!("navigatord: video: {e}");
            }
            launch_retired.store(true, Ordering::SeqCst);
        });
        Ok((Self { socket, slot, island: shared_island, retired }, worker))
    }

    pub fn place(&self, rect: [f32; 4]) -> Result<(), String> {
        *self.island.lock().unwrap() = rect;
        let mut locked = self.slot.lock().unwrap();
        match locked.as_mut() {
            Some(slot) => {
                slot.island = rect;
                slot.place()
            }
            None if self.retired.load(Ordering::SeqCst) => Err("video retired".into()),
            // Still launching: the player takes the latest rectangle.
            None => Ok(()),
        }
    }
}

impl Drop for Video {
    fn drop(&mut self) {
        self.retired.store(true, Ordering::SeqCst);
        let _ = self.socket.shutdown(Shutdown::Both);
        self.slot.lock().unwrap().take();
    }
}

struct Session {
    playback: Arc<Mutex<Option<Instant>>>,
    slot: Arc<Mutex<Option<Slot>>>,
    media: Media,
    bytes: usize,
    append_budget: usize,
    ended: bool,
    /// Start-trace stages already printed (each prints once).
    traced: std::collections::HashSet<&'static str>,
}

impl Session {
    /// One `start-trace` line per stage, the first time it happens.
    fn trace(&mut self, stage: &'static str) {
        if self.traced.insert(stage) {
            limen::start_trace("navigatord", stage, "");
        }
    }

    fn serve(mut self, mut socket: UnixStream) -> Result<(), String> {
        socket
            .set_write_timeout(Some(Duration::from_secs(3)))
            .map_err(|e| e.to_string())?;
        let result = loop {
            match video::receive(&mut socket) {
                Ok(req) => match self.handle(req) {
                    Ok(reply) => {
                        if let Err(e) = video::send(&mut socket, &reply) {
                            break Err(e.to_string());
                        }
                    }
                    Err(e) => {
                        let _ = video::send(&mut socket, &Reply::Failed);
                        break Err(e);
                    }
                },
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break Ok(()),
                Err(e) => break Err(e.to_string()),
            }
        };
        self.playback.lock().unwrap().take();
        self.slot.lock().unwrap().take();
        result
    }

    fn flush(&mut self) -> Result<(), String> {
        let mut locked = self.slot.lock().unwrap();
        let s = locked.as_mut().ok_or("video retired")?;
        for message in self.media.take_out() {
            s.parent
                .send(s.embed, message.encode())
                .map_err(|e| format!("player send: {e:?}"))?;
        }
        self.media.take_events();
        Ok(())
    }

    fn until(&mut self, done: impl Fn(&Media) -> bool) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done(&self.media) {
            if Instant::now() >= deadline {
                return Err("player acknowledgement timed out".into());
            }
            self.poll_player(Duration::from_millis(30))?;
        }
        Ok(())
    }

    fn poll_player(&mut self, timeout: Duration) -> Result<bool, String> {
        let (embed, ev) = {
            let mut locked = self.slot.lock().unwrap();
            let s = locked.as_mut().ok_or("video retired")?;
            (
                s.embed,
                s.parent
                    .poll(timeout)
                    .map_err(|e| format!("player poll: {e:?}"))?,
            )
        };
        let received = ev.is_some();
        match ev {
            Some(ParentEvent::Message { embed: id, bytes }) if id == embed => {
                let m = ToParent::decode(&bytes).ok_or("malformed player message")?;
                if matches!(
                    m,
                    ToParent::Error { .. }
                        | ToParent::CanPlay {
                            supported: false,
                            ..
                        }
                ) {
                    return Err(format!("player refused video: {m:?}"));
                }
                match &m {
                    ToParent::Ready { .. } => self.trace("player-ready"),
                    ToParent::CanPlay { .. } => self.trace("player-canplay"),
                    _ => {}
                }
                match m {
                    ToParent::Time { secs } if !self.media.paused && !self.media.seeking && secs > self.media.current_time => {
                        *self.playback.lock().unwrap() = Some(Instant::now());
                        self.trace("playing");
                    }
                    ToParent::Seeking { .. } | ToParent::Paused | ToParent::Ended => {
                        self.playback.lock().unwrap().take();
                    }
                    _ => {}
                }
                self.media.from_player(m);
                self.flush()?;
            }
            Some(ParentEvent::ChildLost { .. } | ParentEvent::Detached { .. }) => {
                return Err("player stopped".into());
            }
            _ => {}
        }
        Ok(received)
    }

    fn track(&self, mime: &str) -> Option<u8> {
        (0..2).find(|&id| self.media.source_type(id) == Some(mime))
    }

    fn add_track(&mut self, mime: &str) -> Result<Reply, String> {
        if self.ended || self.track(mime).is_some() {
            return Ok(Reply::Failed);
        }
        self.until(|media| media.supports_cached_type(mime))?;
        self.media
            .add_source_buffer(mime)
            .map_err(|e| format!("add track: {e:?}"))?;
        self.flush()?;
        Ok(Reply::Ok)
    }

    fn append_track(&mut self, mime: &str, bytes: Vec<u8>) -> Result<Reply, String> {
        let id = self.track(mime).ok_or("unregistered media track")?;
        if self.ended
            || bytes.is_empty()
            || bytes.len() > video::MAX_APPEND
            || self.bytes + bytes.len() > self.append_budget
        {
            return Err("append outside local video grant".into());
        }
        self.media
            .append_buffer(id, &bytes)
            .map_err(|e| format!("append track: {e:?}"))?;
        self.bytes += bytes.len();
        self.flush()?;
        self.until(|media| !media.updating(id))?;
        Ok(Reply::Appended {
            ranges: self.media.source_buffered(id),
        })
    }

    fn handle(&mut self, req: Request) -> Result<Reply, String> {
        let dom = |e| format!("media operation refused: {e:?}");
        match req {
            Request::Open { rect } if rect[2] > 0 && rect[3] > 0 => {
                self.trace("video-open");
                self.media.attach_source().map_err(dom)?;
                {
                    let mut locked = self.slot.lock().unwrap();
                    let s = locked.as_mut().ok_or("video retired")?;
                    s.element = Some(rect);
                    s.place()?;
                }
                self.media.prime(&[video::MIME, video::OPUS_MIME]);
                self.flush()?;
                Ok(Reply::Ok)
            }
            Request::AddVp9 => { self.trace("add-vp9"); self.add_track(video::MIME) }
            Request::AddOpus => { self.trace("add-opus"); self.add_track(video::OPUS_MIME) }
            Request::Append(bytes) => {
                self.trace("append-vp9");
                let r = self.append_track(video::MIME, bytes);
                self.trace("appended-vp9");
                r
            }
            Request::AppendOpus(bytes) => {
                self.trace("append-opus");
                let r = self.append_track(video::OPUS_MIME, bytes);
                self.trace("appended-opus");
                r
            }
            Request::SetPlaying(playing) => {
                if playing { self.trace("set-playing"); }
                if playing {
                    self.media.play().map_err(dom)?;
                } else {
                    self.playback.lock().unwrap().take();
                    self.media.pause();
                }
                self.flush()?;
                Ok(Reply::Ok)
            }
            Request::Volume(volume) if volume.is_finite() && (0.0..=1.0).contains(&volume) => {
                self.media.set_volume(volume).map_err(dom)?;
                self.flush()?;
                Ok(Reply::Ok)
            }
            Request::Clock | Request::FrameCounts => {
                // Handle queued native clock and state updates without waiting for a new tick.
                for _ in 0..32 {
                    if !self.poll_player(Duration::ZERO)? {
                        break;
                    }
                }
                if matches!(req, Request::FrameCounts) {
                    let (presented, dropped) = self.media.frame_counts;
                    return Ok(Reply::FrameCounts { presented, dropped });
                }
                let secs = self.media.current_time;
                if self.media.ended {
                    Ok(Reply::EndedPosition { secs })
                } else {
                    Ok(Reply::Position { secs })
                }
            }
            Request::End if self.bytes > 0 && !self.ended => {
                self.media.end_of_stream(None).map_err(dom)?;
                self.flush()?;
                self.until(|m| m.ready_state >= 1)?;
                self.ended = true;
                Ok(Reply::Ready {
                    duration: self.media.duration,
                })
            }
            Request::Seek(t) if self.media.ready_state >= 1 && limen::media_player::finite_secs(t) => {
                self.playback.lock().unwrap().take();
                self.media.seek(t).map_err(dom)?;
                self.flush()?;
                self.until(|m| !m.seeking)?;
                Ok(Reply::Seeked {
                    secs: self.media.current_time,
                })
            }
            _ => Err("operation outside local video grant".into()),
        }
    }
}
