//! `fenestra-session (--mint-fd N | --mint-dir DIR) [--bind ADDR] [--grace S]`:
//! one Fenestra session on
//! Atrium (fenestra.md §3.2). `fenestrad` starts it with the link to
//! frescod's broker on its stdin (a socket); it terminates QUIC for the
//! viewers that prove the session, asks for each window's differences over
//! the link and sends them on, with the layout. It parses no scene record
//! and exits when the link ends.
//!
//! The mint: with `--mint-fd N`, fenestrad writes `viewer_key HEX` on fd N
//! and this side answers with the mint line (`fenestra_wire::mint`) and
//! closes it; its keys and `K_sess` stay in this process's memory only.
//! `--mint-dir` is the development form, through files.
//!
//! With `--grace S` the session ends once it has had no viewer for S
//! seconds (from the start, or from its last viewer leaving).

use std::os::fd::FromRawFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use fenestra_session::link::LinkSource;
use fenestra_session::{serve, ViewerStats};
use fenestra_wire::{mint, Host, SessionKey};

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn die(why: impl std::fmt::Display) -> ! {
    eprintln!("fenestra-session: {why}");
    std::process::exit(1)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let how = match (arg(&args, "--mint-fd"), arg(&args, "--mint-dir")) {
        (Some(fd), None) => {
            let fd: i32 = fd.parse().ok().filter(|&n| n > 2).unwrap_or_else(|| die("--mint-fd N (above 2)"));
            // SAFETY: fenestrad passes fd N open for this process alone; it
            // is taken once here and owned by the File from then on.
            MintBy::Fd(unsafe { std::fs::File::from_raw_fd(fd) })
        }
        (None, Some(d)) => MintBy::Dir(PathBuf::from(d)),
        _ => die("one of --mint-fd N or --mint-dir DIR is needed"),
    };
    let grace = arg(&args, "--grace").map(|s| Duration::from_secs(s.parse().unwrap_or_else(|_| die("--grace SECONDS"))));
    let bind = arg(&args, "--bind").unwrap_or_else(|| "0.0.0.0:0".into());
    // SAFETY: fd 0 is ours for the life of the process; fenestrad made it a
    // socket, and nothing else in this process uses it.
    let link = unsafe { std::os::unix::net::UnixStream::from_raw_fd(0) };
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().worker_threads(2).build().unwrap_or_else(|e| die(e));
    rt.block_on(run(link, how, bind, grace));
}

enum MintBy {
    Fd(std::fs::File),
    Dir(PathBuf),
}

/// `viewer_key HEX`, one bounded line from fenestrad.
fn read_viewer_line(f: &mut std::fs::File) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    while line.len() <= mint::MAX_LINE {
        match f.read(&mut b) {
            Ok(0) => return Err("the mint fd closed before a key".into()),
            Ok(_) if b[0] == b'\n' => break,
            Ok(_) => line.push(b[0]),
            Err(e) => return Err(e.to_string()),
        }
    }
    let text = std::str::from_utf8(&line).map_err(|_| "a key line that is not text".to_string())?;
    let hex = text.strip_prefix("viewer_key ").ok_or("not a viewer_key line")?;
    mint::unhex(hex).map_err(|e| e.to_string())
}

async fn run(link: std::os::unix::net::UnixStream, mut how: MintBy, bind: String, grace: Option<Duration>) {
    link.set_nonblocking(true).unwrap_or_else(|e| die(format!("link: {e}")));
    let link = tokio::net::UnixStream::from_std(link).unwrap_or_else(|e| die(format!("link: {e}")));
    let (r, w) = link.into_split();
    let (src, wake) = LinkSource::start(r, w);

    let viewer_key = match &mut how {
        MintBy::Fd(f) => read_viewer_line(f).unwrap_or_else(|e| die(format!("mint: {e}"))),
        MintBy::Dir(dir) => {
            let mut waited = 0;
            loop {
                match mint::read_viewer_key(dir) {
                    Ok(k) => break k,
                    Err(_) if waited < 1200 => {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        waited += 1;
                    }
                    Err(e) => die(format!("no viewer key in {}: {e}", dir.display())),
                }
            }
        }
    };
    let key = SessionKey::generate().unwrap_or_else(|e| die(e));
    let secret = mint::fresh_secret().unwrap_or_else(|e| die(e));
    let host = Host::bind(bind.parse().unwrap_or_else(|_| die("--bind address")), &key, &viewer_key, secret.clone()).unwrap_or_else(|e| die(e));
    let addr = host.local_addr().unwrap_or_else(|e| die(e));
    let m = mint::DevMint { addr, host_key: key.public().to_vec(), secret };
    match how {
        MintBy::Fd(mut f) => {
            use std::io::Write;
            f.write_all(format!("{}\n", m.line()).as_bytes()).unwrap_or_else(|e| die(format!("mint: {e}")));
            drop(f);
        }
        MintBy::Dir(dir) => m.write(&dir).unwrap_or_else(|e| die(e)),
    }
    drop(m);
    eprintln!("fenestra-session: serving on {addr}");
    src.session(true).await.unwrap_or_else(|e| die(e));

    let host = Arc::new(host);
    // Accepting runs on its own: an accept dropped by the select below
    // (a layout arriving mid-handshake) would refuse that viewer.
    let (conn_tx, mut conns) = tokio::sync::mpsc::channel(4);
    {
        let host = host.clone();
        tokio::spawn(async move {
            loop {
                let r = match host.accept().await {
                    Ok(Some(hc)) => Ok(hc),
                    Ok(None) => return,
                    Err(e) => Err(e),
                };
                if conn_tx.send(r).await.is_err() {
                    return;
                }
            }
        });
    }
    let mut n = 0u64;
    let mut ended = wake.clone();
    // Viewers attached now, and when the last one left.
    let (live_tx, mut live) = tokio::sync::watch::channel(0usize);
    let live_tx = Arc::new(live_tx);
    loop {
        let none = *live.borrow() == 0;
        let idle = async {
            match grace {
                Some(g) if none => tokio::time::sleep(g).await,
                _ => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = idle => {
                eprintln!("fenestra-session: no viewer for {}s; ending", grace.map_or(0, |g| g.as_secs()));
                break;
            }
            _ = live.changed() => {}
            a = conns.recv() => match a {
                Some(Ok(hc)) => {
                    n += 1;
                    if src.peer(hc.conn.remote_address().ip()).await.is_err() {
                        break;
                    }
                    let (src, wake, conn, live_tx) = (src.clone(), wake.clone(), n, live_tx.clone());
                    live_tx.send_modify(|v| *v += 1);
                    tokio::spawn(async move {
                        let stats = Arc::new(ViewerStats::default());
                        let r = serve(hc, src, wake, stats.clone()).await;
                        eprintln!(
                            "fenestra-session: viewer {conn} gone ({}): frames {} wholes {} bytes {} acks {} resumed {} resent {} inputs {} dropped {}",
                            r.err().map_or("closed".to_string(), |e| e.to_string()),
                            stats.frames.load(std::sync::atomic::Ordering::Relaxed),
                            stats.wholes.load(std::sync::atomic::Ordering::Relaxed),
                            stats.bytes.load(std::sync::atomic::Ordering::Relaxed),
                            stats.acks.load(std::sync::atomic::Ordering::Relaxed),
                            stats.resumed.load(std::sync::atomic::Ordering::Relaxed),
                            stats.resent.load(std::sync::atomic::Ordering::Relaxed),
                            stats.inputs.load(std::sync::atomic::Ordering::Relaxed),
                            stats.inputs_dropped.load(std::sync::atomic::Ordering::Relaxed),
                        );
                        live_tx.send_modify(|v| *v -= 1);
                    });
                }
                None => break,
                Some(Err(e)) => eprintln!("fenestra-session: refused a connection: {e}"),
            },
            r = ended.changed() => if r.is_err() {
                eprintln!("fenestra-session: the link ended");
                break;
            },
        }
    }
    host.close_and_wait().await;
}
