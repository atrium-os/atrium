//! I0's private measurement parent. The child's channel is handed through
//! Portcullis unchanged. Limen mints the embed connection and binds its token.
use limen::daemon::{Launcher, Shared};
use limen::proto::{EmbedId, ParentEvent};
use std::error::Error;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn serve_fixture(stream: UnixStream) -> std::io::Result<()> {
    use nuntius::{Event, Request};
    let signin = std::env::var_os("I0_SIGN_IN").is_some();
    let urls: &[&str] = if signin {
        &[
            "https://idp.example/sign-in.html",
            "https://idp.example/sign-in.html?step=authorize",
            "https://idp.example/sign-in.html?step=password",
            "https://idp.example/sign-in.html?step=callback",
        ]
    } else {
        &["https://editor.example/editor.html"]
    };
    let bytes: &[u8] = if signin {
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
        if app != limen::catalogue::NAVIGATOR_ENGINE {
            return Err("unexpected worker".into());
        }
        let (fetcher, engine) = UnixStream::pair().map_err(|e| e.to_string())?;
        std::thread::spawn(move || {
            if let Err(e) = serve_fixture(fetcher) {
                eprintln!("fixture broker: {e}");
            }
        });
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
            .stdout(Stdio::from(OwnedFd::from(engine)))
            .spawn()
            .map_err(|e| e.to_string())?;
        let exit = self.exit.clone();
        std::thread::spawn(move || {
            let _ = exit.send(worker.wait().is_ok_and(|s| s.success()));
        });
        Ok(self.uid)
    }
    fn end(&mut self, _: EmbedId) {} // The one-shot process exits after its last acknowledged frame.
}

fn run(dir: &Path, bundle: PathBuf) -> Result<(), Box<dyn Error>> {
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
        Box::new(|uid| (uid == 0).then(|| ("fixture".into(), "org.test.i0parent".into()))),
        Box::new(|_, _| None),
    );
    let (parent, server) = UnixStream::pair()?;
    std::thread::spawn(move || {
        let _ = limen::daemon::serve(aqueduct::Connection::wrap(server).unwrap(), 0, shared);
    });
    let mut parent = limen::client::Parent::wrap(aqueduct::Connection::wrap(parent)?);
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
    let start = Instant::now();
    let embed = parent
        .request("web-island", token)
        .map_err(|e| format!("{e:?}"))?;
    parent
        .place(embed, Some([0., 0., 640., 360.]))
        .map_err(|e| format!("{e:?}"))?;
    let raster_log = dir.join("frescod.log");
    let mut cursor = std::fs::metadata(&raster_log)?.len() as usize;
    let mut attached = false;
    let stages: &[&str] = if std::env::var_os("I0_SIGN_IN").is_some() {
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
        parent
            .send(embed, b"advance".to_vec())
            .map_err(|e| format!("{e:?}"))?;
    }
    if !exit.recv_timeout(Duration::from_secs(15))? {
        return Err("fixture worker failed".into());
    }
    // A late renderer row would shift the attribution of all following frames.
    let log = std::fs::read(&raster_log)?;
    if std::str::from_utf8(&log[cursor..])?.contains("i0_raster width=640 height=360") {
        return Err("extra raster after the final acknowledged frame".into());
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
