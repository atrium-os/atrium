//! monstratord, the link opener (docs/spec/opening-links.md §5–6).
//!
//! Runs jailed as `_monstrator`, with no network and none of the person's
//! files. For each app asking (peer uid), one link at a time: checks it
//! (atrium-links), asks portcullisd for a grant with the asker's press
//! (portcullisd spends it), picks the app (the default, or the only one;
//! the sheet for a choice is L3), and has portcullisd hand it over. Logs
//! name the scheme, never the link.
//!
//! Settings: `MONSTRATOR_SOCKET` (where to listen), `PORTCULLIS_SOCKET`.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use monstrator::{Answer, Ask, MAX_LINE};
use portcullis_ipc::{round_trip, Request, Response};

#[cfg(not(target_os = "linux"))]
fn peer_uid(s: &UnixStream) -> Option<u32> {
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: getpeereid on a connected socket we own, into two locals.
    (unsafe { libc::getpeereid(s.as_raw_fd(), &mut uid, &mut gid) } == 0).then_some(uid)
}

/// (Host builds: the same answer from SO_PEERCRED.)
#[cfg(target_os = "linux")]
fn peer_uid(s: &UnixStream) -> Option<u32> {
    let mut c: libc::ucred = unsafe { std::mem::zeroed() };
    let mut n = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt into a ucred we own, its size given.
    (unsafe { libc::getsockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&mut c as *mut libc::ucred).cast(), &mut n) } == 0).then_some(c.uid)
}

fn portcullis_socket() -> PathBuf {
    std::env::var_os("PORTCULLIS_SOCKET").map(PathBuf::from)
        .or_else(|| portcullis_ipc::resolve_socket_path().map(PathBuf::from))
        .unwrap_or_else(|| portcullis_ipc::SERVICE_SOCKET_PATH.into())
}

/// One link from `asker`, start to end.
fn open(asker: u32, url: &str, activation: [u8; 16]) -> Result<String, String> {
    let link = atrium_links::check(url)?;
    let mut p = UnixStream::connect(portcullis_socket()).map_err(|e| format!("portcullisd: {e}"))?;
    p.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let ask = |p: &mut UnixStream, r: &Request| round_trip(p, r).map_err(|e| format!("portcullisd: {e}"));
    match ask(&mut p, &Request::Hello { version: portcullis_ipc::PROTO_VERSION })? {
        Response::Hello { .. } => {}
        other => return Err(format!("portcullisd: {other:?}")),
    }
    let (grant, apps) = match ask(&mut p, &Request::LinkGrant { asker_uid: asker, activation, scheme: link.scheme.clone(), app: link.app.clone() })? {
        Response::LinkGranted { grant, apps, .. } => (grant, apps),
        Response::Error { message } => return Err(message),
        other => return Err(format!("portcullisd: {other:?}")),
    };
    let Some(app) = monstrator::pick(&apps).map(str::to_string) else {
        return Err(format!("{} apps open {} links; choosing one needs Monstrator's sheet, not built yet", apps.len(), link.scheme));
    };
    match ask(&mut p, &Request::DeliverLink { grant, app: app.clone(), url: link.url, always: false })? {
        Response::LinkDelivered { .. } => Ok(link.scheme),
        Response::Error { message } => Err(message),
        other => Err(format!("portcullisd: {other:?}")),
    }
}

fn serve(s: UnixStream, busy: Arc<Mutex<HashSet<u32>>>) {
    let Some(uid) = peer_uid(&s) else { return };
    let answer = |mut s: &UnixStream, a: Answer| {
        let mut line = serde_json::to_vec(&a).unwrap_or_default();
        line.push(b'\n');
        let _ = s.write_all(&line);
    };
    // One link at a time per asker (§5).
    if !busy.lock().unwrap().insert(uid) {
        return answer(&s, Answer::NotOpened { why: "one link at a time".into() });
    }
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let mut line = String::new();
    let read = BufReader::new((&s).take(MAX_LINE as u64)).read_line(&mut line);
    let result = match read.ok().and_then(|_| serde_json::from_str::<Ask>(line.trim_end()).ok()) {
        Some(Ask::OpenLink { url, activation }) => open(uid, &url, activation),
        None => Err("not a request".into()),
    };
    match result {
        Ok(scheme) => {
            eprintln!("monstratord: a {scheme} link from uid {uid}: opened");
            answer(&s, Answer::Opened);
        }
        Err(why) => {
            eprintln!("monstratord: a link from uid {uid}: not opened: {why}");
            answer(&s, Answer::NotOpened { why });
        }
    }
    busy.lock().unwrap().remove(&uid);
}

fn main() {
    let path = monstrator::socket();
    let _ = std::fs::remove_file(&path);
    let l = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("monstratord: {}: {e}", path.display());
            std::process::exit(1);
        }
    };
    // Anyone may ask; each asker is its peer uid, and portcullisd decides.
    if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)) {
        eprintln!("monstratord: {}: {e}", path.display());
        std::process::exit(1);
    }
    eprintln!("monstratord: listening on {} (uid {})", path.display(), unsafe { libc::getuid() });
    let busy = Arc::new(Mutex::new(HashSet::new()));
    for s in l.incoming().flatten() {
        let busy = busy.clone();
        let _ = std::thread::Builder::new().name("link".into()).spawn(move || serve(s, busy));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use portcullis_ipc::{read_request, write_response, LinkChoice};

    /// A stand-in portcullisd: answers Hello, then `answers` in turn, and
    /// gives back what it was asked.
    fn fake(answers: Vec<Response>) -> (PathBuf, std::thread::JoinHandle<Vec<Request>>) {
        let d = std::env::temp_dir().join(format!("monstrator-{}-{}", std::process::id(), answers.len()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("portcullis.sock");
        let l = UnixListener::bind(&p).unwrap();
        let h = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut w = s;
            let mut got = Vec::new();
            for a in std::iter::once(Response::Hello { version: portcullis_ipc::PROTO_VERSION }).chain(answers) {
                match read_request(&mut r) {
                    Ok(q) => got.push(q),
                    Err(_) => break,
                }
                write_response(&mut w, &a).unwrap();
            }
            got
        });
        (p, h)
    }

    #[test]
    fn a_link_goes_through_portcullisd_once() {
        let (p, h) = fake(vec![
            Response::LinkGranted { grant: [9; 16], person: "ana".into(), apps: vec![LinkChoice { id: "org.example.mail".into(), name: "Mail".into(), default: false }] },
            Response::LinkDelivered { app: "Mail".into() },
        ]);
        std::env::set_var("PORTCULLIS_SOCKET", &p);
        assert_eq!(open(50001, "MAILTO:ana@example.org", [1; 16]).unwrap(), "mailto");
        let got = h.join().unwrap();
        assert!(matches!(&got[1], Request::LinkGrant { asker_uid: 50001, activation, scheme, app: None } if *activation == [1; 16] && scheme == "mailto"));
        assert!(matches!(&got[2], Request::DeliverLink { grant, app, url, always: false } if *grant == [9; 16] && app == "org.example.mail" && url == "mailto:ana@example.org"));
        // Refused before portcullisd is asked anything.
        std::env::set_var("PORTCULLIS_SOCKET", "/nonexistent/portcullis.sock");
        assert!(open(50001, "javascript:alert(1)", [1; 16]).unwrap_err().contains("never opened"));
        assert!(open(50001, "mailto:a@b\nbcc:c@d", [1; 16]).unwrap_err().contains("control"));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }
}
