//! forum-dock — Forum's launcher, an ordinary graphics-only app.
//!
//! It holds no special capability. It lists the installed apps and, on activate,
//! REQUESTS a launch from portcullisd (the TCB), which does the verify → allocate
//! uid → register → jail dance. The dock never launches anything itself; launching
//! is a request authorized by the user's grants (docs/spec/forum.md §6).
//!
//! Usage:
//!   forum-dock              # list installed apps
//!   forum-dock launch <id>  # ask the TCB to launch app <id>
//!
//! Apps dir: FORUM_APPS_DIR (default /var/lib/atrium/apps).

use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use forum_dock::{catalog, APPS_DIR};
use portcullis_ipc::{
    read_response, round_trip, send_fds, write_request, Request, Response, PROTO_VERSION,
    SOCKET_PATH,
};

fn apps_dir() -> PathBuf {
    std::env::var("FORUM_APPS_DIR").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(APPS_DIR))
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("list") => {
            let apps = catalog(&apps_dir());
            if apps.is_empty() {
                println!("forum-dock: no apps installed under {}", apps_dir().display());
            }
            for a in apps {
                let desc = a.description.unwrap_or_default();
                println!("  {:<28} {:<20} {}", a.id, a.name, desc);
            }
            Ok(())
        }
        Some("launch") => {
            let id = args.get(1).cloned().unwrap_or_else(|| {
                eprintln!("usage: forum-dock launch <app-id>");
                std::process::exit(2);
            });
            launch(&id)
        }
        Some(other) => {
            eprintln!("forum-dock: unknown command '{other}' (try: list | launch <id>)");
            std::process::exit(2);
        }
    }
}

/// Request a launch from portcullisd. The dock is just the requester — the daemon
/// does the policy check, the per-app uid, the jail, the teardown. Mirrors the
/// portcullis CLI's wire dance: Hello → Launch → ReadyForFds → hand over stdio
/// (SCM_RIGHTS) → LaunchExit.
fn launch(app_id: &str) -> io::Result<()> {
    let mut s = match UnixStream::connect(SOCKET_PATH) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("forum-dock: portcullisd not reachable at {SOCKET_PATH}: {e}");
            std::process::exit(1);
        }
    };

    match round_trip(&mut s, &Request::Hello { version: PROTO_VERSION })? {
        Response::Hello { version } if version == PROTO_VERSION => {}
        Response::ProtoMismatch { server_version } => {
            eprintln!("forum-dock: portcullisd speaks proto v{server_version}, dock speaks v{PROTO_VERSION}");
            std::process::exit(1);
        }
        other => {
            eprintln!("forum-dock: unexpected handshake reply: {other:?}");
            std::process::exit(1);
        }
    }

    write_request(&mut s, &Request::Launch { app_id: app_id.into(), bypass_policy: false })?;
    match read_response(&mut s)? {
        Response::ReadyForFds => {}
        Response::LaunchNeedsApproval { delta } => {
            eprintln!("forum-dock: '{app_id}' needs the user's approval:");
            for line in delta { eprintln!("  - {line}"); }
            std::process::exit(1);
        }
        Response::LaunchFailed { stage, message } => {
            eprintln!("forum-dock: launch of '{app_id}' failed at {stage}: {message}");
            std::process::exit(1);
        }
        other => {
            eprintln!("forum-dock: unexpected pre-launch reply: {other:?}");
            std::process::exit(1);
        }
    }

    // Hand over our stdio to the launched app (the daemon is blocked in recv_fds).
    let stdio = [
        io::stdin().as_raw_fd(),
        io::stdout().as_raw_fd(),
        io::stderr().as_raw_fd(),
    ];
    send_fds(&s, &stdio)?;

    match read_response(&mut s)? {
        Response::LaunchExit { code } => {
            println!("forum-dock: '{app_id}' exited (code {code:?})");
            Ok(())
        }
        Response::LaunchFailed { stage, message } => {
            eprintln!("forum-dock: '{app_id}' failed at {stage}: {message}");
            std::process::exit(1);
        }
        other => {
            eprintln!("forum-dock: unexpected post-launch reply: {other:?}");
            std::process::exit(1);
        }
    }
}
