//! lane-client: phase-J.2b gate — a Fresco client that asks frescod
//! (the vblank deadline broker) to sponsor its frame thread, then runs
//! a frame loop against the broker's period grid.
//!
//! Flow: connect to FRESCOD_SOCK → OP_LANE_REQUEST {pid, tid, q_us} →
//! frescod verifies the pid against LOCAL_PEERCRED and SPONSOR_FORs us
//! anchored to its latest real vblank → we work `work_us` per frame and
//! LAMIOC_YIELD to the replenish grid. With "stall", we sleep 100 ms
//! mid-run so frescod's miss feed fires (watch the frescod log).
//!
//! usage: lane-client <q_us> <work_us> <n_periods> [stall]

use std::io::ErrorKind;
use std::os::unix::net::UnixStream;

use aqueduct::{Connection as AqConn, CLASS_DISPLAY};
use fresco_protocol::{control, LaneReplyPayload, LaneRequestPayload};

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct LamLaneStats {
    periods: u64,
    misses: u64,
    throttles: u64,
    max_late_us: u64,
    cpu: i32,
    pad: i32,
}

const IOC_VOID: u64 = 0x2000_0000;
const IOC_OUT: u64 = 0x4000_0000;
const LAMIOC_YIELD: u64 = IOC_VOID | (b'L' as u64) << 8 | 3;
const LAMIOC_STATS: u64 = IOC_OUT
    | ((std::mem::size_of::<LamLaneStats>() as u64 & 0x1fff) << 16)
    | (b'L' as u64) << 8
    | 4;

fn now_sec() -> f64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 || args.len() > 5 {
        eprintln!("usage: {} <q_us> <work_us> <n_periods> [stall]", args[0]);
        std::process::exit(2);
    }
    let q_us: u64 = args[1].parse().expect("q_us");
    let work_us: u64 = args[2].parse().expect("work_us");
    let n_periods: u64 = args[3].parse().expect("n_periods");
    let stall = args.len() == 5 && args[4] == "stall";

    let sock = std::env::var("FRESCOD_SOCK")
        .unwrap_or_else(|_| "/tmp/frescod.sock".into());
    let stream = UnixStream::connect(&sock)?;
    let mut conn = AqConn::wrap(stream)?;

    let req = LaneRequestPayload {
        pid: unsafe { libc::getpid() },
        tid: unsafe { libc::pthread_getthreadid_np() },
        q_us,
    };
    conn.send_message(
        CLASS_DISPLAY,
        control::OP_LANE_REQUEST,
        0,
        &fresco_protocol::encode(&req).expect("encode"),
    )?;
    let reply = loop {
        let msg = conn.recv_message()?;
        if msg.op == control::OP_LANE_REQUEST {
            break fresco_protocol::decode::<LaneReplyPayload>(&msg.payload)
                .map_err(|_| {
                    std::io::Error::new(ErrorKind::InvalidData, "bad reply")
                })?;
        }
    };
    if !reply.ok {
        eprintln!("lane-client: sponsorship REFUSED: {}", reply.err);
        std::process::exit(1);
    }
    let t_us = reply.t_us;
    println!(
        "lane-client: sponsored pid {} tid {} q {q_us}us T {t_us}us",
        req.pid, req.tid
    );

    /* The lane fd for YIELD/STATS — self ops on our own entity. */
    let fd = unsafe {
        libc::open(c"/dev/laminar".as_ptr(), libc::O_RDWR)
    };
    if fd < 0 {
        eprintln!("lane-client: open /dev/laminar failed (need root)");
        std::process::exit(1);
    }

    /* Sync onto the period grid (consume the partial first period). */
    unsafe { libc::ioctl(fd, LAMIOC_YIELD) };
    let mut base = LamLaneStats::default();
    unsafe { libc::ioctl(fd, LAMIOC_STATS, &mut base) };

    let t0 = now_sec();
    let mut wake_max_us = 0.0f64;
    for p in 0..n_periods {
        if stall && p == n_periods / 2 {
            println!("lane-client: stalling 100ms (deliberate misses)");
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let until = now_sec() + work_us as f64 / 1e6;
        let mut acc = 0u64;
        while now_sec() < until {
            acc = acc.wrapping_add(p);
        }
        std::hint::black_box(acc);
        unsafe { libc::ioctl(fd, LAMIOC_YIELD) };
        if !stall {
            /* wake jitter vs the grid (only meaningful without misses) */
            let late =
                (now_sec() - t0) - ((p + 1) as f64 * t_us as f64 / 1e6);
            let r = late - (late / (t_us as f64 / 1e6)).round()
                * (t_us as f64 / 1e6);
            wake_max_us = wake_max_us.max(r.abs() * 1e6);
        }
    }

    let mut st = LamLaneStats::default();
    unsafe { libc::ioctl(fd, LAMIOC_STATS, &mut st) };
    let misses = st.misses - base.misses;
    println!(
        "lane-client: cpu={} periods={} misses={} (startup {}) throttles={} \
         max_replenish_late_us={} wake_jitter_max_us={:.0}",
        st.cpu, st.periods, misses, base.misses, st.throttles,
        st.max_late_us, wake_max_us
    );
    unsafe { libc::close(fd) };
    std::process::exit(if stall || misses == 0 { 0 } else { 1 });
}
