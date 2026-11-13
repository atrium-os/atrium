//! `sigillum-p11-check`: runs the sigillum-p11 host over the p11test fixture module
//! (hospes/fixtures/src/p11test.c), the way sigillumd and a token's driver jail would,
//! with a fake CCID token answering on a SOCK_SEQPACKET pair (sigillum.md §16.5, P3).
//!
//! ```text
//! sigillum-p11-check HOST P11TEST.so LIBSTDCXX.so
//! ```
//!
//! It checks certificates are listed, every PIN outcome (needed, wrong, final try,
//! blocked), an ECDSA P-256 signature that verifies, the RSA PKCS#1 `DigestInfo` and PSS
//! parameters the module receives, an unknown key, a digest of the wrong length, and
//! that the host exits when the socket closes. It fails if the host ever calls a
//! function-list entry outside the allow-list or the module's vendor export, or if a
//! PIN shows up in the host's log.
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use p256::ecdsa::signature::hazmat::{PrehashSigner, PrehashVerifier};
use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
use sigillum_ccid::fake::Fake;
use sigillum_ccid::reader::Bulk;
use sigillum_proto::driver::{DriverError, FromDriver, HashAlg, KeyKind, Pin, SignScheme, ToDriver, MAX_MESSAGE};
use sigillum_proto::{read_msg, write_msg};

const PIN: &[u8] = b"123456";
const WRONG: &[u8] = b"654321";
const TRIES: u8 = 3;
const KEY_ID: [u8; 2] = [1, 2];

/// The INNAIT token's class descriptor (sigillum-ccid desc.rs tests).
const DESC: &str = "36211001000703000000650e0000f037000000b526000080f00c0000fe0000000000000000000000be04040000100000000000000001";

#[derive(Default)]
struct Card {
    tries: u8,
    verified: bool,
    pss: Vec<u8>,
    forbidden: Vec<u8>,
    unknown: Vec<[u8; 2]>,
}

fn cert() -> Vec<u8> {
    // Any DER-shaped bytes: the host passes them through; sigillumd parses certificates.
    let mut c = vec![0x30, 0x81, 0xC4];
    c.extend((0..0xC4u32).map(|i| (i * 37 + 11) as u8));
    c
}

fn key() -> SigningKey {
    SigningKey::from_bytes(&[0x42u8; 32].into()).unwrap()
}

fn answer(card: &mut Card, a: &[u8]) -> Vec<u8> {
    let ok = |mut d: Vec<u8>| {
        d.extend_from_slice(&[0x90, 0]);
        d
    };
    let data = |a: &[u8]| -> Vec<u8> { if a.len() > 5 { a[5..5 + a[4] as usize].to_vec() } else { vec![] } };
    match (a[0], a[1]) {
        (0x00, 0x20) => {
            if card.tries == 0 {
                return vec![0x69, 0x83];
            }
            if a.len() == 4 {
                return vec![0x63, 0xC0 | card.tries];
            }
            if data(a) == PIN {
                card.tries = TRIES;
                card.verified = true;
                return ok(vec![]);
            }
            card.verified = false;
            card.tries -= 1;
            if card.tries == 0 {
                vec![0x69, 0x83]
            } else {
                vec![0x63, 0xC0 | card.tries]
            }
        }
        (0x00, 0xB0) => ok(cert()),
        (0x80, 0x2A) => {
            if !card.verified {
                return vec![0x69, 0x82];
            }
            let d = data(a);
            match a[2] {
                1 => {
                    let s: Signature = key().sign_prehash(&d).unwrap();
                    ok(s.to_bytes().to_vec())
                }
                2 => ok(d),
                3 => {
                    card.pss.push(a[3]);
                    ok(d)
                }
                _ => vec![0x6A, 0x86],
            }
        }
        (0x80, 0xFF) => {
            card.forbidden.push(a[2]);
            ok(vec![])
        }
        _ => {
            card.unknown.push([a[0], a[1]]);
            vec![0x6D, 0x00]
        }
    }
}

fn socketpair(kind: i32) -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // Safety: fds is two ints.
    if unsafe { libc::socketpair(libc::AF_UNIX, kind | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // Safety: both just created and ours.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// The token: one CCID message per packet each way.
fn device(fd: OwnedFd, card: Arc<Mutex<Card>>) {
    let mut fake = Fake::new(move |a: &[u8]| answer(&mut card.lock().unwrap(), a));
    let mut buf = vec![0u8; 70_000];
    loop {
        // Safety: buf is ours.
        let n = unsafe { libc::recv(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n <= 0 {
            return;
        }
        fake.send(&buf[..n as usize]).unwrap();
        while let Ok(r) = fake.recv(0) {
            // Safety: r is ours.
            if unsafe { libc::send(fd.as_raw_fd(), r.as_ptr().cast(), r.len(), 0) } != r.len() as isize {
                return;
            }
        }
    }
}

struct Check {
    sock: UnixStream,
    ceremony: u64,
    failed: u32,
}

impl Check {
    fn ask(&mut self, m: ToDriver) -> io::Result<FromDriver> {
        write_msg(&mut self.sock, &m, MAX_MESSAGE)?;
        read_msg(&mut self.sock, MAX_MESSAGE)?.ok_or_else(|| io::Error::other("host closed the socket"))
    }

    fn sign(&mut self, scheme: SignScheme, key_id: &[u8], digest: &[u8], pin: Option<&[u8]>) -> io::Result<FromDriver> {
        self.ceremony += 1;
        let pin = pin.map(|p| Pin(p.to_vec().into()));
        self.ask(ToDriver::SignDigest { ceremony: self.ceremony, key_id: key_id.to_vec(), scheme, digest: digest.to_vec(), pin })
    }

    fn expect(&mut self, what: &str, good: bool, got: &dyn std::fmt::Debug) {
        if good {
            println!("ok   {what}");
        } else {
            println!("FAIL {what}: {got:?}");
            self.failed += 1;
        }
    }
}

fn err(r: &FromDriver) -> Option<DriverError> {
    match r {
        FromDriver::Error { error, .. } => Some(*error),
        _ => None,
    }
}

fn run(host: &str, module: &str, libstdcxx: &str) -> io::Result<u32> {
    let module = std::fs::File::open(module)?;
    let libstdcxx = std::fs::File::open(libstdcxx)?;
    let (sock, sock_c) = socketpair(libc::SOCK_STREAM)?;
    let (ccid, ccid_c) = socketpair(libc::SOCK_SEQPACKET)?;
    let card = Arc::new(Mutex::new(Card { tries: TRIES, ..Card::default() }));

    let fds = [sock_c.as_raw_fd(), module.as_raw_fd(), libstdcxx.as_raw_fd(), ccid_c.as_raw_fd()];
    let n = |i: usize| fds[i].to_string();
    let mut cmd = Command::new(host);
    cmd.args(["--sock", &n(0), "--module", &n(1), "--libstdcxx", &n(2), "--ccid-in", &n(3), "--ccid-out", &n(3)]);
    cmd.args(["--desc", DESC, "--reader", "Sigillum Fake Token 00 00", "--vendor", "0x338c", "--product", "0x0032"]);
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
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
    let mut child = cmd.spawn()?;
    drop((sock_c, ccid_c, module, libstdcxx));
    let mut stderr = child.stderr.take().unwrap();
    let log = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = stderr.read_to_end(&mut s);
        s
    });
    let dev = {
        let card = card.clone();
        std::thread::spawn(move || device(ccid, card))
    };

    let sock = UnixStream::from(sock);
    sock.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut c = Check { sock, ceremony: 0, failed: 0 };

    let hello: Option<FromDriver> = read_msg(&mut c.sock, MAX_MESSAGE)?;
    let good = matches!(&hello, Some(FromDriver::Hello { vendor: 0x338c, product: 0x0032, name, .. }) if name == "Sigillum Fake Token 00 00");
    c.expect("hello", good, &hello);
    if hello.is_none() {
        let _ = child.wait();
        eprint!("{}", String::from_utf8_lossy(&log.join().unwrap()));
        return Ok(c.failed + 1);
    }

    c.ceremony += 1;
    let r = c.ask(ToDriver::ListCertificates { ceremony: c.ceremony })?;
    let good = match &r {
        FromDriver::Certificates { ceremony, token_label, certs } => {
            *ceremony == c.ceremony
                && token_label == "Sigillum test token"
                && certs.len() == 1
                && certs[0].key_id == KEY_ID
                && certs[0].label == "Signing certificate"
                && certs[0].cert == cert()
                && certs[0].key == KeyKind::Ec { bits: 256 }
        }
        _ => false,
    };
    c.expect("list certificates", good, &r);

    let digest: Vec<u8> = (0..32u8).map(|i| i.wrapping_mul(7) ^ 0x5A).collect();
    let ecdsa = SignScheme::Ecdsa(HashAlg::Sha256);

    let r = c.sign(ecdsa, &KEY_ID, &digest, None)?;
    c.expect("no PIN: NeedPin", matches!(r, FromDriver::NeedPin { retries: 255, .. }), &r);
    let r = c.sign(ecdsa, &KEY_ID, &digest, Some(WRONG))?;
    c.expect("wrong PIN: PinInvalid", err(&r) == Some(DriverError::PinInvalid { retries: 255 }), &r);
    let r = c.sign(ecdsa, &KEY_ID, &digest, Some(WRONG))?;
    c.expect("wrong PIN again: final try", err(&r) == Some(DriverError::PinInvalid { retries: 1 }), &r);
    let r = c.sign(ecdsa, &KEY_ID, &digest, None)?;
    c.expect("no PIN on the final try: NeedPin(1)", matches!(r, FromDriver::NeedPin { retries: 1, .. }), &r);

    let r = c.sign(ecdsa, &KEY_ID, &digest, Some(PIN))?;
    let good = match &r {
        FromDriver::Signed { signature, .. } => Signature::from_slice(signature)
            .ok()
            .is_some_and(|s| VerifyingKey::from(&key()).verify_prehash(&digest, &s).is_ok()),
        _ => false,
    };
    c.expect("ECDSA P-256 signature verifies", good, &r);

    let r = c.sign(SignScheme::RsaPkcs1(HashAlg::Sha256), &KEY_ID, &digest, Some(PIN))?;
    let mut info = vec![0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05, 0x00, 0x04, 0x20];
    info.extend_from_slice(&digest);
    c.expect("RSA PKCS#1 signs the DigestInfo", matches!(&r, FromDriver::Signed { signature, .. } if *signature == info), &r);

    let r = c.sign(SignScheme::RsaPss(HashAlg::Sha256), &KEY_ID, &digest, Some(PIN))?;
    let pss = card.lock().unwrap().pss.clone();
    c.expect("RSA PSS: SHA-256, MGF1-SHA-256", matches!(&r, FromDriver::Signed { signature, .. } if *signature == digest) && pss == [0x52], &(&r, pss));

    let r = c.sign(ecdsa, &[9], &digest, Some(PIN))?;
    c.expect("unknown key: NoKey", err(&r) == Some(DriverError::NoKey), &r);
    let r = c.sign(ecdsa, &KEY_ID, &digest[..31], Some(PIN))?;
    c.expect("short digest refused", err(&r) == Some(DriverError::Device), &r);

    // Three wrong PINs block it; the right one then can't help.
    let mut last = None;
    for _ in 0..TRIES {
        last = Some(c.sign(ecdsa, &KEY_ID, &digest, Some(WRONG))?);
    }
    let r = last.unwrap();
    c.expect("third wrong PIN: PinBlocked", err(&r) == Some(DriverError::PinBlocked), &r);
    let r = c.sign(ecdsa, &KEY_ID, &digest, Some(PIN))?;
    c.expect("blocked stays blocked", err(&r) == Some(DriverError::PinBlocked), &r);

    // sigillumd goes away: the host finalizes and exits 0.
    drop(c.sock);
    let t = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break Some(s);
        }
        if t.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut failed = c.failed;
    let mut expect = |what: &str, good: bool, got: &dyn std::fmt::Debug| {
        if good {
            println!("ok   {what}");
        } else {
            println!("FAIL {what}: {got:?}");
            failed += 1;
        }
    };
    expect("exits 0 when the socket closes", status.is_some_and(|s| s.success()), &status);
    let log = log.join().unwrap();
    let _ = dev.join();
    let card = card.lock().unwrap();
    expect("no call outside the allow-list", card.forbidden.is_empty(), &card.forbidden);
    expect("no unexpected APDU", card.unknown.is_empty(), &card.unknown);
    let leaks = |p: &[u8]| log.windows(p.len()).any(|w| w == p);
    expect("no PIN in the host's log", !leaks(PIN) && !leaks(WRONG), &String::from_utf8_lossy(&log));
    if failed > 0 || std::env::var_os("SIGILLUM_P11_LOG").is_some() {
        eprint!("{}", String::from_utf8_lossy(&log));
    }
    Ok(failed)
}

fn main() -> ExitCode {
    let a: Vec<String> = std::env::args().collect();
    if a.len() != 4 {
        eprintln!("usage: sigillum-p11-check HOST P11TEST.so LIBSTDCXX.so");
        return ExitCode::from(2);
    }
    match run(&a[1], &a[2], &a[3]) {
        Ok(0) => {
            println!("sigillum-p11: all checks passed");
            ExitCode::SUCCESS
        }
        Ok(n) => {
            println!("sigillum-p11: {n} checks failed");
            ExitCode::FAILURE
        }
        Err(e) => {
            println!("sigillum-p11-check: {e}");
            ExitCode::FAILURE
        }
    }
}
