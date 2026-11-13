//! `sigillum-p11-usb`: the S5 P4 gate (sigillum.md §16.5) on a Linux guest
//! with the token passed through. It stands in for two Atrium parts: the
//! parent that hands a token driver its USB endpoints (here usbfs, relayed
//! one CCID message per SOCK_SEQPACKET packet, as a FreeBSD ugen endpoint
//! gives them), and sigillumd on the driver socket.
//!
//! ```text
//! sigillum-p11-usb HOST MODULE LIBSTDCXX list
//! sigillum-p11-usb HOST MODULE LIBSTDCXX sign KEY_ID_HEX ecdsa|pkcs1|pss
//! ```
//!
//! `list` writes each certificate to `cert-<key id>.der` and prints what
//! sigillumd reads from it. `sign` signs SHA-256("sigillum P4") with that
//! key: the PIN is read from the terminal with echo off, once, and goes
//! only to the host; on a wrong PIN it stops and prints the tries left
//! (the InnaIT token locks after 8). The signature is checked as the
//! broker checks it. It never sends an APDU itself.
//!
//! `SIGILLUM_TOKEN=vvvv:pppp` picks another token than 338c:0032.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use sigillum_proto::driver::{DriverError, FromDriver, HashAlg, Pin, SignScheme, ToDriver, MAX_MESSAGE};
use sigillum_proto::{read_msg, write_msg};
use sigillumd::cert;
use zeroize::Zeroizing;

// libc::Ioctl: c_ulong on glibc, c_int on musl (a static build for any guest).
const USBDEVFS_BULK: libc::Ioctl = 0xC018_5502_u32 as libc::Ioctl;
const USBDEVFS_CLAIMINTERFACE: libc::Ioctl = 0x8004_550F_u32 as libc::Ioctl;
const USBDEVFS_IOCTL: libc::Ioctl = 0xC010_5512_u32 as libc::Ioctl;
const USBDEVFS_DISCONNECT: libc::c_int = 0x5516;

#[repr(C)]
struct Bulk {
    ep: u32,
    len: u32,
    timeout: u32,
    data: *mut libc::c_void,
}

#[repr(C)]
struct UsbIoctl {
    ifno: libc::c_int,
    code: libc::c_int,
    data: *mut libc::c_void,
}

struct Token {
    dev: File,
    ifno: u8,
    ep_in: u8,
    ep_out: u8,
    desc: Vec<u8>,
    name: String,
    vendor: u16,
    product: u16,
}

fn sysfs(dir: &std::path::Path, f: &str) -> Option<String> {
    fs::read_to_string(dir.join(f)).ok().map(|s| s.trim().to_string())
}

/// The reader name pcscd gives a CCID reader its libccid does not list (338c:0032 is not):
/// "manufacturer product [interface] (serial) 00 00", from the USB strings. Vendor modules
/// pick their reader by it: InnaIT's looks for "InnaitKey", which only the manufacturer
/// string holds ("No InnaITKey reader found" otherwise).
fn reader_name(dir: &std::path::Path, ifno: u8, vendor: u16, product: u16) -> String {
    let mut n = [sysfs(dir, "manufacturer"), sysfs(dir, "product")].into_iter().flatten().collect::<Vec<_>>().join(" ");
    if n.is_empty() {
        n = format!("{vendor:04x}:{product:04x}");
    }
    let base = dir.file_name().and_then(|f| f.to_str()).unwrap_or("");
    if let Some(i) = sysfs(&dir.join(format!("{base}:1.{ifno}")), "interface") {
        n = format!("{n} [{i}]");
    }
    if let Some(s) = sysfs(dir, "serial") {
        n = format!("{n} ({s})");
    }
    format!("{n} 00 00")
}

/// The token's usbfs node, CCID interface and bulk endpoints.
fn find(vendor: u16, product: u16) -> io::Result<Token> {
    for e in fs::read_dir("/sys/bus/usb/devices")? {
        let dir = e?.path();
        let (Some(v), Some(p)) = (sysfs(&dir, "idVendor"), sysfs(&dir, "idProduct")) else { continue };
        if u16::from_str_radix(&v, 16).ok() != Some(vendor) || u16::from_str_radix(&p, 16).ok() != Some(product) {
            continue;
        }
        let bus: u32 = sysfs(&dir, "busnum").and_then(|s| s.parse().ok()).ok_or_else(|| io::Error::other("busnum"))?;
        let devnum: u32 = sysfs(&dir, "devnum").and_then(|s| s.parse().ok()).ok_or_else(|| io::Error::other("devnum"))?;
        let mut dev = OpenOptions::new().read(true).write(true).open(format!("/dev/bus/usb/{bus:03}/{devnum:03}"))?;
        // Reading the node gives the device descriptor and every configuration's.
        let mut d = Vec::new();
        dev.read_to_end(&mut d)?;
        let (mut ifno, mut desc, mut ep_in, mut ep_out, mut in_ccid) = (None, None, None, None, false);
        let mut i = 0;
        while i + 2 <= d.len() {
            let (len, ty) = (d[i] as usize, d[i + 1]);
            if len < 2 || i + len > d.len() {
                break;
            }
            let b = &d[i..i + len];
            match ty {
                4 if len >= 9 => {
                    in_ccid = b[5] == 0x0B && ifno.is_none();
                    if in_ccid {
                        ifno = Some(b[2]);
                    }
                }
                0x21 if in_ccid && len == 0x36 => desc = Some(b.to_vec()),
                5 if in_ccid && len >= 7 && b[3] & 3 == 2 => {
                    if b[2] & 0x80 != 0 {
                        ep_in = Some(b[2]);
                    } else {
                        ep_out = Some(b[2]);
                    }
                }
                _ => {}
            }
            i += len;
        }
        let (Some(ifno), Some(desc), Some(ep_in), Some(ep_out)) = (ifno, desc, ep_in, ep_out) else {
            return Err(io::Error::other("no CCID interface with bulk endpoints"));
        };
        let name = reader_name(&dir, ifno, vendor, product);
        return Ok(Token { dev, ifno, ep_in, ep_out, desc, name, vendor, product });
    }
    Err(io::Error::other(format!("no USB device {vendor:04x}:{product:04x} (is it passed through?)")))
}

impl Token {
    fn claim(&self) -> io::Result<()> {
        let fd = self.dev.as_raw_fd();
        // A kernel driver (none is usual for CCID) lets go first; failing that is fine.
        let mut io = UsbIoctl { ifno: self.ifno as i32, code: USBDEVFS_DISCONNECT, data: std::ptr::null_mut() };
        // Safety: usbfs ioctls with their own structures.
        unsafe { libc::ioctl(fd, USBDEVFS_IOCTL, &mut io) };
        let mut n = self.ifno as u32;
        if unsafe { libc::ioctl(fd, USBDEVFS_CLAIMINTERFACE, &mut n) } < 0 {
            return Err(io::Error::other(format!("claim interface {}: {} (is pcscd running?)", self.ifno, io::Error::last_os_error())));
        }
        Ok(())
    }

    fn bulk(&self, ep: u8, buf: &mut [u8], timeout_ms: u32) -> io::Result<usize> {
        let mut b = Bulk { ep: ep as u32, len: buf.len() as u32, timeout: timeout_ms, data: buf.as_mut_ptr().cast() };
        // Safety: buf outlives the call.
        let n = unsafe { libc::ioctl(self.dev.as_raw_fd(), USBDEVFS_BULK, &mut b) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(n as usize)
    }
}

/// One CCID message from the host to the token, then its replies back (a
/// time extension is a reply of its own; the host reads until it has the
/// one it waits for).
fn relay(t: Token, fd: OwnedFd) {
    let mut buf = vec![0u8; 65_546];
    loop {
        // Safety: buf is ours.
        let n = unsafe { libc::recv(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n <= 0 {
            return;
        }
        let mut out = buf[..n as usize].to_vec();
        if t.bulk(t.ep_out, &mut out, 5_000).is_err() {
            return;
        }
        loop {
            let m = match t.bulk(t.ep_in, &mut buf, 60_000) {
                Ok(m) if m >= 10 => m,
                _ => return,
            };
            // Safety: buf is ours.
            if unsafe { libc::send(fd.as_raw_fd(), buf.as_ptr().cast(), m, 0) } != m as isize {
                return;
            }
            // bStatus's command status 2: time extension, another reply follows.
            if buf[7] >> 6 != 2 {
                break;
            }
        }
    }
}

fn socketpair(kind: i32) -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // Safety: two ints.
    if unsafe { libc::socketpair(libc::AF_UNIX, kind | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // Safety: just made.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// The PIN from the terminal, echo off, wiped after use.
fn read_pin() -> io::Result<Pin> {
    let mut tty = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
    let fd = tty.as_raw_fd();
    // Safety: termios is plain data; restored below.
    let mut old: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut old) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut t = old;
    t.c_lflag &= !(libc::ECHO | libc::ECHONL);
    unsafe { libc::tcsetattr(fd, libc::TCSANOW, &t) };
    write!(tty, "Token PIN (not shown): ")?;
    let mut pin = Zeroizing::new(Vec::new());
    let mut c = [0u8; 1];
    while tty.read(&mut c)? == 1 && c[0] != b'\n' && pin.len() < 32 {
        pin.push(c[0]);
    }
    zeroize::Zeroize::zeroize(&mut c);
    unsafe { libc::tcsetattr(fd, libc::TCSANOW, &old) };
    writeln!(tty)?;
    if pin.is_empty() {
        return Err(io::Error::other("no PIN typed"));
    }
    Ok(Pin(pin))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    (s.len() % 2 == 0).then(|| (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect::<Option<Vec<u8>>>()).flatten()
}

/// The host, killed if the bridge gives up first (a host stuck inside the module would
/// otherwise outlive it, holding the token).
struct Host(Option<std::process::Child>);
impl Drop for Host {
    fn drop(&mut self) {
        if let Some(c) = self.0.as_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn ask(s: &mut UnixStream, m: &ToDriver) -> io::Result<FromDriver> {
    write_msg(s, m, MAX_MESSAGE)?;
    read_msg(s, MAX_MESSAGE)?.ok_or_else(|| io::Error::other("the host closed the socket (see its log above)"))
}

fn run(a: &[String]) -> io::Result<bool> {
    let (vendor, product) = match std::env::var("SIGILLUM_TOKEN") {
        Ok(v) => {
            let (a, b) = v.split_once(':').ok_or_else(|| io::Error::other("SIGILLUM_TOKEN=vvvv:pppp"))?;
            (u16::from_str_radix(a, 16).map_err(io::Error::other)?, u16::from_str_radix(b, 16).map_err(io::Error::other)?)
        }
        Err(_) => (0x338c, 0x0032),
    };
    let t = find(vendor, product)?;
    t.claim()?;
    println!("token: {} ({:04x}:{:04x}), interface {}, bulk in {:#04x} out {:#04x}", t.name, t.vendor, t.product, t.ifno, t.ep_in, t.ep_out);

    let module = File::open(&a[2])?;
    let libstdcxx = File::open(&a[3])?;
    let (sock, sock_c) = socketpair(libc::SOCK_STREAM)?;
    let (ccid, ccid_c) = socketpair(libc::SOCK_SEQPACKET)?;
    let fds = [sock_c.as_raw_fd(), module.as_raw_fd(), libstdcxx.as_raw_fd(), ccid_c.as_raw_fd()];
    let n = |i: usize| fds[i].to_string();
    let mut cmd = Command::new(&a[1]);
    cmd.args(["--sock", &n(0), "--module", &n(1), "--libstdcxx", &n(2), "--ccid-in", &n(3), "--ccid-out", &n(3)]);
    cmd.args(["--desc", &hex(&t.desc), "--reader", &t.name, "--vendor", &t.vendor.to_string(), "--product", &t.product.to_string()]);
    cmd.stdin(Stdio::null());
    // Run under sudo for usbfs: the host itself runs as the user who asked.
    if let (Some(u), Some(g)) = (std::env::var("SUDO_UID").ok().and_then(|v| v.parse().ok()), std::env::var("SUDO_GID").ok().and_then(|v| v.parse().ok())) {
        cmd.uid(u).gid(g);
    }
    // Safety: only fcntl between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            for fd in fds {
                let f = libc::fcntl(fd, libc::F_GETFD);
                if f < 0 || libc::fcntl(fd, libc::F_SETFD, f & !libc::FD_CLOEXEC) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let mut child = Host(Some(cmd.spawn()?));
    drop((sock_c, ccid_c, module, libstdcxx));
    std::thread::spawn(move || relay(t, ccid));

    let mut s = UnixStream::from(sock);
    s.set_read_timeout(Some(Duration::from_secs(900)))?;
    let hello: Option<FromDriver> = read_msg(&mut s, MAX_MESSAGE)?;
    println!("hello: {hello:?}");
    let FromDriver::Certificates { token_label, certs, .. } = ask(&mut s, &ToDriver::ListCertificates { ceremony: 1 })? else {
        return Err(io::Error::other("no certificate list"));
    };
    println!("token label: {token_label:?}, {} certificate(s)", certs.len());
    for c in &certs {
        let f = format!("cert-{}.der", hex(&c.key_id));
        fs::write(&f, &c.cert)?;
        match cert::parse(&c.cert) {
            Ok(i) => println!(
                "  key id {}  {:?}  subject {:?}  issuer {:?}  until {}  sha256 {}  -> {f}",
                hex(&c.key_id),
                c.key,
                i.subject,
                i.issuer,
                i.not_after,
                hex(&i.sha256)
            ),
            Err(e) => println!("  key id {}  {:?}  not readable by sigillumd ({e}) -> {f}", hex(&c.key_id), c.key),
        }
    }
    let mut ok = true;
    if a[4] == "sign" {
        let id = unhex(&a[5]).ok_or_else(|| io::Error::other("key id: hex"))?;
        let c = certs.iter().find(|c| c.key_id == id).ok_or_else(|| io::Error::other("no certificate with that key id"))?;
        let info = cert::parse(&c.cert).map_err(io::Error::other)?;
        let scheme = match a[6].as_str() {
            "ecdsa" => SignScheme::Ecdsa(HashAlg::Sha256),
            "pkcs1" => SignScheme::RsaPkcs1(HashAlg::Sha256),
            "pss" => SignScheme::RsaPss(HashAlg::Sha256),
            _ => return Err(io::Error::other("scheme: ecdsa, pkcs1 or pss")),
        };
        let digest: [u8; 32] = <sigillumd::cert::Sha256 as sigillumd::cert::Digest>::digest(b"sigillum P4").into();
        let req = |pin| ToDriver::SignDigest { ceremony: 2, key_id: id.clone(), scheme, digest: digest.to_vec(), pin };
        let r = match ask(&mut s, &req(None))? {
            FromDriver::NeedPin { retries, .. } => {
                if retries != 255 {
                    println!("tries left: {retries}");
                }
                ask(&mut s, &req(Some(read_pin()?)))?
            }
            r => r,
        };
        match r {
            FromDriver::Signed { signature, .. } => {
                let good = cert::verify(&info.key, scheme, &digest, &signature);
                println!("signature ({} bytes) {}: {}", signature.len(), if good { "verifies against the certificate" } else { "DOES NOT VERIFY" }, hex(&signature));
                ok = good;
            }
            FromDriver::Error { error: DriverError::PinInvalid { retries }, .. } => {
                println!("wrong PIN; tries left: {}. Stopping (no second try in one run).", if retries == 255 { "not reported".into() } else { retries.to_string() });
                ok = false;
            }
            other => {
                println!("sign: {other:?}");
                ok = false;
            }
        }
    }
    drop(s);
    let st = child.0.take().expect("host").wait()?;
    println!("host exited: {st}");
    Ok(ok && st.success())
}

fn main() -> ExitCode {
    let a: Vec<String> = std::env::args().collect();
    let shape = match a.get(4).map(String::as_str) {
        Some("list") => a.len() == 5,
        Some("sign") => a.len() == 7,
        _ => false,
    };
    if !shape {
        eprintln!("usage: sigillum-p11-usb HOST MODULE LIBSTDCXX list\n       sigillum-p11-usb HOST MODULE LIBSTDCXX sign KEY_ID_HEX ecdsa|pkcs1|pss");
        return ExitCode::from(2);
    }
    match run(&a) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("sigillum-p11-usb: {e}");
            ExitCode::FAILURE
        }
    }
}
