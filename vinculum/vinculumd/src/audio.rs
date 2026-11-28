//! A single authorized Classic output. The socket given to the driver is
//! scoped to this link's AVDTP PSM. No handle, CID or PSM comes from the driver.
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::{
    fs::PermissionsExt,
    net::{UnixDatagram, UnixListener, UnixStream},
};
use vinculum_control::{
    audio::{FromHost, ToHost},
    Reader,
};
use vinculum_host::{Host, HostEvent};
use vinculum_sdp::{self as sdp, Element, Query};

const AVDTP: u16 = 0x19;
const AUDIO_SINK: u16 = 0x110b;
const MAX_SDU: usize = 672;
const SDP_WAIT: u64 = 10_000;

pub enum Fd {
    Listener,
    Worker,
    Media,
}

struct Discovery {
    cid: u16,
    query: Query,
    first: Option<Vec<u8>>,
    until: u64,
}

struct Sink {
    handle: u16,
    id: u64,
    name: String,
    sdp: Option<Discovery>,
    signal: Option<u16>,
    attached: bool,
    media: Option<u16>,
    media_fd: Option<UnixDatagram>,
    closing: bool,
}

pub struct Audio {
    listener: Option<UnixListener>,
    uid: u32,
    worker: Option<UnixStream>,
    reader: Reader,
    sink: Option<Sink>,
    // Released on disconnect or revocation. Each entry denotes a live link.
    missing_profile: std::collections::HashSet<u16>,
}

fn bad(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

fn sink_record(r: &sdp::Record) -> bool {
    let Some(Element::Seq(classes)) = sdp::get(r, 1) else {
        return false;
    };
    let Some(Element::Seq(protocols)) = sdp::get(r, 4) else {
        return false;
    };
    classes.contains(&Element::Uuid(sdp::uuid16(AUDIO_SINK)))
        && matches!(protocols.as_slice(), [Element::Seq(l2cap), Element::Seq(avdtp)]
            if matches!(l2cap.as_slice(), [Element::Uuid(id), Element::Uint(0x19)] if *id == sdp::uuid16(0x100))
            && matches!(avdtp.as_slice(), [Element::Uuid(id), Element::Uint(0x0100..=0x01ff)] if *id == sdp::uuid16(AVDTP)))
}

impl Audio {
    pub fn new(path: Option<&str>, uid: u32) -> io::Result<Self> {
        let listener = if let Some(path) = path {
            #[cfg(feature = "atrium")]
            let (listener, bound) = {
                let (listener, origin) = atrium_listen::listener(path)?;
                (listener, origin == atrium_listen::Origin::Bound)
            };
            #[cfg(not(feature = "atrium"))]
            let (listener, bound) = (UnixListener::bind(path)?, true);
            if bound {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
            }
            Some(listener)
        } else {
            None
        };
        Ok(Self {
            listener,
            uid,
            worker: None,
            reader: Reader::default(),
            sink: None,
            missing_profile: Default::default(),
        })
    }

    pub fn idle(&self) -> bool {
        self.worker.is_some() && self.sink.is_none()
    }

    pub fn accept(&mut self) -> bool {
        let Some((s, _)) = self.listener.as_ref().and_then(|l| l.accept().ok()) else {
            return false;
        };
        if super::daemon::peer_uid(&s) != Some(self.uid)
            || self.worker.is_some()
            || s.set_nonblocking(true).is_err()
        {
            return false;
        }
        self.worker = Some(s);
        true
    }

    pub fn start(&mut self, host: &mut Host, handle: u16, id: u64, name: String, now: u64) {
        if !self.idle() || self.missing_profile.contains(&handle) || !host.is_encrypted(handle) {
            return;
        }
        let Ok(cid) = host.l2cap_connect(handle, sdp::PSM, now) else {
            return;
        };
        let (query, first) = Query::new(AUDIO_SINK, &[1, 4]);
        self.sink = Some(Sink {
            handle,
            id,
            name,
            sdp: Some(Discovery {
                cid,
                query,
                first: Some(first),
                until: now + SDP_WAIT,
            }),
            signal: None,
            attached: false,
            media: None,
            media_fd: None,
            closing: false,
        });
    }

    pub fn deadline(&self) -> Option<u64> {
        Some(self.sink.as_ref()?.sdp.as_ref()?.until)
    }

    pub fn check(&mut self, host: &mut Host, now: u64) {
        if let Some(s) = &self.sink {
            if !host.is_encrypted(s.handle) || self.deadline().is_some_and(|d| now >= d) {
                self.reset(host, now);
            }
        }
    }

    pub fn stop(&mut self, host: &mut Host, handle: u16, now: u64) {
        self.missing_profile.remove(&handle);
        if self.sink.as_ref().is_some_and(|s| s.handle == handle) {
            self.reset(host, now);
        }
    }

    fn reset(&mut self, host: &mut Host, now: u64) {
        self.worker = None;
        self.reader = Reader::default();
        if let Some(s) = self.sink.take() {
            for cid in [s.sdp.map(|d| d.cid), s.signal, s.media]
                .into_iter()
                .flatten()
            {
                host.l2cap_disconnect(s.handle, cid, now);
            }
        }
    }

    pub fn fds(&self) -> Vec<(RawFd, Fd)> {
        let mut f = Vec::new();
        if let Some(l) = &self.listener {
            f.push((l.as_raw_fd(), Fd::Listener));
        }
        if let Some(w) = &self.worker {
            f.push((w.as_raw_fd(), Fd::Worker));
        }
        if let Some(m) = self.sink.as_ref().and_then(|s| s.media_fd.as_ref()) {
            f.push((m.as_raw_fd(), Fd::Media));
        }
        f
    }

    pub fn event(&mut self, host: &mut Host, e: &HostEvent, now: u64) -> bool {
        let (h, cid) = match *e {
            HostEvent::ChannelOpen { handle, cid, .. }
            | HostEvent::ChannelData { handle, cid, .. }
            | HostEvent::ChannelFailed { handle, cid, .. }
            | HostEvent::ChannelClosed { handle, cid } => (handle, cid),
            _ => return false,
        };
        let Some(s) = self.sink.as_mut().filter(|s| {
            s.handle == h
                && (s.sdp.as_ref().is_some_and(|d| d.cid == cid)
                    || s.signal == Some(cid)
                    || s.media == Some(cid))
        }) else {
            return false;
        };
        let result = if host.is_encrypted(h) {
            self.worker
                .as_mut()
                .ok_or_else(|| bad("audio driver gone"))
                .and_then(|w| s.event(w, host, cid, e, now))
        } else {
            Err(bad("audio encryption lost"))
        };
        if let Err(err) = result {
            if err.kind() == io::ErrorKind::Unsupported {
                self.missing_profile.insert(h);
            }
            eprintln!("vinculumd: audio: {err}");
            self.reset(host, now);
        }
        true
    }

    pub fn readable(&mut self, which: &Fd, host: &mut Host, now: u64) {
        let result = self.read(which, host, now);
        if let Err(err) = result {
            eprintln!("vinculumd: audio: {err}");
            self.reset(host, now);
        }
    }

    fn read(&mut self, which: &Fd, host: &mut Host, now: u64) -> io::Result<()> {
        let w = self.worker.as_mut().ok_or_else(|| bad("driver gone"))?;
        let mut buf = [0; MAX_SDU + 1];
        let n = match which {
            Fd::Worker => w.read(&mut buf)?,
            Fd::Media => {
                let Some(s) = &self.sink else { return Ok(()) };
                let Some(fd) = &s.media_fd else { return Ok(()) };
                let n = fd.recv(&mut buf)?;
                if n == 0 || n > MAX_SDU || !host.is_encrypted(s.handle) {
                    return Err(bad("invalid media SDU"));
                }
                return host
                    .l2cap_send(
                        s.handle,
                        s.media.ok_or_else(|| bad("media closed"))?,
                        &buf[..n],
                        now,
                    )
                    .map_err(bad);
            }
            Fd::Listener => return Ok(()),
        };
        if n == 0 {
            return Err(bad("audio driver closed"));
        }
        let s = self
            .sink
            .as_mut()
            .filter(|s| s.attached && host.is_encrypted(s.handle))
            .ok_or_else(|| bad("no authorized audio link"))?;
        self.reader.push(&buf[..n]);
        while let Some(request) = self
            .reader
            .next::<ToHost>()
            .map_err(|_| bad("malformed audio request"))?
        {
            match request {
                ToHost::Signal(data) if !data.is_empty() && data.len() <= MAX_SDU => {
                    host.l2cap_send(
                        s.handle,
                        s.signal.ok_or_else(|| bad("signalling closed"))?,
                        &data,
                        now,
                    )
                    .map_err(bad)?;
                }
                ToHost::OpenMedia if s.media.is_none() => {
                    s.media = Some(host.l2cap_connect(s.handle, AVDTP, now).map_err(bad)?)
                }
                ToHost::CloseMedia if s.media_fd.take().is_some() => {
                    s.closing = true;
                    host.l2cap_disconnect(
                        s.handle,
                        s.media.ok_or_else(|| bad("no media channel"))?,
                        now,
                    );
                }
                _ => return Err(bad("invalid audio channel operation")),
            }
        }
        Ok(())
    }
}

impl Sink {
    fn event(
        &mut self,
        w: &mut UnixStream,
        host: &mut Host,
        cid: u16,
        e: &HostEvent,
        now: u64,
    ) -> io::Result<()> {
        let h = self.handle;
        if let Some(d) = self.sdp.as_mut().filter(|d| d.cid == cid) {
            let next = match e {
                HostEvent::ChannelOpen { .. } => {
                    d.first.take().ok_or_else(|| bad("unexpected SDP open"))?
                }
                HostEvent::ChannelData { payload, .. } => match d
                    .query
                    .on_response(payload)
                    .map_err(|_| bad("invalid Audio Sink SDP response"))?
                {
                    Err(next) => next,
                    Ok(records) => {
                        host.l2cap_disconnect(h, cid, now);
                        self.sdp = None;
                        if !records.iter().any(sink_record) {
                            return Err(io::Error::new(
                                io::ErrorKind::Unsupported,
                                "no AVDTP Audio Sink record",
                            ));
                        }
                        self.signal = Some(host.l2cap_connect(h, AVDTP, now).map_err(bad)?);
                        return Ok(());
                    }
                },
                _ => return Err(bad("SDP channel closed")),
            };
            return host.l2cap_send(h, cid, &next, now).map_err(bad);
        }
        let reply = match e {
            HostEvent::ChannelOpen { .. } if self.signal == Some(cid) => {
                self.attached = true;
                FromHost::Attach {
                    id: self.id,
                    name: self.name.clone(),
                }
            }
            HostEvent::ChannelData { payload, .. }
                if self.signal == Some(cid) && payload.len() <= MAX_SDU =>
            {
                FromHost::Signal(payload.clone())
            }
            HostEvent::ChannelOpen { peer_mtu, .. } if self.media == Some(cid) => {
                let (mine, theirs) = UnixDatagram::pair()?;
                mine.set_nonblocking(true)?;
                w.write_all(&vinculum_control::encode(&FromHost::MediaReady {
                    mtu: *peer_mtu,
                }))?;
                vinculum_hid_wire::send_with_fd(w, b"F", theirs.as_raw_fd())?;
                self.media_fd = Some(mine);
                return Ok(());
            }
            HostEvent::ChannelClosed { .. } if self.media == Some(cid) && self.closing => {
                self.media = None;
                self.closing = false;
                FromHost::MediaClosed
            }
            _ => return Err(bad("profile channel closed or unexpected data")),
        };
        w.write_all(&vinculum_control::encode(&reply))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdp_requires_sink_class_and_avdtp_on_its_assigned_psm_in_one_record() {
        let uuid = |v| Element::Uuid(sdp::uuid16(v));
        let record = |class, psm, version| {
            vec![
                (1, Element::Seq(vec![uuid(class)])),
                (
                    4,
                    Element::Seq(vec![
                        Element::Seq(vec![uuid(0x100), Element::Uint(psm)]),
                        Element::Seq(vec![uuid(AVDTP), Element::Uint(version)]),
                    ]),
                ),
            ]
        };
        assert!(sink_record(&record(AUDIO_SINK, 0x19, 0x0103)));
        assert!(!sink_record(&record(0x110a, 0x19, 0x0103)));
        assert!(!sink_record(&record(AUDIO_SINK, 0x11, 0x0103)));
        assert!(!sink_record(&record(AUDIO_SINK, 0x19, 0x0200)));
    }

    #[test]
    fn only_the_configured_uid_can_become_the_audio_worker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audio.sock");
        let uid = super::super::daemon::getuid();
        let mut broker = Audio::new(path.to_str(), uid.wrapping_add(1)).unwrap();
        let _wrong = UnixStream::connect(&path).unwrap();
        assert!(!broker.accept());
        assert!(!broker.idle());
        broker.uid = uid;
        let _right = UnixStream::connect(path).unwrap();
        assert!(broker.accept());
        assert!(broker.idle());
    }
}
