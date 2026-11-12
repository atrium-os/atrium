//! `hospes-cdm-check`: run the test CDM's scenario through `hospes-cdm` (docs/spec/hospes.md
//! §11.3) and print the same transcript as the Linux reference host (`test-cdm/refhost.cc`).
//!
//! ```text
//! hospes-cdm-check HOSPES_CDM LIB VECTORS_DIR [STORE_DIR]
//! ```
//!
//! How the host is started:
//! - by default, with `--ctrl/--data/--lib/--store` descriptors;
//! - `HOSPES_CDM_CHECK_FDS_FROM=1`: with no arguments, the four descriptors and the key
//!   system on its stdin, as portcullisd's `OpenCdm` hands them over;
//! - `HOSPES_CDM_CHECK_BROKER=1`: by portcullisd itself (`OpenCdm`, from inside an app
//!   jail holding `protected-media`): HOSPES_CDM, LIB and STORE_DIR are then unused (`-`),
//!   since portcullisd picks the host, the library and the store. The origin is
//!   `HOSPES_CDM_CHECK_ORIGIN` (default `https://check.example`).
//!
//! It plays the supervisor (control channel) and the decode worker (data channel), so the
//! comparison covers hospes-proto as well as the host. The store directory defaults to a
//! fresh one under the temp directory.
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use hospes_proto::io::{recv, send};
use hospes_proto::{scheme, status, Msg, Sample};

#[allow(dead_code)]
#[path = "../../hospes-cdm/src/abi.rs"]
mod abi;

const KID1_B64: &str = "ABEiM0RVZneImaq7zN3u_w";
const KID1: [u8; 16] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
const KID2: [u8; 16] = [0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xab, 0xac, 0xad, 0xae, 0xaf];
const CLEARKEY_SYSTEM: [u8; 16] = [0x10, 0x77, 0xef, 0xec, 0xc0, 0xb2, 0x4d, 0x02, 0xac, 0xe3, 0x3c, 0x1e, 0x52, 0xe2, 0xfb, 0x4b];
const TEST_DECODER: &[u8] = b"hospes-test-decoder";

struct Vector {
    op: u8,
    expect: u32,
    sample: Sample,
    clear: Vec<u8>,
}

fn load_vectors(dir: &str) -> io::Result<(Vec<Vector>, Vec<u8>, Vec<u8>)> {
    let v = std::fs::read(format!("{dir}/vectors.bin"))?;
    let lic = std::fs::read(format!("{dir}/licence.json"))?;
    let lic1 = std::fs::read(format!("{dir}/licence-kid1.json"))?;
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "vectors.bin truncated");
    let mut p = 0usize;
    let mut take = |n: usize| -> io::Result<&[u8]> {
        let s = v.get(p..p + n).ok_or_else(bad)?;
        p += n;
        Ok(s)
    };
    let mut out = Vec::new();
    loop {
        let h = match take(24) {
            Ok(h) => h.to_vec(),
            Err(_) => break,
        };
        let (op, sch, ivn, expect) = (h[0], h[1] as u32, h[2] as usize, h[3] as u32);
        let crypt = u32::from_le_bytes(h[4..8].try_into().unwrap());
        let skip = u32::from_le_bytes(h[8..12].try_into().unwrap());
        let ts = i64::from_le_bytes(h[12..20].try_into().unwrap());
        let mut kid = h[20..24].to_vec();
        kid.extend_from_slice(take(12)?);
        let iv = take(ivn)?.to_vec();
        let ns = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
        let mut subs = Vec::new();
        for _ in 0..ns {
            let b = take(8)?;
            subs.push((u32::from_le_bytes(b[..4].try_into().unwrap()), u32::from_le_bytes(b[4..].try_into().unwrap())));
        }
        let n = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
        let data = take(n)?.to_vec();
        let n = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
        let clear = take(n)?.to_vec();
        out.push(Vector {
            op,
            expect,
            sample: Sample { scheme: sch, key_id: kid, iv, subsamples: subs, crypt_byte_block: crypt, skip_byte_block: skip, timestamp: ts, data },
            clear,
        });
    }
    Ok((out, lic, lic1))
}

fn clearkey_pssh() -> Vec<u8> {
    let mut b = vec![0, 0, 0, 0, b'p', b's', b's', b'h', 1, 0, 0, 0];
    b.extend_from_slice(&CLEARKEY_SYSTEM);
    b.extend_from_slice(&[0, 0, 0, 2]);
    b.extend_from_slice(&KID1);
    b.extend_from_slice(&KID2);
    b.extend_from_slice(&[0, 0, 0, 0]);
    b[3] = b.len() as u8;
    b
}

fn fnv(d: &[u8]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for &c in d {
        h ^= c as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn hex(d: &[u8]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

fn s(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

struct Check {
    ctrl: UnixStream,
    data: UnixStream,
}

impl Check {
    /// Print control messages until none arrives for a short while (the host has settled).
    fn settle(&mut self) -> io::Result<()> {
        let quiet = Duration::from_millis(if cfg!(target_os = "freebsd") { 400 } else { 150 });
        let end = Instant::now() + Duration::from_secs(10);
        self.ctrl.set_read_timeout(Some(quiet))?;
        while Instant::now() < end {
            match recv(&mut self.ctrl) {
                Ok(Some(m)) => print_ctrl(&m),
                Ok(None) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "hospes-cdm closed the control channel")),
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => return Ok(()),
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn ctl(&mut self, m: Msg) -> io::Result<()> {
        send(&mut self.ctrl, &m)?;
        self.settle()
    }

    fn call(&mut self, m: Msg) -> io::Result<Msg> {
        send(&mut self.data, &m)?;
        recv(&mut self.data)?.ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "data channel closed"))
    }

    fn decrypt(&mut self, i: usize, v: &Vector) -> io::Result<()> {
        match self.call(Msg::Decrypt { sample: v.sample.clone() })? {
            Msg::Decrypted { status: st, timestamp, data } => {
                let ok = st == v.expect && (st != status::SUCCESS || data == v.clear);
                println!("decrypt i={i} status={st} ts={} len={} ok={}", if st == 0 { timestamp } else { 0 }, data.len(), ok as u8);
            }
            m => println!("decrypt i={i} unexpected {m:?}"),
        }
        Ok(())
    }

    fn decode(&mut self, i: usize, v: Option<&Vector>) -> io::Result<()> {
        let sample = v.map(|v| v.sample.clone()).unwrap_or_default();
        match self.call(Msg::DecryptAndDecode { sample })? {
            Msg::Decoded { frame: f } if f.status == 0 => {
                let o = f.offsets;
                let st = f.strides;
                let valid = f.planes(4096, 4096).is_some();
                println!(
                    "frame i={i} status=0 fmt={} size={}x{} off={},{},{} stride={},{},{} ts={} fnv={:016x}{}",
                    f.format,
                    f.width,
                    f.height,
                    o[0],
                    o[1],
                    o[2],
                    st[0],
                    st[1],
                    st[2],
                    f.timestamp,
                    fnv(&f.data),
                    if valid { "" } else { " INVALID-GEOMETRY" }
                );
            }
            Msg::Decoded { frame: f } => println!("frame i={i} status={}", f.status),
            m => println!("frame i={i} unexpected {m:?}"),
        }
        Ok(())
    }
}

fn print_ctrl(m: &Msg) {
    match m {
        Msg::Initialized { ok } => println!("initialized {}", *ok as u8),
        Msg::KeyStatus { promise, status } => println!("key-status p={promise} s={status}"),
        Msg::NewSession { promise, id } => println!("new-session p={promise} id={}", s(id)),
        Msg::Resolved { promise } => println!("resolve p={promise}"),
        Msg::Rejected { promise, exception, system_code, message } => {
            println!("reject p={promise} e={exception} code={system_code} msg={}", s(message))
        }
        Msg::SessionMessage { id, message_type, message } => println!("message id={} type={message_type} {}", s(id), s(message)),
        Msg::KeysChange { id, has_new_usable, keys } => {
            let mut l = format!("keys id={} new={}", s(id), *has_new_usable as u8);
            for k in keys {
                l += &format!(" {}={}/{}", hex(&k.key_id), k.status, k.system_code);
            }
            println!("{l}");
        }
        Msg::Expiration { id, .. } => println!("expiration id={}", s(id)),
        Msg::Closed { id } => println!("closed id={}", s(id)),
        Msg::Fail { code, what } => println!("FAIL {code} {}", s(what)),
        m => println!("unexpected {m:?}"),
    }
}

fn pass_fd(cmd: &mut Command, fd: i32) {
    // Safety: only fcntl between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            let f = libc::fcntl(fd, libc::F_GETFD);
            if f < 0 || libc::fcntl(fd, libc::F_SETFD, f & !libc::FD_CLOEXEC) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// The host: our child, or portcullisd's (the connection that keeps it alive).
enum Host {
    Child(std::process::Child),
    Broker(UnixStream),
}

impl Host {
    fn kill(&mut self) {
        match self {
            Host::Child(c) => {
                let _ = c.kill();
            }
            Host::Broker(s) => {
                let _ = s.shutdown(std::net::Shutdown::Both);
            }
        }
    }
    /// `None` if it exited 0, otherwise how it ended.
    fn wait(&mut self) -> io::Result<Option<String>> {
        match self {
            Host::Child(c) => {
                let st = c.wait()?;
                Ok((!st.success()).then(|| st.to_string()))
            }
            Host::Broker(s) => match portcullis_ipc::read_response(s)? {
                portcullis_ipc::Response::LaunchExit { code: Some(0) } => Ok(None),
                r => Ok(Some(format!("{r:?}"))),
            },
        }
    }
}

/// Ask portcullisd for a CDM of our own (`OpenCdm`), on these two channels.
fn open_cdm(ctrl: &UnixStream, data: &UnixStream) -> io::Result<UnixStream> {
    use portcullis_ipc::{read_response, round_trip, send_fds, write_request, Request, Response};
    let path = portcullis_ipc::resolve_socket_path().ok_or_else(|| io::Error::other("portcullisd's socket is not reachable (protected-media?)"))?;
    let mut s = UnixStream::connect(path)?;
    match round_trip(&mut s, &Request::Hello { version: portcullis_ipc::PROTO_VERSION })? {
        Response::Hello { .. } => {}
        r => return Err(io::Error::other(format!("portcullisd hello: {r:?}"))),
    }
    let origin = std::env::var("HOSPES_CDM_CHECK_ORIGIN").unwrap_or_else(|_| "https://check.example".into());
    write_request(&mut s, &Request::OpenCdm { key_system: "org.w3.clearkey".into(), origin })?;
    match read_response(&mut s)? {
        Response::ReadyForFds => {}
        r => return Err(io::Error::other(format!("refused: {r:?}"))),
    }
    send_fds(&s, &[ctrl.as_raw_fd(), data.as_raw_fd(), 2])?;
    Ok(s)
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 4 {
        eprintln!("usage: hospes-cdm-check HOSPES_CDM LIB VECTORS_DIR [STORE_DIR]");
        std::process::exit(2);
    }
    std::process::exit(match run(&a[1], &a[2], &a[3], a.get(4).map(String::as_str)) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("hospes-cdm-check: {e}");
            1
        }
    });
}

fn run(host: &str, lib: &str, vecdir: &str, store: Option<&str>) -> io::Result<()> {
    let (vecs, licence, licence1) = load_vectors(vecdir)?;
    let store_dir = match store {
        Some(d) => d.to_string(),
        None => {
            let d = std::env::temp_dir().join(format!("hospes-cdm-check.{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d)?;
            d.to_string_lossy().into_owned()
        }
    };
    let (ctrl, ctrl_c) = UnixStream::pair()?;
    let (data, data_c) = UnixStream::pair()?;
    let mut host = if std::env::var_os("HOSPES_CDM_CHECK_BROKER").is_some() {
        Host::Broker(open_cdm(&ctrl_c, &data_c)?)
    } else {
        let libf = File::open(lib)?;
        let storef = File::open(&store_dir)?;
        let mut cmd = Command::new(host);
        if std::env::var_os("HOSPES_CDM_CHECK_FDS_FROM").is_some() {
            let (ours, theirs) = UnixStream::pair()?;
            portcullis_ipc::send_fds_with(&ours, &[ctrl_c.as_raw_fd(), data_c.as_raw_fd(), libf.as_raw_fd(), storef.as_raw_fd()], b"org.w3.clearkey 512\n")?;
            cmd.stdin(Stdio::from(OwnedFd::from(theirs)));
        } else {
            for (flag, fd) in [("--ctrl", ctrl_c.as_raw_fd()), ("--data", data_c.as_raw_fd()), ("--lib", libf.as_raw_fd()), ("--store", storef.as_raw_fd())] {
                cmd.arg(flag).arg(fd.to_string());
                pass_fd(&mut cmd, fd);
            }
            cmd.stdin(Stdio::null());
        }
        Host::Child(cmd.spawn()?)
    };
    drop((ctrl_c, data_c));
    let mut c = Check { ctrl, data };

    // The reference host prints the version from GetCdmVersion; we get it in Ready.
    c.ctrl.set_read_timeout(Some(Duration::from_secs(30)))?;
    match recv(&mut c.ctrl)? {
        Some(Msg::Ready { cdm_version }) => println!("version {}", s(&cdm_version)),
        other => {
            println!("no Ready: {other:?}");
            host.kill();
            return Err(io::Error::other("hospes-cdm did not start"));
        }
    }

    // The scenario: the same steps, in the same order, as refhost.cc.
    c.ctl(Msg::Init { allow_persistent: true, link_mask: 0 })?;
    c.ctl(Msg::GetStatusForPolicy { promise: 1, min_hdcp: 0 })?;
    c.ctl(Msg::GetStatusForPolicy { promise: 2, min_hdcp: 8 })?; // kHdcpVersion2_2
    c.ctl(Msg::SetServerCertificate { promise: 3, cert: b"x".to_vec() })?;
    c.ctl(Msg::CreateSession { promise: 4, session_type: 0, init_type: 0, init: clearkey_pssh() })?;
    c.ctl(Msg::UpdateSession { promise: 5, id: b"1".to_vec(), response: licence })?;
    for (i, v) in vecs.iter().enumerate().filter(|(_, v)| v.op == 1) {
        c.decrypt(i, v)?;
    }
    match c.call(Msg::InitAudio { codec: 1, channels: 2, bits: 16, rate: 48000, extra: vec![], scheme: scheme::CENC })? {
        Msg::InitResult { status } => println!("audio-init status={status}"),
        m => println!("audio-init unexpected {m:?}"),
    }
    for extra in [Vec::new(), TEST_DECODER.to_vec()] {
        match c.call(Msg::InitVideo { codec: 3, profile: 9, format: 2, width: 64, height: 48, extra, scheme: scheme::CENC })? {
            Msg::InitResult { status } => println!("video-init status={status}"),
            m => println!("video-init unexpected {m:?}"),
        }
    }
    for (i, v) in vecs.iter().enumerate().filter(|(_, v)| v.op == 2) {
        c.decode(i, Some(v))?;
    }
    c.decode(vecs.len(), None)?;
    c.call(Msg::ResetDecoder { stream: 1 })?;
    c.call(Msg::DeinitDecoder { stream: 1 })?;
    let kids = format!("{{\"kids\":[\"{KID1_B64}\"]}}").into_bytes();
    c.ctl(Msg::CreateSession { promise: 6, session_type: 1, init_type: 1, init: kids })?;
    c.ctl(Msg::UpdateSession { promise: 7, id: b"2".to_vec(), response: licence1 })?;
    c.ctl(Msg::CloseSession { promise: 8, id: b"2".to_vec() })?;
    c.ctl(Msg::CloseSession { promise: 9, id: b"1".to_vec() })?;
    c.decrypt(1, &vecs[1])?;
    c.ctl(Msg::LoadSession { promise: 10, session_type: 1, id: b"2".to_vec() })?;
    c.decrypt(1, &vecs[1])?;
    c.ctl(Msg::LoadSession { promise: 11, session_type: 1, id: b"99".to_vec() })?;
    c.ctl(Msg::RemoveSession { promise: 12, id: b"2".to_vec() })?;
    c.ctl(Msg::CloseSession { promise: 13, id: b"2".to_vec() })?;
    c.ctl(Msg::LoadSession { promise: 14, session_type: 1, id: b"2".to_vec() })?;
    c.ctl(Msg::UpdateSession { promise: 15, id: b"7".to_vec(), response: b"{}".to_vec() })?;

    // Close the control channel: the host destroys the CDM and exits 0.
    drop(c);
    let st = host.wait()?;
    println!("end");
    if let Some(st) = st {
        return Err(io::Error::other(format!("hospes-cdm exited with {st}")));
    }
    if store.is_none() {
        let _ = std::fs::remove_dir_all(&store_dir);
    }
    Ok(())
}
