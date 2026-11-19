// Files-grid image requests run off the window thread. The Limen child
// stays unplaced so raster previews cannot take focus or cover a tile.
use std::os::fd::AsFd;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use atrium_mail::view::Mail;
use limen::client::Parent;
use limen::doc_viewer::{ToChild, ToParent, THUMBNAIL_INPUT_MAX};
use limen::proto::{EmbedId, ParentEvent, WindowToken};
use mail_engine::{files::PicturePart, Parser};
use mail_store::Store;
use pergola::node::Image;

pub struct Thumbnails {
    jobs: mpsc::Sender<(WindowToken, PicturePart)>,
    results: mpsc::Receiver<Option<Image>>,
    pending: Option<PicturePart>,
}

impl Thumbnails {
    pub fn start(store: Store, parser: Parser) -> Self {
        let (jobs, requests) = mpsc::channel::<(WindowToken, PicturePart)>();
        let (completed, results) = mpsc::channel();
        std::thread::spawn(move || {
            let mut viewer = None;
            loop {
                match requests.recv_timeout(Duration::from_secs(15)) {
                    Ok((token, part)) => {
                        let image = render(&store, &parser, token, part, &mut viewer);
                        if completed.send(image).is_err() {
                            break;
                        }
                    }
                    // Closing the separate Limen connection also retires its child.
                    Err(mpsc::RecvTimeoutError::Timeout) => viewer = None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        });
        Self {
            jobs,
            results,
            pending: None,
        }
    }

    pub fn pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn step(&mut self, mail: &Mail, token: Option<WindowToken>) {
        match self.results.try_recv() {
            Ok(image) => {
                if let Some(part) = self.pending.take() {
                    mail.thumbnail_ready(part, image);
                }
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.pending = None;
                return;
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        if self.pending.is_none() {
            if let (Some(token), Some(part)) = (token, mail.next_thumbnail()) {
                if self.jobs.send((token, part.clone())).is_ok() {
                    self.pending = Some(part);
                }
            }
        }
    }
}

fn render(
    store: &Store,
    parser: &Parser,
    token: WindowToken,
    part: PicturePart,
    viewer: &mut Option<(Parent, EmbedId)>,
) -> Option<Image> {
    let raw = store.blob(&part.blob).ok()?;
    let attachment = parser
        .attachment(&raw, &mail_schema::Which::Part(part.path), None)
        .ok()??;
    if attachment.bytes.len() > THUMBNAIL_INPUT_MAX {
        return None;
    }
    let fd = atrium_mail::files::sealed(&attachment.bytes).ok()?;
    if viewer.is_none() {
        let mut parent = Parent::connect().ok()?;
        let id = parent.request("doc-viewer", token).ok()?;
        *viewer = Some((parent, id));
    }
    let (parent, id) = viewer.as_mut()?;
    match request(parent, *id, fd.as_fd()) {
        Ok(image) => image,
        Err(()) => {
            // A late or malformed reply must never be used for the next tile.
            *viewer = None;
            None
        }
    }
}

fn request(
    parent: &mut Parent,
    id: EmbedId,
    fd: std::os::fd::BorrowedFd<'_>,
) -> Result<Option<Image>, ()> {
    parent
        .send_fd(id, ToChild::MakeThumbnail.encode(), fd)
        .map_err(|_| ())?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(());
        }
        match parent.poll(left).map_err(|_| ())? {
            Some(ParentEvent::Message { embed, bytes }) if embed == id => {
                return match ToParent::decode(&bytes) {
                    Some(ToParent::Thumbnail {
                        width,
                        height,
                        rgba,
                    }) => Ok(Some(Image::new(width.into(), height.into(), rgba))),
                    Some(ToParent::Error { .. }) => Ok(None),
                    _ => Err(()),
                };
            }
            Some(ParentEvent::ChildLost { embed } | ParentEvent::Detached { embed })
                if embed == id =>
            {
                return Err(())
            }
            None => return Err(()),
            _ => {}
        }
    }
}
