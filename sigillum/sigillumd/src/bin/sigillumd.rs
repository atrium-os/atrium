//! `sigillumd`, Sigillum's broker (docs/spec/sigillum.md §7).
//!
//! Runs as `_sigillum` (65028) in a jail with no devices and no network.
//!
//! Environment:
//! - `SIGILLUM_WEB_BROKER`: `app_id publisher-hex`, the app that may run
//!   WebAuthn for any RP ID (Navigator's broker); unset, none may.
//! - `SIGILLUM_APP_REGISTRY` (default the launch registry).
//! - `SIGILLUM_ARCA_UID`, `SIGILLUM_OSTIARIUS_UID`: the uids allowed
//!   `VerifyUser`. Unset = nobody.
//! - `SIGILLUM_DEV_UID` (default 65029), `SIGILLUM_UI_UID` (default 65030):
//!   the only uids served on the driver and UI sockets.
//! - `SIGILLUM_SOCKET`, `SIGILLUM_DRIVER_SOCKET`, `SIGILLUM_UI_SOCKET`:
//!   override the socket paths (tests).
//!
//! It hardens itself before anything else (no core dumps, not traceable)
//! and never opens a device or a network socket.

use std::process::ExitCode;
use std::sync::Arc;

use sigillumd::broker::{Config, CEREMONY_MS};
use sigillumd::daemon::{Daemon, NoPasscode, Settings};
use sigillumd::resolve::{parse_web_broker, Resolver};

fn env_uid(name: &str) -> Option<u32> {
    std::env::var(name).ok().and_then(|v| v.parse().ok())
}

fn harden() -> std::io::Result<()> {
    let zero = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: plain syscalls on this process with valid arguments.
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &zero) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    #[cfg(target_os = "freebsd")]
    {
        let mut arg: libc::c_int = libc::PROC_TRACE_CTL_DISABLE;
        if unsafe { libc::procctl(libc::P_PID, 0, libc::PROC_TRACE_CTL, &mut arg as *mut _ as *mut libc::c_void) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    #[cfg(target_os = "linux")]
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn main() -> ExitCode {
    if let Err(e) = harden() {
        eprintln!("sigillumd: cannot harden process: {e}");
        return ExitCode::FAILURE;
    }
    let web_broker = std::env::var("SIGILLUM_WEB_BROKER").ok().and_then(|s| parse_web_broker(&s));
    if std::env::var("SIGILLUM_WEB_BROKER").is_ok() && web_broker.is_none() {
        eprintln!("sigillumd: SIGILLUM_WEB_BROKER is malformed; no web broker");
    }
    let resolver = Resolver {
        registry: std::env::var("SIGILLUM_APP_REGISTRY").unwrap_or_else(|_| portcullis_peer::DEFAULT_REGISTRY.into()).into(),
        arca_uid: env_uid("SIGILLUM_ARCA_UID"),
        ostiarius_uid: env_uid("SIGILLUM_OSTIARIUS_UID"),
    };
    let mut settings = Settings::standard(Config { web_broker, ceremony_ms: CEREMONY_MS });
    if let Some(u) = env_uid("SIGILLUM_DEV_UID") {
        settings.dev_uid = u;
    }
    if let Some(u) = env_uid("SIGILLUM_UI_UID") {
        settings.ui_uid = u;
    }
    for (var, slot) in [
        ("SIGILLUM_SOCKET", &mut settings.client_socket),
        ("SIGILLUM_DRIVER_SOCKET", &mut settings.driver_socket),
        ("SIGILLUM_UI_SOCKET", &mut settings.ui_socket),
    ] {
        if let Ok(p) = std::env::var(var) {
            *slot = p.into();
        }
    }
    let daemon = match Daemon::bind(settings.clone(), Arc::new(resolver), Box::new(NoPasscode)) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("sigillumd: bind: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "sigillumd: clients {}, drivers {} (uid {}), ui {} (uid {})",
        settings.client_socket.display(),
        settings.driver_socket.display(),
        settings.dev_uid,
        settings.ui_socket.display(),
        settings.ui_uid
    );
    daemon.run();
    ExitCode::SUCCESS
}
