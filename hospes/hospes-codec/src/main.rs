//! `hospes-codec`: Cisco's OpenH264 decoder under Hospes (docs/spec/hospes.md §9).
//!
//! ```text
//! hospes-codec --sock FD --lib FD [--mem MiB] [--max WxH]   # production: fds from the supervisor
//! hospes-codec --file LIB IN.264 OUT.yuv [--mem MiB]         # conformance: Annex-B in, I420 out
//! ```
//!
//! Whatever the mode, every file is open before `cap_enter`, and capability mode starts
//! before the library's ELF is parsed. The library gets the glibc shim and nothing else:
//! one thread (H0 §4), no files, no network. A crash ends this process only; the worker
//! sees the socket close.
#![no_std]
#![no_main]

extern crate alloc;

mod openh264;

use alloc::vec::Vec;
use hospes_codec_wire as wire;
use hospes_rt::load::{self, Library};
use hospes_rt::start::StartInfo;
use hospes_rt::{heap, hlog, sys};
use openh264::Decoder;

fn num(s: &[u8]) -> Option<usize> {
    core::str::from_utf8(s).ok()?.parse().ok()
}

fn read_all(fd: i32) -> Result<Vec<u8>, sys::Errno> {
    let mut v = Vec::new();
    let mut chunk = [0u8; 65536];
    loop {
        let n = sys::read(fd, &mut chunk)?;
        if n == 0 {
            return Ok(v);
        }
        v.extend_from_slice(&chunk[..n]);
    }
}

fn read_exact(fd: i32, mut b: &mut [u8]) -> bool {
    while !b.is_empty() {
        match sys::read(fd, b) {
            Ok(0) | Err(_) => return false,
            Ok(n) => b = &mut b[n..],
        }
    }
    true
}

struct Args {
    sock: Option<i32>,
    lib: Option<i32>,
    file: Option<(&'static [u8], &'static [u8], &'static [u8])>,
    mem_mib: usize,
    max: (u32, u32),
}

fn parse_args(info: &'static StartInfo) -> Option<Args> {
    let mut a = Args { sock: None, lib: None, file: None, mem_mib: 512, max: (4096, 2304) };
    let mut i = 1;
    while let Some(x) = info.arg(i) {
        match x {
            b"--sock" => a.sock = Some(num(info.arg(i + 1)?)? as i32),
            b"--lib" => a.lib = Some(num(info.arg(i + 1)?)? as i32),
            b"--mem" => a.mem_mib = num(info.arg(i + 1)?)?,
            b"--max" => {
                let s = core::str::from_utf8(info.arg(i + 1)?).ok()?;
                let (w, h) = s.split_once('x')?;
                a.max = (w.parse().ok()?, h.parse().ok()?);
            }
            b"--file" => {
                a.file = Some((info.arg_z(i + 1)?, info.arg_z(i + 2)?, info.arg_z(i + 3)?));
                i += 2;
            }
            _ => return None,
        }
        i += 2;
    }
    Some(a)
}

#[no_mangle]
pub fn hospes_main(info: &'static StartInfo) -> i32 {
    let Some(args) = parse_args(info) else {
        hlog!("usage: hospes-codec --sock FD --lib FD [--mem MiB] [--max WxH] | --file LIB IN OUT");
        return 2;
    };
    // Open what we need, then give up the global namespaces.
    let (lib_fd, files) = match args.file {
        Some((lib, inp, out)) => {
            let o = |p: &[u8], fl| sys::openat(sys::AT_FDCWD, p, fl);
            let (Ok(l), Ok(i), Ok(w)) = (o(lib, sys::O_RDONLY), o(inp, sys::O_RDONLY), sys::create(out)) else {
                hlog!("hospes-codec: cannot open the files");
                return 2;
            };
            (l, Some((i, w)))
        }
        None => match (args.lib, args.sock) {
            (Some(l), Some(_)) => (l, None),
            _ => {
                hlog!("hospes-codec: --sock and --lib are required");
                return 2;
            }
        },
    };
    if let Err(e) = sys::cap_enter() {
        hlog!("hospes-codec: cap_enter: {:?}", e);
        return 3;
    }
    heap::set_limit(args.mem_mib << 20);
    hospes_glibc::init();
    let opts = load::Options { hwcap: sys::linux_hwcap(info.auxv), allow_unknown: false, trap: false };
    let lib = match load::load(lib_fd, &opts, &hospes_glibc::Glibc) {
        Ok(l) => l,
        Err(e) => {
            hlog!("hospes-codec: load: {:?}", e);
            if let Some(s) = args.sock {
                fail(s, 1);
            }
            return 4;
        }
    };
    let _ = sys::close(lib_fd);
    run_init(&lib);
    let Some(mut dec) = Decoder::new(&lib) else {
        hlog!("hospes-codec: WelsCreateDecoder failed");
        if let Some(s) = args.sock {
            fail(s, 2);
        }
        return 5;
    };
    match files {
        Some((i, w)) => file_mode(&mut dec, i, w),
        None => sock_mode(&mut dec, args.sock.unwrap(), args.max),
    }
}

fn run_init(lib: &Library) {
    // glibc hands constructors the real argv; ours tells the library nothing.
    static ARGV0: &[u8] = b"hospes-codec\0";
    let argv: [*const u8; 2] = [ARGV0.as_ptr(), core::ptr::null()];
    let envp: [*const u8; 1] = [core::ptr::null()];
    unsafe { lib.run_init(1, argv.as_ptr(), envp.as_ptr()) };
}

fn fail(sock: i32, code: u32) {
    let _ = sys::write_all(sock, &wire::header(wire::FAIL, 4));
    let _ = sys::write_all(sock, &code.to_le_bytes());
}

fn file_mode(dec: &mut Decoder, inp: i32, out: i32) -> i32 {
    let Ok(data) = read_all(inp) else { return 6 };
    let mut frames = 0u32;
    let mut emit = |p: &openh264::Frame| {
        p.write_i420(|b| {
            let _ = sys::write_all(out, b);
        });
        frames += 1;
    };
    for (n, r) in wire::split_annexb(&data).enumerate() {
        dec.decode(&data[r], n as i64, &mut emit);
    }
    dec.drain(&mut emit);
    hlog!("hospes-codec: frames={} heap={} KiB", frames, heap::mapped() / 1024);
    0
}

fn sock_mode(dec: &mut Decoder, s: i32, max: (u32, u32)) -> i32 {
    if sys::write_all(s, &wire::header(wire::READY, 4)).is_err() || sys::write_all(s, &wire::VERSION.to_le_bytes()).is_err() {
        return 7;
    }
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let mut h = [0u8; 8];
        if !read_exact(s, &mut h) {
            return 0; // worker gone
        }
        let (kind, len) = wire::parse_header(&h);
        if len > wire::MAX_AU {
            fail(s, 3);
            return 8;
        }
        buf.resize(len as usize, 0);
        if !read_exact(s, &mut buf) {
            return 0;
        }
        let mut ok = true;
        let mut send = |p: &openh264::Frame| {
            if !ok || p.width > max.0 || p.height > max.1 {
                ok = false;
                return;
            }
            let size = wire::i420_size(p.width, p.height).unwrap_or(0);
            let total = (wire::PIC_HEADER + size) as u32;
            ok = sys::write_all(s, &wire::header(wire::PICTURE, total)).is_ok()
                && sys::write_all(s, &wire::Picture::header(p.pts, p.width, p.height)).is_ok();
            p.write_i420(|b| {
                if ok {
                    ok = sys::write_all(s, b).is_ok();
                }
            });
        };
        let state = match kind {
            wire::DECODE if buf.len() >= 8 => {
                let pts = i64::from_le_bytes(buf[..8].try_into().unwrap());
                dec.decode(&buf[8..], pts, &mut send)
            }
            wire::DRAIN => dec.drain(&mut send),
            wire::RESET => {
                if !dec.reset() {
                    fail(s, 2);
                    return 9;
                }
                0
            }
            _ => {
                fail(s, 3);
                return 8;
            }
        };
        if !ok {
            return 10;
        }
        if sys::write_all(s, &wire::header(wire::DONE, 4)).is_err() || sys::write_all(s, &state.to_le_bytes()).is_err() {
            return 0;
        }
    }
}
