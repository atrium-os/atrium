//! limend, the embed broker (docs/spec/limen.md).
//!
//! Environment:
//! - `LIMEN_SOCKET` (default /atrium/sockets/limen/limend.sock, linked from
//!   /var/run/atrium/limend.sock)
//! - `LIMEN_APP_REGISTRY` (default the launch registry)
//! - `LIMEN_DEFAULTS`: `role=app-id,...`, the provider of each user role
//!   until Curia holds the user's choice (limen.md §3.2); a role it doesn't
//!   name goes to an installed app whose `[roles] implements` has it
//!   ([`limen::manifests::Installed::provider`]).
//!
//! A parent may embed only the roles its installed manifest's
//! `[capabilities] embeds` names (a platform bundle without `embeds` keeps
//! every role, as before).
//!
//! - `LIMEN_ARCA_UID`: the uid of arca-ui, the `credential` provider
//!   (default 65021, `_arca-ui`); only it may provide that role.
//! - `LIMEN_SHARE_UID`: the uid of the share sheet, the `share` provider
//!   (default 65045, `_share`); only it may provide that role, and it may
//!   choose only installed `share-target`s for the parent's person.
//! - `LIMEN_PRELUM_UID`: the uid of prelum-sheet, the `print` provider
//!   (default 65046, `_prelum-sheet`); only it may provide that role.
//! - `LIMEN_USER`: limend's user name as worker rules name it (default `_limen`).
//! - `LIMEN_CHORAGUS`: choragusd's attribution socket, for embedded players'
//!   audio tickets (default /atrium/sockets/audio-attribution/audio-attribution.sock).
//! - `LIMEN_ISLAND_STANDBY=1`: keep one warmed Navigator engine per Navigator
//!   uid for its next island (navigator-islands.md D7). Off by default until
//!   the installed engine knows the standby phase.
//!
//! User-role children start through portcullisd's `ExecInstance`
//! ([`limen::launch`]). Slots are bound with frescod's slot op on its
//! activation socket (`LIMEN_FRESCOD_ACTIVATION`, default
//! /atrium/sockets/fresco-activation/fresco-activation.sock; [`limen::fresco`]).
//!
//! At boot limend runs jailed as a service (etc/services.d/41-atrium-limend.toml):
//! the supervisor binds its socket and hands it over (`atrium_listen`), so
//! apps can connect before limend is up.

use std::os::fd::AsRawFd;

use limen::daemon::Shared;

fn main() -> std::io::Result<()> {
    let set = std::env::var("LIMEN_SOCKET").ok().filter(|p| !p.is_empty());
    let default = set.is_none();
    let path = set.unwrap_or_else(|| limen::client::DEFAULT_SOCKET.into());
    let registry = std::env::var("LIMEN_APP_REGISTRY").unwrap_or_else(|_| portcullis_peer::DEFAULT_REGISTRY.into());
    let defaults: Vec<(String, String)> = std::env::var("LIMEN_DEFAULTS").unwrap_or_default()
        .split(',')
        .filter_map(|kv| kv.split_once('=').map(|(k, v)| (k.trim().to_string(), v.trim().to_string())))
        .collect();
    let reg_path = registry.clone();
    let persona_of: limen::daemon::PersonaOf = Box::new(move |uid: u32| {
        let reg = portcullis_peer::AppRegistry::load(&reg_path).ok()?;
        reg.persona(uid).flatten().map(String::from)
    });
    let sheet_registry = registry.clone();
    let smime_registry = registry.clone();
    let resolve = Box::new(move |uid: u32| {
        let reg = portcullis_peer::AppRegistry::load(&registry).ok()?;
        reg.resolve(uid).map(|(u, a)| (u.to_string(), a.to_string()))
    });
    // A role's provider: LIMEN_DEFAULTS, else from installed manifests'
    // [roles] implements (app-composition.md §5.1).
    let installed = limen::manifests::Installed::default();
    let inst = installed.clone();
    let defaults = Box::new(move |user: &str, role: &str| {
        defaults.iter().find(|(r, _)| r == role).map(|(_, a)| a.clone())
            .or_else(|| inst.provider(user, role))
    });
    let arca_uid: u32 = std::env::var("LIMEN_ARCA_UID").ok().and_then(|v| v.parse().ok()).unwrap_or(65021);
    let shared = Shared::new(Box::new(limen::fresco::Frescod::new(std::env::var("LIMEN_FRESCOD_ACTIVATION").unwrap_or_else(|_| limen::fresco::DEFAULT_SOCKET.into()))), Box::new(limen::launch::Portcullis::new(std::env::var("LIMEN_USER").unwrap_or_else(|_| "_limen".into()))), resolve, defaults);
    // arca-ui, the credential provider, is a system service (`_arca-ui`),
    // not a registry app.
    shared.set_system_uid(limen::catalogue::ARCA, arca_uid);
    if let Ok(publisher) = std::env::var("ARCA_MAIL_PUBLISHER") {
        let publisher = publisher.trim().to_ascii_lowercase();
        shared.set_smime_mail(Box::new(move |uid| {
            let reg = portcullis_peer::AppRegistry::load(&smime_registry).ok()?;
            let (_, app) = reg.resolve(uid)?;
            let key = reg.publisher(uid)??;
            let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
            if app != "org.atrium.mail" || key == [0; 32] || hex != publisher {
                return None;
            }
            Some(reg.persona(uid)?.map(str::to_owned))
        }));
    }
    // The picker is told its parent's persona (personas.md §4.1).
    if let Ok(config) = std::env::var("ARCA_WEB_BROKER") {
        shared.set_mail_sheet_provider(Box::new(move |uid| {
            let Ok(reg) = portcullis_peer::AppRegistry::load(&sheet_registry) else { return false; };
            let Some(Some(key)) = reg.publisher(uid) else { return false; };
            let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
            key != [0; 32] && config == format!("org.atrium.navigator {hex}")
        }));
    }
    shared.set_persona_lookup(persona_of);
    // The share sheet is a system service (`_share`), not a registry app;
    // it may choose only installed share targets for the parent's person.
    let share_uid: u32 = std::env::var("LIMEN_SHARE_UID").ok().and_then(|v| v.parse().ok()).unwrap_or(65045);
    shared.set_system_uid(limen::catalogue::SHARE, share_uid);
    let targets = installed.clone();
    shared.set_target_check(Box::new(move |person: &str, app: &str| targets.may_target(person, app)));
    let panes = installed.clone();
    shared.set_pane_check(Box::new(move |person: &str, app: &str| panes.may_provide(person, app, "app-settings")));
    let plugins = installed.clone();
    shared.set_plugin_check(Box::new(move |person: &str, app: &str| plugins.may_provide(person, app, "editor-extension")));
    // So is prelum-sheet, the print sheet (`_prelum-sheet`, printing.md §5).
    let prelum_uid: u32 = std::env::var("LIMEN_PRELUM_UID").ok().and_then(|v| v.parse().ok()).unwrap_or(65046);
    shared.set_system_uid(limen::catalogue::PRELUM, prelum_uid);
    // Who may embed what: the parent's installed manifest (`embeds`).
    shared.set_embed_check(Box::new(move |app: &str, role: &str| installed.may_embed(app, role)));
    shared.set_attribution(Box::new(limen::audio::Choragus::new(
        std::env::var("LIMEN_CHORAGUS").unwrap_or_else(|_| limen::audio::DEFAULT_SOCKET.into()))));
    if let Some(d) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(d);
    }
    // The supervisor's socket if it bound one (the services.d manifest sets
    // its owner and mode), else bound here.
    let (listener, origin) = atrium_listen::listener(&path)?;
    if origin == atrium_listen::Origin::Bound {
        if default {
            let _ = std::fs::remove_file(limen::client::OLD_SOCKET);
            if let Err(e) = std::os::unix::fs::symlink(&path, limen::client::OLD_SOCKET) {
                eprintln!("limend: link {} -> {path}: {e}", limen::client::OLD_SOCKET);
            }
        }
        // Every app may connect: who it is comes from the connection
        // (getpeereid), never from the socket's mode.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))?;
    }
    eprintln!("limend: listening on {path}");
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
