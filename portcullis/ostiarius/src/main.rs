//! ostiarius — the doorkeeper daemon.
//!
//!   ostiarius --demo [gui|cli]   exercise the flow with a logging launcher (no
//!                                jaild needed): boot → auth → login → logout,
//!                                against a real (temp) seat. Host-runnable.
//!   ostiarius                    the daemon (drives the real JaildLauncher) —
//!                                the boot→login loop lands incrementally.

use ostiarius::{Frontend, JaildLauncher, Launcher, LaunchSpec, Ostiarius};

/// A launcher that logs instead of contacting jaild — for the demo.
struct LogLauncher;
impl Launcher for LogLauncher {
    fn launch(&mut self, s: &LaunchSpec) -> Result<i32, String> {
        // `owner` is the human identity the app belongs to — NOT its run uid. The
        // real JaildLauncher allocates a dedicated per-app uid (≥50000) and execs
        // under it; no app runs as root. We log a placeholder uid to make that
        // explicit (the demo doesn't actually allocate one).
        eprintln!(
            "  → request jaild: launch {} (owner={}, exec as per-app uid ~50000) ({}{}) caps={:?}",
            s.app_id, s.owner, s.bin,
            if s.argv.is_empty() { String::new() } else { format!(" {}", s.argv.join(" ")) },
            s.caps
        );
        Ok(50_000)
    }
    fn teardown(&mut self, app: &str) -> Result<(), String> {
        eprintln!("  → request jaild: teardown {app}");
        Ok(())
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--demo") {
        let frontend = if args.iter().any(|a| a == "cli") { Frontend::Cli } else { Frontend::Gui };
        let seat = format!("/tmp/ostiarius-demo-{}", std::process::id());
        let _ = std::fs::remove_file(&seat);
        let mut o = Ostiarius::new(LogLauncher).with_seat_path(seat.clone());

        eprintln!("boot: bring up the login screen");
        let _ = o.boot();
        eprintln!("authenticate alice (stub)");
        let human = o.authenticate("alice", "pw").expect("auth");
        eprintln!("login: bind the seat + launch the {frontend:?} session");
        o.login(&human, frontend).expect("login");
        eprintln!("seat active = {:?}", o.active_human());
        eprintln!("logout");
        o.logout().expect("logout");
        eprintln!("seat active = {:?} (back to the login screen)", o.active_human());
        let _ = std::fs::remove_file(&seat);
        return;
    }

    // Demo daemon: serve the control socket with the LogLauncher (no jaild, no
    // PAM → the stub auth accepts any non-empty credential), so the
    // vestibulum→ostiarius→session-launch handoff can be exercised in-VM without
    // the full TCB. `OSTIARIUS_SOCK` sets the socket; `OSTIARIUS_OPEN=1` bypasses
    // the vestibulum peer gate (else the caller must be registered as
    // org.atrium.vestibulum). The launches are logged, not really jailed.
    if args.iter().any(|a| a == "--serve-demo") {
        let sock = std::env::var("OSTIARIUS_SOCK")
            .unwrap_or_else(|_| "/var/run/atrium/ostiarius.sock".into());
        let seat = std::env::var("OSTIARIUS_SEAT")
            .unwrap_or_else(|_| format!("/tmp/ostiarius-seat-{}", std::process::id()));
        let _ = std::fs::remove_file(&seat);
        let mut o = Ostiarius::new(LogLauncher).with_seat_path(seat);
        eprintln!("ostiarius(demo): serving {sock} with LogLauncher");
        let r = if std::env::var("OSTIARIUS_OPEN").is_ok() {
            ostiarius::control::serve(&mut o, &sock, |_| true)
        } else {
            ostiarius::control::serve(&mut o, &sock, ostiarius::control::vestibulum_gate)
        };
        if let Err(e) = r {
            eprintln!("ostiarius(demo): serve {sock}: {e}");
            std::process::exit(1);
        }
        return;
    }

    // Daemon mode: listen on the control socket for vestibulum's authenticated
    // logins, peer-gated by getpeereid. Boot launches the login UI first.
    const SOCK: &str = "/var/run/atrium/ostiarius.sock";
    // Real PAM (/etc/pam.d/atrium-login) when built --features pam; the stub
    // otherwise (which refuses, so the production daemon must be a pam build).
    let mut o = Ostiarius::new(JaildLauncher::default()).with_pam("atrium-login");
    if let Err(e) = o.boot() {
        eprintln!("ostiarius: boot (launch vestibulum): {e}");
    }
    eprintln!("ostiarius: serving on {SOCK} (vestibulum-only)");
    if let Err(e) = ostiarius::control::serve(&mut o, SOCK, ostiarius::control::vestibulum_gate) {
        eprintln!("ostiarius: serve {SOCK}: {e}");
        std::process::exit(1);
    }
}
