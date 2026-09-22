//! Drive the corpus through the broker over real worker processes — jailed
//! or not, depending on how it is configured.
//!
//! ★ A BINARY, NOT A `#[test]`, because this runs in the FreeBSD VM and cargo
//! does not: the VM is for RUNNING, and everything that builds is
//! cross-compiled on the host. A test harness that cannot be staged as a
//! single file is a test that only ever runs where the jails are not.
//!
//!   jailed_corpus <recordings-dir> <worker-or-app-tree> [launcher args…]
//!
//! With no launcher arguments the worker is spawned directly — process
//! isolation only. With them, the worker is launched through the confining
//! command, and `{instance}` in any argument becomes the session id:
//!
//!   jailed_corpus /root/recordings /root/worker-app \
//!       /root/portcullis exec --instance {instance}

use navigator_backend::jailed::{Confinement, JailedHost, WorkerConfig};
use navigator_backend::navigatord::{Event, Navigatord, Request};
use navigator_backend::reverse::Back;
use navigator_backend::session::SessionLimits;
use std::time::Duration;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 3 {
        eprintln!("usage: {} <recordings-dir> <worker|app-tree> [launcher args…]", a[0]);
        std::process::exit(2);
    }
    let (dir, worker) = (a[1].clone(), a[2].clone());
    let confinement = if a.len() > 3 {
        Confinement::Launcher { program: a[3].clone(), args: a[4..].to_vec() }
    } else {
        Confinement::None
    };
    let confined = matches!(confinement, Confinement::Launcher { .. });
    let cfg = WorkerConfig { worker, confinement,
                             deadline: Duration::from_secs(60),
                             shutdown_grace: Duration::from_secs(5) };
    println!("confinement: {}", if confined { "LAUNCHER (see spec §6.5.2)" }
                                else { "none — process isolation only, NOT a jail" });

    let mut n = Navigatord::new(JailedHost::new(cfg, SessionLimits::default()));
    n.tick(1_000);

    let mut files: Vec<_> = std::fs::read_dir(&dir).expect("readable directory")
        .flatten().map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "json").unwrap_or(false))
        .collect();
    files.sort();

    let (mut opened, mut navigated, mut rewound) = (0u32, 0u32, 0u32);
    let mut failures: Vec<String> = vec![];
    let started = std::time::Instant::now();
    // ★ Per-session costs, because a total wall time cannot answer the M3
    // question (spec §10 q1): is ONE jail per document cheap enough? Open is
    // launch + jail + worker start + the first parse; close is the launcher's
    // teardown. The unconfined arm is the same minus the jail, so the jail's
    // price is the difference between arms — never a number from one arm.
    let (mut open_ms, mut nav_ms, mut close_ms) = (vec![], vec![], vec![]);

    for p in &files {
        let bytes = std::fs::read(p).expect("readable file");
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        let t = std::time::Instant::now();
        let opened_ev = n.handle(Request::OpenSession { recording: bytes }).pop();
        open_ms.push(t.elapsed().as_secs_f64() * 1e3);
        let (session, triggers) = match opened_ev {
            Some(Event::SessionOpened { session, triggers }) => { opened += 1; (session, triggers) }
            Some(Event::Blocked { why, .. }) => { failures.push(format!("{name}: {why}")); continue }
            other => { failures.push(format!("{name}: {other:?}")); continue }
        };
        for trigger in triggers {
            let t = std::time::Instant::now();
            let ev = n.handle(Request::Navigate { session, trigger: trigger.clone() }).pop();
            nav_ms.push(t.elapsed().as_secs_f64() * 1e3);
            match ev {
                Some(Event::SceneReady { .. }) => navigated += 1,
                other => failures.push(format!("{name}: {trigger} -> {other:?}")),
            }
            match n.handle(Request::Back { session }).pop() {
                Some(Event::Rewound { how: Back::Stepped, .. }) => rewound += 1,
                other => failures.push(format!("{name}: back -> {other:?}")),
            }
        }
        let t = std::time::Instant::now();
        n.handle(Request::Close { session });
        close_ms.push(t.elapsed().as_secs_f64() * 1e3);
    }

    println!("{opened} sessions, {navigated} navigations, {rewound} rewinds, \
              {} failures, {:.1}s", failures.len(), started.elapsed().as_secs_f64());
    for (what, v) in [("open", &mut open_ms), ("navigate", &mut nav_ms), ("close", &mut close_ms)] {
        println!("{}", dist(what, v));
    }
    for f in failures.iter().take(10) { println!("  {f}"); }
    // ★ A run that opened nothing is not a pass. The most likely way this
    // reports success having proved nothing is an empty or wrong directory.
    if opened == 0 { println!("NOTHING WAS OPENED — this proved nothing"); std::process::exit(1) }
    if navigated != rewound || !failures.is_empty() { std::process::exit(1) }
    println!("OK");
}

/// `name n=… p50=… p90=… p99=… max=… mean=…` in milliseconds — one line, so
/// a harness can grep it without parsing prose.
fn dist(name: &str, v: &mut [f64]) -> String {
    if v.is_empty() { return format!("{name}_ms n=0"); }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let q = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
    format!("{name}_ms n={} p50={:.1} p90={:.1} p99={:.1} max={:.1} mean={:.1}",
            v.len(), q(0.5), q(0.9), q(0.99), v[v.len() - 1],
            v.iter().sum::<f64>() / v.len() as f64)
}
