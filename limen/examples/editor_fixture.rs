//! I0's private measurement parent. The child's channel is handed through
//! Portcullis unchanged. Limen mints the embed connection and binds its token.
use limen::daemon::{Launcher, Shared};
use limen::proto::{EmbedId, ParentEvent};
use std::error::Error;
use std::io::Write;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn serve_fixture(stream: UnixStream) -> std::io::Result<()> {
    use nuntius::{Event, Request};
    let signin = std::env::var_os("I0_SIGN_IN").is_some();
    let counter = std::env::var_os("I1_COUNTER").is_some();
    let urls: &[&str] = if counter {
        &["https://widget.example/counter.html"]
    } else if signin {
        &[
            "https://idp.example/sign-in.html",
            "https://idp.example/sign-in.html?step=authorize",
            "https://idp.example/sign-in.html?step=password",
            "https://idp.example/sign-in.html?step=callback",
        ]
    } else {
        &["https://editor.example/editor.html"]
    };
    let bytes: &[u8] = if counter {
        include_bytes!("../../test-assets/navigator-i0/counter.html")
    } else if signin {
        include_bytes!("../../test-assets/navigator-i0/sign-in.html")
    } else {
        include_bytes!("../../test-assets/navigator-i0/editor.html")
    };
    let mut step = 0;
    let (mut input, mut output) = nuntius::server::channel(stream)?;
    while let Some(req) = input.recv()? {
        let Request::Open(open) = req else {
            return Err(std::io::Error::other("unexpected fixture request"));
        };
        let id = open.id;
        if urls.get(step).copied() != Some(open.url.as_str())
            || id != step as u32 + 1
            || open.method != nuntius::Method::Get
            || open.body.is_some()
            || open.deliver != nuntius::Deliver::Stream
            || (bytes.len() as u64) > open.limits.max_bytes
        {
            output.send(Event::Error {
                id,
                code: nuntius::ErrorCode::Policy,
            })?;
            continue;
        }
        let url = urls[step];
        step += 1;
        eprintln!("broker GET {id} {url} bytes={}", bytes.len());
        output.send(Event::Head {
            id,
            status: 200,
            final_url: url.into(),
            redirects: 0,
            content_type: Some("text/html".into()),
            length: Some(bytes.len() as u64),
            accept_ranges: false,
            validator: None,
        })?;
        output.send(Event::Data {
            id,
            offset: 0,
            bytes: bytes.to_vec(),
        })?;
        output.send(Event::Done {
            id,
            bytes: bytes.len() as u64,
        })?;
    }
    Ok(())
}

struct Fixture {
    bundle: PathBuf,
    uid: u32,
    exit: mpsc::Sender<bool>,
}

impl Launcher for Fixture {
    fn launch(&mut self, app: &str, embed: EmbedId, channel: UnixStream) -> Result<u32, String> {
        let mail = std::env::var_os("I0_MAIL_DOCUMENT").is_some();
        let expected = if mail { "org.atrium.navigator.mail-page" } else { limen::catalogue::NAVIGATOR_ENGINE };
        if app != expected {
            return Err("unexpected worker".into());
        }
        let stdout = if mail { Stdio::inherit() } else {
            let (fetcher, engine) = UnixStream::pair().map_err(|e| e.to_string())?;
            std::thread::spawn(move || {
                if let Err(e) = serve_fixture(fetcher) { eprintln!("fixture broker: {e}"); }
            });
            Stdio::from(OwnedFd::from(engine))
        };
        self.spawn(embed, channel, stdout)
    }

    fn launch_island(&mut self, embed: EmbedId, channel: UnixStream, _: u32, fetch: UnixStream) -> Result<u32, String> {
        self.spawn(embed, channel, Stdio::from(OwnedFd::from(fetch)))
    }

    fn end(&mut self, _: EmbedId) {} // The one-shot process exits after its last acknowledged frame.
}

impl Fixture {
    fn spawn(&self, embed: EmbedId, channel: UnixStream, stdout: Stdio) -> Result<u32, String> {
        let mut worker = Command::new("/usr/local/bin/portcullis")
            .args([
                "exec",
                "--require-memory-limit",
                "--user",
                "alice",
                "--instance",
                &format!("i0-{embed:x}"),
            ])
            .arg(&self.bundle)
            .stdin(Stdio::from(OwnedFd::from(channel)))
            .stdout(stdout)
            .spawn()
            .map_err(|e| e.to_string())?;
        let exit = self.exit.clone();
        std::thread::spawn(move || {
            let _ = exit.send(worker.wait().is_ok_and(|s| s.success()));
        });
        Ok(self.uid)
    }

}

fn fixture_parent(dir: &Path, bundle: PathBuf, counter: bool) -> Result<(limen::client::Parent, mpsc::Receiver<bool>), Box<dyn Error>> {
    let uid = Command::new("id").args(["-u", "alice"]).output()?;
    if !uid.status.success() {
        return Err("no fixture user".into());
    }
    let uid = std::str::from_utf8(&uid.stdout)?.trim().parse()?;
    let (done, exit) = mpsc::channel();
    let shared = Shared::new(
        Box::new(limen::fresco::Frescod::new(dir.join("act.sock"))),
        Box::new(Fixture {
            bundle,
            uid,
            exit: done,
        }),
        Box::new(move |uid| (uid == 0).then(|| ("fixture".into(), if counter { "org.atrium.navigator" } else { "org.test.i0parent" }.into()))),
        Box::new(|_, _| None),
    );
    if counter { shared.set_mail_sheet_provider(Box::new(|uid| uid == 0)); }
    let (parent, server) = UnixStream::pair()?;
    std::thread::spawn(move || {
        let _ = limen::daemon::serve(aqueduct::Connection::wrap(server).unwrap(), 0, shared);
    });
    Ok((limen::client::Parent::wrap(aqueduct::Connection::wrap(parent)?), exit))
}

fn run(dir: &Path, bundle: PathBuf) -> Result<(), Box<dyn Error>> {
    let counter = std::env::var_os("I1_COUNTER").is_some();
    let (mut parent, exit) = match std::env::var_os("I1_LIMEND_SOCKET") {
        Some(socket) if counter => (limen::client::Parent::at(socket).map_err(|e| format!("{e:?}"))?, None),
        _ => {
            let (parent, exit) = fixture_parent(dir, bundle, counter)?;
            (parent, Some(exit))
        }
    };
    let mut display = fresco_client::Connection::connect(dir.join("app.sock"))?;
    display.set_read_timeout(Some(Duration::from_secs(10)))?;
    let window = display.window_create(
        720,
        420,
        "I0 measurement parent",
        fresco_protocol::WindowHints {
            initial_position: Some((0, 0)),
            server_decorations: Some(false),
            ..Default::default()
        },
    )?;
    let until = Instant::now() + Duration::from_secs(10);
    let token = loop {
        if Instant::now() >= until {
            return Err("no parent window token".into());
        }
        if let Some(fresco_client::Event::EmbedToken { window_id, token }) =
            display.wait_event(Some(until.saturating_duration_since(Instant::now())))?
        {
            if window_id == window {
                break token;
            }
        }
    };
    if counter {
        return counter_gate(dir, display, window, token, parent, exit);
    }
    let document = std::env::var_os("I0_MAIL_DOCUMENT");
    let start = Instant::now();
    let embed = parent
        .request(if document.is_some() { "mail-sign-in-page" } else { "web-island" }, token)
        .map_err(|e| format!("{e:?}"))?;
    parent
        .place(embed, Some([0., 0., 640., 360.]))
        .map_err(|e| format!("{e:?}"))?;
    let raster_log = dir.join("frescod.log");
    let mut cursor = std::fs::metadata(&raster_log)?.len() as usize;
    let mut attached = false;
    if let Some(path) = &document {
        let data = std::fs::read(path)?;
        let fd = limen::fdpass::sealed_file("development-first-screen", &data)?;
        parent.send_fd(embed, b"https://login.microsoftonline.com/first".to_vec(), fd.as_fd()).map_err(|e| format!("{e:?}"))?;
    }
    let stages: &[&str] = if document.is_some() {
        &["baseline"]
    } else if std::env::var_os("I0_SIGN_IN").is_some() {
        &["baseline", "refused", "callback"]
    } else {
        &["baseline", "blink", "restore", "key", "scroll"]
    };
    let signin = std::env::var_os("I0_SIGN_IN").is_some();
    for (index, &stage) in stages.iter().enumerate() {
        let until = Instant::now() + Duration::from_secs(45);
        loop {
            if Instant::now() >= until {
                return Err(format!("timeout at {stage}").into());
            }
            match parent
                .poll(until.saturating_duration_since(Instant::now()))
                .map_err(|e| format!("{e:?}"))?
            {
                Some(ParentEvent::Attached { embed: id }) if id == embed => attached = true,
                Some(ParentEvent::Message { embed: id, bytes })
                    if attached && id == embed && bytes == b"frame" =>
                {
                    break;
                }
                e => return Err(format!("unexpected {stage} event {e:?}").into()),
            }
        }
        loop {
            let log = std::fs::read(&raster_log)?;
            let tail = std::str::from_utf8(&log[cursor..])?;
            if let Some(row) = tail.split_inclusive('\n').find(|line| {
                line.contains("i0_raster width=640 height=360") && line.ends_with('\n')
            }) {
                let row = row.trim_end();
                cursor += tail.find(row).unwrap() + row.len() + 1;
                if signin {
                    let commit = row
                        .split_whitespace()
                        .next()
                        .and_then(|token| token.strip_prefix("commit="))
                        .and_then(|n| n.parse::<usize>().ok())
                        .ok_or("renderer missing commit number")?;
                    if commit == index
                        && index > 0
                        && row.ends_with("full=false pixel_mismatches=0")
                    {
                        println!("additional raster for {} {row}", stages[index - 1]);
                        continue;
                    }
                    if commit != index + 1 {
                        return Err(format!("unexpected scene commit for {stage}: {row}").into());
                    }
                }
                let valid = if stage == "baseline" {
                    row.ends_with("full=true")
                } else {
                    row.ends_with("full=false pixel_mismatches=0")
                };
                if !valid {
                    return Err(format!("invalid raster at {stage}: {row}").into());
                }
                println!("{stage} {row}");
                if std::env::var_os("I0_SIGN_IN").is_some() {
                    println!("{stage} elapsed_us={}", start.elapsed().as_micros());
                }

                break;
            }
            if Instant::now() >= until {
                return Err(format!("missing raster for {stage}").into());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if stage == "restore" {
            // Raster completion can precede presentation. Wait until the
            // compositor actually hit-tests the child before pressing a key.
            let start = std::fs::metadata(&raster_log)?.len() as usize;
            loop {
                let mut probe = UnixStream::connect(dir.join("input.sock"))?;
                probe.write_all(b"MOVE 40 40\n")?;
                drop(probe);
                std::thread::sleep(Duration::from_millis(50));
                let log = std::fs::read(&raster_log)?;
                let shown = std::str::from_utf8(&log[start..])?
                    .lines()
                    .filter_map(|line| line.strip_prefix("frescod: inject MOVE 40 40 -> window "))
                    .filter_map(|id| id.parse::<u64>().ok())
                    .last();
                if shown.is_some_and(|id| id != 0 && id != window as u64) {
                    break;
                }
                if Instant::now() >= until {
                    return Err("editor was never hit-testable".into());
                }
            }
        }
        if stage == "restore" || stage == "key" {
            let mut inject = UnixStream::connect(dir.join("input.sock"))?;
            if stage == "restore" {
                inject.write_all(b"MOVE 40 40\nBTN 1 1\nBTN 1 0\nKEY 0x1b 1\nKEY 0x1b 0\n")?;
            } else {
                inject.write_all(b"MOVE 40 40\nSCROLL 24\n")?;
            }
        }
        if document.is_some() {
            parent.detach(embed).map_err(|e| format!("{e:?}"))?;
            break;
        }
        parent
            .send(embed, b"advance".to_vec())
            .map_err(|e| format!("{e:?}"))?;
    }
    if !exit.ok_or("missing fixture launcher")?.recv_timeout(Duration::from_secs(15))? {
        return Err("fixture worker failed".into());
    }
    // A late renderer row would shift the attribution of all following frames.
    let log = std::fs::read(&raster_log)?;
    if std::str::from_utf8(&log[cursor..])?.contains("i0_raster width=640 height=360") {
        return Err("extra raster after the final acknowledged frame".into());
    }
    Ok(())
}

// The fixture parent uses the island rectangle from the normalised document scene. The driver sends
// pointer input through frescod and reads the composed screen at each step.
fn counter_gate(dir: &Path, mut display: fresco_client::Connection, window: u32, token: [u8; 16],
    mut parent: limen::client::Parent, exit: Option<mpsc::Receiver<bool>>) -> Result<(), Box<dyn Error>> {
    let bounds = std::fs::read_to_string(dir.join("counter.rect"))?;
    let values = bounds.split_whitespace().map(str::parse::<u32>).collect::<Result<Vec<_>, _>>()?;
    let [x, y, w, h]: [u32; 4] = values.try_into().map_err(|_| "expected four rectangle coordinates")?;
    if w == 0 || h == 0 || x.saturating_add(w) > 720 || y.saturating_add(h) > 420 {
        return Err("counter rectangle outside the parent".into());
    }
    let [left, top, width, height] = [x, y, w, h].map(|n| n as f32);
    let font = display.font_open("system-sans")?.font_id;
    display.set_default_window(window.try_into()?);
    let mut frame = display.frame()?;
    frame.rect(fresco_protocol::RectParams { x: left, y: top, w: width, h: height, r: 0.92, g: 0.92, b: 0.92, a: 1., radius: 0. })?;
    frame.text_run(font, 20., left + 30., top + 60., [0.1, 0.1, 0.1, 1.], "Start widget.example counter")?;
    frame.finish()?;
    let until = Instant::now() + Duration::from_secs(100);
    loop {
        if Instant::now() >= until { return Err("widget press timed out".into()); }
        if let Some(fresco_client::Event::PointerButton { window_id, x, y, button: 1, pressed: true, .. }) =
            display.wait_event(Some(Duration::from_millis(100)))? {
            if window_id == window && (left..left + width).contains(&x) && (top..top + height).contains(&y) { break; }
        }
    }
    let id = parent.request_island(token, 1).map_err(|e| format!("{e:?}"))?;
    parent.place(id, Some([left, top, width, height])).map_err(|e| format!("{e:?}"))?;
    let mut reports = limen::web_island::Lifecycle::default();
    let mut cover = None;
    loop {
        if Instant::now() >= until { return Err("widget lifecycle timed out".into()); }
        if let Some(event) = parent.poll(Duration::from_millis(50)).map_err(|e| format!("{e:?}"))? {
            match event {
                ParentEvent::IslandFetch { embed } if embed == id => {
                    let socket = parent.take_island_fetch(id).ok_or("missing counter fetch channel")?;
                    std::thread::spawn(move || {
                        if let Err(e) = serve_fixture(socket) { eprintln!("counter fetch: {e}"); }
                    });
                }
                ParentEvent::Attached { .. } => println!("counter attached"),
                ParentEvent::Message { embed, bytes } if embed == id => match reports.receive(&bytes) {
                    Some(limen::web_island::Report::Started { origin: "https://widget.example" }) => println!("counter frame"),
                    Some(limen::web_island::Report::Title(_) | limen::web_island::Report::Resize { .. }) => {}
                    other => return Err(format!("unexpected counter report {other:?}").into()),
                },
                other => return Err(format!("unexpected widget event {other:?}").into()),
            }
        }
        if dir.join("cover").exists() && cover.is_none() {
            let mut c = fresco_client::Connection::connect(dir.join("app.sock"))?;
            let w = c.window_create(720, 420, "Cover", fresco_protocol::WindowHints {
                initial_position: Some((0, 0)), server_decorations: Some(false), ..Default::default() })?;
            c.set_default_window(w.try_into()?);
            let font = c.font_open("system-sans")?.font_id;
            let mut f = c.frame()?;
            f.rect(fresco_protocol::RectParams { x: 0., y: 0., w: 720., h: 420., r: 1., g: 1., b: 1., a: 1., radius: 0. })?;
            f.text_run(font, 24., 80., 120., [0., 0., 0., 1.], "Cover window")?;
            f.finish()?;
            cover = Some((c, w));
        }
        if let Some((c, _)) = cover.as_mut() {
            if matches!(c.poll_event()?, Some(fresco_client::Event::PointerButton { pressed: true, button: 1, .. })) {
                println!("cover received press");
            }
        }
        if !dir.join("cover").exists() {
            if let Some((mut c,w)) = cover.take() { c.window_destroy(w)?; }
        }
        if dir.join("close-page").exists() { break; }
    }
    display.window_destroy(window)?;
    drop(parent);
    if let Some(exit) = exit {
        if !exit.recv_timeout(Duration::from_secs(12))? { return Err("widget did not exit cleanly".into()); }
        println!("counter exited after parent closed");
    } else {
        println!("counter parent closed");
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.len() != 3 {
        return Err("usage: editor_fixture SOCKET_DIRECTORY SIGNED_BUNDLE".into());
    }
    run(Path::new(&args[1]), PathBuf::from(&args[2]))
}
