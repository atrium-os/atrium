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
        eprintln!(
            "  → request jaild: launch {} as {} ({}{}) caps={:?}",
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

    // Daemon mode: the real launcher. The boot→login service loop (listen for the
    // login backend's authenticated handoff, handle logout/FUS) lands next.
    let _ = JaildLauncher::default();
    eprintln!("ostiarius: daemon loop not yet wired — run `ostiarius --demo [gui|cli]`");
    std::process::exit(2);
}
