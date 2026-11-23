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
    sync::{Arc, Mutex},
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
    slot: Arc<Mutex<Option<Slot>>>,
}

impl Video {
    pub fn start(
        path: &Path,
        window: [u8; 16],
        island: [f32; 4],
    ) -> Result<(Self, UnixStream), String> {
        let mut parent = Parent::at(path).map_err(|e| format!("player connection: {e:?}"))?;
        let embed = parent
            .request_at("media-player", window, 2)
            .map_err(|e| format!("player launch: {e:?}"))?;
        let slot = Arc::new(Mutex::new(Some(Slot {
            parent,
            embed,
            island,
            element: None,
        })));
        let (server, worker) = UnixStream::pair().map_err(|e| e.to_string())?;
        let socket = server.try_clone().map_err(|e| e.to_string())?;
        let session = Session {
            slot: slot.clone(),
            media: Media::new(),
            bytes: 0,
            ended: false,
        };
        std::thread::spawn(move || {
            if let Err(e) = session.serve(server) {
                eprintln!("navigatord: video: {e}");
            }
        });
        Ok((Self { socket, slot }, worker))
    }

    pub fn place(&self, rect: [f32; 4]) -> Result<(), String> {
        let mut locked = self.slot.lock().unwrap();
        let slot = locked.as_mut().ok_or("video retired")?;
        slot.island = rect;
        slot.place()
    }
}

impl Drop for Video {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(Shutdown::Both);
        self.slot.lock().unwrap().take();
    }
}

struct Session {
    slot: Arc<Mutex<Option<Slot>>>,
    media: Media,
    bytes: usize,
    ended: bool,
}

impl Session {
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
            || self.bytes + bytes.len() > 65536
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
            Request::AddVp9 => self.add_track(video::MIME),
            Request::AddOpus => self.add_track(video::OPUS_MIME),
            Request::Append(bytes) => self.append_track(video::MIME, bytes),
            Request::AppendOpus(bytes) => self.append_track(video::OPUS_MIME, bytes),
            Request::SetPlaying(playing) => {
                if playing {
                    self.media.play().map_err(dom)?;
                } else {
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
            Request::Clock => {
                // Handle queued native clock and state updates without waiting for a new tick.
                for _ in 0..32 {
                    if !self.poll_player(Duration::ZERO)? {
                        break;
                    }
                }
                Ok(Reply::Position {
                    secs: self.media.current_time,
                })
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
            Request::Seek(t) if self.ended && limen::media_player::finite_secs(t) => {
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
