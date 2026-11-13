//! limend for development: limend's own code with a launcher that starts
//! each provider as an ordinary process instead of in a portcullisd jail.
//!
//! ★ UNCONFINED. For the VM runs of a role before its provider has a
//! signed bundle and a worker rule (navigator-ui.md §14 step 2 with the
//! doc-viewer). It is an example, so no package ships it.
//!
//!   LIMEN_SOCKET=… LIMEN_FRESCOD_ACTIVATION=… \
//!   LIMEN_DEFAULTS=doc-viewer=org.atrium.navigator.page \
//!   LIMEN_DEV_APPS=org.atrium.navigator.page=/opt/x/navigator-page \
//!   LIMEN_DEV_APP=org.atrium.navigator limend-dev
//!
//! Every connection is `LIMEN_DEV_APP` (the parent's app id); each child
//! runs as `LIMEN_DEV_UID` (limend-dev as root; frescod gives embed
//! connections only to uids that may not bind slots) with its channel on
//! stdin and limend's environment.

use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{Child, Command, Stdio};

use limen::daemon::{Launcher, Shared};
use limen::proto::EmbedId;

fn pairs(var: &str) -> Vec<(String, String)> {
    std::env::var(var).unwrap_or_default()
        .split(',')
        .filter_map(|kv| kv.split_once('=').map(|(k, v)| (k.trim().to_string(), v.trim().to_string())))
        .collect()
}

struct Spawn {
    apps: Vec<(String, String)>,
    kids: HashMap<EmbedId, Child>,
    uid: Option<u32>,
}

impl Launcher for Spawn {
    fn launch(&mut self, app_id: &str, embed: EmbedId, channel: UnixStream) -> Result<u32, String> {
        let path = self.apps.iter().find(|(a, _)| a == app_id).map(|(_, p)| p.clone())
            .ok_or_else(|| format!("no LIMEN_DEV_APPS entry for {app_id}"))?;
        // frescod hands embed connections only to uids that are not
        // binders, so a child runs as `LIMEN_DEV_UID` (limend-dev then runs
        // as root), else as limend-dev's own uid.
        use std::os::unix::process::CommandExt;
        let mut cmd = Command::new(&path);
        cmd.stdin(Stdio::from(OwnedFd::from(channel))).stdout(Stdio::null());
        // SAFETY: getuid has no preconditions.
        let uid = self.uid.unwrap_or_else(|| unsafe { libc::getuid() });
        if let Some(u) = self.uid {
            cmd.uid(u).gid(u);
        }
        let child = cmd.spawn().map_err(|e| format!("{path}: {e}"))?;
        eprintln!("limend-dev: started {app_id} for embed {embed} (pid {})", child.id());
        self.kids.insert(embed, child);
        Ok(uid)
    }

    fn end(&mut self, embed: EmbedId) {
        if let Some(mut c) = self.kids.remove(&embed) {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn main() -> std::io::Result<()> {
    let path = std::env::var("LIMEN_SOCKET").unwrap_or_else(|_| limen::client::DEFAULT_SOCKET.into());
    let app = std::env::var("LIMEN_DEV_APP").unwrap_or_else(|_| "org.atrium.dev".into());
    let defaults = pairs("LIMEN_DEFAULTS");
    let resolve = Box::new(move |_uid: u32| Some(("dev".to_string(), app.clone())));
    let defaults = Box::new(move |_user: &str, role: &str| defaults.iter().find(|(r, _)| r == role).map(|(_, a)| a.clone()));
    let uid = std::env::var("LIMEN_DEV_UID").ok().and_then(|v| v.parse().ok());
    let launcher = Spawn { apps: pairs("LIMEN_DEV_APPS"), kids: HashMap::new(), uid };
    let fresco = limen::fresco::Frescod::new(std::env::var("LIMEN_FRESCOD_ACTIVATION").unwrap_or_else(|_| limen::fresco::DEFAULT_SOCKET.into()));
    let shared = Shared::new(Box::new(fresco), Box::new(launcher), resolve, defaults);
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))?;
    }
    eprintln!("limend-dev: UNCONFINED providers; listening on {path}");
    for s in listener.incoming() {
        let Ok(s) = s else { continue };
        let Ok((uid, _)) = portcullis_peer::uid_gid(s.as_raw_fd()) else { continue };
        let shared = shared.clone();
        std::thread::spawn(move || {
            if let Ok(c) = aqueduct::Connection::wrap(s) {
                let _ = limen::daemon::serve(c, uid, shared);
            }
        });
    }
    Ok(())
}
