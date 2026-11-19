//! Drive `atrium-crypta-ac` on the host against a real gateway (ocserv in
//! the Ubuntu VM, scripts/crypta-ocserv-vm.sh), standing in for cryptad,
//! the tun and the secrets helper:
//!
//!   ac_harness ENGINE HOST ADDR:PORT CA.pem [field=value ...] [--gp] [--saved PW] [--user U] [--ping A.B.C.D] [--hold]
//!
//! Prompts are answered from the `field=value` list (a value `@held:X` goes
//! as an Arca handle the fake helper redeems to X; `@totp` is the code of
//! the gate's OTP user). With `--ping` it sends an ICMP echo through the
//! tunnel and waits for the answer. It prints one line per event and ends
//! `harness: ok` or `harness: FAIL why`. On FreeBSD, `--native-tun /dev/tunN`
//! uses a configured point-to-point tun and the system ping. `--jid ID`
//! attaches and drops privileges before starting the engine.

use std::io::{BufRead, BufReader, Read};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aura_ipc::Line;
use crypta_proto::engine::{self, FromEngine, StartAc, ToEngine, Value};

fn fail(s: &str) -> ! {
    println!("harness: FAIL {s}");
    std::process::exit(1)
}

fn totp() -> String {
    let out = Command::new("sh").arg(concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/crypta-ocserv-vm.sh")).arg("totp").output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn csum(b: &[u8]) -> u16 {
    let mut s = 0u32;
    for c in b.chunks(2) {
        s += u32::from(u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]));
    }
    while s > 0xffff {
        s = (s & 0xffff) + (s >> 16);
    }
    !(s as u16)
}

fn echo(src: [u8; 4], dst: [u8; 4]) -> Vec<u8> {
    let mut icmp = vec![8, 0, 0, 0, 0x12, 0x34, 0, 1];
    icmp.extend_from_slice(b"crypta-ac");
    let c = csum(&icmp);
    icmp[2..4].copy_from_slice(&c.to_be_bytes());
    let len = 20 + icmp.len();
    let mut ip = vec![0x45, 0, (len >> 8) as u8, len as u8, 0, 1, 0, 0, 64, 1, 0, 0];
    ip.extend_from_slice(&src);
    ip.extend_from_slice(&dst);
    let c = csum(&ip);
    ip[10..12].copy_from_slice(&c.to_be_bytes());
    let mut f = 2u32.to_be_bytes().to_vec();
    f.extend(ip);
    f.extend(icmp);
    f
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 5 {
        fail("usage: ac_harness ENGINE HOST ADDR:PORT CA.pem [field=value ...]");
    }
    let (exe, host, gw, ca) = (&a[1], &a[2], a[3].parse().unwrap_or_else(|_| fail("ADDR:PORT")), &a[4]);
    let mut answers: Vec<(String, String)> = Vec::new();
    let (mut saved, mut user, mut ping, mut pin, mut group) = (None, None, None, None, None);
    // `--cert chain.pem key.pem`: the fake helper signs with the key file
    // (in the real one, Arca does).
    let mut cert_chain: Vec<Vec<u8>> = Vec::new();
    let mut key_pem: Option<String> = None;
    let mut gp = false;
    let mut native_tun = None;
    let mut jid = None;
    let mut i = 5;
    while i < a.len() {
        match a[i].as_str() {
            "--gp" => gp = true,
            "--native-tun" => {
                native_tun = Some(a[i + 1].clone());
                i += 1;
            }
            "--jid" => {
                jid = Some(a[i + 1].parse::<i32>().expect("jail ID"));
                i += 1;
            }
            "--saved" => {
                saved = Some(a[i + 1].clone());
                i += 1;
            }
            "--cert" => {
                let pem = std::fs::read_to_string(&a[i + 1]).unwrap();
                for blk in pem.split("-----BEGIN CERTIFICATE-----").skip(1) {
                    let b64: String = blk.split("-----END").next().unwrap().lines().collect();
                    cert_chain.push(crypta_proto::b64::decode(&b64).unwrap());
                }
                key_pem = Some(a[i + 2].clone());
                i += 2;
            }
            "--group" => {
                group = Some(a[i + 1].clone());
                i += 1;
            }
            "--user" => {
                user = Some(a[i + 1].clone());
                i += 1;
            }
            "--ping" => {
                ping = Some(a[i + 1].parse::<std::net::Ipv4Addr>().unwrap().octets());
                i += 1;
            }
            "--pin" => {
                let h: Vec<u8> = (0..32).map(|k| u8::from_str_radix(&a[i + 1][2 * k..2 * k + 2], 16).unwrap()).collect();
                pin = Some(<[u8; 32]>::try_from(h).unwrap());
                i += 1;
            }
            kv => {
                let (k, v) = kv.split_once('=').unwrap_or_else(|| fail(kv));
                answers.push((k.into(), v.into()));
            }
        }
        i += 1;
    }
    let cas = if pin.is_some() {
        vec![]
    } else {
        let pem = std::fs::read(ca).unwrap_or_else(|e| fail(&format!("{ca}: {e}")));
        let b64: String = String::from_utf8_lossy(&pem).lines().filter(|l| !l.starts_with("-----")).collect();
        vec![crypta_proto::b64::decode(&b64).unwrap_or_else(|| fail("CA base64"))]
    };
    let (ctl, ctl_e) = UnixStream::pair().unwrap();
    let (tun, tun_e): (_, OwnedFd) = match native_tun {
        Some(ref path) => (None, open_tun(path)),
        None => {
            let (read, write) = UnixDatagram::pair().unwrap();
            (Some(read), write.into())
        }
    };
    let (sec, sec_e) = UnixStream::pair().unwrap();
    let fds: Vec<OwnedFd> = vec![ctl_e.into(), tun_e, sec_e.into()];
    let raw: Vec<i32> = fds.iter().map(|f| f.as_raw_fd()).collect();
    let mut cmd = Command::new(exe);
    cmd.stdin(Stdio::null()).stdout(Stdio::null());
    // SAFETY: dup2 between fork and exec; the descriptors are live.
    unsafe {
        cmd.pre_exec(move || {
            if let Some(jid) = jid {
                attach(jid)?;
            }
            libc::dup2(raw[0], 0);
            libc::dup2(raw[0], 1);
            libc::dup2(raw[1], 3);
            libc::dup2(raw[2], 4);
            Ok(())
        });
    }
    let mut child = cmd.spawn().unwrap_or_else(|e| fail(&format!("{exe}: {e}")));
    drop(fds);

    // The fake secrets helper: handles `@held:X` stand for, the saved
    // password, the session cookie.
    let cookie: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let held: Arc<Mutex<Vec<([u8; 16], String)>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let (cookie, held, saved, key_pem) = (cookie.clone(), held.clone(), saved.clone(), key_pem.clone());
        let mut w = sec.try_clone().unwrap();
        std::thread::spawn(move || {
            for l in BufReader::new(sec).lines() {
                let Ok(l) = l else { return };
                let Some(r) = Line::parse(&l) else { return };
                let ans = match r.verb.as_str() {
                    "saved" => match &saved {
                        Some(p) => Line::new("ok").with_hex("value", p.as_bytes()),
                        None => Line::new("error").with("status", 1),
                    },
                    "take" => {
                        let h = r.hex("handle").unwrap_or_default();
                        let mut hs = held.lock().unwrap();
                        match hs.iter().position(|(k, _)| k[..] == h[..]) {
                            Some(p) => Line::new("ok").with_hex("value", hs.remove(p).1.as_bytes()),
                            None => Line::new("error").with("status", 1),
                        }
                    }
                    "cookie-set" => {
                        *cookie.lock().unwrap() = r.hex("value");
                        println!("secrets: cookie kept");
                        Line::new("ok")
                    }
                    "cookie" => match cookie.lock().unwrap().clone() {
                        Some(c) => Line::new("ok").with_hex("value", &c),
                        None => Line::new("error").with("status", 1),
                    },
                    "cookie-drop" => {
                        *cookie.lock().unwrap() = None;
                        println!("secrets: cookie dropped");
                        Line::new("ok")
                    }
                    "sign" => {
                        // openssl signs the digest as the key file's owner.
                        let d = r.hex("digest").unwrap_or_default();
                        let kp = key_pem.clone().unwrap_or_default();
                        let dir = std::env::temp_dir().join(format!("harness-{}", std::process::id()));
                        std::fs::create_dir_all(&dir).unwrap();
                        std::fs::write(dir.join("d"), &d).unwrap();
                        let o = Command::new("openssl").args(["pkeyutl", "-sign", "-inkey", &kp, "-in"]).arg(dir.join("d")).output().unwrap();
                        println!("secrets: signed a handshake");
                        if o.status.success() { Line::new("ok").with_hex("sig", &o.stdout) } else { Line::new("error").with("status", 7) }
                    }
                    "commit" => {
                        println!("secrets: password saved");
                        Line::new("ok")
                    }
                    v => {
                        println!("secrets: unexpected {v}");
                        Line::new("error").with("status", 1)
                    }
                };
                if ans.send(&mut w).is_err() {
                    return;
                }
            }
        });
    }

    let mut c = ctl;
    let start = StartAc {
        host: host.clone(),
        gateway: gw,
        path: "/".into(),
        cas,
        pin,
        hostname: "atrium".into(),
        mtu: 1406,
        username: user,
        group: group.clone(),
        saved: saved.is_some(),
        cert_chain: cert_chain.clone(),
        resume: false,
    };
    engine::write(&mut c, &if gp { ToEngine::StartGp(start) } else { ToEngine::StartAc(start) }).unwrap();
    let mut rd = engine::Reader::default();
    let t0 = Instant::now();
    let mut addr = None;
    let mut buf = [0u8; 65536];
    let mut ups = 0;
    if let Some(tun) = &tun {
        tun.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    }
    c.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    while t0.elapsed() < Duration::from_secs(120) {
        match c.read(&mut buf) {
            Ok(0) => fail("the engine closed"),
            Ok(n) => rd.push(&buf[..n]),
            Err(_) => {}
        }
        while let Ok(Some(m)) = rd.next::<FromEngine>() {
            match m {
                FromEngine::Prompt(p) => {
                    println!("prompt: {:?} error={:?} fields={:?}", p.message, p.error, p.fields.iter().map(|f| (&f.name, f.kind)).collect::<Vec<_>>());
                    let mut vals = Vec::new();
                    for f in &p.fields {
                        let Some(k) = answers.iter().position(|(k, _)| *k == f.name) else { fail(&format!("no answer for {}", f.name)) };
                        let v = answers.remove(k).1;
                        let v = if v == "@totp" { totp() } else { v };
                        if let Some(x) = v.strip_prefix("@held:") {
                            let mut h = [0u8; 16];
                            getrandom(&mut h);
                            held.lock().unwrap().push((h, x.to_string()));
                            vals.push((f.name.clone(), Value::Held { handle: h, keep: true }));
                        } else if f.kind == 1 || f.kind == 2 {
                            // Secrets always go as handles, as Curia sends them.
                            let mut h = [0u8; 16];
                            getrandom(&mut h);
                            held.lock().unwrap().push((h, v));
                            vals.push((f.name.clone(), Value::Held { handle: h, keep: false }));
                        } else {
                            vals.push((f.name.clone(), Value::Text(v)));
                        }
                    }
                    engine::write(&mut c, &ToEngine::Answer(Some(vals))).unwrap();
                }
                FromEngine::Pushed(p) => {
                    println!("pushed: addr={}/{} mtu={} dns={:?} domains={:?} includes={} full={} ignored={:?} banner={:?}", p.addr, p.prefix, p.mtu, p.dns, p.domains, p.includes.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(","), p.full, p.ignored, p.banner);
                    addr = Some(p.addr.octets());
                    if gp {
                        engine::write(&mut c, &ToEngine::Applied).unwrap();
                    }
                }
                FromEngine::Up => {
                    ups += 1;
                    println!("up");
                    match (ping, addr) {
                        (Some(d), Some(s)) => match &tun {
                            Some(tun) => { tun.send(&echo(s, d)).unwrap(); }
                            None => {
                                if let Some(jid) = jid {
                                    if !jailed_in_capmode(child.id(), jid) {
                                        let _ = child.kill();
                                        fail("native engine jail or capability mode");
                                    }
                                    println!("engine: jail {jid}, capability mode");
                                }
                                let dst = std::net::Ipv4Addr::from(d);
                                let src = std::net::Ipv4Addr::from(s);
                                let status = Command::new("/sbin/ping")
                                    .args(["-n", "-c", "1", "-t", "5", "-S", &src.to_string(), &dst.to_string()])
                                    .status().unwrap();
                                if !status.success() {
                                    let _ = child.kill();
                                    fail("native tun ping");
                                }
                                if std::env::var("HARNESS_REBIND").is_ok() && ups == 1 {
                                    engine::write(&mut c, &ToEngine::Rebind).unwrap();
                                    continue;
                                }
                                let _ = engine::write(&mut c, &ToEngine::Stop);
                                let _ = child.wait();
                                println!("harness: ok (native tun)");
                                return;
                            }
                        },
                        _ => {
                            println!("harness: ok");
                            let _ = engine::write(&mut c, &ToEngine::Stop);
                            let _ = child.wait();
                            return;
                        }
                    }
                }
                FromEngine::Down { why } => println!("down: {why}"),
                FromEngine::Refused { why } => {
                    println!("refused: {why}");
                    let _ = child.wait();
                    println!("harness: refused");
                    return;
                }
                other => println!("{other:?}"),
            }
        }
        if let Some(n) = tun.as_ref().and_then(|tun| tun.recv(&mut buf).ok()) {
            if n > 24 && buf[24] == 0 {
                println!("echo reply from {}.{}.{}.{}", buf[16], buf[17], buf[18], buf[19]);
                if std::env::var("HARNESS_REBIND").is_ok() && ups == 1 {
                    println!("rebind");
                    engine::write(&mut c, &ToEngine::Rebind).unwrap();
                    continue;
                }
                println!("harness: ok");
                let _ = engine::write(&mut c, &ToEngine::Stop);
                let _ = child.wait();
                return;
            }
        }
    }
    let _ = child.kill();
    fail("timed out");
}

fn getrandom(b: &mut [u8]) {
    std::fs::File::open("/dev/urandom").unwrap().read_exact(b).unwrap();
}



#[cfg(target_os = "freebsd")]
fn open_tun(path: &str) -> OwnedFd {
    // FreeBSD TUNSIFHEAD is _IOW('t', 96, int).
    const TUNSIFHEAD: libc::c_ulong = 0x8004_7460;
    let tun = std::fs::OpenOptions::new().read(true).write(true).open(path).unwrap();
    let one: libc::c_int = 1;
    // SAFETY: tun is live and the ioctl reads an int.
    if unsafe { libc::ioctl(tun.as_raw_fd(), TUNSIFHEAD, &one) } != 0 {
        fail("setting TUNSIFHEAD");
    }
    tun.into()
}

#[cfg(not(target_os = "freebsd"))]
fn open_tun(_: &str) -> OwnedFd {
    fail("native tun mode requires FreeBSD")
}

#[cfg(target_os = "freebsd")]
fn attach(jid: i32) -> std::io::Result<()> {
    let gid = 65534;
    let uid = 65534;
    // SAFETY: called only in the child before exec, with one valid group.
    unsafe {
        if libc::jail_attach(jid) != 0 || libc::setgroups(1, &gid) != 0 ||
            libc::setgid(gid) != 0 || libc::setuid(uid) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut group = 0;
        if libc::getuid() != uid || libc::geteuid() != uid || libc::getgid() != gid ||
            libc::getegid() != gid || libc::getgroups(1, &mut group) != 1 || group != gid {
            return Err(std::io::Error::other("jail test privilege drop failed"));
        }
    }
    Ok(())
}

#[cfg(not(target_os = "freebsd"))]
fn attach(_: i32) -> std::io::Result<()> {
    Err(std::io::Error::other("jail mode requires FreeBSD"))
}


fn jailed_in_capmode(pid: u32, jid: i32) -> bool {
    let pid = pid.to_string();
    let jail = Command::new("/bin/ps").args(["-o", "jid=", "-p", &pid]).output();
    let caps = Command::new("/usr/bin/procstat").args(["-s", &pid]).output();
    jail.is_ok_and(|out| out.status.success() && String::from_utf8_lossy(&out.stdout).trim().parse() == Ok(jid))
        && caps.is_ok_and(|out| {
            out.status.success() && String::from_utf8_lossy(&out.stdout).lines()
                .nth(1).and_then(|row| row.split_whitespace().nth(9))
                .is_some_and(|flags| flags.contains('C'))
        })
}
