//! `sigillum-p11`: one token's vendor PKCS#11 module under Hospes, serving
//! sigillumd as a token driver (sigillum.md §16.5).
//!
//! ```text
//! sigillum-p11 --sock FD --module FD --libstdcxx FD --ccid-in FD --ccid-out FD
//!              --desc HEX --reader NAME [--vendor N] [--product N] [--mem MiB]
//! ```
//!
//! The parent (portcullisd starting the token's driver jail) passes every
//! descriptor: sigillumd's driver socket, the module and the libstdc++ it was
//! built against (read-only), and the token's CCID bulk endpoints (the same
//! descriptor twice for a socket). `--desc` is the CCID class descriptor
//! (hex), `--reader` the reader name pcscd would show. We enter capability
//! mode before parsing either library, load both as one set over the glibc
//! shim with our own `SCard*` (sigillum-pcsc) and libudev/libcurl stand-ins,
//! and then answer `ListCertificates` and `SignDigest` through the
//! allow-listed calls in [`p11`]. The module never sees a path, the network,
//! or anything of the machine but its token.
#![no_std]
#![no_main]

extern crate alloc;

mod p11;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use hospes_rt::load::{self, Resolver};
use hospes_rt::start::StartInfo;
use hospes_rt::{heap, hlog, sys, trap};
use sigillum_ccid::desc::Ccid;
use sigillum_ccid::reader::{Bulk, Reader};
use sigillum_ccid::Error;
use sigillum_proto::driver::{DeviceClass, DriverError, FromDriver, ToDriver, MAX_MESSAGE};

fn num(s: &[u8]) -> Option<usize> {
    let s = core::str::from_utf8(s).ok()?;
    match s.strip_prefix("0x") {
        Some(h) => usize::from_str_radix(h, 16).ok(),
        None => s.parse().ok(),
    }
}

fn hex(s: &[u8]) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    s.chunks(2).map(|p| u8::from_str_radix(core::str::from_utf8(p).ok()?, 16).ok()).collect()
}

struct Args {
    sock: i32,
    module: i32,
    libstdcxx: i32,
    ccid_in: i32,
    ccid_out: i32,
    desc: Vec<u8>,
    reader: String,
    vendor: u16,
    product: u16,
    mem_mib: usize,
}

fn parse_args(info: &'static StartInfo) -> Option<Args> {
    let mut a = Args {
        sock: -1,
        module: -1,
        libstdcxx: -1,
        ccid_in: -1,
        ccid_out: -1,
        desc: Vec::new(),
        reader: String::new(),
        vendor: 0,
        product: 0,
        mem_mib: 256,
    };
    let mut i = 1;
    while let Some(x) = info.arg(i) {
        let v = info.arg(i + 1)?;
        match x {
            b"--sock" => a.sock = num(v)? as i32,
            b"--module" => a.module = num(v)? as i32,
            b"--libstdcxx" => a.libstdcxx = num(v)? as i32,
            b"--ccid-in" => a.ccid_in = num(v)? as i32,
            b"--ccid-out" => a.ccid_out = num(v)? as i32,
            b"--desc" => a.desc = hex(v)?,
            b"--reader" => a.reader = String::from(core::str::from_utf8(v).ok().filter(|r| !r.is_empty() && r.len() <= 64)?),
            b"--vendor" => a.vendor = num(v)? as u16,
            b"--product" => a.product = num(v)? as u16,
            b"--mem" => a.mem_mib = num(v)?,
            _ => return None,
        }
        i += 2;
    }
    if [a.sock, a.module, a.libstdcxx, a.ccid_in, a.ccid_out].iter().any(|&f| f < 0) || a.reader.is_empty() {
        return None;
    }
    Some(a)
}

/// The token's bulk endpoints as descriptors: one CCID message per write, one per read
/// (a ugen bulk-IN read ends at the short packet; a test socket is SOCK_SEQPACKET).
struct FdBulk {
    r: i32,
    w: i32,
}

impl Bulk for FdBulk {
    fn send(&mut self, m: &[u8]) -> Result<(), Error> {
        sys::write_all(self.w, m).map_err(|_| Error::Io)
    }
    fn recv(&mut self, cap: usize) -> Result<Vec<u8>, Error> {
        let mut b = vec![0u8; cap];
        match sys::read(self.r, &mut b) {
            Ok(0) | Err(_) => Err(Error::Io),
            Ok(n) => {
                b.truncate(n);
                Ok(b)
            }
        }
    }
}

/// PC/SC, udev and curl from us; everything else of libc from the shim.
struct Res;
impl Resolver for Res {
    fn resolve(&self, name: &str, version: Option<&str>) -> Option<usize> {
        if version.is_none() {
            if let Some(a) = sigillum_pcsc::resolve(name).or_else(|| sigillum_pcsc::platform::resolve(name)) {
                return Some(a);
            }
        }
        hospes_glibc::Glibc.resolve(name, version)
    }
}

fn send(sock: i32, rec: &FromDriver) -> bool {
    match sigillum_proto::encode(rec, MAX_MESSAGE) {
        Some(b) => sys::write_all(sock, &b).is_ok(),
        None => false,
    }
}

/// One `ToDriver`, or `None` when sigillumd closed the socket or sent something malformed.
fn recv(sock: i32) -> Option<ToDriver> {
    let mut len = [0u8; 4];
    if !sys::read_exact(sock, &mut len).ok()? {
        return None;
    }
    let n = u32::from_le_bytes(len) as usize;
    if n > MAX_MESSAGE {
        return None;
    }
    let mut body = vec![0u8; n];
    if !sys::read_exact(sock, &mut body).ok()? {
        return None;
    }
    let m = sigillum_proto::decode(&body);
    // The body may hold a PIN.
    body.iter_mut().for_each(|b| *b = 0);
    m
}

fn log_line(a: core::fmt::Arguments) {
    hlog!("sigillum-p11: {}", a);
}

#[no_mangle]
pub fn hospes_main(info: &'static StartInfo) -> i32 {
    let Some(args) = parse_args(info) else {
        hlog!("usage: sigillum-p11 --sock FD --module FD --libstdcxx FD --ccid-in FD --ccid-out FD --desc HEX --reader NAME [--vendor N] [--product N] [--mem MiB]");
        return 2;
    };
    {
        use sys::rights::*;
        for fd in [args.module, args.libstdcxx] {
            if sys::cap_rights_limit(fd, READ | SEEK | FSTAT | MMAP_RX).is_err() {
                hlog!("sigillum-p11: cap_rights_limit on a library failed");
                return 3;
            }
        }
    }
    let trap = match trap::init(hospes_glibc::syscall::handle) {
        Ok(()) => true,
        Err(_) if cfg!(target_os = "freebsd") => {
            hlog!("sigillum-p11: no system call trap (is hospes_trap.ko loaded?)");
            return 3;
        }
        Err(e) => {
            hlog!("sigillum-p11: no system call trap ({:?})", e);
            false
        }
    };
    if sys::cap_enter().is_err() {
        hlog!("sigillum-p11: cap_enter failed");
        return 3;
    }
    heap::set_limit(args.mem_mib << 20);

    let desc = match Ccid::parse(&args.desc) {
        Ok(d) => d,
        Err(e) => {
            hlog!("sigillum-p11: CCID descriptor: {:?}", e);
            return 4;
        }
    };
    let reader = match Reader::new(FdBulk { r: args.ccid_in, w: args.ccid_out }, desc) {
        Ok(r) => r,
        Err(e) => {
            hlog!("sigillum-p11: reader: {:?}", e);
            return 4;
        }
    };
    sigillum_pcsc::install(sigillum_pcsc::Setup {
        card: Box::new(reader),
        name: args.reader.clone(),
        sleep_ms: |ms| sys::nanosleep(ms as i64 / 1000, (ms as i64 % 1000) * 1_000_000),
    });
    sigillum_pcsc::platform::set_log(log_line);

    hospes_glibc::init_from(info);
    let opts = load::Options { hwcap: sys::linux_hwcap(info.auxv), allow_unknown: false, trap };
    let libs = match load::load_set(&[args.libstdcxx, args.module], &opts, &Res) {
        Ok(l) => l,
        Err(e) => {
            hlog!("sigillum-p11: load: {:?}", e);
            return 5;
        }
    };
    let _ = (sys::close(args.libstdcxx), sys::close(args.module));
    for l in &libs {
        hospes_glibc::library_loaded(l);
    }
    for l in &libs {
        // Safety: each library's initialisers, dependencies first, as ld.so runs them.
        unsafe { l.run_init(info.argc, info.argv, info.envp) };
    }
    let Some(gfl) = libs[1].sym("C_GetFunctionList") else {
        hlog!("sigillum-p11: the module exports no C_GetFunctionList");
        return 6;
    };
    // Safety: the module's own C_GetFunctionList.
    let module = match unsafe { p11::Module::new(gfl) } {
        Ok(m) => m,
        Err(e) => {
            hlog!("sigillum-p11: the module did not start: {:?}", e);
            return 6;
        }
    };

    if !send(args.sock, &FromDriver::Hello { class: DeviceClass::Ccid, vendor: args.vendor, product: args.product, name: args.reader.clone(), fido: None }) {
        return 7;
    }
    while let Some(req) = recv(args.sock) {
        let rec = serve(&module, req);
        let Some(rec) = rec else { continue };
        let gone = matches!(rec, FromDriver::Error { error: DriverError::Gone, .. });
        if !send(args.sock, &rec) || gone {
            break;
        }
    }
    module.finalize();
    0
}

fn serve(m: &p11::Module, req: ToDriver) -> Option<FromDriver> {
    let ceremony = req.ceremony();
    let err = |f: p11::Fail| match f {
        p11::Fail::Driver(e) => FromDriver::Error { ceremony, error: e },
        p11::Fail::NeedPin(retries) => FromDriver::NeedPin { ceremony, retries },
        p11::Fail::Rv(what, rv) => {
            hlog!("sigillum-p11: {} returned {:#x}", what, rv);
            FromDriver::Error { ceremony, error: DriverError::Device }
        }
    };
    Some(match req {
        ToDriver::ListCertificates { .. } => match m.certificates() {
            Ok((token_label, certs)) => FromDriver::Certificates { ceremony, token_label, certs },
            Err(f) => err(f),
        },
        ToDriver::SignDigest { key_id, scheme, digest, pin, .. } => match m.sign(&key_id, scheme, &digest, pin.as_ref()) {
            Ok(signature) => FromDriver::Signed { ceremony, signature },
            Err(f) => err(f),
        },
        // One request at a time, each answered before the next: nothing to cancel.
        ToDriver::Cancel { .. } => return None,
        ToDriver::Register { .. } | ToDriver::Assert { .. } => FromDriver::Error { ceremony, error: DriverError::Unsupported },
    })
}
