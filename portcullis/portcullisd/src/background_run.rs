//! Running background apps (background-apps.md §3.3): `StartBackground` at
//! sign-in, a supervising thread per instance, and the stop at sign-out.
//!
//! Each run goes through the ordinary Launch path (`launch_instance`): the
//! same jail, uid, overlay and persona key group as when the person opens
//! the app. Before every start and restart the thread looks again at the
//! installed manifest and the person's policy; `portcullisd::background`
//! decides what each exit means.

use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use portcullis_policy::{decide, hash_manifest, Policy, SYSTEM_PATH};
use portcullis_toml::ResidentRestart;
use portcullisd::background::{Exit, Key, Next, Recheck, Registry, Run};
use portcullisd::manifest_trust;

use crate::launch::{self, LaunchError, LaunchOutcome};
use crate::Tenants;

fn registry() -> &'static Mutex<Registry> {
    static R: OnceLock<Mutex<Registry>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(Registry::default()))
}

fn reg() -> std::sync::MutexGuard<'static, Registry> {
    registry().lock().unwrap_or_else(|e| e.into_inner())
}

/// What the installed manifest and `user`'s policy say about `k` now. An
/// error anywhere (missing, unsigned, unparsable, unreadable policy) is
/// "not allowed".
pub fn recheck(shared: &Mutex<Tenants>, user: &str, k: &Key) -> Recheck {
    let deny = Recheck { allowed: false, restart_never: false };
    let Some((tree, text, m)) = launch::installed_texts().into_iter().find(|(_, _, m)| m.app.id == k.app_id) else { return deny };
    let Some(res) = m.resident() else { return deny };
    if manifest_trust::verify(&tree, &text).is_err() {
        return deny;
    }
    let hash = hash_manifest(text.as_bytes());
    let system = Policy::load_system(std::path::Path::new(SYSTEM_PATH));
    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(tp) = s.get_or_load(user) else { return deny };
    let allowed = tp.policy.background_allowed(&k.app_id, k.persona.as_deref(), &hash)
        && decide(&k.app_id, &m.capabilities, &hash, &tp.policy, system.as_ref()).delta.is_empty();
    Recheck { allowed, restart_never: res.restart == ResidentRestart::Never }
}

/// The person's allowed sign-in apps, every persona.
fn wanted(shared: &Mutex<Tenants>, user: &str) -> Vec<Key> {
    let mut out = Vec::new();
    let system = Policy::load_system(std::path::Path::new(SYSTEM_PATH));
    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(tp) = s.get_or_load(user) else { return out };
    for (tree, text, m) in launch::installed_texts() {
        if m.resident().map(|r| r.start) != Some(portcullis_toml::ResidentStart::SignIn) {
            continue;
        }
        let hash = hash_manifest(text.as_bytes());
        if !decide(&m.app.id, &m.capabilities, &hash, &tp.policy, system.as_ref()).delta.is_empty() {
            continue;
        }
        if manifest_trust::verify(&tree, &text).is_err() {
            continue;
        }
        for p in tp.policy.background_personas(&m.app.id, &hash) {
            let persona = (p != portcullis_policy::PERSONAL).then_some(p);
            out.push(Key { app_id: m.app.id.clone(), persona });
        }
    }
    out
}

/// `StartBackground { owner }`: start `owner`'s allowed sign-in apps that
/// aren't known this session. The caller has checked it is the session
/// launcher; here, that `owner` is a person with a home whose session is
/// the active one.
pub fn start(shared: &Arc<Mutex<Tenants>>, owner: &str) -> Result<usize, String> {
    if !portcullis_overlay::plain_segment(owner) || portcullis_overlay::home_of(owner).is_none() {
        return Err(format!("{owner:?} is not a person with a home"));
    }
    if !portcullis_peer::seat::is_active(owner) {
        return Err(format!("{owner} has no live session"));
    }
    let wanted = wanted(shared, owner);
    let start = reg().sign_in(owner, wanted);
    let mut n = 0;
    for (k, run) in start {
        if launch::running_jid_instance(&k.app_id, k.persona.as_deref()).is_some() {
            // Opened already: it isn't this session's to supervise.
            reg().forget(owner, &k);
            continue;
        }
        let (shared, owner) = (shared.clone(), owner.to_string());
        std::thread::spawn(move || { let _ = supervise(&shared, &owner, &k, run, None); });
        n += 1;
    }
    Ok(n)
}

/// `user` opened `app_id` in `persona` from the dock (or a CLI). When it is
/// a background app they allowed there, in a live session, this run is
/// supervised: the launch's own thread restarts it.
pub fn supervise_opened(shared: &Mutex<Tenants>, user: &str, app_id: &str, persona: Option<&str>) -> Option<(Key, Run)> {
    let k = Key { app_id: app_id.into(), persona: persona.map(String::from) };
    if !reg().live(user) || !recheck(shared, user, &k).allowed {
        return None;
    }
    let run = reg().opened(user, &k)?;
    Some((k, run))
}

/// Run `k` for `user` until the registry says stop. The first run gets
/// `stdio` (the dock's), restarts the daemon's own. Returns the last run's
/// outcome.
pub fn supervise(shared: &Mutex<Tenants>, user: &str, k: &Key, run: Run, stdio: Option<[OwnedFd; 3]>) -> Result<LaunchOutcome, LaunchError> {
    let mut stdio = stdio;
    loop {
        eprintln!("portcullisd: background: starting {} in {} for {user}", k.app_id, k.persona.as_deref().unwrap_or("personal"));
        let r = launch::launch_instance(&k.app_id, k.persona.as_deref(), user, stdio.take());
        let exit = match &r {
            Ok(o) => o.exit_code.map(Exit::Code).unwrap_or(Exit::Signal),
            Err(e) => {
                eprintln!("portcullisd: background: {} for {user}: {}", k.app_id, e.message());
                Exit::LaunchFailed
            }
        };
        let check = recheck(shared, user, k);
        let mut next = reg().on_exit(user, k, run, exit, Instant::now(), check);
        while let Next::Pause(until) = next {
            eprintln!("portcullisd: background: {} for {user} crashed too often; pausing restarts", k.app_id);
            while Instant::now() < until && reg().live(user) {
                std::thread::sleep(Duration::from_millis(500));
            }
            let check = recheck(shared, user, k);
            next = reg().resume(user, k, run, check);
        }
        match next {
            Next::Restart => eprintln!("portcullisd: background: {} for {user} ended ({exit:?}); restarting", k.app_id),
            Next::Stop(why) => {
                eprintln!("portcullisd: background: {} for {user} stopped: {why}", k.app_id);
                return r;
            }
            Next::Pause(_) => unreachable!("pauses are waited out above"),
        }
    }
}

/// The bar's list for `user`: what runs or is paused, named from the
/// installed manifest.
pub fn list(shared: &Mutex<Tenants>, user: &str) -> Vec<portcullis_ipc::BackgroundApp> {
    use portcullisd::background::State;
    let rows = reg().list(user);
    if rows.is_empty() {
        return Vec::new();
    }
    let installed = launch::installed_texts();
    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
    let policy = s.get_or_load(user).ok().map(|tp| &tp.policy);
    rows.into_iter().filter(|(_, st)| matches!(st, State::Running | State::Paused(_))).map(|(k, st)| {
        let m = installed.iter().find(|(_, _, m)| m.app.id == k.app_id);
        let name = m.map(|(_, _, m)| m.app.name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| k.app_id.clone());
        let at_sign_in = m.is_some_and(|(_, t, m)| {
            m.resident().is_some_and(|r| r.start == portcullis_toml::ResidentStart::SignIn)
                && policy.is_some_and(|p| p.background_allowed(&k.app_id, k.persona.as_deref(), &hash_manifest(t.as_bytes())))
        });
        let (connections, not_on_this_network) = crate::listen_run::bar_status(user, &k.app_id, k.persona.as_deref()).unwrap_or((None, false));
        portcullis_ipc::BackgroundApp { app_id: k.app_id, name, persona: k.persona, paused: matches!(st, State::Paused(_)), at_sign_in,
                                        connections, not_on_this_network }
    }).collect()
}

/// The person quit `k` from the bar: it is not started again this session,
/// and its processes are ended (its launch thread then tears the jail down).
pub fn quit(user: &str, k: &Key) -> Result<(), String> {
    if !reg().quit(user, k) {
        return Err(format!("{} is not running in the background", k.app_id));
    }
    if let Some(uid) = launch::instance_uid(user, &k.app_id, k.persona.as_deref()) {
        launch::end_processes(&[uid]);
    }
    Ok(())
}

/// `user` is signing out: nothing of theirs restarts from now on. Called
/// before their processes are killed.
pub fn stopping(user: &str) {
    reg().stopping(user);
}

/// `user`'s sign-out is done.
pub fn ended(user: &str) {
    reg().ended(user);
}
