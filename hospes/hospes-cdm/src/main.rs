//! `hospes-cdm`: a Chromium-interface CDM under Hospes (docs/spec/hospes.md §3, §8).
//!
//! ```text
//! hospes-cdm --ctrl FD --data FD --lib FD [--store FD] [--key-system NAME] [--mem MiB]
//! ```
//!
//! The supervisor passes every fd: the control socket (its own end), the data socket (the
//! decode worker's), the library (read-only) and the origin's store directory. We enter
//! capability mode before parsing the library, load it against the glibc shim, create one
//! CDM instance and then serve both channels from one loop until the control channel
//! closes. The CDM never sees a path, a socket of its own or the network.
#![no_std]
#![no_main]

extern crate alloc;

mod abi;
mod host;
mod store;

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::ffi::c_void;

use hospes_proto::{status, Msg, Sample};
use hospes_rt::load::{self, Library};
use hospes_rt::start::StartInfo;
use hospes_rt::{heap, hlog, sys, trap};

use abi::{AudioDecoderConfig2, Cdm, InputBuffer2, Obj, Policy, SubsampleEntry, VideoDecoderConfig2};
use host::{st, State};

/// The largest frame we pass on, whatever the CDM claims.
const MAX_W: u32 = 7680;
const MAX_H: u32 = 4320;

fn num(s: &[u8]) -> Option<usize> {
    core::str::from_utf8(s).ok()?.parse().ok()
}

struct Args {
    ctrl: i32,
    data: i32,
    lib: i32,
    store: Option<i32>,
    key_system: &'static [u8],
    mem_mib: usize,
}

fn parse_args(info: &'static StartInfo) -> Option<Args> {
    let (mut ctrl, mut data, mut lib) = (None, None, None);
    let mut a = Args { ctrl: -1, data: -1, lib: -1, store: None, key_system: b"org.w3.clearkey", mem_mib: 512 };
    let mut i = 1;
    while let Some(x) = info.arg(i) {
        let v = info.arg(i + 1)?;
        match x {
            b"--ctrl" => ctrl = Some(num(v)? as i32),
            b"--data" => data = Some(num(v)? as i32),
            b"--lib" => lib = Some(num(v)? as i32),
            b"--store" => a.store = Some(num(v)? as i32),
            b"--key-system" => a.key_system = v,
            b"--mem" => a.mem_mib = num(v)?,
            _ => return None,
        }
        i += 2;
    }
    a.ctrl = ctrl?;
    a.data = data?;
    a.lib = lib?;
    Some(a)
}

/// Report and exit. The supervisor sees `Fail`, then the channel close.
pub fn fatal(code: u32, what: &str) -> ! {
    // Not through the control channel: this may run off the loop's thread. The supervisor
    // sees the channel close and the exit status.
    hlog!("hospes-cdm: {}", what);
    sys::exit(100 + code as i32)
}

fn fail(ctrl: i32, code: u32, what: &str) -> i32 {
    let _ = sys::write_all(ctrl, &Msg::Fail { code, what: what.as_bytes().to_vec() }.encode());
    hlog!("hospes-cdm: {}", what);
    code as i32
}

#[no_mangle]
pub fn hospes_main(info: &'static StartInfo) -> i32 {
    let Some(args) = parse_args(info) else {
        hlog!("usage: hospes-cdm --ctrl FD --data FD --lib FD [--store FD] [--key-system NAME] [--mem MiB]");
        return 2;
    };
    // The trap facility is found by name, which capability mode does not allow: first.
    let trap = match trap::init(hospes_glibc::syscall::handle) {
        Ok(()) => true,
        Err(_) if cfg!(target_os = "freebsd") => {
            return fail(args.ctrl, 3, "no system call trap (is hospes_trap.ko loaded?)");
        }
        Err(e) => {
            hlog!("hospes-cdm: no system call trap ({:?})", e);
            false
        }
    };
    if let Err(e) = sys::cap_enter() {
        return fail(args.ctrl, 3, if e.0 == sys::ENOSYS { "no capability mode" } else { "cap_enter failed" });
    }
    heap::set_limit(args.mem_mib << 20);
    hospes_glibc::init_from(info);
    let opts = load::Options { hwcap: sys::linux_hwcap(info.auxv), allow_unknown: false, trap };
    let lib = match load::load(args.lib, &opts, &hospes_glibc::Glibc) {
        Ok(l) => l,
        Err(e) => {
            hlog!("hospes-cdm: load: {:?}", e);
            return fail(args.ctrl, 4, "the library did not load");
        }
    };
    let _ = sys::close(args.lib);
    hospes_glibc::library_loaded(&lib);
    run_init(&lib);

    host::install(State {
        ctrl: args.ctrl,
        cdm: None,
        timers: Vec::new(),
        deferred: VecDeque::new(),
        store: args.store.map(store::Store::open),
        files: Vec::new(),
        buffers: Vec::new(),
        link_mask: 0,
        thread: sys::gettid(),
        ctrl_broken: false,
    });

    type Init = extern "C" fn();
    type Create = extern "C" fn(i32, *const u8, u32, extern "C" fn(i32, *mut c_void) -> *mut c_void, *mut c_void) -> *mut Obj;
    type Version = extern "C" fn() -> *const u8;
    let (Some(init), Some(create), Some(version)) = (lib.sym("InitializeCdmModule_4"), lib.sym("CreateCdmInstance"), lib.sym("GetCdmVersion")) else {
        return fail(args.ctrl, 5, "the library is not a CDM (entry points missing)");
    };
    // Safety: the CDM interface's exported C functions (content_decryption_module.h).
    let (init, create, version): (Init, Create, Version) = unsafe { (core::mem::transmute(init), core::mem::transmute(create), core::mem::transmute(version)) };
    init();
    let ks = args.key_system;
    let cdm = create(10, ks.as_ptr(), ks.len() as u32, host::get_host, core::ptr::null_mut());
    if cdm.is_null() {
        return fail(args.ctrl, 6, "CreateCdmInstance refused (interface version 10, this key system)");
    }
    st().cdm = Some(Cdm(cdm));
    let v = version();
    let mut ver = Vec::new();
    if !v.is_null() {
        // Safety: a NUL-terminated string from the CDM; capped.
        for i in 0..256 {
            let c = unsafe { *v.add(i) };
            if c == 0 {
                break;
            }
            ver.push(c);
        }
    }
    host::send(&Msg::Ready { cdm_version: ver });

    let rc = serve(args.ctrl, args.data);
    if let Some(c) = st().cdm.take() {
        c.destroy();
    }
    if let Some(deinit) = lib.sym("DeinitializeCdmModule") {
        let f: Init = unsafe { core::mem::transmute(deinit) };
        f();
    }
    rc
}

fn run_init(lib: &Library) {
    static ARGV0: &[u8] = b"hospes-cdm\0";
    let argv: [*const u8; 2] = [ARGV0.as_ptr(), core::ptr::null()];
    let envp: [*const u8; 1] = [core::ptr::null()];
    unsafe { lib.run_init(1, argv.as_ptr(), envp.as_ptr()) };
}

/// Read one message; `None` when the peer closed (or sent garbage, which ends the channel).
fn recv(fd: i32) -> Option<Msg> {
    let mut h = [0u8; 8];
    if !matches!(sys::read_exact(fd, &mut h), Ok(true)) {
        return None;
    }
    let (k, n) = hospes_proto::parse_header(&h).ok()?;
    let mut p = alloc::vec![0u8; n as usize];
    if n != 0 && !matches!(sys::read_exact(fd, &mut p), Ok(true)) {
        return None;
    }
    match Msg::decode(k, &p) {
        Ok(m) => Some(m),
        Err(e) => {
            hlog!("hospes-cdm: malformed message: {}", e.0);
            None
        }
    }
}

fn serve(ctrl: i32, data: i32) -> i32 {
    let mut data = Some(data);
    loop {
        while host::run_one_deferred() {}
        fire_timers();
        if st().ctrl_broken {
            return 0;
        }
        while host::run_one_deferred() {}
        let now = sys::monotonic_ns();
        let timeout = st().timers.iter().map(|t| t.0).min().map_or(-1, |at| (at.saturating_sub(now) / 1_000_000).min(60_000) as i32 + 1);
        let mut fds = [
            sys::PollFd { fd: ctrl, events: sys::POLLIN, revents: 0 },
            sys::PollFd { fd: data.unwrap_or(-1), events: sys::POLLIN, revents: 0 },
        ];
        match sys::poll(&mut fds, timeout) {
            Ok(_) => {}
            Err(sys::Errno(e)) if e == sys::EINTR => continue,
            Err(_) => return 20,
        }
        if fds[0].revents != 0 {
            match recv(ctrl) {
                Some(m) => control(m),
                None => return 0, // the supervisor is gone: so are we
            }
        }
        if let Some(d) = data {
            if fds[1].revents != 0 {
                match recv(d) {
                    Some(m) => {
                        let reply = data_call(m);
                        if sys::write_all(d, &reply.encode()).is_err() {
                            data = None;
                        }
                    }
                    None => data = None, // the worker is gone; sessions stay for the next one
                }
            }
        }
    }
}

fn fire_timers() {
    let now = sys::monotonic_ns();
    loop {
        let s = st();
        let Some(i) = s.timers.iter().enumerate().filter(|(_, t)| t.0 <= now).min_by_key(|(_, t)| t.0).map(|(i, _)| i) else { return };
        let (_, ctx) = s.timers.swap_remove(i);
        if let Some(c) = &s.cdm {
            Cdm(c.0).timer_expired(ctx);
        }
        while host::run_one_deferred() {}
    }
}

fn cdm() -> Cdm {
    Cdm(st().cdm.as_ref().map(|c| c.0).unwrap_or(core::ptr::null_mut()))
}

fn control(m: Msg) {
    let c = cdm();
    match m {
        Msg::Init { allow_persistent, link_mask } => {
            st().link_mask = link_mask;
            c.initialize(false, allow_persistent && st().store.is_some(), false);
        }
        Msg::GetStatusForPolicy { promise, min_hdcp } => c.get_status_for_policy(promise, &Policy { min_hdcp_version: min_hdcp }),
        Msg::SetServerCertificate { promise, cert } => c.set_server_certificate(promise, &cert),
        Msg::CreateSession { promise, session_type, init_type, init } => c.create_session(promise, session_type, init_type, &init),
        Msg::LoadSession { promise, session_type, id } => c.load_session(promise, session_type, &id),
        Msg::UpdateSession { promise, id, response } => c.update_session(promise, &id, &response),
        Msg::CloseSession { promise, id } => c.close_session(promise, &id),
        Msg::RemoveSession { promise, id } => c.remove_session(promise, &id),
        _ => hlog!("hospes-cdm: unexpected control message"),
    }
}

/// `InputBuffer_2` over a sample; the borrows must outlive the call.
fn input<'a>(s: &'a Sample, subs: &'a [SubsampleEntry]) -> InputBuffer2 {
    InputBuffer2 {
        data: if s.data.is_empty() { core::ptr::null() } else { s.data.as_ptr() },
        data_size: s.data.len() as u32,
        encryption_scheme: s.scheme,
        key_id: s.key_id.as_ptr(),
        key_id_size: s.key_id.len() as u32,
        _pad0: 0,
        iv: s.iv.as_ptr(),
        iv_size: s.iv.len() as u32,
        _pad1: 0,
        subsamples: if subs.is_empty() { core::ptr::null() } else { subs.as_ptr() },
        num_subsamples: subs.len() as u32,
        _pad2: 0,
        crypt_byte_block: s.crypt_byte_block,
        skip_byte_block: s.skip_byte_block,
        timestamp: s.timestamp,
    }
}

fn data_call(m: Msg) -> Msg {
    let c = cdm();
    let r = match m {
        Msg::Decrypt { sample } => {
            let subs: Vec<SubsampleEntry> = sample.subsamples.iter().map(|&(a, b)| SubsampleEntry { clear_bytes: a, cipher_bytes: b }).collect();
            let inp = input(&sample, &subs);
            let mut blk = host::BlockObj::new();
            let st_ = c.decrypt(&inp, &mut blk as *mut host::BlockObj as *mut Obj);
            let data = if blk.buffer.is_null() { Some(Vec::new()) } else { host::take(blk.buffer) };
            match data {
                Some(d) if st_ == status::SUCCESS => Msg::Decrypted { status: st_, timestamp: blk.timestamp, data: d },
                Some(_) => Msg::Decrypted { status: st_, timestamp: 0, data: Vec::new() },
                None => Msg::Decrypted { status: status::DECRYPT_ERROR, timestamp: 0, data: Vec::new() },
            }
        }
        Msg::InitAudio { codec, channels, bits, rate, extra, scheme } => {
            let cfg = AudioDecoderConfig2 {
                codec,
                channel_count: channels as i32,
                bits_per_channel: bits as i32,
                samples_per_second: rate as i32,
                extra_data: if extra.is_empty() { core::ptr::null() } else { extra.as_ptr() },
                extra_data_size: extra.len() as u32,
                encryption_scheme: scheme,
            };
            Msg::InitResult { status: c.init_audio(&cfg) }
        }
        Msg::InitVideo { codec, profile, format, width, height, extra, scheme } => {
            let cfg = VideoDecoderConfig2 {
                codec,
                profile,
                format,
                _pad: 0,
                width: width.min(MAX_W) as i32,
                height: height.min(MAX_H) as i32,
                extra_data: if extra.is_empty() { core::ptr::null() } else { extra.as_ptr() },
                extra_data_size: extra.len() as u32,
                encryption_scheme: scheme,
            };
            Msg::InitResult { status: c.init_video(&cfg) }
        }
        Msg::DecryptAndDecode { sample } => {
            let subs: Vec<SubsampleEntry> = sample.subsamples.iter().map(|&(a, b)| SubsampleEntry { clear_bytes: a, cipher_bytes: b }).collect();
            let inp = input(&sample, &subs);
            let mut fr = host::FrameObj::new();
            let s = c.decrypt_and_decode_frame(&inp, &mut fr as *mut host::FrameObj as *mut Obj);
            let data = if fr.buffer.is_null() { None } else { host::take(fr.buffer) };
            let mut f = hospes_proto::Frame { status: s, ..Default::default() };
            if s == status::SUCCESS {
                match data {
                    Some(d) => {
                        f.format = fr.format;
                        f.width = fr.size.width.max(0) as u32;
                        f.height = fr.size.height.max(0) as u32;
                        f.offsets = fr.offsets;
                        f.strides = fr.strides;
                        f.timestamp = fr.timestamp;
                        f.data = d;
                    }
                    None => f.status = status::DECODE_ERROR,
                }
            }
            Msg::Decoded { frame: f }
        }
        Msg::ResetDecoder { stream } => {
            c.reset_decoder(stream);
            Msg::Done
        }
        Msg::DeinitDecoder { stream } => {
            c.deinit_decoder(stream);
            Msg::Done
        }
        _ => Msg::Decrypted { status: status::DECRYPT_ERROR, timestamp: 0, data: Vec::new() },
    };
    while host::run_one_deferred() {}
    r
}
