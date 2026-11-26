//! The viewer's side of a session (fenestra.md §3.3, §14): each window's
//! scene stream into a [`Reader`] over a frontend of its own, acks back as
//! datagrams, and what it holds kept across connections so a reconnect
//! resumes (§12).
//!
//! The frontend each remote window is applied to is the same validator a
//! local app meets (H9), in this process. Off Atrium this process is
//! confined by the OS (§14.3, not in R2 yet); on Atrium the same code
//! runs in the renderer jail.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fenestra_wire::frame::Msg;
use fenestra_wire::scene::{self, Ack, Control, WindowHeader, WINDOW_HEADER_LEN};
use fenestra_wire::{HostConn, WireError};
use fresco_render_core::pack::SceneState;
use fresco_render_core::record;
use fresco_render_core::stream::{self, Reader};
use fresco_scene_server::command::envelope_frontend::EnvelopeFrontend;

/// Where the viewer's input comes from (a window's events, or a script).
#[cfg(feature = "window")]
pub mod window;

pub type Inputs = Arc<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<scene::Input>>>;

/// Most remote windows one viewer holds.
pub const MAX_WINDOWS: usize = scene::MAX_RESUME;
const CHUNK: usize = 64 * 1024;

pub struct View {
    pub fe: EnvelopeFrontend,
    pub scene: SceneState,
    pub reader: Reader,
    pub bytes: u64,
    /// The last raster, and the version it is of.
    drawn: Option<((u64, u64), (u32, u32, Vec<u32>, String))>,
}

impl View {
    fn new() -> View {
        let (fe, id) = record::viewer_window(1, 1);
        View { fe, scene: SceneState::default(), reader: Reader::new(id, record::VIEW_CLIENT), bytes: 0, drawn: None }
    }

    /// [`View::raster`], drawn again only when the version moved.
    pub fn raster_cached(&mut self) -> Option<(u32, u32, Vec<u32>, String)> {
        let held = self.reader.held()?;
        if let Some((v, r)) = &self.drawn {
            if *v == held {
                return Some(r.clone());
            }
        }
        let r = self.raster()?;
        self.drawn = Some((held, r.clone()));
        Some(r)
    }

    /// The window as drawn here: `(width, height, pixels, sha256)`.
    pub fn raster(&self) -> Option<(u32, u32, Vec<u32>, String)> {
        let size = self.reader.size();
        if size.0 == 0 || size.1 == 0 {
            return None;
        }
        record::raster(&self.fe, &self.scene, self.reader.window() as u32, size)
    }
}

/// What the viewer holds, by the host's window id; it outlives
/// connections.
#[derive(Default)]
pub struct Views {
    pub windows: Mutex<BTreeMap<u32, Arc<Mutex<View>>>>,
    pub bytes: AtomicU64,
    pub frames: AtomicU64,
    /// Window streams that broke the protocol (each drops its connection).
    pub errors: AtomicU64,
    /// Whole-desktop sessions: the host's screen and where its windows
    /// are, bottom to top.
    pub layout: Mutex<Option<((u32, u32), Vec<scene::Placed>)>>,
}

impl Views {
    fn resume(&self) -> Vec<(u32, u64, u64)> {
        let w = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        w.iter()
            .filter_map(|(id, v)| v.lock().ok()?.reader.held().map(|(e, ver)| (*id, e, ver)))
            .collect()
    }
}

/// Take one connection's streams until it ends. Errors from the host's
/// side of the protocol end the connection (the host is not trusted).
pub async fn run(hc: HostConn, views: Arc<Views>, input: Inputs) -> Result<(), WireError> {
    let (mut control, mut control_in) = hc.control.split();
    control.send(&Msg::App(Control::Resume(views.resume()).encode())).await?;
    // The person's input, to the host on the control stream. What came
    // while no connection was up is dropped, never replayed (§8.6): it was
    // meant for a screen the person could not see.
    let sending = async move {
        let mut rx = input.lock().await;
        while rx.try_recv().is_ok() {}
        while let Some(i) = rx.recv().await {
            control.send(&Msg::App(Control::Input(i).encode())).await?;
        }
        std::future::pending::<Result<(), WireError>>().await
    };
    let conn = hc.conn.clone();
    let closing = {
        let views = views.clone();
        let conn_c = conn.clone();
        async move {
            loop {
                match control_in.recv().await {
                    Ok(Msg::App(b)) => match Control::decode(&b)? {
                        Control::Closed(w) => {
                            views.windows.lock().unwrap_or_else(|e| e.into_inner()).remove(&w);
                        }
                        Control::Layout { screen, windows } => {
                            *views.layout.lock().unwrap_or_else(|e| e.into_inner()) = Some((screen, windows));
                        }
                        Control::Resume(_) | Control::Input(_) => return Err(WireError::Protocol("a viewer's message from the host")),
                    },
                    Ok(_) => return Err(WireError::Protocol("unexpected control message")),
                    // The host ending the session is an end, not an error.
                    Err(_) if matches!(conn_c.close_reason(), Some(quinn::ConnectionError::ApplicationClosed(_)) | Some(quinn::ConnectionError::LocallyClosed)) => return Ok(()),
                    Err(e) => return Err(e),
                }
            }
        }
    };
    let streams = async {
        loop {
            let recv = match conn.accept_uni().await {
                Ok(r) => r,
                Err(quinn::ConnectionError::ApplicationClosed(_)) | Err(quinn::ConnectionError::LocallyClosed) => return Ok::<(), WireError>(()),
                Err(e) => return Err(WireError::Quic(e.to_string())),
            };
            let (views, conn) = (views.clone(), conn.clone());
            let views_e = views.clone();
            tokio::spawn(async move {
                if let Err(e) = window_stream(recv, views, conn.clone()).await {
                    eprintln!("fenestra-view: window stream: {e}");
                    views_e.errors.fetch_add(1, Ordering::Relaxed);
                    conn.close(3u32.into(), b"stream");
                }
            });
        }
    };
    tokio::select! {
        r = closing => r,
        r = streams => r,
        r = sending => r,
    }
}

async fn window_stream(mut recv: quinn::RecvStream, views: Arc<Views>, conn: quinn::Connection) -> Result<(), WireError> {
    let mut head = [0u8; WINDOW_HEADER_LEN];
    tokio::time::timeout(Duration::from_secs(10), recv.read_exact(&mut head))
        .await
        .map_err(|_| WireError::Timeout)?
        .map_err(|e| WireError::Quic(e.to_string()))?;
    let h = WindowHeader::decode(&head)?;
    let view = {
        let mut w = views.windows.lock().unwrap_or_else(|e| e.into_inner());
        if !w.contains_key(&h.window) && w.len() >= MAX_WINDOWS {
            return Err(WireError::Protocol("too many windows"));
        }
        w.entry(h.window).or_insert_with(|| Arc::new(Mutex::new(View::new()))).clone()
    };
    view.lock().unwrap_or_else(|e| e.into_inner()).reader.new_stream();
    loop {
        let chunk = match recv.read_chunk(CHUNK, true).await {
            Ok(Some(c)) => c,
            Ok(None) => return Ok(()),
            // The connection ending (the host closed, or we dropped it to
            // reconnect) ends the stream; anything else is an error.
            Err(quinn::ReadError::ConnectionLost(quinn::ConnectionError::ApplicationClosed(_) | quinn::ConnectionError::LocallyClosed)) => return Ok(()),
            Err(e) => return Err(WireError::Quic(format!("{e:?}"))),
        };
        let ack = {
            let mut v = view.lock().unwrap_or_else(|e| e.into_inner());
            let v = &mut *v;
            v.bytes += chunk.bytes.len() as u64;
            v.reader.feed(&mut v.fe, &mut v.scene, &chunk.bytes).map_err(|e: stream::StreamError| {
                log::warn!("fenestra-view: window {}: {e}", h.window);
                WireError::Protocol("the host broke the scene stream")
            })?
        };
        views.bytes.fetch_add(chunk.bytes.len() as u64, Ordering::Relaxed);
        let a = match ack {
            stream::Ack::None => continue,
            stream::Ack::Holds(epoch, version) => {
                views.frames.fetch_add(1, Ordering::Relaxed);
                Ack::Holds { window: h.window, epoch, version }
            }
            stream::Ack::Whole => Ack::Whole { window: h.window },
        };
        // A lost ack is covered by the next one.
        let _ = conn.send_datagram(bytes::Bytes::copy_from_slice(&a.encode()));
    }
}

/// Write each window as a PNG (`window-ID.png`) and the report lines
/// `window ID WxH SHA256` to `dir/viewer.txt`.
pub fn snapshot(views: &Views, dir: &std::path::Path) -> std::io::Result<String> {
    std::fs::create_dir_all(dir)?;
    let mut report = String::new();
    let windows: Vec<(u32, Arc<Mutex<View>>)> = views.windows.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(k, v)| (*k, v.clone())).collect();
    for (id, v) in windows {
        let v = v.lock().unwrap_or_else(|e| e.into_inner());
        let Some((w, h, px, sha)) = v.raster() else { continue };
        report += &format!("window {id} {w}x{h} {sha}\n");
        write_png(&dir.join(format!("window-{id}.png")), w, h, &px)?;
    }
    if let Some((_, placed)) = views.layout.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        for (id, x, y, w, h) in placed {
            report += &format!("placed {id} {x} {y} {w} {h}\n");
        }
    }
    if let Some((desk, sha)) = desktop(views) {
        report += &format!("desktop {}x{} {sha}\n", desk.0, desk.1);
        write_png(&dir.join("desktop.png"), desk.0, desk.1, &desk.2)?;
    }
    report += &format!(
        "bytes {}\nframes {}\nerrors {}\n",
        views.bytes.load(Ordering::Relaxed),
        views.frames.load(Ordering::Relaxed),
        views.errors.load(Ordering::Relaxed)
    );
    std::fs::write(dir.join("viewer.txt"), &report)?;
    Ok(report)
}

/// BGRA8 words to an RGBA PNG.
fn write_png(path: &std::path::Path, w: u32, h: u32, px: &[u32]) -> std::io::Result<()> {
    let f = std::fs::File::create(path)?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut wr = enc.write_header().map_err(std::io::Error::other)?;
    let mut rgba = Vec::with_capacity(px.len() * 4);
    for p in px {
        let [b, g, r, a] = p.to_le_bytes();
        rgba.extend_from_slice(&[r, g, b, a]);
    }
    wr.write_image_data(&rgba).map_err(std::io::Error::other)
}

/// The whole desktop as the layout places the windows held: each window
/// over the ones below it (premultiplied BGRA, source over), on black.
pub fn desktop(views: &Views) -> Option<((u32, u32, Vec<u32>), String)> {
    // A host that sends no layout (the simulator): its windows at the
    // origin, on a screen as large as the largest.
    let given = views.layout.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let (screen, placed) = match given {
        Some(l) => l,
        None => {
            let ws = views.windows.lock().unwrap_or_else(|e| e.into_inner());
            let placed: Vec<scene::Placed> = ws
                .iter()
                .filter_map(|(id, v)| {
                    let (w, h) = v.lock().unwrap_or_else(|e| e.into_inner()).reader.size();
                    (w > 0 && h > 0).then_some((*id, 0, 0, w, h))
                })
                .collect();
            let screen = placed.iter().fold((0, 0), |(a, b), p| (a.max(p.3), b.max(p.4)));
            if placed.is_empty() {
                return None;
            }
            (screen, placed)
        }
    };
    let (sw, sh) = screen;
    let mut out = vec![0xff00_0000u32; sw as usize * sh as usize];
    let windows = views.windows.lock().unwrap_or_else(|e| e.into_inner());
    for (id, x, y, _, _) in placed {
        let Some(v) = windows.get(&id) else { continue };
        let Some((w, h, px, _)) = v.lock().unwrap_or_else(|e| e.into_inner()).raster_cached() else { continue };
        for row in 0..h as i64 {
            let dy = i64::from(y) + row;
            if dy < 0 || dy >= i64::from(sh) {
                continue;
            }
            for col in 0..w as i64 {
                let dx = i64::from(x) + col;
                if dx < 0 || dx >= i64::from(sw) {
                    continue;
                }
                let s = px[(row * i64::from(w) + col) as usize];
                let d = &mut out[(dy * i64::from(sw) + dx) as usize];
                *d = over(s, *d);
            }
        }
    }
    drop(windows);
    let mut bytes = Vec::with_capacity(out.len() * 4);
    for p in &out {
        bytes.extend_from_slice(&p.to_le_bytes());
    }
    let sha = fresco_scene_server::cas::store::CasStore::hash(&bytes);
    let hex: String = sha.iter().map(|b| format!("{b:02x}")).collect();
    Some(((sw, sh, out), hex))
}

fn over(s: u32, d: u32) -> u32 {
    let a = s >> 24;
    if a == 255 {
        return s;
    }
    if a == 0 {
        return d;
    }
    let mut o = 0u32;
    for sh in [0, 8, 16, 24] {
        let sc = (s >> sh) & 0xff;
        let dc = (d >> sh) & 0xff;
        let c = sc + (dc * (255 - a) + 127) / 255;
        o |= c.min(255) << sh;
    }
    o
}
