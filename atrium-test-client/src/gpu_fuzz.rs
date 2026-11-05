//! atrium-gpu-fuzz — the app GPU channel (F6 G2) from the app's side.
//!
//! - `atrium-gpu-fuzz honest <gpu.sock>`: an honest app runs its own
//!   kernel through frescod and reads the result back; then the checks a
//!   broker must pass: a store outside its buffers loses its work (and a
//!   fresh context works), bad headers and oversized dispatches are
//!   refused, and a uid can't hold more than its share of contexts.
//! - `atrium-gpu-fuzz hostile <gpu.sock> <seconds> <seed>`: hostile apps
//!   send malformed and mutated records, random slot contents, work aimed
//!   at random addresses, and connection storms. Prints PASS if an honest
//!   app is still served at the end.

use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

use atrium_gpu_app::{op, record, CodeHeader, OP_RECORD};
use atrium_rdna4_isa::encode;
use fresco_compute::fuzz::Rng;
use fresco_split::chan::{connect, Chan, Raw, Shm};
use fresco_split::gpu::{reason, GpuMsg};
use fresco_split::MSG_BYTES;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let ok = match a.get(1).map(String::as_str) {
        Some("honest") if a.len() == 3 => honest(&a[2]),
        Some("hostile") if a.len() == 5 => hostile(&a[2], a[3].parse().unwrap_or(30), a[4].parse().unwrap_or(1)),
        _ => {
            eprintln!("usage: atrium-gpu-fuzz honest <gpu.sock> | hostile <gpu.sock> <seconds> <seed>");
            std::process::exit(2);
        }
    };
    std::process::exit(if ok { 0 } else { 1 });
}

/// An app's end of one context.
struct App {
    c: Chan,
    upload: Shm,
    work: Shm,
    arch: u32,
}

fn recv(c: &Chan) -> Result<(GpuMsg, Vec<OwnedFd>), String> {
    match c.recv_raw().map_err(|e| e.to_string())? {
        Raw::Rec(r, fds) => GpuMsg::decode(&r).map(|m| (m, fds)).map_err(|e| e.0.to_string()),
        Raw::Refused(e) => Err(e.0.to_string()),
        Raw::Eof => Err("closed".into()),
    }
}

impl App {
    fn open(sock: &str) -> Result<App, String> {
        let c = connect(std::path::Path::new(sock)).map_err(|e| e.to_string())?;
        match recv(&c)? {
            (GpuMsg::Hello { arch, upload_bytes, work_bytes }, mut fds) if fds.len() == 2 => {
                let w = fds.pop().unwrap();
                let u = fds.pop().unwrap();
                let upload = Shm::open(u, upload_bytes as usize).map_err(|e| e.to_string())?;
                let work = Shm::open(w, work_bytes as usize).map_err(|e| e.to_string())?;
                Ok(App { c, upload, work, arch })
            }
            (m, _) => Err(format!("first record {m:?}")),
        }
    }

    fn send(&self, m: GpuMsg) -> Result<(), String> {
        self.c.send_raw(&m.encode(), &[]).map_err(|e| e.to_string())
    }

    fn call(&self, m: GpuMsg) -> Result<(GpuMsg, Vec<OwnedFd>), String> {
        self.send(m)?;
        recv(&self.c)
    }

    fn code(&mut self, id: u32, slot: &[u8]) -> Result<GpuMsg, String> {
        self.upload.as_mut_slice()[..slot.len()].copy_from_slice(slot);
        Ok(self.call(GpuMsg::Code { id, bytes: slot.len() as u32 })?.0)
    }

    fn buffer(&self, id: u32, bytes: u32) -> Result<(u64, Shm), String> {
        match self.call(GpuMsg::Buffer { id, bytes })? {
            (GpuMsg::BufferOk { va, bytes: b, .. }, mut fds) if fds.len() == 1 && b == bytes => {
                Ok((va, Shm::open(fds.pop().unwrap(), bytes as usize).map_err(|e| e.to_string())?))
            }
            (m, _) => Err(format!("buffer: {m:?}")),
        }
    }

    fn submit(&mut self, fence: u32, records: &[u8]) -> Result<GpuMsg, String> {
        self.work.as_mut_slice()[..records.len()].copy_from_slice(records);
        Ok(self.call(GpuMsg::Submit { fence, bytes: records.len() as u32 })?.0)
    }
}

fn inline_int(v: u32) -> u64 {
    128 + v as u64
}

/// 64 threads; thread i stores `value` at out[i], out address in s[0:1].
fn app_code(arch: u32, value: u32) -> Vec<u8> {
    let mut w: Vec<u32> = Vec::new();
    w.extend(encode("V_LSHLREV_B32", "ENC_VOP2", "default", &[("VDST", 1), ("SRC0", inline_int(2)), ("VSRC1", 0)]).unwrap());
    w.extend(encode("V_MOV_B32", "ENC_VOP1", "default", &[("VDST", 2), ("SRC0", 255)]).unwrap());
    w.push(value);
    w.extend(encode("GLOBAL_STORE_B32", "ENC_VGLOBAL", "default", &[("SADDR", 0), ("VADDR", 1), ("VSRC", 2)]).unwrap());
    w.extend(encode("S_ENDPGM", "ENC_SOPP", "default", &[]).unwrap());
    let code: Vec<u8> = w.iter().flat_map(|d| d.to_le_bytes()).collect();
    let mut slot = CodeHeader { code_bytes: code.len() as u32, vgprs: 3, user_sgprs: 2, lds_bytes: 0, scratch_dwords: 0, threads: [64, 1, 1], wave32: true }
        .encode(&code);
    slot[44..48].copy_from_slice(&arch.to_le_bytes());
    slot
}

fn work(code: u32, va: u64, groups: [u32; 3]) -> Vec<u8> {
    [
        record(op::BIND_CODE, &[code]),
        record(op::USER_DATA, &[2, va as u32, (va >> 32) as u32]),
        record(op::DISPATCH, &[groups[0], groups[1], groups[2], 0, 0, 0]),
    ]
    .concat()
}

/// One honest run: load, fill, dispatch, read back.
fn honest_once(sock: &str, value: u32) -> Result<(), String> {
    let mut a = App::open(sock)?;
    let slot = app_code(a.arch, value);
    match a.code(0, &slot)? {
        GpuMsg::CodeOk { id: 0 } => {}
        m => return Err(format!("code: {m:?}")),
    }
    let (va, mut st) = a.buffer(1, 4096)?;
    st.as_mut_slice()[..256].fill(0);
    a.send(GpuMsg::Write { id: 1, offset: 0, bytes: 256 })?;
    match a.submit(7, &work(0, va, [1, 1, 1]))? {
        GpuMsg::Done { fence: 7 } => {}
        m => return Err(format!("submit: {m:?}")),
    }
    match a.call(GpuMsg::Read { id: 1, offset: 0, bytes: 256 })?.0 {
        GpuMsg::ReadDone { id: 1 } => {}
        m => return Err(format!("read: {m:?}")),
    }
    let words: Vec<u32> = st.as_slice()[..256].chunks(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
    if words.iter().all(|&w| w == value) { Ok(()) } else { Err(format!("read back 0x{:x}", words[0])) }
}

fn honest(sock: &str) -> bool {
    let mut failures = 0;
    let mut check = |ok: bool, what: String| {
        println!("  {} {what}", if ok { "ok  " } else { failures += 1; "FAIL" });
    };
    println!("F6 G2: an app's own kernel, through frescod's GPU channel");
    let r = honest_once(sock, 0xa11ce);
    check(r.is_ok(), format!("the app's kernel filled its buffer, read back through staging ({r:?})"));

    // Aim the kernel at an address nothing is bound to in this VM: the
    // store faults, the work is lost with EIO, and the context ends.
    let r = (|| -> Result<GpuMsg, String> {
        let mut a = App::open(sock)?;
        let slot = app_code(a.arch, 0xdead);
        a.code(0, &slot)?;
        a.submit(1, &work(0, 0x7f00_0000_0000, [1, 1, 1]))
    })();
    check(matches!(r, Ok(GpuMsg::Lost { errno: 5, .. })), format!("a store to an unbound address loses the work with EIO ({r:?})"));
    let r = honest_once(sock, 0xb0b);
    check(r.is_ok(), format!("a fresh context works after one was lost ({r:?})"));

    let r = (|| -> Result<GpuMsg, String> {
        let mut a = App::open(sock)?;
        let mut slot = app_code(a.arch, 1);
        slot[12..16].copy_from_slice(&999u32.to_le_bytes());
        a.code(0, &slot)
    })();
    check(r == Ok(GpuMsg::Refused { reason: reason::CODE, detail: 0 }), format!("a header asking for 999 VGPRs is refused ({r:?})"));
    let r = (|| -> Result<GpuMsg, String> {
        let mut a = App::open(sock)?;
        let slot = app_code(a.arch, 1);
        a.code(0, &slot)?;
        a.submit(1, &work(0, 0, [1 << 16, 1 << 16, 1]))
    })();
    check(r == Ok(GpuMsg::Refused { reason: reason::WORK, detail: 2 }), format!("a 2^32-workgroup dispatch is refused at record 2 ({r:?})"));
    let r = (|| -> Result<GpuMsg, String> {
        let a = App::open(sock)?;
        a.call(GpuMsg::Done { fence: 1 }).map(|m| m.0)
    })();
    check(r == Ok(GpuMsg::Refused { reason: reason::MALFORMED, detail: 0 }), format!("a broker's record from an app is refused ({r:?})"));

    let held: Vec<_> = (0..4).map(|_| App::open(sock)).collect();
    let fifth = App::open(sock).err();
    check(held.iter().all(|a| a.is_ok()) && fifth.is_some(), format!("a uid holds at most 4 contexts (fifth: {fifth:?})"));
    drop(held);
    std::thread::sleep(Duration::from_millis(200));
    let r = honest_once(sock, 0xc0de);
    check(r.is_ok(), format!("closing contexts gives the slots back ({r:?})"));

    println!("{}", if failures == 0 { "ALL OK".to_string() } else { format!("{failures} FAILURES") });
    failures == 0
}

/// A record: a valid one with a few bytes flipped, or random words in the
/// GPU kind range.
fn wild_record(r: &mut Rng) -> [u8; MSG_BYTES] {
    let pick = r.below(8);
    let mut rec = match pick {
        0 => GpuMsg::Code { id: r.below(70), bytes: r.below(3 << 20) }.encode(),
        1 => GpuMsg::Buffer { id: r.below(260), bytes: r.below(80 << 20) }.encode(),
        2 => GpuMsg::Free { id: r.below(260) }.encode(),
        3 => GpuMsg::Write { id: r.below(4), offset: r.next() as u32, bytes: r.below(1 << 14) }.encode(),
        4 => GpuMsg::Read { id: r.below(4), offset: r.below(8192), bytes: r.below(8192) }.encode(),
        5 => GpuMsg::Submit { fence: r.next() as u32, bytes: r.below(32768) }.encode(),
        6 => GpuMsg::Hello { arch: 1, upload_bytes: 1, work_bytes: 1 }.encode(),
        _ => {
            let mut b = [0u8; MSG_BYTES];
            b[0..4].copy_from_slice(&(0x300 + r.below(0x90)).to_le_bytes());
            for i in 1..8 {
                b[i * 4..i * 4 + 4].copy_from_slice(&(r.next() as u32).to_le_bytes());
            }
            b
        }
    };
    if r.chance(20) {
        let i = r.below(MSG_BYTES as u32) as usize;
        rec[i] ^= 1 << r.below(8);
    }
    rec
}

/// Work records: mostly well formed, aimed at random addresses, with
/// random user data and dims, sometimes mutated.
fn wild_work(r: &mut Rng) -> Vec<u8> {
    let n = 1 + r.below(12) as usize;
    let mut v = Vec::new();
    for _ in 0..n {
        let rec = match r.below(5) {
            0 => record(op::BIND_CODE, &[r.below(3)]),
            1 => {
                let count = if r.chance(80) { 2 } else { r.below(18) };
                let mut w = vec![count];
                w.extend((0..count.min(16)).map(|_| r.next() as u32));
                record(op::USER_DATA, &w)
            }
            2 => record(op::DISPATCH, &[1 + r.below(4), 1 + r.below(2), 1, r.below(4), 0, 0]),
            3 => record(op::BARRIER, &[]),
            _ => record(r.below(8), &[r.next() as u32, r.next() as u32]),
        };
        v.extend_from_slice(&rec);
    }
    if r.chance(10) {
        v.truncate(v.len() - r.below(OP_RECORD as u32) as usize);
    }
    v
}

/// One hostile context: returns how it ended.
fn hostile_context(sock: &str, r: &mut Rng, end: Instant) -> String {
    let mut a = match App::open(sock) {
        Ok(a) => a,
        Err(e) => return format!("open: {e}"),
    };
    let honest_code = app_code(a.arch, r.next() as u32);
    for _ in 0..1 + r.below(40) {
        if Instant::now() > end {
            return "time".into();
        }
        let step = r.below(10);
        let res = match step {
            0 | 1 => {
                let mut slot = honest_code.clone();
                if r.chance(50) {
                    let i = r.below(slot.len() as u32) as usize;
                    slot[i] = r.next() as u8;
                }
                a.code(r.below(3), &slot)
            }
            2 => a.buffer(r.below(4), 1 + r.below(1 << 16)).map(|(va, _)| GpuMsg::BufferOk { id: 0, va, bytes: 1 }),
            3 | 4 | 5 => {
                let w = wild_work(r);
                a.submit(r.next() as u32, &w)
            }
            6 => {
                // Random bytes in the slots under a request that uses them.
                for b in a.work.as_mut_slice()[..4096].iter_mut() {
                    *b = r.next() as u8;
                }
                a.call(GpuMsg::Submit { fence: 1, bytes: 80 * (1 + r.below(50)) }).map(|m| m.0)
            }
            7 => {
                // A record with an fd attached.
                let fd = a.upload.fd();
                a.c.send_raw(&GpuMsg::Free { id: 0 }.encode(), &[fd]).map_err(|e| e.to_string()).and_then(|_| recv(&a.c).map(|m| m.0))
            }
            8 => return "walked away".into(),
            _ => {
                let rec = wild_record(r);
                a.c.send_raw(&rec, &[]).map_err(|e| e.to_string()).and_then(|_| recv(&a.c).map(|m| m.0))
            }
        };
        match res {
            Ok(GpuMsg::Refused { reason, .. }) => return format!("refused ({reason})"),
            Ok(GpuMsg::Lost { .. }) => return "lost".into(),
            Ok(_) => {}
            Err(e) => return e,
        }
    }
    "done".into()
}

fn hostile(sock: &str, secs: u64, seed: u64) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    let threads: Vec<_> = (0..3u64)
        .map(|t| {
            let sock = sock.to_string();
            std::thread::spawn(move || {
                let mut r = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15).wrapping_add(t + 1));
                let mut ends = std::collections::BTreeMap::<String, u32>::new();
                while Instant::now() < end {
                    let why = hostile_context(&sock, &mut r, end);
                    let key = why.split(':').next().unwrap_or("").to_string();
                    *ends.entry(key).or_default() += 1;
                }
                ends
            })
        })
        .collect();
    let mut total = std::collections::BTreeMap::<String, u32>::new();
    for t in threads {
        for (k, v) in t.join().unwrap() {
            *total.entry(k).or_default() += v;
        }
    }
    println!("hostile contexts ended: {total:?}");
    std::thread::sleep(Duration::from_millis(300));
    let r = honest_once(sock, 0x600d);
    println!("honest app after the storm: {r:?}");
    let ok = r.is_ok();
    println!("{}", if ok { "PASS" } else { "FAIL" });
    ok
}
