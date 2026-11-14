//! Connects, checks or learns the host key, logs in with a password from
//! SSH_PROBE_PASSWORD, starts the sftp subsystem and asks for its version.
//!   ssh-probe HOST PORT USER [SHA256:pin]
use std::net::TcpStream;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (host, port, user) = (&a[1], a[2].parse::<u16>().unwrap(), &a[3]);
    let policy = match a.get(4) {
        Some(p) => capsa_ssh::HostKeyPolicy::Pinned(p.clone()),
        None => capsa_ssh::HostKeyPolicy::Learn,
    };
    let s = TcpStream::connect((host.as_str(), port)).expect("connect");
    let mut c = match capsa_ssh::Client::connect(s, &policy) {
        Ok(c) => c,
        Err(e) => { eprintln!("ssh-probe: {e}"); std::process::exit(2) }
    };
    println!("host key {}", c.host_fingerprint());
    if matches!(policy, capsa_ssh::HostKeyPolicy::Learn) {
        return;
    }
    let pw = std::env::var("SSH_PROBE_PASSWORD").unwrap_or_default();
    if let Err(e) = c.login(user, capsa_ssh::Auth::Password(pw.as_bytes()), "sftp") {
        eprintln!("ssh-probe: {e}");
        std::process::exit(3);
    }
    // SSH_FXP_INIT, version 3.
    c.write_data(&[0, 0, 0, 5, 1, 0, 0, 0, 3]).unwrap();
    let len = c.read_exact_data(4).unwrap();
    let n = u32::from_be_bytes([len[0], len[1], len[2], len[3]]) as usize;
    let body = c.read_exact_data(n).unwrap();
    println!("sftp: type {} version {}", body[0], u32::from_be_bytes([body[1], body[2], body[3], body[4]]));
    c.disconnect();
}
