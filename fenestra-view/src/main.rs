//! `fenestra-view (--ssh "ssh -T user@host" | --mint-dir DIR) [--take-over]
//! [--connect ADDR]`: a window showing the host's desktop; keyboard,
//! pointer and wheel go to the host.
//!
//! `fenestra-view --headless --mint-dir DIR --out DIR [--run-secs N]
//! [--reconnect-after S] [--connect ADDR] [--idle-after S] [--input FILE]`
//! (gates, §14.2): draw the host's windows for `--run-secs`, then write
//! `window-ID.png`, `desktop.png` and `viewer.txt` to `--out`.
//! `--reconnect-after S` drops the connection after S seconds and connects
//! again, resuming from what it holds. `--input FILE` lines: `SECONDS
//! MOVE x y | BTN b 0|1 | KEY usage 0|1 [mods] | SCROLL dy | RESIZE w h |
//! FOCUS` (the last two for the topmost window of a per-app session). `--output WxH[@SCALE]` asks the host
//! for a remote output of that size and scale (§4.1); the windowed viewer
//! asks for its window's, and again as it is resized.
//!
//! The mint (`fenestra_wire::mint`): with `--ssh CMD`, it runs `CMD
//! fenestra-shell mint KEY` (CMD split at spaces, no shell) and reads the
//! `FENESTRA_MINT` line back from the SSH channel; sshd authenticates the
//! person, and the session's keys are pinned through that channel.
//! `--take-over` ends a session another viewer has. With `--mint-dir DIR`
//! (gates, the host simulator) it leaves its public key in DIR and waits
//! for `DIR/mint`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use fenestra_view::{run, snapshot, Views};
use fenestra_wire::scene::Input;
use fenestra_wire::{mint, SessionKey, Viewer, WireError};

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn die(why: impl std::fmt::Display) -> ! {
    eprintln!("fenestra-view: {why}");
    std::process::exit(1)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let headless = args.iter().any(|a| a == "--headless");
    let ssh = arg(&args, "--ssh");
    let take_over = args.iter().any(|a| a == "--take-over");
    let dir: Option<PathBuf> = arg(&args, "--mint-dir").map(PathBuf::from);
    if ssh.is_some() == dir.is_some() {
        die("one of --ssh CMD or --mint-dir DIR is needed");
    }
    let out: Option<PathBuf> = arg(&args, "--out").map(PathBuf::from);
    if headless && out.is_none() {
        die("--headless needs --out DIR");
    }
    let run_secs: u64 = arg(&args, "--run-secs").and_then(|s| s.parse().ok()).unwrap_or(if headless { 20 } else { 7 * 86400 }).min(30 * 86400);
    let reconnect: Option<u64> = arg(&args, "--reconnect-after").and_then(|s| s.parse().ok());
    // The host's address as this side reaches it (a forwarded port), in
    // place of the one in the mint.
    // Bytes received after this many seconds count as idle (`idle_bytes`).
    // Input to send, from a script (`--input FILE`): lines of
    // `SECONDS MOVE x y | BTN button 0|1 | KEY usage 0|1 [modifiers] | SCROLL dy`,
    // seconds from the start, in the host screen's pixels. A line ending in
    // `PROBE` also times the input to the next frame (`probe AT MS`).
    let script = arg(&args, "--input");
    let idle_after: Option<u64> = arg(&args, "--idle-after").and_then(|s| s.parse().ok());
    // `--transport udp|tcp|auto`: QUIC over UDP, over TCP on the same port
    // (UDP blocked, §8.6), or UDP first and TCP when UDP gets no answer.
    let transport = arg(&args, "--transport").unwrap_or_else(|| "auto".into());
    if !["udp", "tcp", "auto"].contains(&transport.as_str()) {
        die("--transport udp|tcp|auto");
    }
    let connect: Option<std::net::SocketAddr> = arg(&args, "--connect").map(|a| a.parse().unwrap_or_else(|_| die("--connect ADDR:PORT")));
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().worker_threads(2).build().unwrap_or_else(|e| die(e));
    let views = Arc::new(Views::default());
    // `--output WxH[@SCALE]`: the remote output a headless viewer asks for
    // (SCALE like 1.5); the windowed viewer asks for its window's.
    if let Some(o) = arg(&args, "--output") {
        let (size, scale) = o.split_once('@').unwrap_or((o.as_str(), "1"));
        let (w, h) = size.split_once('x').and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?))).unwrap_or_else(|| die("--output WxH[@SCALE]"));
        let s: f64 = scale.parse().ok().filter(|s: &f64| s.is_finite() && *s > 0.0).unwrap_or_else(|| die("--output WxH[@SCALE]"));
        views.set_output(w, h, (s * 1000.0).round() as u32);
    }
    let (input_tx, input_rx) = tokio::sync::mpsc::channel::<Input>(1024);
    let window_input = input_tx.clone();
    let views_w = views.clone();
    let title = format!("Fenestra: {}", ssh.as_deref().and_then(|c| c.split_whitespace().last()).map_or_else(|| dir.as_ref().map(|d| d.display().to_string()).unwrap_or_default(), str::to_string));
    let session = move || rt.block_on(async move {
        let key = SessionKey::generate().unwrap_or_else(|e| die(e));
        let m = match (&ssh, &dir) {
            (Some(cmd), _) => ssh_mint(cmd, key.public(), take_over).unwrap_or_else(|e| die(format!("mint over ssh: {e}"))),
            (None, Some(dir)) => {
                mint::write_viewer_key(dir, key.public()).unwrap_or_else(|e| die(format!("{}: {e}", dir.display())));
                let mut tries = 0;
                loop {
                    match mint::DevMint::read(dir) {
                        Ok(m) => break m,
                        Err(_) if tries < 600 => {
                            tries += 1;
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                        Err(e) => die(format!("no mint: {e}")),
                    }
                }
            }
            (None, None) => unreachable!("checked above"),
        };
        let viewer = Viewer::new(&key, &m.host_key).unwrap_or_else(|e| die(e));
        let addr = connect.unwrap_or(m.addr);
        let probes: Arc<std::sync::Mutex<Vec<(f64, Option<f64>)>>> = Arc::default();
        let inputs: fenestra_view::Inputs = Arc::new(tokio::sync::Mutex::new(input_rx));
        if let Some(f) = &script {
            let lines = std::fs::read_to_string(f).unwrap_or_else(|e| die(format!("{f}: {e}")));
            let steps = parse_script(&lines).unwrap_or_else(|e| die(format!("{f}: {e}")));
            let start = tokio::time::Instant::now();
            let (views, probes) = (views.clone(), probes.clone());
            tokio::spawn(async move {
                for (at, i, probe) in steps {
                    tokio::time::sleep_until(start + Duration::from_secs_f64(at)).await;
                    let f0 = views.frames.load(std::sync::atomic::Ordering::Relaxed);
                    let t = tokio::time::Instant::now();
                    if input_tx.send(i).await.is_err() {
                        return;
                    }
                    if probe {
                        // Input to the first frame that follows it.
                        let (views, probes) = (views.clone(), probes.clone());
                        tokio::spawn(async move {
                            let mut ms = None;
                            while t.elapsed() < PROBE_TIMEOUT {
                                if views.frames.load(std::sync::atomic::Ordering::Relaxed) > f0 {
                                    ms = Some(t.elapsed().as_secs_f64() * 1e3);
                                    break;
                                }
                                tokio::time::sleep(Duration::from_millis(2)).await;
                            }
                            probes.lock().unwrap_or_else(|e| e.into_inner()).push((at, ms));
                        });
                    }
                }
            });
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(run_secs);
        let mark = Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX));
        if let Some(s) = idle_after {
            let (views, mark) = (views.clone(), mark.clone());
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(s)).await;
                mark.store(views.bytes.load(std::sync::atomic::Ordering::Relaxed), std::sync::atomic::Ordering::Relaxed);
            });
        }
        let mut first = true;
        let mut connections = 0u32;
        // The transport that last worked; auto tries it first.
        let mut tcp = transport == "tcp";
        let auto = transport == "auto";
        let mut used_tcp = false;
        'session: loop {
            // A connection lost to the network (a blackout past the idle
            // timeout, a host gone a while) is tried again until the
            // deadline; a host that breaks the protocol is not.
            let hc = loop {
                if tokio::time::Instant::now() >= deadline {
                    break 'session;
                }
                let attempt = |over_tcp: bool| {
                    let (viewer, secret) = (&viewer, &m.secret);
                    async move {
                        // UDP gets a short wait under auto: no answer at all is
                        // what a blocked path looks like.
                        let wait = if auto && !over_tcp { UDP_PROBE } else { CONNECT_TIMEOUT };
                        let r = if over_tcp {
                            tokio::time::timeout(wait, viewer.connect_tcp(addr, secret)).await
                        } else {
                            tokio::time::timeout(wait, viewer.connect(addr, secret)).await
                        };
                        r
                    }
                };
                let mut r = attempt(tcp).await;
                if auto && !matches!(r, Ok(Ok(_))) && !matches!(r, Ok(Err(WireError::Proof | WireError::Protocol(_) | WireError::Tls(_)))) {
                    eprintln!("fenestra-view: no answer over {}; trying {}", if tcp { "TCP" } else { "UDP" }, if tcp { "UDP" } else { "TCP" });
                    tcp = !tcp;
                    r = attempt(tcp).await;
                }
                match r {
                    Ok(Ok(hc)) => {
                        used_tcp |= tcp;
                        break hc;
                    }
                    Ok(Err(e @ (WireError::Proof | WireError::Protocol(_) | WireError::Tls(_)))) => die(e),
                    Ok(Err(e)) => eprintln!("fenestra-view: connect: {e}; trying again"),
                    Err(_) => eprintln!("fenestra-view: connect timed out; trying again"),
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            };
            connections += 1;
            eprintln!("fenestra-view: connected to {addr} over {}", if tcp { "tcp" } else { "udp" });
            let cut = match (first, reconnect) {
                (true, Some(s)) => tokio::time::Instant::now() + Duration::from_secs(s),
                _ => deadline,
            };
            first = false;
            let conn = hc.conn.clone();
            tokio::select! {
                r = run(hc, views.clone(), inputs.clone()) => match r {
                    Ok(()) => break,
                    Err(e @ (WireError::Quic(_) | WireError::Timeout | WireError::Io(_))) => {
                        eprintln!("fenestra-view: connection lost ({e}); connecting again");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    Err(e) => die(e),
                },
                _ = tokio::time::sleep_until(cut) => {
                    if cut >= deadline {
                        break;
                    }
                    eprintln!("fenestra-view: dropping the connection to reconnect");
                    conn.close(0u32.into(), b"reconnect");
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
            }
        }
        let Some(out) = out else {
            viewer.close();
            return;
        };
        let mut report = snapshot(&views, &out).unwrap_or_else(|e| die(e));
        let mut extra = format!("connections {connections}\ntransport {}\n", if used_tcp { "tcp" } else { "udp" });
        let mut ps = probes.lock().unwrap_or_else(|e| e.into_inner()).clone();
        ps.sort_by(|a, b| a.0.total_cmp(&b.0));
        for (at, ms) in ps {
            extra += &match ms {
                Some(ms) => format!("probe {at} {ms:.1}\n"),
                None => format!("probe {at} timeout\n"),
            };
        }
        report += &extra;
        let _ = std::fs::OpenOptions::new().append(true).open(out.join("viewer.txt")).and_then(|mut f| std::io::Write::write_all(&mut f, extra.as_bytes()));
        let at = mark.load(std::sync::atomic::Ordering::Relaxed);
        if at != u64::MAX {
            let line = format!("idle_bytes {}\n", views.bytes.load(std::sync::atomic::Ordering::Relaxed).saturating_sub(at));
            report += &line;
            let _ = std::fs::OpenOptions::new().append(true).open(out.join("viewer.txt")).and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
        }
        print!("{report}");
        viewer.close();
    });
    if headless {
        session();
        return;
    }
    #[cfg(feature = "window")]
    {
        // The session on its own thread, the window on this one.
        std::thread::spawn(move || {
            session();
            eprintln!("fenestra-view: the session ended");
        });
        if let Err(e) = fenestra_view::window::run(views_w, window_input, title) {
            die(e);
        }
        std::process::exit(0);
    }
    #[cfg(not(feature = "window"))]
    {
        let _ = (views_w, window_input, title);
        die("built without the window feature: only --headless");
    }
}

/// Run `cmd fenestra-shell mint KEY` (the SSH transport) and take the mint
/// from its output: the one `FENESTRA_MINT` line, within a bound. ssh's own
/// prompts go to the terminal (stdin and stderr are inherited).
fn ssh_mint(cmd: &str, public: &[u8], take_over: bool) -> Result<mint::DevMint, String> {
    use std::io::Read;
    let mut w = cmd.split_whitespace();
    let prog = w.next().ok_or("an empty --ssh command")?;
    let mut c = std::process::Command::new(prog);
    c.args(w).arg("fenestra-shell").arg("mint").arg(mint::hex(public));
    if take_over {
        c.arg("--take-over");
    }
    let mut child = c.stdout(std::process::Stdio::piped()).spawn().map_err(|e| format!("{prog}: {e}"))?;
    let mut out = String::new();
    let read = child.stdout.take().ok_or("no stdout")?.take(MAX_SSH_OUTPUT).read_to_string(&mut out);
    let status = child.wait().map_err(|e| e.to_string())?;
    read.map_err(|e| format!("reading the mint: {e}"))?;
    if let Some(why) = out.lines().find_map(|l| l.strip_prefix("FENESTRA_REFUSED ")) {
        return Err(format!("the host refused: {why}"));
    }
    let line = out.lines().find_map(|l| l.strip_prefix("FENESTRA_MINT ")).ok_or_else(|| format!("no mint in the output ({status})"))?;
    mint::DevMint::parse_line(line).map_err(|e| e.to_string())
}

/// Most output read from the SSH command.
const MAX_SSH_OUTPUT: u64 = 16 * 1024;
/// How long a probe waits for its frame.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long auto waits for an answer over UDP before trying TCP.
const UDP_PROBE: Duration = Duration::from_secs(4);
/// How long one connection attempt may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(12);

fn parse_script(text: &str) -> Result<Vec<(f64, Input, bool)>, String> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let bad = || format!("line {}: {line:?}", n + 1);
        let mut w: Vec<&str> = line.split_whitespace().collect();
        let probe = w.last() == Some(&"PROBE");
        if probe {
            w.pop();
        }
        let f = |i: usize| w.get(i).and_then(|v| v.parse::<f32>().ok()).filter(|v| v.is_finite()).ok_or_else(bad);
        let on = |i: usize| match w.get(i) {
            Some(&"1") => Ok(true),
            Some(&"0") => Ok(false),
            _ => Err(bad()),
        };
        let at = w.first().and_then(|v| v.parse::<f64>().ok()).filter(|v| v.is_finite() && *v >= 0.0).ok_or_else(bad)?;
        let i = match w.get(1).copied() {
            Some("MOVE") => Input::Motion { x: f(2)?, y: f(3)? },
            Some("BTN") => Input::Button { button: w.get(2).and_then(|v| v.parse().ok()).filter(|b| (1..=5).contains(b)).ok_or_else(bad)?, pressed: on(3)? },
            Some("KEY") => Input::Key {
                usage: w.get(2).and_then(|v| v.parse().ok()).ok_or_else(bad)?,
                pressed: on(3)?,
                modifiers: w.get(4).map_or(Some(0), |v| v.parse().ok()).ok_or_else(bad)?,
            },
            Some("SCROLL") => Input::Scroll { dx: 0.0, dy: f(2)? },
            // Per-app sessions, the topmost window: the person resizes it
            // or moves into it on the viewer.
            Some("RESIZE") => {
                let side = |i: usize| w.get(i).and_then(|v| v.parse::<u32>().ok()).filter(|v| fenestra_wire::scene::OUTPUT_SIDE.contains(v)).ok_or_else(bad);
                Input::Resize { window: 0, width: side(2)?, height: side(3)? }
            }
            Some("FOCUS") => Input::Focus { window: 0 },
            _ => return Err(bad()),
        };
        out.push((at, i, probe));
    }
    Ok(out)
}
