//! An exec-only Navigator worker. Its installed bundle restricts the caller
//! to the broker. The broker's socket carries one Start byte and one length-prefixed document.
use navigator_fetch::{addr::Policy, tls_config, youtube, Conversation};
use std::io::Write;

fn run() -> Result<(), String> {
    #[cfg(not(target_os = "freebsd"))]
    if std::env::args().nth(1).as_deref() != Some("--unconfined-for-testing") {
        return Err("the host control requires --unconfined-for-testing".into());
    }
    #[cfg(target_os = "freebsd")]
    let mut out = {
        use std::io::Read;
        use std::os::fd::AsFd;
        use std::os::unix::net::UnixStream;
        let fd = std::io::stdin()
            .as_fd()
            .try_clone_to_owned()
            .map_err(|e| e.to_string())?;
        let mut channel = UnixStream::from(fd);
        let timeout = Some(std::time::Duration::from_secs(10));
        channel
            .set_read_timeout(timeout)
            .and_then(|_| channel.set_write_timeout(timeout))
            .map_err(|e| e.to_string())?;
        let mut start = [0];
        channel.read_exact(&mut start).map_err(|e| e.to_string())?;
        if start != [1] {
            return Err("missing broker start".into());
        }
        channel
    };
    #[cfg(not(target_os = "freebsd"))]
    let mut out = std::io::stdout().lock();
    let pem = std::fs::read("/etc/ssl/cert.pem").map_err(|e| e.to_string())?;
    let tls = tls_config(&pem)?;
    #[cfg(target_os = "freebsd")]
    let net = {
        use navigator_fetch::{sandbox, Locked};
        let net = sandbox::enter().map_err(|e| e.to_string())?;
        if !sandbox::in_capability_mode() {
            return Err("Capsicum entry failed".into());
        }
        Locked(std::sync::Mutex::new(net))
    };
    #[cfg(not(target_os = "freebsd"))]
    let net = navigator_fetch::Direct;
    let doc = youtube::entry(&mut Conversation::new(&net, tls, Policy::default()))?;
    out.write_all(&(doc.len() as u32).to_be_bytes())
        .and_then(|_| out.write_all(&doc))
        .and_then(|_| out.flush())
        .map_err(|e| e.to_string())
}

fn main() {
    if let Err(reason) = run() {
        eprintln!("navigator-youtube-entry: {reason}");
        std::process::exit(1);
    }
}
