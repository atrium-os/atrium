//! Artifex's side of the inspector (dev-tools.md §3.6, D23–D25): the
//! socket portcullisd hands back with a Run, to the dev app it started.
//! A Pergola dev build answers on it (`pergola::inspect`); anything else
//! stays silent, and the view says so.
//!
//! The app is the person's own code, but it is still a peer: every frame
//! and list from it is bounded, and nothing it says is run or opened
//! without the person's click.

use std::collections::BTreeSet;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;

use pergola::inspect::{Ask, Decoder, Item, Tell, MAX_FRAME, MAX_ITEMS};

/// The socket to an inspected app.
pub struct Client {
    stream: UnixStream,
    rx: Decoder,
    tx: Vec<u8>,
}

impl Client {
    pub fn new(fd: OwnedFd) -> std::io::Result<Self> {
        let stream = UnixStream::from(fd);
        stream.set_nonblocking(true)?;
        Ok(Self { stream, rx: Decoder::default(), tx: Vec::new() })
    }

    pub fn send(&mut self, ask: &Ask) {
        if let Some(f) = pergola::inspect::encode(ask) {
            self.tx.extend_from_slice(&f);
        }
        self.flush();
    }

    fn flush(&mut self) {
        use std::io::Write;
        while !self.tx.is_empty() {
            match self.stream.write(&self.tx) {
                Ok(0) | Err(_) => break,
                Ok(n) => { self.tx.drain(..n); }
            }
        }
    }

    /// What the app said since the last call. `Err` when it ended or broke
    /// the wire: drop it.
    pub fn poll(&mut self) -> Result<Vec<Tell>, String> {
        use std::io::Read;
        self.flush();
        let mut out = Vec::new();
        let mut buf = [0u8; 16384];
        loop {
            match self.stream.read(&mut buf) {
                Ok(0) => return Err("the app ended".into()),
                Ok(n) => {
                    self.rx.push(&buf[..n]);
                    while let Some(t) = self.rx.next::<Tell>()? {
                        out.push(t);
                    }
                    if self.rx.pending() > MAX_FRAME + 4 {
                        return Err("the app sent more than a frame may hold".into());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok(out)
    }
}

/// One node as the view lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub id: u32,
    pub depth: u16,
    pub kind: String,
    pub detail: String,
    /// x, y, w, h in the app's window, in points.
    pub rect: Option<[f32; 4]>,
    /// The source line, as compiled, and as a file in `work` when it is one
    /// (a jump goes there).
    pub site: Option<(String, u32, Option<String>)>,
    pub children: bool,
}

/// The inspector's view of one app.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Inspect {
    pub app: String,
    /// The folder of `work` the app was built from.
    pub project: String,
    /// The socket is open.
    pub live: bool,
    /// It said hello (a Pergola dev build that answers).
    pub answers: bool,
    pub rows: Vec<Row>,
    pub picking: bool,
    pub bounds: bool,
    pub chosen: Option<u32>,
    pub hover: Option<u32>,
    /// Rows whose children are hidden.
    pub closed: BTreeSet<u32>,
    /// The first row shown.
    pub top: usize,
    /// A pick whose row isn't shown yet.
    pub picked_pending: bool,
}

impl Inspect {
    /// The rows not under a closed one, by index into `rows`.
    pub fn visible(&self) -> Vec<usize> {
        let mut out = Vec::new();
        let mut hide_below: Option<u16> = None;
        for (i, r) in self.rows.iter().enumerate() {
            if let Some(d) = hide_below {
                if r.depth > d {
                    continue;
                }
                hide_below = None;
            }
            out.push(i);
            if r.children && self.closed.contains(&r.id) {
                hide_below = Some(r.depth);
            }
        }
        out
    }

    /// Open every row above `id`, so it shows.
    pub fn reveal(&mut self, id: u32) {
        let Some(i) = self.rows.iter().position(|r| r.id == id) else { return };
        let mut depth = self.rows[i].depth;
        for r in self.rows[..i].iter().rev() {
            if r.depth < depth {
                self.closed.remove(&r.id);
                depth = r.depth;
            }
        }
    }

    pub fn chosen_row(&self) -> Option<&Row> {
        self.chosen.and_then(|id| self.rows.iter().find(|r| r.id == id))
    }
}

/// `file` (a source path as the app was compiled) as a file in `work`:
/// relative to the crate's folder `project`, or under `remote_root` (where
/// the workspace's processes see `work`). `None` for a path anywhere else
/// (a dependency's source).
pub fn local_file(file: &str, project: &str, remote_root: Option<&str>) -> Option<String> {
    let rel = if let Some(abs) = file.strip_prefix('/') {
        let root = remote_root?.trim_end_matches('/').trim_start_matches('/');
        abs.strip_prefix(root)?.strip_prefix('/')?.to_string()
    } else if project.is_empty() {
        file.to_string()
    } else {
        format!("{}/{file}", project.trim_end_matches('/'))
    };
    let ok = !rel.is_empty() && rel.split('/').all(|c| !c.is_empty() && c != "." && c != "..");
    ok.then_some(rel)
}

/// The app's tree as rows, bounded.
pub fn rows(items: Vec<Item>, project: &str, remote_root: Option<&str>) -> Vec<Row> {
    let n = items.len().min(MAX_ITEMS);
    let mut out: Vec<Row> = Vec::with_capacity(n);
    for it in items.into_iter().take(n) {
        if let Some(prev) = out.last_mut() {
            if it.depth > prev.depth {
                prev.children = true;
            }
        }
        let clip = |s: String, n: usize| -> String { s.chars().take(n).collect() };
        out.push(Row {
            id: it.id,
            depth: it.depth.min(64),
            kind: clip(it.kind, 32),
            detail: clip(it.detail, 400),
            rect: it.rect.filter(|r| r.iter().all(|v| v.is_finite())),
            site: it.site.map(|s| {
                let local = local_file(&s.file, project, remote_root);
                (clip(s.file, 1024), s.line, local)
            }),
            children: false,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pergola::inspect::Site;

    fn item(id: u32, depth: u16, file: &str, line: u32) -> Item {
        Item { id, parent: None, depth, kind: "Stack".into(), rect: Some([0.0, 0.0, 10.0, 10.0]), detail: String::new(),
               site: Some(Site { file: file.into(), line, col: 1 }) }
    }

    #[test]
    fn a_site_is_a_file_in_work_only_when_it_is_one() {
        assert_eq!(local_file("src/main.rs", "ui", None).as_deref(), Some("ui/src/main.rs"));
        assert_eq!(local_file("/home/alice/work/ui/src/main.rs", "ui", Some("/home/alice/work")).as_deref(), Some("ui/src/main.rs"));
        assert_eq!(local_file("/home/alice/.cargo/registry/src/pergola-0.1/src/view.rs", "ui", Some("/home/alice/work")), None);
        assert_eq!(local_file("../../etc/passwd", "ui", None), None);
        assert_eq!(local_file("/home/alice/workshop/x.rs", "ui", Some("/home/alice/work")), None);
    }

    #[test]
    fn rows_fold_and_open_to_show_a_pick() {
        let items = vec![item(1, 0, "src/main.rs", 10), item(2, 1, "src/main.rs", 11), item(3, 2, "src/main.rs", 12), item(4, 1, "src/main.rs", 13)];
        let mut v = Inspect { rows: rows(items, "ui", None), ..Default::default() };
        assert!(v.rows[0].children && v.rows[1].children && !v.rows[2].children && !v.rows[3].children);
        assert_eq!(v.rows[2].site.as_ref().map(|s| (s.1, s.2.clone())), Some((12, Some("ui/src/main.rs".into()))));
        assert_eq!(v.visible(), vec![0, 1, 2, 3]);
        v.closed.insert(1);
        assert_eq!(v.visible(), vec![0]);
        v.closed.insert(2);
        v.reveal(3);
        assert_eq!(v.visible(), vec![0, 1, 2, 3], "every row above the pick opens");
    }

    #[test]
    fn the_client_hears_the_app_and_notices_it_end() {
        use std::io::Write;
        let (ours, mut app) = UnixStream::pair().unwrap();
        let mut c = Client::new(ours.into()).unwrap();
        assert_eq!(c.poll().unwrap(), vec![]);
        app.write_all(&pergola::inspect::encode(&Tell::Hello { pid: 7 }).unwrap()).unwrap();
        app.write_all(&pergola::inspect::encode(&Tell::Changed).unwrap()).unwrap();
        assert_eq!(c.poll().unwrap(), vec![Tell::Hello { pid: 7 }, Tell::Changed]);
        c.send(&Ask::Bounds(true));
        let mut d = Decoder::default();
        let mut buf = [0u8; 64];
        let n = std::io::Read::read(&mut app, &mut buf).unwrap();
        d.push(&buf[..n]);
        assert_eq!(d.next::<Ask>().unwrap(), Some(Ask::Bounds(true)));
        drop(app);
        assert!(c.poll().is_err());
    }
}
