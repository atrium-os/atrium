//! The core of scripts/vm-workers-check.sh: calls its worker and prints
//! `workers-gate <check> <result>` lines (to portcullisd's log).
use std::io::{Read, Write};
use atrium_worker::{Error, Message, Workers};

#[derive(Message, Debug)]
#[message(version = 1)]
pub struct Ask {
    #[message(max_len = 32)]
    pub what: String,
}

#[derive(Message, Debug)]
#[message(version = 1)]
pub struct Answer {
    #[message(max_len = 4096)]
    pub text: String,
}

fn say(check: &str, r: String) {
    println!("workers-gate {check} {r}");
    let _ = std::io::stdout().flush();
}

fn ask(w: &Workers, what: &str, f: Option<std::fs::File>) -> String {
    match w.call::<Ask, Answer>("echo", &Ask { what: what.into() }, f) {
        Ok(a) => format!("ok {}", a.text),
        Err(Error::Worker(m)) => format!("worker-error {m}"),
        Err(Error::Crashed(c)) => format!("crashed {c:?}"),
        Err(Error::Timeout(d)) => format!("timeout {}", d.as_secs()),
        Err(e) => format!("error {e}"),
    }
}

/// ExecInstance of another app's id, on portcullisd's own socket if we have it.
fn exec_other() -> String {
    let p = ["/atrium/sockets/portcullis/portcullis.sock", "/atrium/sockets/workers/workers.sock"]
        .into_iter().find(|p| std::path::Path::new(p).exists());
    let Some(p) = p else { return "no-socket".into() };
    let Ok(mut s) = std::os::unix::net::UnixStream::connect(p) else { return "no-connect".into() };
    let _ = s.write_all(b"{\"op\":\"hello\",\"version\":1}\n");
    let _ = s.write_all(b"{\"op\":\"exec_instance\",\"app_id\":\"org.atrium.navigator\",\"instance\":\"x\",\"tmpfs_mb\":16}\n");
    let mut out = String::new();
    let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(10)));
    let _ = s.read_to_string(&mut out);
    format!("{} {}", p.rsplit('/').next().unwrap_or(""), out.lines().nth(1).unwrap_or("").chars().take(160).collect::<String>())
}

/// A socket worker in its own jail, given one more descriptor (a pipe) at 3.
fn extra_fd(w: &Workers) -> String {
    use std::io::BufRead;
    let mut p = [0i32; 2];
    // SAFETY: pipe(2) into a two-int array.
    if unsafe { libc::pipe(p.as_mut_ptr()) } != 0 { return "no pipe".into() }
    use std::os::fd::{AsFd, FromRawFd, OwnedFd};
    let (r, wr) = unsafe { (OwnedFd::from_raw_fd(p[0]), OwnedFd::from_raw_fd(p[1])) };
    let mut wf = std::fs::File::from(wr);
    let _ = writeln!(wf, "a line through the pipe");
    drop(wf);
    match w.spawn_socket("sock", &[r.as_fd()]) {
        Ok(sw) => {
            let mut line = String::new();
            let _ = sw.sock.set_read_timeout(Some(std::time::Duration::from_secs(30)));
            let _ = std::io::BufReader::new(&sw.sock).read_line(&mut line);
            let code = sw.wait();
            format!("{} (exit {code:?})", line.trim())
        }
        Err(e) => format!("error {e}"),
    }
}

fn main() {
    let w = match Workers::get() {
        Ok(w) => w,
        Err(e) => { say("start", format!("error {e}")); return }
    };
    say("core-uid", format!("{}", unsafe { libc::getuid() }));
    say("uid", ask(&w, "uid", None));
    say("uid-again", ask(&w, "uid", None));
    say("read", ask(&w, "read", std::fs::File::open("/atrium.toml").ok()));
    say("fail", ask(&w, "fail", None));
    say("net", ask(&w, "net", None));
    say("nest", ask(&w, "nest", None));
    say("ifs", ask(&w, "ifs", None));
    // 20 calls, one jail each: how long a call takes, start to answer.
    let mut ms: Vec<f64> = (0..20).map(|_| {
        let t = std::time::Instant::now();
        let _ = ask(&w, "uid", None);
        t.elapsed().as_secs_f64() * 1000.0
    }).collect();
    ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    say("timing", format!("p50 {:.1} ms, p90 {:.1} ms, mean {:.1} ms over {}",
        ms[ms.len() / 2], ms[ms.len() * 9 / 10], ms.iter().sum::<f64>() / ms.len() as f64, ms.len()));
    say("crash", ask(&w, "crash", None));
    say("timeout", ask(&w, "sleep", None));
    say("memory", ask(&w, "hog", None));
    say("after-memory", ask(&w, "uid", None));
    match w.call::<Ask, Answer>("nope", &Ask { what: "uid".into() }, None) {
        Err(Error::NoSuchWorker(_)) => say("nope", "no-such-worker".into()),
        r => say("nope", format!("{r:?}")),
    }
    say("exec-other", exec_other());
    say("extra-fd", extra_fd(&w));
    say("done", "1".into());
}
