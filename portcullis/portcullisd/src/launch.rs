//! Per-app jail lifecycle, daemon-side. Mirrors what the CLI used
//! to do directly in cmd_launch — moved here so the daemon owns
//! every privileged operation and the CLI becomes a thin client.
//!
//! Two-phase launch (Phase 4.5):
//!   1. If `<overlay>/.atrium-firstrun-done` is absent and the app
//!      manifest has a `[setup]` block, build a *setup jail* with
//!      merged caps (runtime + setup overrides), run setup.command,
//!      write the sentinel on success, then continue.
//!   2. Build the runtime jail with plain `[capabilities]` and run
//!      the app.
//!
//! Both phases share the same overlay mount, so anything the setup
//! script writes to /usr/local, /etc, /var, etc. lands in the
//! overlay and is visible to the runtime app — that's the whole
//! point of separating the two phases (e.g., `pkg install` during
//! setup with `network = "full"`, then runtime sees the installed
//! files with `network = "none"`).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

use portcullis_jail::{build, jail_name_from_app_id, BuildOpts, JailConfig};
use portcullis_toml::{merge_capabilities, Manifest};

const APPS_DIR:     &str = "/var/lib/atrium/apps";
pub const JAILS_DIR:    &str = "/var/lib/atrium/jails";
const FILES_APP: &str = "org.atrium.scrinium";
const SENTINEL:     &str = ".atrium-firstrun-done";

#[derive(Debug)]
pub enum LaunchError {
    /// `(stage, message)` — `stage` is "manifest", "build", "mount",
    /// "jail-c", "setup", "teardown", etc., useful for the client to
    /// show where in the lifecycle things broke.
    Failed(&'static str, String),
}

impl LaunchError {
    pub fn stage(&self) -> &'static str {
        match self { LaunchError::Failed(s, _) => s }
    }
    pub fn message(&self) -> String {
        match self { LaunchError::Failed(_, m) => m.clone() }
    }
}

pub struct LaunchOutcome {
    /// The entry's exit code; `None` if a signal ended it.
    pub exit_code: Option<i32>,
}

// Files has one jail across all personas. Its launch thread owns both the
// restart and teardown, so a second launch cannot mount over a dying jail.
#[derive(Default)]
struct FilesRun {
    owner: String,
    refresh: bool,
    logging_out: bool,
}
static FILES_RUN: Mutex<Option<FilesRun>> = Mutex::new(None);

pub fn refresh_files_groups(owner: &str) {
    if let Some(run) = FILES_RUN.lock().unwrap().as_mut() {
        if run.owner == owner && !run.logging_out {
            run.refresh = true;
        }
    }
}

pub fn stop_files_for_logout(owner: &str) {
    if let Some(run) = FILES_RUN.lock().unwrap().as_mut() {
        if run.owner == owner {
            run.logging_out = true;
        }
    }
}

type BundleIdentity = (String, Option<[u8; 32]>);

/// Launch `(user, app_id)`'s instance in `persona` (personas.md §3; `None`:
/// Personal). A named persona's instance is its own jail
/// (`<id>@<persona>` root, `<name>__p_<persona>` jail), its own uid (registry
/// line `persona=<name>`), its own overlay (`persona-apps/<persona>/<id>`) and its own
/// synthetic host identity, so the two instances share nothing an app could
/// use to link them. The caller has checked the persona is the person's.
pub fn launch_instance(
    app_id:  &str,
    persona: Option<&str>,
    user:    &str,
    stdio:   Option<[OwnedFd; 3]>,
) -> Result<LaunchOutcome, LaunchError> {
    if app_id != FILES_APP || persona.is_some() {
        return launch_inner(app_id, persona, user, stdio, None, None, None);
    }
    {
        let mut active = FILES_RUN.lock().unwrap();
        if active.is_some() || running_jid_instance(app_id, None).is_some() {
            return Err(LaunchError::Failed("jail-c", "Files is already running or changing jails".into()));
        }
        *active = Some(FilesRun { owner: user.into(), ..FilesRun::default() });
    }

    let mut bundle = None;
    loop {
        // Keep the original launch's stdio available for every replacement.
        let copy_stdio = || -> std::io::Result<Option<[OwnedFd; 3]>> {
            stdio.as_ref().map(|[input, output, error]| {
                Ok([input.try_clone()?, output.try_clone()?, error.try_clone()?])
            }).transpose()
        };
        let result = copy_stdio()
            .map_err(|e| LaunchError::Failed("fdpass", e.to_string()))
            .and_then(|fds| launch_inner(app_id, persona, user, fds, None, Some(&mut bundle), None));
        let mut active = FILES_RUN.lock().unwrap();
        let run = active.as_mut().unwrap();
        if result.is_err() || run.logging_out || !std::mem::take(&mut run.refresh) {
            *active = None;
            return result;
        }
        eprintln!("portcullisd: restarting Files for {user} with the current persona folders");
    }
}

/// Run dev app `app_id` under the debugger (dev-tools.md §7.2,
/// dev-tools-plan.md M10): `lldb-server gdbserver` is its parent in its own
/// jail, speaking over `conn`, a connection a debugger in the workspace
/// made. Only `dev.` apps (D9): an installed app is refused.
pub fn launch_debug(app_id: &str, user: &str, conn: OwnedFd) -> Result<LaunchOutcome, LaunchError> {
    if !app_id.starts_with("dev.") {
        return Err(LaunchError::Failed("debug", format!("{app_id} is not a dev build: installed apps are never debugged")));
    }
    launch_inner(app_id, None, user, None, Some(conn), None, None)
}

/// What a debug run mounts at [`DEBUG_IN_JAIL`]: lldb-server and sh from
/// the base (copied, so nothing else of /usr/bin goes in), and the script
/// that starts lldb-server on the debugger's connection. The base's /lib
/// and /usr/lib go in beside them, read-only, for their libraries, which
/// the loader is told of with `-o LIBRARY_PATH=`: it isn't put in the
/// environment, so the app lldb-server starts loads its own as ever.
const DEBUG_TREE: &str = "/var/lib/atrium/debug";
const DEBUG_IN_JAIL: &str = "atrium/debug";
/* ★ The connection arrives as the entry's stdin; the script moves it to 3
 * and gives the program /dev/null. */
const DEBUG_LD: &str = "/libexec/ld-elf.so.1";
const DEBUG_LD_ARGS: [&str; 2] = ["-o", "LIBRARY_PATH=/atrium/debug/lib:/atrium/debug/usr-lib"];
const DEBUG_START: &str = "exec 3<&0 0</dev/null\nexec /libexec/ld-elf.so.1 -o LIBRARY_PATH=/atrium/debug/lib:/atrium/debug/usr-lib /atrium/debug/bin/lldb-server gdbserver --fd 3 -- \"$@\"\n";

fn debug_tree() -> Result<PathBuf, String> {
    let t = Path::new(DEBUG_TREE);
    let bin = t.join("bin");
    fs::create_dir_all(&bin).map_err(|e| format!("{}: {e}", bin.display()))?;
    let _ = fs::remove_file(bin.join("env"));
    for (from, to) in [("/usr/bin/lldb-server", "lldb-server"), ("/bin/sh", "sh")] {
        let dst = bin.join(to);
        let fresh = fs::metadata(from).and_then(|a| fs::metadata(&dst).map(|b| (a, b)))
            .is_ok_and(|(a, b)| a.len() == b.len() && b.modified().ok() >= a.modified().ok());
        if !fresh {
            fs::copy(from, &dst).map_err(|e| format!("{from}: {e} (is lldb in the base?)"))?;
            fs::set_permissions(&dst, fs::Permissions::from_mode(0o555)).map_err(|e| e.to_string())?;
        }
    }
    fs::write(t.join("start"), DEBUG_START).map_err(|e| format!("start: {e}"))?;
    for d in ["lib", "usr-lib"] {
        fs::create_dir_all(t.join(d)).map_err(|e| e.to_string())?;
    }
    Ok(t.to_path_buf())
}

use portcullis_ipc::PendingWorkbook;

// Broker-owned marker. The document jail never sees the marker or its siblings.
fn write_workbook_marker(path: &Path, name: &str) -> std::io::Result<PendingWorkbook> {
    use std::io::Read;
    let mut id = [0; 16];
    fs::File::open("/dev/urandom")?.read_exact(&mut id)?;
    let book = PendingWorkbook { id, starting: false, name: name.chars().filter(|c| !c.is_control()).take(255).collect() };
    store_workbook_marker(path, &book, false)?;
    Ok(book)
}

#[derive(serde::Deserialize, serde::Serialize)]
struct WorkbookMarker {
    #[serde(flatten)]
    book: PendingWorkbook,
    #[serde(default)]
    checked_detached: bool,
}

fn store_workbook_marker(path: &Path, book: &PendingWorkbook, checked_detached: bool) -> std::io::Result<()> {
    use std::io::Write;
    let temp = path.with_extension("pending.tmp");
    let mut file = fs::File::create(&temp)?;
    file.write_all(&serde_json::to_vec(&WorkbookMarker { book: book.clone(), checked_detached })?)?;
    file.sync_all()?;
    fs::rename(temp, path)?;
    fs::File::open(path.parent().unwrap())?.sync_all()
}

fn pending_workbook(path: &Path, index: usize) -> std::io::Result<PendingWorkbook> {
    let bytes = fs::read(path)?;
    if bytes.is_empty() {
        // A marker from before recovery choices had names.
        return write_workbook_marker(path, &format!("Recovered workbook {}", index + 1));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

struct WorkbookLaunch {
    blank: bool,
    recover: bool,
    publisher: Option<[u8; 32]>,
    pending: PathBuf,
    name: String,
    prepared: Option<RecoveryMount>,
    // Drop any prepared mount before making the slot available again.
    slot: WorkbookSlot,
    setup_ready: Option<mpsc::SyncSender<mpsc::Receiver<()>>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum WorkbookAction {
    New,
    Recover,
    Select([u8; 16]),
}

pub fn new_workbook(user: &str, persona: Option<&str>, action: WorkbookAction, publisher: Option<[u8; 32]>) -> std::io::Result<Vec<PendingWorkbook>> {
    launch_workbook(user, persona, "Untitled", None, action, Some(publisher))
}
pub fn open_workbook(
    user: &str, persona: Option<&str>, name: &str, mime: &str, write: bool, fd: OwnedFd, commit: Option<[u8; 16]>,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let (sender, receiver) = std::os::unix::net::UnixStream::pair()?;
    atrium_open::send_with_commit(&sender, name, mime, write, fd.as_raw_fd(), commit)?;
    drop(sender);
    launch_workbook(user, persona, name, Some(receiver.into()), WorkbookAction::New, None).map(|_| ())
}

fn workbook_marker_dir(user: &str, persona: Option<&str>, publisher: Option<[u8; 32]>) -> std::io::Result<PathBuf> {
    let home = portcullis_overlay::home_of(user).ok_or_else(|| std::io::Error::other("missing workbook owner's home"))?;
    let parent = match persona {
        None => portcullis_overlay::apps_root(&home).map_err(std::io::Error::other)?,
        Some(p) => portcullis_overlay::persona_apps_dir(&home, p),
    };
    let key = publisher.map(hex::encode).unwrap_or_else(|| "unsigned".into());
    let dir = parent.join(format!(".sheets-recovery-{key}"));
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    if !parent.exists() {
        fs::DirBuilder::new().mode(0o700).create(&parent)?;
    }
    let parent_meta = fs::symlink_metadata(&parent)?;
    if !parent_meta.is_dir() || parent_meta.uid() != 0 || parent_meta.mode() & 0o022 != 0 {
        return Err(std::io::Error::other("invalid workbook recovery parent"));
    }
    match fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let meta = fs::symlink_metadata(&dir)?;
    if !meta.is_dir() || meta.uid() != 0 || meta.mode() & 0o077 != 0 {
        return Err(std::io::Error::other("invalid workbook recovery directory"));
    }
    Ok(dir)
}

const DOCUMENT_LIMIT: usize = 8;
enum SlotState {
    Free,
    Active,
    Delayed { pending: PathBuf, uid: u32, jail: String },
}

static ACTIVE_WORKBOOKS: Mutex<[SlotState; DOCUMENT_LIMIT]> = Mutex::new([const { SlotState::Free }; DOCUMENT_LIMIT]);

pub fn name_workbook_recovery(uid: u32, registry: &portcullis_peer::AppRegistry, id: [u8; 16], name: String) -> std::io::Result<()> {
    let Some((user, app)) = registry.resolve(uid) else {
        return Err(std::io::Error::other("unknown workbook identity"));
    };
    let Some(slot) = registry.document_slot(uid) else {
        return Err(std::io::Error::other("only a Sheets document can update its recovery name"));
    };
    if app != portcullis_policy::opens::SHEETS || usize::from(slot) >= DOCUMENT_LIMIT {
        return Err(std::io::Error::other("only a Sheets document can update its recovery name"));
    }
    let dir = workbook_marker_dir(user, registry.persona(uid).flatten(), registry.publisher(uid).flatten())?;
    let path = dir.join(format!("{slot}.pending"));
    let _guard = ACTIVE_WORKBOOKS.lock().unwrap();
    rename_workbook_marker(&path, id, name)
}

fn valid_workbook_name(name: String) -> std::io::Result<String> {
    if name.is_empty() || name.len() > 255 || name.contains('/') || name.chars().any(char::is_control) {
        return Err(std::io::Error::other("invalid workbook display name"));
    }
    Ok(name)
}

fn rename_workbook_marker(path: &Path, id: [u8; 16], name: String) -> std::io::Result<()> {
    let name = valid_workbook_name(name)?;
    let mut book: PendingWorkbook = serde_json::from_slice(&fs::read(path)?)?;
    if book.id != id {
        return Err(std::io::Error::other("this workbook recovery has been replaced"));
    }
    if book.name == name { return Ok(()); }
    book.name = name;
    store_workbook_marker(path, &book, false)
}

static SLOT_RELEASED: std::sync::Condvar = std::sync::Condvar::new();

struct WorkbookSlot(usize);
impl Drop for WorkbookSlot {
    fn drop(&mut self) {
        ACTIVE_WORKBOOKS.lock().unwrap()[self.0] = SlotState::Free;
        SLOT_RELEASED.notify_all();
    }
}

/// Reserve a document slot until its launch thread has torn down the jail.
fn launch_workbook(user: &str, persona: Option<&str>, name: &str, startup: Option<OwnedFd>, action: WorkbookAction, caller_key: Option<Option<[u8; 32]>>) -> std::io::Result<Vec<PendingWorkbook>> {
    use std::os::fd::AsFd;

    let recover = action != WorkbookAction::New;
    let blank = startup.is_none();
    let stdio = startup.map(|receiver| -> std::io::Result<_> {
        Ok([
            receiver,
            std::io::stdout().as_fd().try_clone_to_owned()?,
            std::io::stderr().as_fd().try_clone_to_owned()?,
        ])
    }).transpose()?;
    let app = portcullis_policy::opens::SHEETS;
    let tree = Path::new(APPS_DIR).join(app);
    let publisher = crate::manifest_trust::verify_publisher(&tree, &fs::read_to_string(tree.join("atrium.toml"))?).map_err(std::io::Error::other)?;
    if caller_key.is_some_and(|key| key != publisher) {
        return Err(std::io::Error::other("Sheets publisher changed; reopen the launcher"));
    }
    portcullisd::home::ensure_app_data(user, persona).map_err(std::io::Error::other)?;
    let dir = workbook_marker_dir(user, persona, publisher)?;
    let (slot, pending, recovery) = {
        let mut active = ACTIVE_WORKBOOKS.lock().unwrap();
        let registry = portcullis_peer::AppRegistry::load(portcullis_peer::DEFAULT_REGISTRY)?;
        let deadline = Instant::now() + Duration::from_secs(2);
        'reserve: loop {
            let mut finishing = false;
            let mut choices = Vec::new();
            for (index, used) in active.iter_mut().enumerate() {
                let pending = dir.join(format!("{index}.pending"));
                let occupied = pending.try_exists()?;
                if recover && !occupied { continue; }
                if let SlotState::Delayed { pending: owner_path, uid, jail } = used {
                    if recover && *owner_path == pending {
                        if recovery_process_running(jail, *uid) {
                            *used = SlotState::Active;
                        } else {
                            let mut book = pending_workbook(&pending, index)?;
                            book.starting = true;
                            match action {
                                WorkbookAction::Recover => choices.push((index, book, Ok(()))),
                                WorkbookAction::Select(id) if id == book.id => return Ok(vec![book]),
                                _ => {}
                            }
                            continue;
                        }
                    }
                }
                if !matches!(used, SlotState::Free) {
                    finishing |= recover;
                    continue;
                }
                let uid = registry.uid_for_document(user, app, publisher.as_ref(), persona, Some(index as u8));
                if let Some(uid) = uid {
                    let instance = format!("document-{uid}");
                    let name = portcullis_jail::jail_name_for_instance(app, Some(&instance));
                    if workbook_jail_active(&name, &instance, &dir, index, occupied)? {
                        continue;
                    }
                }
                let mut inspection = Ok(());
                if occupied {
                    let store = workbook_store(&dir, index);
                    if portcullis_mounts::under(&[&store.dir]).contains(&store.dir) {
                        let Some(uid) = uid else {
                            continue; // An attached volume needs a known jail root for teardown.
                        };
                        if !reclaim_abandoned_workbook(&store, index, &format!("document-{uid}"), &pending)? {
                            continue;
                        }
                    } else if let Err(e) = check_detached_workbook(&store, &pending, index) {
                        // An unreadable slot stays occupied. Other workbooks can still open.
                        eprintln!("portcullisd: checking detached workbook slot {index}: {e}");
                        inspection = Err(e);
                    }
                }
                if pending.try_exists()? != recover { continue; }
                if recover {
                    let book = pending_workbook(&pending, index)?;
                    let inspection = inspection.map_err(|e| std::io::Error::new(e.kind(),
                        format!("The recovery image for “{}” is unavailable.", book.name)));
                    if let WorkbookAction::Select(id) = action {
                        if book.id != id { continue; }
                        inspection?;
                        *used = SlotState::Active;
                        break 'reserve (WorkbookSlot(index), pending, Some(book));
                    } else {
                        choices.push((index, book, inspection));
                        continue;
                    }
                }
                *used = SlotState::Active;
                break 'reserve (WorkbookSlot(index), pending, None);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if !choices.is_empty() && finishing && !left.is_zero() {
                active = SLOT_RELEASED.wait_timeout(active, left).unwrap().0;
                continue;
            }
            if choices.len() > 1 || choices.iter().any(|(_, book, _)| book.starting) {
                return Ok(choices.into_iter().map(|(_, book, _)| book).collect());
            }
            if let Some((index, book, inspection)) = choices.pop() {
                inspection?;
                active[index] = SlotState::Active;
                break 'reserve (WorkbookSlot(index), dir.join(format!("{index}.pending")), Some(book));
            }
            // The window can vanish before jail teardown and disk detach finish.
            // The releasing thread wakes Recover after the volume is safe to reopen.
            if !finishing || left.is_zero() {
                return Err(std::io::Error::other(if matches!(action, WorkbookAction::Select(_)) {
                    "That recovery is no longer available. Choose Recover again."
                } else if recover {
                    "No closed workbook is waiting for recovery"
                } else { "All workbook slots are open or awaiting recovery" }));
            }
            active = SLOT_RELEASED.wait_timeout(active, left).unwrap().0;
        }
    };
    // The reservation survives the handoff to the launch thread. Mount once,
    // outside the slot lock, while an actual failure can still reach the picker.
    let name = recovery.as_ref().map_or(name, |book| &book.name).to_owned();
    let prepared = if recover {
        let store = workbook_store(&dir, slot.0);
        mount_existing_workbook(&store).map_err(|e| {
            eprintln!("portcullisd: could not mount selected workbook slot {}: {e}", slot.0);
            std::io::Error::new(e.kind(), format!("The recovery image for “{name}” is unavailable."))
        })?;
        Some(RecoveryMount { store, pending: pending.clone(), clean: false, id: [0; 16] })
    } else { None };
    let (setup_ready, setup_result) = recovery.map(|book| {
        let (sender, receiver) = mpsc::sync_channel(0);
        (sender, (receiver, book))
    }).unzip();
    let document = WorkbookLaunch { blank, recover, pending, publisher, name, prepared, slot, setup_ready };
    let (user, persona) = (user.to_owned(), persona.map(str::to_owned));
    std::thread::Builder::new().name("sheets-document".into()).spawn(move || {
        let result = launch_inner(portcullis_policy::opens::SHEETS, persona.as_deref(),
                                  &user, stdio, None, None, Some(document));
        if let Err(e) = result {
            eprintln!("portcullisd: Sheets workbook launch: {e:?}");
        }
    })?;
    if let Some((result, mut book)) = setup_result {
        let name = &book.name;
        // Dropping the receiver on timeout also cancels a worker still in setup.
        let deadline = Instant::now() + Duration::from_secs(2);
        let started = result.recv_timeout(deadline.saturating_duration_since(Instant::now())).map_err(|_| std::io::Error::other(
            format!("Could not prepare a recovery window for “{name}”. Try Recover again.")))?;
        match started.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(()) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err(std::io::Error::other(
                format!("Could not start Sheets for “{name}”. Try Recover again."))),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                book.starting = true;
                return Ok(vec![book]);
            }
        }
    }
    Ok(Vec::new())
}

fn launch_inner(
    app_id:  &str,
    persona: Option<&str>,
    user:    &str,
    stdio:   Option<[OwnedFd; 3]>,
    debug:   Option<OwnedFd>,
    files_bundle: Option<&mut Option<BundleIdentity>>,
    mut document: Option<WorkbookLaunch>,
) -> Result<LaunchOutcome, LaunchError> {
    let mut stdio = stdio;
    if persona.is_some_and(|p| !portcullis_peer::valid_persona(p)) {
        return Err(LaunchError::Failed("persona", "invalid persona name".into()));
    }
    let tree = PathBuf::from(APPS_DIR).join(app_id);
    if !tree.exists() {
        return Err(LaunchError::Failed("manifest",
            format!("app id {app_id:?} not installed at {}", tree.display())));
    }

    let manifest_path = tree.join("atrium.toml");
    let text = fs::read_to_string(&manifest_path)
        .map_err(|e| LaunchError::Failed("manifest",
                 format!("{}: {e}", manifest_path.display())))?;
    /* manifest TRUST gate — the SAME shared check as the Launch path, so a
     * session/stdio-launched app is signature-verified identically (any
     * user-app launch vector goes through one gate, not a per-path copy). */
    let publisher = crate::manifest_trust::verify_publisher(&tree, &text)
        .map_err(|m| LaunchError::Failed("signature", m))?;
    if document.as_ref().is_some_and(|d| d.publisher != publisher) {
        return Err(LaunchError::Failed("signature", "Sheets publisher changed".into()));
    }
    let manifest = Manifest::from_str(&text)
        .map_err(|e| LaunchError::Failed("manifest",
                 format!("parse error: {e}")))?;
    let calendar_window = if app_id == crate::calendar::APP {
        if manifest.app.id != app_id {
            return Err(LaunchError::Failed("signature", "Calendar bundle ID mismatch".into()));
        }
        Some(crate::calendar::reserve_window(publisher.as_ref(), persona)
            .map_err(|e| LaunchError::Failed("signature", e.to_string()))?)
    } else { None };
    let refresh_groups = files_bundle.is_some();
    if let Some(pinned) = files_bundle {
        // A changed manifest or publisher must go through a new launch
        // request and its normal policy review.
        match pinned {
            Some((original, key)) if *original != text || *key != publisher => {
                return Err(LaunchError::Failed("manifest", "Files changed while running; open it again".into()));
            }
            None => *pinned = Some((text, publisher)),
            _ => {}
        }
    }
    let report = portcullis_toml::validate(&manifest);
    if !report.is_ok() {
        let msg = report.errors.join("; ");
        return Err(LaunchError::Failed("manifest", msg));
    }

    if document.is_some() && (manifest.setup.is_some()
        || manifest.capabilities.network.as_ref().is_some_and(|net| net.mode() != portcullis_toml::NetworkCap::None)
        || manifest.capabilities.filesystem.is_some()
        || not_instanced_manifest(&manifest).is_some()) {
        return Err(LaunchError::Failed("document", "workbook jail must have no setup, network or broad filesystem grants".into()));
    }
    if persona.is_some() {
        if let Some(why) = not_instanced_manifest(&manifest) {
            return Err(LaunchError::Failed("persona", format!("{app_id} runs once for every persona: {why}")));
        }
    }

    let key = instance_key(app_id, persona);
    let (app_uid, run_as_user) = resolve_launch_uid(user, app_id, publisher.as_ref(), persona, document.as_ref().map(|doc| doc.slot.0 as u8))?;
    let instance = document.as_ref().map(|_| format!("document-{app_uid}"));
    let jail_path = PathBuf::from(JAILS_DIR).join(instance.as_ref().unwrap_or(&key));

    /* PRIVILEGE INVARIANT (portcullis.md §9.0): the app runs under a dedicated,
     * non-root, non-human per-app uid (50000+) — NEVER root, NEVER the connecting
     * human's uid. The connecting human (`user`) is recorded as the OWNER in the
     * launch registry; the process executes as the per-app uid that services
     * peer-cred back to (owner, app_id). $HOME is the app uid's own (nologin)
     * home, not the human's. */
    eprintln!("portcullisd: launching {app_id} as uid {app_uid} ({run_as_user}); owner={user} persona={}",
              persona.unwrap_or("personal"));
    let user_home = match std::ffi::CString::new(run_as_user.as_str()).ok().and_then(|cuser| {
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buf = vec![0u8 as libc::c_char; 4096];
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let r = unsafe {
            libc::getpwnam_r(cuser.as_ptr(), &mut pwd, buf.as_mut_ptr(),
                             buf.len(), &mut result)
        };
        if r != 0 || result.is_null() { None }
        else {
            let dir = unsafe { std::ffi::CStr::from_ptr(pwd.pw_dir) };
            Some(PathBuf::from(dir.to_string_lossy().into_owned()))
        }
    }) {
        Some(h) => h,
        None    => PathBuf::from("/nonexistent"),
    };
    /* ★ The app's synthetic host identity (portcullis.md §9.1c) — derived from
     * the BASE app id, so the setup phase below sees the same hostid/UUID the
     * app will: a licence registered during setup must still match at run. */
    let host_identity = portcullis_identity::for_app(&instance_key(&manifest.app.id, persona))
        .map_err(|e| LaunchError::Failed("identity", e.to_string()))?;
    /* jaild resolves the class again, and adds `develop` for a debug run. */
    let sysfil = app_sysfil(&manifest)?;
    let mut app_run = AppRun {
        owner: user.to_string(), app_id: app_id.to_string(), debug: debug.is_some(),
        uid: app_uid, user: run_as_user.clone(), home: user_home.clone(), args: Vec::new(),
        syscall_class: portcullis_jail::sysfil_class(&manifest.capabilities),
        app_data: None,
    };
    let debug_tree = match &debug {
        Some(_) => Some(debug_tree().map_err(|e| LaunchError::Failed("debug", e))?),
        None => None,
    };
    let opts = BuildOpts {
        root_path:    jail_path.clone(),
        host_sockets: PathBuf::from("/atrium/sockets"),
        user_home:    user_home.clone(),
        user_name:    run_as_user.clone(),
        devfs_ruleset: portcullis_jail::APP_DEVFS_RULESET,
        /* Named launches stay single-instance: this path owns
         * /var/lib/atrium/jails/<id> and one persistent overlay, so a second
         * concurrent jail here would share both. Concurrency belongs to the
         * one-shot piped path, which gets its own root per instance. */
        instance: instance.clone().or_else(|| persona.map(|p| format!("p-{p}"))),
        // A workbook jail ends with its document, even if its launch thread is gone.
        persist: document.is_none(),
        host_identity,
        sysfil,
    };

    /* Mount overlay once; both setup and runtime jails see the same
     * union, so setup writes to /usr/local etc. are visible to the
     * runtime app. */

    /* STEP 0 (portcullis.md §4.2): bring up the app's own overlay VOLUME, so
     * the jail's statfs reports its own quota instead of the pool. That is
     * what closes channel 1 of the dedup existence oracle, and it is the
     * reason the overlay is a volume rather than a directory: quota-scoped
     * statfs is per-mount and cannot be per-path.
     *
     * Stays mounted across launches — it holds the app's persistent state —
     * until `portcullis remove`, or the person's logout (E6: its image is in
     * their key group, which locks then). */
    // A workbook gets a fresh upper layer on every launch, even when its
    // bounded UID slot is reused later. The person's file stays outside it.
    let upper = instance.as_ref().map(|id| PathBuf::from(JAILS_DIR).join(format!("{id}-upper")));
    let _document_mounts = upper.as_ref().map(|upper| DocumentMounts { root: &jail_path, upper });
    let overlay_dir = if let Some(upper) = upper.as_ref() {
        let name = portcullis_jail::jail_name_for_instance(app_id, instance.as_deref());
        if !full_teardown(&jail_path, Some(&name)) {
            return Err(LaunchError::Failed("document", "previous workbook jail is still mounted".into()));
        }
        if !portcullis_mounts::converge(&[&upper], portcullis_mounts::Force::No).is_empty() {
            return Err(LaunchError::Failed("document", "previous workbook upper layer is still mounted".into()));
        }
        fs::create_dir_all(&upper).map_err(|e| LaunchError::Failed("document", e.to_string()))?;
        run("mount", &["-t", "tmpfs", "-o", "size=128m", "tmpfs", upper.to_str().unwrap()])
            .map_err(|e| LaunchError::Failed("document", e.to_string()))?;
        upper.clone()
    } else {
        let overlay_quota = manifest.resources.as_ref()
            .and_then(|r| r.storage.as_deref())
            .and_then(portcullis_overlay::parse_size)
            .unwrap_or(portcullis_overlay::DEFAULT_QUOTA_BYTES);
        // The person's app data in their key group (E6), before it mounts.
        if portcullis_overlay::home_of(user).is_some() {
            app_run.app_data = Some(portcullisd::home::ensure_app_data(user, persona)
                .map_err(|m| LaunchError::Failed("app data", m))?);
        }
        // The person's own overlay, in their home (homes.md §3).
        let place = match persona {
            None => portcullis_overlay::place(Some(user), app_id),
            Some(p) => portcullis_overlay::persona_place(user, app_id, p).ok_or_else(||
                LaunchError::Failed("overlay", format!("no home place for {user}'s {p} instance of {app_id}")))?,
        };
        let overlay_dir = place.dir.clone();
        let backing = portcullis_overlay::ensure_mounted(&place, app_id, overlay_quota)
            .map_err(|m| LaunchError::Failed("mount", m))?;
        if backing == portcullis_overlay::Backing::Directory {
            /* No volume, so the structural mitigation is not in place. Fall back
             * to the behavioural one: a per-directory dedup domain on `deferred`,
             * which costs a full write per duplicate but keeps the observable
             * free-space number content-independent. */
            fs::create_dir_all(&overlay_dir)
                .map_err(|e| LaunchError::Failed("mount",
                         format!("create {}: {e}", overlay_dir.display())))?;
            arm_overlay_dedup(&overlay_dir)?;
        }
        overlay_dir
    };
    /* homes.md §2: the overlay is the app uid's, 0700. Inside the jail it is
     * the union's root, so the app can make its own /var, /tmp and $HOME
     * there (the signed tree's folders stay root's); outside, nobody but
     * root and the app reads it, other apps of the same person included. */
    std::os::unix::fs::chown(&overlay_dir, Some(app_uid), Some(app_uid))
        .and_then(|_| fs::set_permissions(&overlay_dir, fs::Permissions::from_mode(0o700)))
        .map_err(|e| LaunchError::Failed("mount",
                 format!("own {}: {e}", overlay_dir.display())))?;
    fs::create_dir_all(&jail_path)
        .map_err(|e| LaunchError::Failed("mount",
                 format!("create {}: {e}", jail_path.display())))?;

    /* Stale-mount cleanup. Silent — common case is "nothing to
     * unmount" and the noise was clutter in the CLI version. */
    let _ = umount_silent(&jail_path);
    let _ = umount_silent(&jail_path);

    if let Err(e) = run("mount", &["-t", "nullfs", "-o", "ro",
                                    tree.to_str().unwrap(),
                                    jail_path.to_str().unwrap()]) {
        return Err(LaunchError::Failed("mount", format!("nullfs: {e}")));
    }
    if let Err(e) = run("mount", &["-t", "unionfs",
                                    overlay_dir.to_str().unwrap(),
                                    jail_path.to_str().unwrap()]) {
        let _ = umount(&jail_path);
        return Err(LaunchError::Failed("mount", format!("unionfs: {e}")));
    }

    /* jaild mounts the jail's devfs on /dev, which must
     * exist inside the root. App trees don't ship /dev/, so create
     * it now via the unionfs upper layer (writes land in overlay/dev). */
    if let Err(e) = fs::create_dir_all(jail_path.join("dev")) {
        let _ = umount(&jail_path);
        let _ = umount(&jail_path);
        return Err(LaunchError::Failed("mount",
            format!("mkdir jail/dev: {e}")));
    }

    /* The entry starts in its account's home (the request's cwd,
     * entered after the drop). App trees don't ship that dir, so
     * create it inside the jail (writes land in overlay).
     * Empty dir is fine — apps that need real $HOME contents
     * declare a `filesystem` capability that bind-mounts them.
     * Path varies by user: /home/<user>, /root, etc. */
    let home_in_jail = jail_path.join(
        user_home.strip_prefix("/").unwrap_or(&user_home));
    if let Err(e) = fs::create_dir_all(&home_in_jail) {
        let _ = umount(&jail_path);
        let _ = umount(&jail_path);
        return Err(LaunchError::Failed("mount",
            format!("mkdir {}: {e}", home_in_jail.display())));
    }
    // Made by root through the union; the app's own (homes.md §2).
    let _ = std::os::unix::fs::chown(&home_in_jail, Some(app_uid), Some(app_uid));

    /* Phase 4.5: first-run setup. Sentinel lives in the overlay so
     * it persists across launches; portcullis reinstall removes it
     * to force a re-run. */
    let sentinel_path = overlay_dir.join(SENTINEL);
    if !sentinel_path.exists() {
        if let Some(setup) = &manifest.setup {
            let setup_caps = match &setup.capabilities {
                Some(ovr) => merge_capabilities(&manifest.capabilities, ovr),
                None      => manifest.capabilities.clone(),
            };
            /* Synthesise a setup-time manifest with merged caps and
             * setup.command as the entry. Same opts → same root_path
             * → setup runs in the overlay we just mounted. */
            let mut setup_manifest = clone_manifest_with(
                &manifest, setup_caps, setup.command.clone());
            setup_manifest.app.id = format!("{}_setup", manifest.app.id);
            let setup_jc = build(&setup_manifest, &opts)
                .map_err(|e| {
                    full_teardown(&jail_path, None);
                    LaunchError::Failed("build",
                        format!("setup-jail build: {e}"))
                })?;
            ensure_mountpoints(&setup_jc)
                .map_err(|e| { full_teardown(&jail_path, None); e })?;
            let setup_run = AppRun {
                debug: false, syscall_class: portcullis_jail::sysfil_class(&setup_manifest.capabilities), ..app_run.clone()
            };
            /* The setup's output goes to the daemon's. */
            let setup_exit = run_one_jail(&setup_jc, &jail_path, None, &setup_run, None, None)
                .map_err(|e| { full_teardown(&jail_path, None); e })?;
            if setup_exit != Some(0) {
                full_teardown(&jail_path, None);
                return Err(LaunchError::Failed("setup",
                    format!("setup script exited {:?} (sentinel not written; \
                             next launch will retry)", setup_exit)));
            }
            /* Sentinel write goes directly to the overlay-on-host;
             * unionfs writes from inside the jail land there too,
             * but the daemon owning this is more honest. */
            if let Err(e) = fs::write(&sentinel_path, b"ok\n") {
                full_teardown(&jail_path, None);
                return Err(LaunchError::Failed("setup",
                    format!("write sentinel: {e}")));
            }
        } else {
            /* No [setup] block → still write sentinel so we don't
             * stat() the file every launch from now on. */
            let _ = fs::write(&sentinel_path, b"ok\n");
        }
    }

    /* Runtime phase. Build and run the actual app jail with plain
     * runtime capabilities. */
    let jc = build(&manifest, &opts)
        .map_err(|e| {
            full_teardown(&jail_path, None);
            LaunchError::Failed("build", e.to_string())
        })?;
    /* ★ The document channel (opening-files.md §6.2): an app that opens
     * types gets its own directory of the channel hub, and only its own,
     * at /atrium/sockets/opens. Made before the mount, so the source exists. */
    let mut jc = jc;
    /* A persona whose `vpn` names a profile (vpn.md §9.7): the instance's
     * network goes in that profile's route domain, whether or not the
     * tunnel is up. Down, the table is empty and the instance reaches
     * nothing (the kill switch, §5.3). No routing table, no launch: never
     * the host's table instead. */
    if jc.needs_routed_net.is_some() {
        let fib = portcullisd::route_domain::for_persona(user, persona)
            .map_err(|e| { full_teardown(&jail_path, None); LaunchError::Failed("network", e) })?;
        if let Some(f) = fib {
            eprintln!("portcullisd: {app_id} in {}: route domain fib {f}", persona.unwrap_or_default());
        }
        jc.route_fib = fib;
    }
    // The terminal gets its consoles the same way (homes.md §4).
    /* A named persona's instance has a channel of its own, keyed
     * `<id>@<persona>`: what reaches it comes from its own persona
     * (personas.md §3; crossing is P3, off by default). */
    if let Some((signer, _lease)) = &calendar_window {
        let mountpoint = crate::calendar::socket_mountpoint(&jail_path)
            .map_err(|e| { full_teardown(&jail_path, None); LaunchError::Failed("mount", e.to_string()) })?;
        let dir = crate::calendar::endpoint(user, persona, signer, app_uid)
            .map_err(|e| { full_teardown(&jail_path, None); LaunchError::Failed("fasti", e.to_string()) })?;
        jc.add_mount(&dir, &mountpoint, "nullfs", &["ro", "nosuid", "noexec"]);
        // Presentation only. Fasti authorizes commands from kernel credentials.
        app_run.args = vec!["--persona".into(), persona.unwrap_or("personal").into()];
    }
    let mut recovery_mount = None;
    if let Some(doc) = &mut document {
        let mount = RecoveryMount::attach(doc, app_uid, &jail_path, &mut jc)?;
        let recovery_id = u128::from_be_bytes(mount.id);
        recovery_mount = Some(mount);
        let option = if doc.recover { "--recover-document" } else if doc.blank { "--blank-document" } else { "--document" };
        app_run.args = vec![option.into(), recovery_id.to_string()];
    } else if calendar_window.is_some() || (app_id != portcullis_policy::opens::SHEETS && !manifest.opens.is_empty())
        || !manifest.links.is_empty() || manifest.capabilities.consoles == Some(true) {
        let dir = portcullisd::opens::Hub::global().listen(&portcullisd::opens::channel_key(app_id, persona))
            .map_err(|e| { full_teardown(&jail_path, None); LaunchError::Failed("opens", format!("document channel: {e}")) })?;
        jc.add_mount(&dir, &jail_path.join("atrium/sockets/opens"), "nullfs", &["rw"]);
    }
    /* ★ The person's own files (homes.md §2, scrinium.md §9 H1): only for an
     * app granted `home-files` (the file manager). Its own uid gets the
     * person's files group; the five folders go in at /atrium/home. */
    if manifest.capabilities.home_files == Some(true) {
        let person = portcullisd::home::ensure_person(user)
            .and_then(|(home, gid)| portcullisd::home::grant_group(user, &run_as_user).map(|_| (home, gid)));
        match person {
            Ok((home, gid)) => {
                for (src, dst) in portcullisd::home::mounts(&home, &jail_path) {
                    jc.add_mount(&src, &dst, "nullfs", &["rw"]);
                }
                /* The named personas' folders (personas.md §4.1): the file
                 * manager spans them, with each one's group; they go in at
                 * /atrium/personas/<persona>/. One persona failing leaves
                 * the others. */
                let named: Vec<String> = portcullis_policy::Policy::load(&portcullis_policy::Policy::user_path(user)).ok()
                    .filter(|p| p.check_personas().is_ok()).map(|p| p.personas.keys().cloned().collect()).unwrap_or_default();
                let mut any = false;
                for p in &named {
                    match portcullisd::home::ensure_persona(user, p).and_then(|_| portcullisd::home::grant_persona_group(user, p, &run_as_user)) {
                        Ok(()) => any = true,
                        Err(e) => eprintln!("portcullisd: {app_id}: {p}'s folders: {e}"),
                    }
                }
                if any {
                    jc.add_mount(&home.join(portcullisd::home::PERSONAS_DIR), &jail_path.join(portcullisd::home::PERSONAS_IN_JAIL), "nullfs", &["rw"]);
                }
                /* And the workspaces (homes.md §5): each one's `work`; their
                 * `.state` is root's alone. */
                match crate::workspace::root_dir(&home, gid) {
                    Ok(ws) => jc.add_mount(&ws, &jail_path.join(crate::workspace::IN_FILE_MANAGER), "nullfs", &["rw"]),
                    Err(e) => eprintln!("portcullisd: {app_id}: no workspaces place: {e}"),
                }
            }
            Err(e) => {
                full_teardown(&jail_path, None);
                return Err(LaunchError::Failed("home-files", e));
            }
        }
    }
    /* An IDE (`workspace-exec`, homes.md W6): /work, and the `work` of each
     * workspace the person picked in it. */
    if manifest.capabilities.workspace_exec == Some(true) {
        if let Err(e) = crate::ide::launch_mounts(user, app_id, &run_as_user, &jail_path, &mut jc) {
            full_teardown(&jail_path, None);
            return Err(LaunchError::Failed("workspace-exec", e));
        }
    }
    /* ★ A dev build runs from its bundle's own path (M10), mounted there in
     * its jail too: the kernel names what a jailed process maps by the
     * host's path (the nullfs root's lower files), and lldb needs the
     * program where the process says it was started. The workspace that
     * made it sees the bundle at the same path (workspace::show_bundle), so
     * a debugger there, or a core, finds the very files. */
    let mut entry_at = format!("/{}", manifest.entry());
    if portcullisd::manifest_trust::is_dev(app_id) && persona.is_none() {
        let at = Path::new(APPS_DIR).join(app_id);
        jc.add_mount(&tree, &jail_path.join(at.strip_prefix("/").unwrap_or(&at)), "nullfs", &["ro"]);
        entry_at = format!("{}/{}", at.display(), manifest.entry());
        jc.set("exec.start", portcullis_jail::Value::String(entry_at.clone()));
    }
    /* ★ Under the debugger (M10): lldb-server is the entry, the app its
     * child; debugging is allowed in this jail alone, as in a workspace. */
    if let (Some(conn), Some(t)) = (debug, &debug_tree) {
        let at = jail_path.join(DEBUG_IN_JAIL);
        jc.add_mount(t, &at, "nullfs", &["ro"]);
        jc.add_mount(Path::new("/lib"), &at.join("lib"), "nullfs", &["ro"]);
        jc.add_mount(Path::new("/usr/lib"), &at.join("usr-lib"), "nullfs", &["ro"]);
        jc.set("allow.unprivileged_proc_debug", portcullis_jail::Value::Bool(true));
        jc.set("exec.start", portcullis_jail::Value::String(DEBUG_LD.into()));
        let mut args: Vec<String> = DEBUG_LD_ARGS.iter().map(|a| a.to_string()).collect();
        args.extend([format!("/{DEBUG_IN_JAIL}/bin/sh"), format!("/{DEBUG_IN_JAIL}/start"), entry_at.clone()]);
        args.append(&mut app_run.args);
        app_run.args = args;
        let err = |e: std::io::Error| { full_teardown(&jail_path, None); LaunchError::Failed("debug", e.to_string()) };
        let out = std::os::fd::AsFd::as_fd(&std::io::stderr()).try_clone_to_owned().map_err(err)?;
        let out2 = out.try_clone().map_err(err)?;
        stdio = Some([conn, out, out2]);
        eprintln!("portcullisd: {app_id} starts under lldb-server");
    }
    if let Err(e) = ensure_mountpoints(&jc) {
        full_teardown(&jail_path, None);
        return Err(e);
    }
    let recovery_start = if let Some((doc, ready)) = document.as_mut().and_then(|doc| doc.setup_ready.take().map(|ready| (doc, ready))) {
        let (started, result) = mpsc::channel();
        ready.send(result).map_err(|_| LaunchError::Failed("document", "recovery setup timed out".into()))?;
        Some(RecoveryStart { uid: app_uid, started, slot: doc.slot.0, pending: doc.pending.clone() })
    } else { None };
    let outcome = run_one_jail(&jc, &jail_path, stdio, &app_run, refresh_groups.then_some(app_uid), recovery_start);

    let clean = full_teardown(&jail_path, Some(&jc.name));
    if refresh_groups && !clean {
        return Err(LaunchError::Failed("teardown", "Files' old mounts are still in use".into()));
    }

    if let Some(mount) = &mut recovery_mount {
        mount.clean = clean;
    }
    outcome.map(|exit_code| LaunchOutcome { exit_code })
}

struct RecoveryMount {
    id: [u8; 16],
    store: portcullis_overlay::Place,
    pending: PathBuf,
    clean: bool,
}

impl RecoveryMount {
    fn attach(doc: &mut WorkbookLaunch, app_uid: u32, jail_path: &Path, jc: &mut JailConfig) -> Result<Self, LaunchError> {
        let fail = |e: String| LaunchError::Failed("recovery", e);
        let mut mount = match doc.prepared.take() {
            Some(mount) => mount,
            None => {
                let store = workbook_store(doc.pending.parent().unwrap(), doc.slot.0);
                portcullis_overlay::mount_recovery(&store, portcullis_overlay::WORKBOOK_RECOVERY_BYTES).map_err(fail)?;
                Self { store, pending: doc.pending.clone(), clean: false, id: [0; 16] }
            }
        };
        // The root of the image belongs to the broker. Only this child
        // directory is ever writable or visible inside the workbook jail.
        let data = mount.store.dir.join("autosave");
        fs::create_dir_all(&data).map_err(|e| fail(e.to_string()))?;
        if !doc.recover && fs::read_dir(&data).map_err(|e| fail(e.to_string()))?.next().is_some() {
            fs::File::create(&doc.pending).map_err(|e| fail(e.to_string()))?;
            return Err(fail("slot contains a previous recovery".into()));
        }
        if !doc.recover {
            portcullis_overlay::resize_empty_recovery(&mount.store).map_err(fail)?;
        }
        // Across boots the registry may assign this slot a different UID.
        // The detached private volume has no external hard links. Walk it
        // physically so even app-created symlinks stay inside the operation.
        use std::os::unix::fs::MetadataExt;
        if doc.recover && fs::metadata(&data).map_err(|e| fail(e.to_string()))?.uid() != app_uid {
            run("chown", &["-RhxP", &format!("{app_uid}:{app_uid}"), data.to_str().unwrap()])
                .map_err(|e| fail(e.to_string()))?;
        }
        std::os::unix::fs::chown(&data, Some(app_uid), Some(app_uid))
            .and_then(|_| fs::set_permissions(&data, fs::Permissions::from_mode(0o700)))
            .map_err(|e| fail(e.to_string()))?;
        mount.id = {
            let _guard = ACTIVE_WORKBOOKS.lock().unwrap();
            let book = if doc.recover {
                pending_workbook(&doc.pending, doc.slot.0).and_then(|book| {
                    store_workbook_marker(&doc.pending, &book, false)?;
                    Ok(book)
                })
            } else {
                write_workbook_marker(&doc.pending, &doc.name)
            };
            book.map_err(|e| fail(e.to_string()))?.id
        };
        jc.add_mount(&data, &jail_path.join("atrium/recovery"), "nullfs", &["rw", "nosuid", "noexec"]);
        Ok(mount)
    }
}

/// Only after the jail and its mounts are gone. The image root is owned by
/// the broker. Everything below autosave is document-controlled.
fn closed_workbook_name(data: &Path) -> std::io::Result<String> {
    use std::io::{Error, Read};
    use std::os::unix::fs::OpenOptionsExt;
    let mut entries = fs::read_dir(data)?;
    let entry = entries.next().transpose()?.ok_or_else(|| Error::other("missing autosave"))?;
    if entries.next().is_some() || !entry.file_type()?.is_dir() {
        return Err(Error::other("expected one private workbook autosave directory"));
    }
    let name = entry.path().join("name");
    if !fs::symlink_metadata(&name)?.is_file() {
        return Err(Error::other("expected a regular workbook name file"));
    }
    let file = fs::OpenOptions::new().read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(name)?;
    let mut label = String::new();
    file.take(256).read_to_string(&mut label)?;
    Ok(label)
}

fn workbook_store(dir: &Path, index: usize) -> portcullis_overlay::Place {
    portcullis_overlay::Place {
        dir: dir.join(index.to_string()), img: dir.join(format!("{index}.img")), person: None,
    }
}

/// A previous broker may have left a jail after a clean close: an empty one
/// on this workbook's root, with nothing saved, is ended here.
fn workbook_jail_active(name: &str, instance: &str, dir: &Path, index: usize, occupied: bool) -> std::io::Result<bool> {
    let jail = Command::new("jls").args(["-j", name, "jid", "path"]).output()?;
    if !jail.status.success() { return Ok(false); }
    let row = String::from_utf8_lossy(&jail.stdout);
    let fields: Vec<_> = row.split_whitespace().collect();
    let [jid, path] = fields.as_slice() else { return Ok(true); };
    if !occupied || Path::new(path) != Path::new(JAILS_DIR).join(instance) {
        return Ok(true);
    }
    let store = workbook_store(dir, index);
    if !fs::read_dir(store.dir.join("autosave")).is_ok_and(|mut entries| entries.next().is_none())
        || !portcullis_mounts::under(&[&store.dir]).contains(&store.dir)
    {
        return Ok(true);
    }
    let jid = jid.parse::<u32>().map_err(std::io::Error::other)?.to_string();
    let processes = Command::new("pgrep").args(["-j", &jid, ".*"]).output()?;
    match processes.status.code() {
        Some(0) => return Ok(true),
        Some(1) => {}
        _ => return Err(std::io::Error::other("cannot verify empty workbook jail")),
    }
    portcullisd::jaild_jail::remove(name).map_err(std::io::Error::other)?;
    Ok(!portcullis_mounts::wait_jail_gone(name, portcullis_mounts::JAIL_GONE_TIMEOUT))
}

fn require_workbook_image(store: &portcullis_overlay::Place) -> std::io::Result<()> {
    if !store.img.try_exists()? && !store.img.with_extension("ufs").try_exists()? {
        return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "missing workbook recovery image"));
    }
    Ok(())
}

fn mount_existing_workbook(store: &portcullis_overlay::Place) -> std::io::Result<()> {
    require_workbook_image(store)?;
    portcullis_overlay::mount_recovery(store, portcullis_overlay::WORKBOOK_RECOVERY_BYTES).map_err(std::io::Error::other)
}

/// After reboot the old jail and mounts are gone. Occupancy-only markers
/// from older brokers also need their label checked.
fn check_detached_workbook(store: &portcullis_overlay::Place, pending: &Path, index: usize) -> std::io::Result<()> {
    let bytes = fs::read(pending)?;
    if !bytes.is_empty() && serde_json::from_slice::<WorkbookMarker>(&bytes)?.checked_detached {
        // The cached name remains useful when the image goes offline.
        return require_workbook_image(store);
    }
    mount_existing_workbook(store)?;
    // DirEntry retains the directory descriptor. Drop it before unmounting.
    let data = store.dir.join("autosave");
    let nonempty = fs::read_dir(&data)
        .and_then(|mut entries| entries.next().transpose().map(|entry| entry.is_some()));
    // An empty legacy marker has no prior identity or label. Keep its numbered
    // fallback. For named markers, read only the bounded name before detach.
    let name = (matches!(nonempty, Ok(true)) && !bytes.is_empty())
        .then(|| closed_workbook_name(&data).and_then(valid_workbook_name));
    portcullis_overlay::unmount_recovery(store).map_err(std::io::Error::other)?;
    if nonempty? {
        let mut book = pending_workbook(pending, index)?;
        match name {
            Some(Ok(name)) => book.name = name,
            Some(Err(e)) => {
                eprintln!("portcullisd: keeping previous workbook recovery label: {e}");
                return Ok(());
            }
            None => {}
        }
        store_workbook_marker(pending, &book, true)
    } else {
        remove_workbook_marker(pending)
    }
}

fn remove_workbook_marker(pending: &Path) -> std::io::Result<()> {
    fs::remove_file(pending)?;
    fs::File::open(pending.parent().unwrap())?.sync_all()
}

/// A broker restart loses the launch thread that would normally detach this
/// image. The caller holds the slot lock. A live or dying jail keeps the slot.
fn reclaim_abandoned_workbook(store: &portcullis_overlay::Place, index: usize, instance: &str, pending: &Path) -> std::io::Result<bool> {
    let root = Path::new(JAILS_DIR).join(instance);
    let upper = Path::new(JAILS_DIR).join(format!("{instance}-upper"));
    let jails = Command::new("jls").args(["-d", "path"]).output()?;
    if !jails.status.success() {
        return Err(std::io::Error::other("cannot verify closed workbook jail"));
    }
    if String::from_utf8_lossy(&jails.stdout).lines().any(|line| {
        let path = Path::new(line.trim());
        path.starts_with(&root) || path.starts_with(&upper)
    }) { return Ok(false); }
    let left = portcullis_mounts::converge(&[&root, &upper], portcullis_mounts::Force::No);
    if !left.is_empty() {
        return Err(std::io::Error::other("closed workbook still has busy mounts"));
    }
    let id = pending_workbook(pending, index)?.id;
    finish_closed_recovery(store, pending, id)?;
    let _ = fs::remove_dir(&root);
    let _ = fs::remove_dir(&upper);
    Ok(true)
}

/// Called with the slot locked and all jail mounts released.
fn finish_closed_recovery(store: &portcullis_overlay::Place, pending: &Path, id: [u8; 16]) -> std::io::Result<()> {
    let data = store.dir.join("autosave");
    let empty = fs::read_dir(&data).is_ok_and(|mut entries| entries.next().is_none());
    if !empty {
        let result = closed_workbook_name(&data).and_then(|name| rename_workbook_marker(pending, id, name));
        if let Err(e) = result {
            eprintln!("portcullisd: keeping previous workbook recovery label: {e}");
        }
    }
    portcullis_overlay::unmount_recovery(store).map_err(std::io::Error::other)?;
    if empty {
        remove_workbook_marker(pending)?;
    }
    Ok(())
}

impl Drop for RecoveryMount {
    fn drop(&mut self) {
        let result = if self.clean {
            let _guard = ACTIVE_WORKBOOKS.lock().unwrap();
            finish_closed_recovery(&self.store, &self.pending, self.id)
        } else {
            portcullis_overlay::unmount_recovery(&self.store).map_err(std::io::Error::other)
        };
        if let Err(e) = result {
            eprintln!("portcullisd: closing workbook recovery volume: {e}");
        }
    }
}

struct DocumentMounts<'a> {
    root: &'a Path,
    upper: &'a Path,
}

impl Drop for DocumentMounts<'_> {
    fn drop(&mut self) {
        if full_teardown(self.root, None) {
            let left = portcullis_mounts::converge(&[self.upper], portcullis_mounts::Force::No);
            portcullis_mounts::warn_survivors("portcullisd", &left);
            let _ = fs::remove_dir(self.root);
            let _ = fs::remove_dir(self.upper);
        }
    }
}

/// Clone a manifest with substituted capabilities + entry. Used to
/// build the setup-phase manifest from the runtime one without
/// disturbing the original.
fn clone_manifest_with(
    base:  &Manifest,
    caps:  portcullis_toml::Capabilities,
    entry: String,
) -> Manifest {
    Manifest {
        app: portcullis_toml::AppSection {
            id:          base.app.id.clone(),
            name:        base.app.name.clone(),
            version:     base.app.version.clone(),
            entry,
            sdk_version: base.app.sdk_version.clone(),
            description: base.app.description.clone(),
            icon:        base.app.icon.clone(),
            updates:     base.app.updates.clone(),
        },
        /* The setup phase runs a script entry, not the bundled app — bundle/arch
         * facts don't apply, so it carries no [bundle] section. */
        bundle:       None,
        capabilities: caps,
        setup:        None,           /* setup never recurses */
        resources:    None,
        supervision:  None,
        opens:        Vec::new(),
        links:        Vec::new(),
        /* The setup script is no app: no workers, roles or IPC of its own. */
        workers:      Vec::new(),
        roles:        None,
        ipc:          None,
    }
}

/// Create every capability mount's destination (the mountpoint) before jaild
/// makes the jail. jaild does not create mountpoints, and the read-only app tree can't
/// carry them, so without this each `mount +=` (fresco.sock, forum-ctl.sock,
/// clipboard.sock, a filesystem dir, …) fails and the jail never starts. We stat
/// the source to pick the mountpoint type: a directory source → a directory
/// mountpoint; anything else (a unix socket or file, e.g. the compositor socket)
/// → a regular-file mountpoint. Writes land in the unionfs overlay, leaving the
/// read-only app tree untouched. Mirrors the existing dev/ + home/ creation above.
pub fn ensure_mountpoints(jc: &JailConfig) -> Result<(), LaunchError> {
    portcullis_mounts::ensure_mountpoints(jc).map_err(|e| LaunchError::Failed("mount", e))
}

/// Who an app's entry runs as, and how (app-jails-via-jaild.md §4): what a
/// [`JailConfig`] cannot say and jaild checks.
#[derive(Clone)]
pub struct AppRun {
    /// The person the instance is for; the launch registry binds `uid` to
    /// them and `app_id`.
    pub owner:  String,
    pub app_id: String,
    /// Started under lldb-server (M10).
    pub debug:  bool,
    pub uid:    u32,
    /// The uid's account name, for its supplementary groups.
    pub user:   String,
    /// The account's home, which the entry starts in.
    pub home:   PathBuf,
    /// Arguments after the entry's name.
    pub args:   Vec<String>,
    /// The manifest's system call class; jaild resolves it again.
    pub syscall_class: String,
    /// What the kernel said about the app's data folder (mail.md §6.4a
    /// A2), passed on as `ATRIUM_APP_DATA`; `None` for a jail with no
    /// persistent app data (a workbook's tmpfs layer).
    pub app_data: Option<portcullisd::home::AppData>,
}

/// The entry's whole environment: the account's HOME, SHELL and USER, a
/// PATH, the caller's TERM, and ATRIUM_APP_DATA (`sealed` or `plain`).
fn clean_env(run: &AppRun) -> Vec<jaild::protocol::EnvPair> {
    let pair = |k: &str, v: String| jaild::protocol::EnvPair { key: k.into(), value: v };
    let shell = account_shell(&run.user).unwrap_or_else(|| "/bin/sh".into());
    let mut env = vec![
        pair("HOME", run.home.display().to_string()),
        pair("SHELL", shell),
        pair("USER", run.user.clone()),
        pair("PATH", "/sbin:/bin:/usr/sbin:/usr/bin:/usr/local/sbin:/usr/local/bin".into()),
    ];
    if let Ok(t) = std::env::var("TERM") {
        env.push(pair("TERM", t));
    }
    if let Some(d) = run.app_data {
        env.push(pair("ATRIUM_APP_DATA", d.env_value().into()));
    }
    env
}

fn account_shell(user: &str) -> Option<String> {
    let cuser = std::ffi::CString::new(user).ok()?;
    // SAFETY: getpwnam_r writes `pwd` and points its strings into `buf`,
    // which outlives the reads below.
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let r = unsafe { libc::getpwnam_r(cuser.as_ptr(), &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result) };
    if r != 0 || result.is_null() || pwd.pw_shell.is_null() {
        return None;
    }
    // SAFETY: a NUL-terminated string in `buf`, per getpwnam_r's success.
    Some(unsafe { std::ffi::CStr::from_ptr(pwd.pw_shell) }.to_string_lossy().into_owned())
}

/// The supplementary groups `user` is a member of past `gid` (the owner's
/// files groups, which home::grant_group adds). jaild refuses any that is
/// not one.
fn account_groups(user: &str, gid: u32) -> Result<Vec<u32>, String> {
    let cuser = std::ffi::CString::new(user).map_err(|_| format!("account name {user:?}"))?;
    let mut groups = vec![0 as libc::gid_t; jaild::protocol::MAX_EXEC_GROUPS + 2];
    let mut n = groups.len() as libc::c_int;
    // SAFETY: `groups` holds `n` entries; getgrouplist writes at most that
    // many and sets `n` to how many there are.
    let rc = unsafe { libc::getgrouplist(cuser.as_ptr(), gid as libc::gid_t, groups.as_mut_ptr(), &mut n) };
    if rc != 0 {
        return Err(format!("{user} is in more than {} groups", jaild::protocol::MAX_EXEC_GROUPS));
    }
    let mut out: Vec<u32> = groups[..n.max(0) as usize].iter().map(|&g| g as u32).filter(|&g| g != gid).collect();
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// Run `jc`'s entry in a jail jaild makes on `jail_path`, wait for it, and
/// drop the jail's record. The caller owns the root's mounts.
fn run_one_jail(
    jc:        &JailConfig,
    jail_path: &Path,
    stdio:     Option<[OwnedFd; 3]>,
    run:       &AppRun,
    refresh_uid: Option<u32>,
    recovery_start: Option<RecoveryStart>,
) -> Result<Option<i32>, LaunchError> {
    let fail = |stage: &'static str| move |e: String| LaunchError::Failed(stage, e);
    /* ★★ A `full` network is jaild's (network.md §0): it builds the epair
     * with the app's derived MAC from the request, and releases it with the
     * jail. The app's finer grants (network.md §0.1), hostnames resolved now. */
    let grants = match &jc.routed_net_spec {
        Some((id, spec)) => portcullis_oneshot::net_grants(spec, id).map_err(fail("network"))?,
        None => None,
    };
    if jc.needs_routed_net.is_some() {
        /* Names resolve through the host's resolvers, over NAT. Written into
         * the overlay, never the signed tree. */
        let etc = jail_path.join("etc");
        let _ = fs::create_dir_all(&etc);
        let _ = portcullis_oneshot::write_resolv_conf(&etc, jc.route_fib);
    }
    let gid = jaild::ffi::account(run.uid).map(|(_, g)| g)
        .ok_or_else(|| LaunchError::Failed("account", format!("uid {} has no account", run.uid)))?;
    let groups = account_groups(&run.user, gid).map_err(fail("groups"))?;
    let parts = portcullisd::jail_request::RequestParts {
        class: jaild::protocol::JailClass::App { owner: run.owner.clone(), app_id: run.app_id.clone(), debug: run.debug },
        run: Some(portcullisd::jail_request::RunAs {
            uid: run.uid, gid, groups, args: run.args.clone(), env: clean_env(run),
            stdio: true, cwd: Some(run.home.display().to_string()),
        }),
        grants,
        syscall_profile: Some(run.syscall_class.clone()),
    };
    let req = portcullisd::jail_request::to_create_request(jc, parts).map_err(fail("jail"))?;
    /* jaild mounts this jail's devfs; one left from a run that died would
     * be stacked under it. */
    let _ = umount_silent(&jail_path.join("dev"));
    let running = portcullisd::jaild_jail::create(req, stdio).map_err(fail("jail"))?
        .ok_or_else(|| LaunchError::Failed("jail", "jaild started no entry".into()))?;
    let status = match (recovery_start, refresh_uid) {
        (Some(start), _) => wait_for_recovery(&running, &jc.name, start),
        (_, Some(uid)) => wait_for_files(&running, &jc.name, uid),
        _ => running.wait(),
    };
    drop(running);
    /* The jail ended with its entry; this drops jaild's record and, for a
     * networked jail, its epair. */
    if let Err(e) = portcullisd::jaild_jail::remove(&jc.name) {
        eprintln!("portcullisd: {}: {e}", jc.name);
    }
    /* ★★ A dying jail pins its root; unmount only once it is gone. */
    portcullis_mounts::wait_jail_gone(&jc.name, portcullis_mounts::JAIL_GONE_TIMEOUT);
    unmount_dev_left(jail_path);
    /* A non-zero exit is the caller's to read: the setup phase fails on it,
     * the runtime phase passes it through. */
    status.map(portcullisd::jaild_jail::exit_code)
        .map_err(|e| LaunchError::Failed("jail", format!("wait for the entry: {e}")))
}

/// Create a jail that stays, with nothing running in it: a workspace
/// (homes.md §5) or a console, which lives until stopped. Its network, if
/// any, is jaild's as for an app, and goes with [`remove_persistent`].
pub fn create_persistent(jc: &JailConfig, jail_path: &Path, class: jaild::protocol::JailClass, syscall_class: &str) -> Result<(), LaunchError> {
    let fail = |stage: &'static str| move |e: String| LaunchError::Failed(stage, e);
    /* A record jaild still holds for this name is left from one that ended
     * without a stop (a crash, a reboot of portcullisd). */
    let _ = portcullisd::jaild_jail::remove(&jc.name);
    if jc.needs_routed_net.is_some() {
        let etc = jail_path.join("etc");
        let _ = fs::create_dir_all(&etc);
        let _ = portcullis_oneshot::write_resolv_conf(&etc, jc.route_fib);
    }
    let parts = portcullisd::jail_request::RequestParts {
        class, run: None, grants: None, syscall_profile: Some(syscall_class.into()),
    };
    let req = portcullisd::jail_request::to_create_request(jc, parts).map_err(fail("jail"))?;
    portcullisd::jaild_jail::create(req, None).map(|_| ()).map_err(fail("jail"))
}

/// End a jail made by [`create_persistent`] and take down its mounts.
pub fn remove_persistent(name: &str, jail_path: &Path) {
    if let Err(e) = portcullisd::jaild_jail::remove(name) {
        eprintln!("portcullisd: remove {name}: {e}");
    }
    portcullis_mounts::wait_jail_gone(name, portcullis_mounts::JAIL_GONE_TIMEOUT);
    unmount_dev_left(jail_path);
    full_teardown(jail_path, None);
}

const JAILD_POLICY: &str = "/etc/atrium/jaild.policy.toml";

/// The app jail's system call filter (docs/spec/jail-sysfil.md §5.2): the
/// manifest's class, resolved from jaild's policy, as jaild resolves it
/// again from the request's class name. `None` on a kernel
/// without sysfil, which would refuse the parameters, unless the policy
/// says a filter is required.
pub fn app_sysfil(manifest: &Manifest) -> Result<Option<jaild_policy::SysfilParams>, LaunchError> {
    let fail = |s: String| LaunchError::Failed("sysfil", s);
    let policy = jaild_policy::Policy::load(JAILD_POLICY)
        .map_err(|e| fail(format!("{JAILD_POLICY}: {e}")))?;
    if !jaild::ffi::sysfil_supported() {
        if policy.sysfil.required {
            return Err(fail("this kernel has no system call filter and policy.sysfil.required is set".into()));
        }
        return Ok(None);
    }
    let class = portcullis_jail::sysfil_class(&manifest.capabilities);
    // ★ `develop` (ptrace, ktrace, procctl) is portcullisd's for workspaces
    // alone (dev-tools-plan.md M0): an app's manifest naming it is refused.
    if class == DEVELOP_CLASS {
        return Err(fail(format!("{}: class {class:?} is for workspaces only", manifest.app.id)));
    }
    policy.sysfil.resolve(&class).map(Some)
        .map_err(|e| fail(format!("{}: class {class:?}: {e}", manifest.app.id)))
}

/// The class portcullisd gives workspace jails (dev-tools-plan.md M0).
pub const DEVELOP_CLASS: &str = "develop";
/// The class of a console's shell: the empty console's jail and the console
/// jail beside an app's (dev-tools-plan.md M0, P1).
pub const TERMINAL_CLASS: &str = "terminal";

/// The filter for a jail portcullisd makes itself (a workspace, a console),
/// by class name, not from a manifest: [`app_sysfil`] refuses `develop`.
pub fn sysfil_for_class(class: &str) -> Result<Option<jaild_policy::SysfilParams>, LaunchError> {
    let fail = |s: String| LaunchError::Failed("sysfil", s);
    let policy = jaild_policy::Policy::load(JAILD_POLICY)
        .map_err(|e| fail(format!("{JAILD_POLICY}: {e}")))?;
    if !jaild::ffi::sysfil_supported() {
        if policy.sysfil.required {
            return Err(fail("this kernel has no system call filter and policy.sysfil.required is set".into()));
        }
        return Ok(None);
    }
    policy.sysfil.resolve(class).map(Some).map_err(|e| fail(format!("class {class:?}: {e}")))
}

struct RecoveryStart {
    uid: u32,
    started: mpsc::Sender<()>,
    slot: usize,
    pending: PathBuf,
}

fn recovery_process_running(jail: &str, uid: u32) -> bool {
    Command::new("pgrep").args(["-j", jail, "-U", &uid.to_string(), "-x", "atrium-sheets"])
        .output().is_ok_and(|out| out.status.success())
}

/// Confirm Sheets is running in this document's jail before replying, then
/// let the launch thread supervise its exit.
fn wait_for_recovery(running: &portcullisd::jaild_jail::Running, jail: &str, start: RecoveryStart) -> std::io::Result<i32> {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Some(status) = running.wait_for(Some(Duration::from_millis(25)))? { return Ok(status); }
        if recovery_process_running(jail, start.uid) {
            let _ = start.started.send(());
            return running.wait();
        }
    }
    // Wake a repeated Recover already waiting on this reservation. After the
    // deadline, probe again only on demand while the original launch continues.
    ACTIVE_WORKBOOKS.lock().unwrap()[start.slot] = SlotState::Delayed {
        pending: start.pending, uid: start.uid, jail: jail.to_owned(),
    };
    SLOT_RELEASED.notify_all();
    let status = running.wait();
    ACTIVE_WORKBOOKS.lock().unwrap()[start.slot] = SlotState::Active;
    status
}

fn files_should_stop() -> bool {
    FILES_RUN.lock().unwrap().as_ref().is_some_and(|run| run.refresh || run.logging_out)
}

fn wait_for_files(running: &portcullisd::jaild_jail::Running, jail: &str, uid: u32) -> std::io::Result<i32> {
    let uid = uid.to_string();
    let signal = |sig: &str| {
        Command::new("pkill").args([sig, "-j", jail, "-U", &uid])
            .status().is_ok_and(|s| s.success())
    };
    let mut term_at: Option<Instant> = None;
    loop {
        if let Some(status) = running.wait_for(Some(Duration::from_millis(100)))? {
            return Ok(status);
        }
        if let Some(at) = term_at {
            if at.elapsed() >= Duration::from_secs(3) {
                if !signal("-KILL") && running.wait_for(Some(Duration::ZERO))?.is_none() {
                    eprintln!("portcullisd: could not signal Files in {jail} after TERM");
                }
                return running.wait();
            }
        } else if files_should_stop() && signal("-TERM") {
            // A refresh during startup stays pending until the app is
            // actually in the jail and can receive the signal.
            term_at = Some(Instant::now());
        }
    }
}

/// Final teardown: end the jail through jaild if it is named (a jail
/// already gone is fine), then unmount everything the launch mounted,
/// deepest first, convergently.
///
/// The mechanics moved to `portcullis-mounts` when the one-shot lane turned
/// out to have a near-copy of them and the CLI's fallback a worse one. The
/// history is worth keeping here because it is why the loop has the shape it
/// does: capability mounts live UNDER the jail path
/// (<jail>/atrium/sockets/fresco, …), so they held the overlay busy and the
/// whole stack survived —
///
///     umount: unmount of /var/lib/atrium/jails/<app> failed: Device busy
///
/// Removing a jail is documented to drop its own mounts and demonstrably
/// does not always. Each relaunch then stacked a fresh set on top of the
/// survivors: 12 mounts with no jail running, no process, no open file.
///
/// ★ And a stacked pile cannot be cleaned up afterwards by path: a path
/// resolves through the topmost layer, so buried mounts answer "not a file
/// system root directory" and are reachable only by fsid. So the fix has to
/// be never to leave the pile, not to clean it up later.
///
/// ★ NOT forced, unlike the one-shot lane: an application's mounts may be
/// genuinely busy, and forcing would take a filesystem away from something
/// still using it. This path reports survivors instead.
pub fn full_teardown(jail_path: &Path, jail_name: Option<&str>) -> bool {
    if let Some(n) = jail_name {
        let _ = portcullisd::jaild_jail::remove(n);
    }
    // ★ A console jail on the same root (M0/P1) holds its mounts busy and
    // would outlive them: it goes first.
    remove_jails_at(jail_path);
    let mut left = portcullis_mounts::converge(&[jail_path], portcullis_mounts::Force::No);
    // A process that was in the jail can still hold the root for a moment
    // after the jail is gone (a console's shell being reaped): retry briefly
    // before calling a mount a survivor.
    let start = std::time::Instant::now();
    while !left.is_empty() && start.elapsed() < std::time::Duration::from_secs(5) {
        std::thread::sleep(std::time::Duration::from_millis(100));
        left = portcullis_mounts::converge(&[jail_path], portcullis_mounts::Force::No);
    }
    if start.elapsed() >= std::time::Duration::from_millis(100) {
        eprintln!("portcullisd: {} unmounted after {} ms of retries{}", jail_path.display(), start.elapsed().as_millis(),
                  if left.is_empty() { "" } else { " (not all)" });
    }
    portcullis_mounts::warn_survivors("portcullisd", &left);
    left.is_empty()
}

/// Resolve the dedicated per-app uid the app executes as (PRIVILEGE INVARIANT,
/// portcullis.md §9.0). The connecting human is the OWNER (recorded in the launch
/// registry so services can peer-cred a connection back to `(owner, app_id)`); the
/// app itself runs as a 50000+ nologin "nobody" uid — never root, never the human.
///
/// Re-launches REUSE the app's existing uid (stable identity, no registry/passwd
/// leak); otherwise a fresh uid is allocated, given a nologin passwd entry, and
/// registered. Returns `(uid, run-as username)`.
///
/// NOTE: creating the passwd entry here mirrors the launch path already shelling
/// out (mount/jail/umount). Longer-term this privileged mutation belongs in jaild
/// (the audited broker) or at app-install time, not in portcullisd per-launch.
pub fn resolve_app_uid(owner: &str, app_id: &str, publisher: Option<&[u8; 32]>) -> Result<(u32, String), LaunchError> {
    resolve_instance_uid(owner, app_id, publisher, None)
}

/// [`resolve_app_uid`] for the instance in `persona`: each persona's instance
/// has its own uid (personas.md §3), reused across its own launches only.
pub fn resolve_instance_uid(owner: &str, app_id: &str, publisher: Option<&[u8; 32]>, persona: Option<&str>) -> Result<(u32, String), LaunchError> {
    resolve_launch_uid(owner, app_id, publisher, persona, None)
}

fn resolve_launch_uid(owner: &str, app_id: &str, publisher: Option<&[u8; 32]>, persona: Option<&str>, document: Option<u8>) -> Result<(u32, String), LaunchError> {
    let reg = portcullis_peer::DEFAULT_REGISTRY;
    let fail = |s: String| LaunchError::Failed("uid", s);
    if let Some(dir) = Path::new(reg).parent() {
        let _ = fs::create_dir_all(dir);
    }
    let _guard = portcullis_peer::lock_registry(reg).map_err(|e| fail(e.to_string()))?;
    /* Reuse only a binding with the SAME publisher: an app id re-installed under
     * another publisher's key is another app (arca.md §4.1) and gets a new uid. */
    let registry = portcullis_peer::AppRegistry::load(reg).map_err(|e| fail(format!("read {reg}: {e}")))?;
    let uid = match registry.uid_for_document(owner, app_id, publisher, persona, document) {
        Some(uid) => uid,
        None => {
            let uid = portcullis_peer::allocate(reg).map_err(|e| fail(format!("allocate: {e}")))?;
            /* A fresh binding may reuse the account of a uid whose registry
             * entry is gone (the registry is in /var/run, which a boot
             * clears; the account and its group memberships are not). It
             * must start with no supplementary groups, or it carries the
             * last owner's files groups into this app. A group(5) line can
             * name the account even when its passwd entry is gone, and the
             * entry is made again below, so the account is made first and
             * cleared by its canonical name either way. */
            let name = ensure_passwd(uid).map_err(fail)?;
            drop_supplementary_groups(&name).map_err(fail)?;
            portcullis_peer::register_document(reg, uid, owner, app_id, publisher, persona, document)
                .map_err(|e| fail(format!("register uid {uid}: {e}")))?;
            uid
        }
    };
    let run_as = ensure_passwd(uid).map_err(fail)?;
    Ok((uid, run_as))
}

/// Take an app account out of every group it is a member of, and check
/// that it is out (an account left in one would get it at initgroups).
fn drop_supplementary_groups(name: &str) -> Result<(), String> {
    run("pw", &["usermod", name, "-G", ""]).map_err(|e| format!("clear the groups of {name}: {e}"))?;
    let o = std::process::Command::new("pw").args(["groupshow", "-a"]).output()
        .map_err(|e| format!("pw groupshow -a: {e}"))?;
    if !o.status.success() {
        return Err(format!("pw groupshow -a: {}", String::from_utf8_lossy(&o.stderr).trim()));
    }
    let text = String::from_utf8_lossy(&o.stdout);
    if let Some(g) = still_member(&text, name) {
        return Err(format!("{name} is still in group {g}"));
    }
    eprintln!("portcullisd: reusing the account {name} with no supplementary groups");
    Ok(())
}

/// The first group in `group(5)` text that lists `name` as a member.
fn still_member<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    text.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.len() >= 4 && f[3].split(',').any(|m| m == name)).then_some(f[0])
    })
}

/// Ensure a host passwd entry exists for a dedicated 50000+ uid (its
/// groups and home are read from it, and portcullisd refuses a peer whose
/// uid isn't in passwd). Returns the username. Used for apps and session components.
pub fn ensure_passwd(uid: u32) -> Result<String, String> {
    match portcullis_peer::username(uid) {
        Some(name) => Ok(name),
        None => {
            let name = portcullis_peer::app_username(uid);
            run("pw", &["useradd", &name, "-u", &uid.to_string(),
                        "-d", "/nonexistent", "-s", "/usr/sbin/nologin"])
                .map_err(|e| format!("create passwd {name} (uid {uid}): {e}"))?;
            Ok(name)
        }
    }
}

/* ── dedup-oracle containment on overlays (tessera-quotas.md §3.6.2) ──
 *
 * An overlay is the one place a jailed app writes to the shared Tessera
 * volume, so it is where the dedup existence oracle leaks: write a candidate
 * file, fsync, read free space — unchanged means those bytes already exist
 * somewhere on the system. Measured on a GLOBAL domain, 4 MiB duplicate costs
 * 20 K vs 4156 K for novel content: a 208x, noise-free signal.
 *
 * `deferred` closes it. The write always allocates, so free space moves by the
 * full size regardless of content and the observer learns nothing; the
 * duplicate extents go into the dead-extent log and the GC reclaims them
 * later. Dedup is preserved — measured, 5 duplicates consumed 20700 K and
 * recovered 20740 K after the drain, files intact. That is why this is not
 * "give every app its own volume": a volume is the dedup boundary, and every
 * bundle shares a base, so per-app volumes would store it once per app.
 *
 * NEITHER PREREQUISITE IS A DEFAULT. Domains initialise to GLOBAL and
 * kern.tessera.dedup_deferred_enable is 0, so this must be armed explicitly —
 * which is the whole reason this function exists.
 */

/* FreeBSD _IOW('T', n, uint64_t): IOC_IN | (sizeof<<16) | ('T'<<8) | n */
const TESSERA_IOC_QUOTA_SET:    libc::c_ulong = 0x8008_5401;
const TESSERA_IOC_DEDUP_POLICY: libc::c_ulong = 0x8008_5403;
const TESSERA_DEDUP_DEFERRED:   u64 = 1;

/* The quota exists to CREATE THE DOMAIN — the kmod refuses a dedup policy on a
 * directory that is not already a quota root (EINVAL), because a policy on a
 * plain directory would silently apply to whatever domain it inherits, i.e.
 * the whole filesystem. It is not a meaningful cap on overlay growth; sizing
 * that is a separate question (storage.md). f_bavail is clamped to real pool
 * free space, so a limit above the volume size is harmless. */
const OVERLAY_DOMAIN_QUOTA_BYTES: u64 = 64 * 1024 * 1024 * 1024;

fn is_tessera(dir: &Path) -> bool {
    let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    let mut sfs: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut sfs) } != 0 { return false; }
    let name: Vec<u8> = sfs.f_fstypename.iter()
        .take_while(|&&ch| ch != 0).map(|&ch| ch as u8).collect();
    name == b"tessera"
}

fn arm_overlay_dedup(overlay_dir: &Path) -> Result<(), LaunchError> {
    /* Not on Tessera (a dev box on ZFS, say). Nothing to arm — but do NOT
     * pass silently: the containment simply is not in place, and a warning
     * that says so is the difference between a known gap and a believed-closed
     * one. */
    if !is_tessera(overlay_dir) {
        eprintln!("portcullisd: WARNING: {} is not on Tessera — the dedup existence oracle (tessera-quotas.md §3.6.2) is NOT closed for this app",
            overlay_dir.display());
        return Ok(());
    }

    /* Host-wide prerequisite. The kmod refuses to arm `deferred` without it,
     * deliberately: gating the publish pre-check without the dead-extent log
     * would leave the oracle exactly as open while paying the extra write. */
    let enabled = Command::new("sysctl")
        .args(["-n", "kern.tessera.dedup_deferred_enable"])
        .output().ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim() == "1").unwrap_or(false);
    if !enabled {
        run("sysctl", &["kern.tessera.dedup_deferred_enable=1"]).map_err(|e| {
            LaunchError::Failed("mount",
                format!("enable deferred dedup: {e} (required by tessera-quotas.md §3.6.2)"))
        })?;
        eprintln!("portcullisd: enabled kern.tessera.dedup_deferred_enable");
    }

    let f = fs::File::open(overlay_dir).map_err(|e| {
        LaunchError::Failed("mount", format!("open {}: {e}", overlay_dir.display()))
    })?;
    let fd = std::os::unix::io::AsRawFd::as_raw_fd(&f);

    let mut limit: u64 = OVERLAY_DOMAIN_QUOTA_BYTES;
    if unsafe { libc::ioctl(fd, TESSERA_IOC_QUOTA_SET, &mut limit) } != 0 {
        return Err(LaunchError::Failed("mount", format!(
            "quota-set on {}: {} — cannot create the dedup domain",
            overlay_dir.display(), std::io::Error::last_os_error())));
    }
    let mut pol: u64 = TESSERA_DEDUP_DEFERRED;
    if unsafe { libc::ioctl(fd, TESSERA_IOC_DEDUP_POLICY, &mut pol) } != 0 {
        /* HARD FAILURE, deliberately. Launching with the oracle open would be
         * a silent security regression, and the failure is actionable. */
        return Err(LaunchError::Failed("mount", format!(
            "dedup-policy=deferred on {}: {} — refusing to launch with the \
dedup existence oracle open (tessera-quotas.md §3.6.2)",
            overlay_dir.display(), std::io::Error::last_os_error())));
    }
    eprintln!("portcullisd: {} armed deferred dedup (oracle closed)",
        overlay_dir.display());
    Ok(())
}

pub fn run(cmd: &str, args: &[&str]) -> std::io::Result<()> {
    let st = Command::new(cmd).args(args).status()?;
    if !st.success() {
        return Err(std::io::Error::other(format!("{cmd} {args:?} failed: {st}")));
    }
    Ok(())
}

/// A jail's devfs is jaild's, and RemoveJail unmounts it; a jail still
/// dying past jaild's short retries can keep it busy. Unmount it here
/// only when it is still a mount (its device differs from the root's),
/// and say so: that is jaild's miss.
fn unmount_dev_left(jail_path: &Path) {
    use std::os::unix::fs::MetadataExt;
    let dev = jail_path.join("dev");
    let (Ok(root), Ok(d)) = (fs::symlink_metadata(jail_path), fs::symlink_metadata(&dev)) else { return };
    if d.is_dir() && d.dev() != root.dev() {
        eprintln!("portcullisd: {} was still mounted after RemoveJail; unmounting it", dev.display());
        if let Err(e) = umount(&dev) {
            eprintln!("portcullisd: {e}");
        }
    }
}

fn umount(p: &Path) -> std::io::Result<()> {
    let st = Command::new("umount").arg(p).status()?;
    if !st.success() {
        return Err(std::io::Error::other(format!("umount {} failed: {st}", p.display())));
    }
    Ok(())
}

pub fn umount_silent(p: &Path) -> std::io::Result<()> {
    let _ = Command::new("umount")
        .arg(p)
        .stderr(std::process::Stdio::null())
        .status()?;
    Ok(())
}

/// Suppress "unused" warning for jail_name_from_app_id which is used
/// transitively via build()/render() but not directly here anymore.
#[allow(dead_code)]
fn _silence(s: &str) -> String { jail_name_from_app_id(s) }

/// The installed-app catalog, as the daemon is willing to describe it.
///
/// Reads the app tree that only the TCB can see, so a jailed launcher never
/// needs `/var/lib/atrium/apps` mounted — it asks for this instead. Apps whose
/// manifest is unreadable or unparseable are skipped rather than reported: a
/// broken bundle is not something a launcher can act on, and listing it would
/// only offer the human a button that cannot work.
pub fn catalog(user: Option<&str>) -> Vec<portcullis_ipc::CatalogEntry> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(APPS_DIR) else { return out };
    /* Each app's uid for this person, so the launcher can match windows
     * (their owner uid) to apps (forum.md §6.1). Loaded once per catalog. */
    let registry = user.and_then(|_| portcullis_peer::AppRegistry::load(portcullis_peer::DEFAULT_REGISTRY).ok());
    let colours = user.map(portcullisd::persona_colours::read).unwrap_or_default();
    for e in entries.flatten() {
        let Ok(text) = fs::read_to_string(e.path().join("atrium.toml")) else { continue };
        let Ok(m) = Manifest::from_str(&text) else { continue };
        // Dev builds are Run from their workspace, never from the grid.
        if portcullisd::manifest_trust::is_dev(&m.app.id) {
            continue;
        }
        out.push(portcullis_ipc::CatalogEntry {
            id:          m.app.id.clone(),
            name:        m.app.name.clone(),
            description: m.app.description.clone(),
            icon:        m.app.icon.clone(),
            uid:         registry.as_ref().zip(user).and_then(|(r, u)| r.uid_for(u, &m.app.id)),
            icon_svg:    m.app.icon.as_deref()
                .and_then(|i| portcullis_toml::icon::read_bundle_svg(&e.path(), i)),
            console:     no_console(&m).is_none(),
            instances:   registry.as_ref().zip(user).map(|(r, u)| r.named_instances(u, &m.app.id).into_iter()
                .map(|(uid, p)| portcullis_ipc::CatalogInstance { persona: p.to_string(), uid, colour: colours.get(p).cloned() })
                .collect()).unwrap_or_default(),
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// `app_id`'s installed manifest, if it reads and parses.
fn installed(app_id: &str) -> Option<Manifest> {
    if app_id.is_empty() || app_id.contains(['/', '\0']) || app_id.starts_with('.') {
        return None;
    }
    let text = fs::read_to_string(PathBuf::from(APPS_DIR).join(app_id).join("atrium.toml")).ok()?;
    Manifest::from_str(&text).ok()
}

/// Is `app_id` a file manager (`removable-files` in its INSTALLED
/// manifest)? The only caller of OpenApps and OpenFile (opening-files.md
/// §6.2), as of dispensatord's CONNECT.
/// Why an app has no console (homes.md §4, decision H5), or `None` when
/// it may have one: the installer granted it (part of the system), or it
/// asks for a system-only capability.
pub fn no_console(m: &Manifest) -> Option<String> {
    let name = if m.app.name.is_empty() { &m.app.id } else { &m.app.name };
    let system = portcullis_policy::Policy::load_system(Path::new(portcullis_policy::SYSTEM_PATH))
        .map(|p| p.grants.contains_key(&m.app.id))
        .unwrap_or(true); // an unreadable system policy: assume the worst
    if system {
        return Some(format!("{name} is part of the system: it has no console"));
    }
    let caps = m.capabilities.system_only();
    if !caps.is_empty() {
        return Some(format!("{name} is part of the system ({}): it has no console", caps.join(", ")));
    }
    None
}

/// `app_id`'s installed manifest, for the console's checks.
pub fn installed_manifest(app_id: &str) -> Option<Manifest> {
    installed(app_id)
}

/// May this app open attachments (its INSTALLED manifest holds
/// `open-attachments`, mail.md §8.2)?
pub fn app_opens_attachments(app_id: &str) -> bool {
    installed(app_id).is_some_and(|m| m.capabilities.open_attachments == Some(true))
}

/// Is this the Navigator (its INSTALLED manifest holds `navigator-broker`)?
/// The only caller of SaveDownload.
pub fn app_is_navigator(app_id: &str) -> bool {
    installed(app_id).is_some_and(|m| m.capabilities.navigator_broker == Some(true))
}

/// Is this the terminal (its INSTALLED manifest holds `consoles`)?
pub fn app_shows_consoles(app_id: &str) -> bool {
    installed(app_id).is_some_and(|m| m.capabilities.consoles == Some(true))
}

/// Is this the settings app (its INSTALLED manifest holds `app-grants`,
/// curia.md §13)? The only caller of Access, RemoveAccess, ApproveAccess.
pub fn app_manages_grants(app_id: &str) -> bool {
    installed(app_id).is_some_and(|m| m.capabilities.app_grants == Some(true))
}

/// Every installed app's tree and manifest text, for Settings' Apps page:
/// trees whose manifest names another id are left out, as they can't be
/// launched by it.
pub fn installed_texts() -> Vec<(PathBuf, String, Manifest)> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(APPS_DIR) else { return out };
    for e in entries.flatten() {
        let Ok(text) = fs::read_to_string(e.path().join("atrium.toml")) else { continue };
        let Ok(m) = Manifest::from_str(&text) else { continue };
        if e.file_name().to_str() == Some(m.app.id.as_str()) {
            out.push((e.path(), text, m));
        }
    }
    out
}

pub fn app_is_file_manager(app_id: &str) -> bool {
    installed(app_id).is_some_and(|m| m.capabilities.removable_files == Some(true))
}

/// How `app_id` opens `mime` (its INSTALLED `[[opens]]`), and its name.
pub fn app_opens(app_id: &str, mime: &str) -> Option<(portcullis_toml::OpenMode, String)> {
    let m = installed(app_id)?;
    m.opens(mime).map(|mode| (mode, if m.app.name.is_empty() { m.app.id.clone() } else { m.app.name.clone() }))
}


/// An installed app's name (for `atrium-app:` links).
pub fn app_name(app_id: &str) -> Option<String> {
    let m = installed(app_id)?;
    Some(if m.app.name.is_empty() { m.app.id.clone() } else { m.app.name.clone() })
}

/// Every installed app that takes links of `scheme`: (id, name), by name.
pub fn apps_taking_link(scheme: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(APPS_DIR) else { return out };
    for e in entries.flatten() {
        let Ok(text) = fs::read_to_string(e.path().join("atrium.toml")) else { continue };
        let Ok(m) = Manifest::from_str(&text) else { continue };
        if e.file_name().to_str() != Some(m.app.id.as_str()) || !m.takes_link(scheme) {
            continue;
        }
        // Web links are the Navigator's alone, whatever a tree says (D5).
        if atrium_links::WEB.contains(&scheme) && m.capabilities.navigator_broker != Some(true) {
            continue;
        }
        out.push((m.app.id.clone(), if m.app.name.is_empty() { m.app.id.clone() } else { m.app.name.clone() }));
    }
    out.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()).then(a.0.cmp(&b.0)));
    out
}

/// Every installed share-target `person` may share to (share-sheet.md
/// §4.3: dev builds only their own person's): (id, name), by name.
pub fn share_targets_for(person: &str) -> Vec<(String, String)> {
    let mine = format!("dev.{person}.");
    let mut out: Vec<(String, String)> = installed_texts().into_iter()
        .filter(|(_, _, m)| m.roles.as_ref().is_some_and(|r| r.implements.iter().any(|i| i == "share-target")))
        .filter(|(_, _, m)| !m.app.id.starts_with("dev.") || m.app.id.starts_with(&mine))
        .map(|(_, _, m)| (m.app.id.clone(), if m.app.name.is_empty() { m.app.id.clone() } else { m.app.name.clone() }))
        .collect();
    out.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()).then(a.0.cmp(&b.0)));
    out
}

/// Every link scheme an installed app takes (not `atrium-app`), sorted.
pub fn link_schemes() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let Ok(entries) = fs::read_dir(APPS_DIR) else { return out };
    for e in entries.flatten() {
        let Ok(text) = fs::read_to_string(e.path().join("atrium.toml")) else { continue };
        let Ok(m) = Manifest::from_str(&text) else { continue };
        if e.file_name().to_str() != Some(m.app.id.as_str()) {
            continue;
        }
        for s in m.links.iter().flat_map(|l| l.schemes.iter()) {
            if s != atrium_links::APP && atrium_links::declarable(s).is_ok() && !out.contains(s) {
                out.push(s.clone());
            }
        }
    }
    out.retain(|s| !apps_taking_link(s).is_empty());
    out.sort();
    out
}

/// Every installed app that opens `mime`: (id, name, mode), by name.
pub fn apps_opening(mime: &str) -> Vec<(String, String, portcullis_toml::OpenMode)> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(APPS_DIR) else { return out };
    for e in entries.flatten() {
        let Ok(text) = fs::read_to_string(e.path().join("atrium.toml")) else { continue };
        let Ok(m) = Manifest::from_str(&text) else { continue };
        // The tree's name is the id the app is launched by.
        if e.file_name().to_str() != Some(m.app.id.as_str()) {
            continue;
        }
        if let Some(mode) = m.opens(mime) {
            out.push((m.app.id.clone(), if m.app.name.is_empty() { m.app.id.clone() } else { m.app.name.clone() }, mode));
        }
    }
    out.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()).then(a.0.cmp(&b.0)));
    out
}

/// Does `app_id`'s installed manifest hold the `app-launch` capability?
///
/// The gate for [`catalog`]: the caller is identified by peer uid through the
/// launch registry, so this reads the capability off the INSTALLED manifest, not
/// off anything the caller sent. An app that is not in the registry at all (not
/// Portcullis-launched) resolves to no app-id and is refused by the caller.
/// Is this an IDE (its INSTALLED manifest holds `workspace-exec`, W6)?
pub fn app_execs_in_workspaces(app_id: &str) -> bool {
    installed(app_id).is_some_and(|m| m.capabilities.workspace_exec == Some(true))
}

/// Does this app's INSTALLED manifest hold `workspaces` (homes.md §5)?
pub fn app_manages_workspaces(app_id: &str) -> bool {
    installed(app_id).is_some_and(|m| m.capabilities.workspaces == Some(true))
}

pub fn app_may_launch_apps(app_id: &str) -> bool {
    let path = PathBuf::from(APPS_DIR).join(app_id).join("atrium.toml");
    let Ok(text) = fs::read_to_string(path) else { return false };
    let Ok(m) = Manifest::from_str(&text) else { return false };
    m.capabilities.app_launch == Some(true)
}

/// Does this app's INSTALLED manifest hold `net-fetch` (docs/spec/nuntius.md
/// §2)? Read from the app tree, never from anything the caller sends.
pub fn app_may_fetch(app_id: &str) -> bool {
    let path = PathBuf::from(APPS_DIR).join(app_id).join("atrium.toml");
    let Ok(text) = fs::read_to_string(path) else { return false };
    let Ok(m) = Manifest::from_str(&text) else { return false };
    m.capabilities.net_fetch == Some(true)
}

/// Does this app's INSTALLED manifest hold `network-portal`
/// (atrium-net.md §8.1)? Read from the app tree, never from the caller.
/// The installed app that opens the captive portal's sign-in window: the
/// one whose manifest holds `network-portal` (the Navigator); the first by
/// id if more do.
pub fn portal_app() -> Option<String> {
    let mut v: Vec<String> = installed_texts().into_iter()
        .filter(|(_, _, m)| m.capabilities.network_portal == Some(true)).map(|(_, _, m)| m.app.id).collect();
    v.sort();
    v.into_iter().next()
}

pub fn app_may_open_portal(app_id: &str) -> bool {
    let path = PathBuf::from(APPS_DIR).join(app_id).join("atrium.toml");
    let Ok(text) = fs::read_to_string(path) else { return false };
    let Ok(m) = Manifest::from_str(&text) else { return false };
    m.capabilities.network_portal == Some(true)
}

/// Does this app's INSTALLED manifest hold `protected-media`
/// (docs/spec/hospes.md §3)? Read from the app tree, never from the caller.
pub fn app_may_play_protected(app_id: &str) -> bool {
    let path = PathBuf::from(APPS_DIR).join(app_id).join("atrium.toml");
    let Ok(text) = fs::read_to_string(path) else { return false };
    let Ok(m) = Manifest::from_str(&text) else { return false };
    m.capabilities.protected_media == Some(true)
}

/// The site list this app's INSTALLED manifest narrows its fetcher to
/// (`net-fetch-sites`, nuntius.md §4), in the fetcher's environment form;
/// `Ok(None)` is unrestricted. A list that does not parse is an error, so
/// the fetcher is refused rather than started unlimited.
pub fn app_fetch_sites(app_id: &str) -> Result<Option<String>, String> {
    let path = PathBuf::from(APPS_DIR).join(app_id).join("atrium.toml");
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let m = Manifest::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    fetch_sites_of(m.capabilities.net_fetch_sites.as_deref())
}

/// `app_fetch_sites` without the file.
pub fn fetch_sites_of(list: Option<&[String]>) -> Result<Option<String>, String> {
    list.map(|l| nuntius::Sites::parse(l).map(|s| s.to_env_value())).transpose()
}

/// The jid of a running jail for `app_id`, or `None`.
///
/// Asks the kernel rather than tracking state in-process: portcullisd can
/// be restarted while jails keep running.
pub fn running_jid(app_id: &str) -> Option<i32> {
    jaild::ffi::jail_id_by_name(&jail_name_from_app_id(app_id))
}

/// The key of an instance: `<id>` for Personal, `<id>@<persona>` for a named
/// persona. Names the jail root and seeds the synthetic host identity.
pub fn instance_key(app_id: &str, persona: Option<&str>) -> String {
    match persona {
        None => app_id.to_string(),
        Some(p) => format!("{app_id}@{p}"),
    }
}

/// As [`running_jid`], for the instance in `persona`.
pub fn running_jid_instance(app_id: &str, persona: Option<&str>) -> Option<i32> {
    let name = portcullis_jail::jail_name_for_instance(app_id, persona.map(|p| format!("p-{p}")).as_deref());
    jaild::ffi::jail_id_by_name(&name)
}

/// The apps of `user`'s persona `persona` with a process running now (any
/// of the persona's uids), by app id.
pub fn running_in_persona(user: &str, persona: &str) -> Vec<String> {
    let reg = crate::load_registry();
    let mut v: Vec<String> = reg.persona_uids(user, persona).into_iter()
        .filter(|(uid, _)| Command::new("pgrep").args(["-U", &uid.to_string()]).output().map(|o| o.status.success()).unwrap_or(false))
        .map(|(_, app)| app.to_string())
        .collect();
    v.sort();
    v.dedup();
    v
}

/// Every app uid of `user` (each persona's instances and workers; not
/// session components, which ostiarius tears down itself).
pub fn user_uids(user: &str) -> Vec<u32> {
    crate::load_registry().user_uids(user)
}

/// End the processes of `uids`: TERM, up to 3 s for them to go, then KILL.
/// Each app's launch then tears its jail down as usual.
pub fn end_processes(uids: &[u32]) {
    let alive = |u: u32| Command::new("pgrep").args(["-U", &u.to_string()]).output().map(|o| o.status.success()).unwrap_or(false);
    let live: Vec<u32> = uids.iter().copied().filter(|u| alive(*u)).collect();
    if live.is_empty() {
        return;
    }
    for u in &live {
        let _ = Command::new("pkill").args(["-TERM", "-U", &u.to_string()]).status();
    }
    for _ in 0..30 {
        if !live.iter().any(|u| alive(*u)) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    for u in &live {
        let _ = Command::new("pkill").args(["-KILL", "-U", &u.to_string()]).status();
    }
}

/// The uid of `user`'s instance of `app_id` in `persona`, if one was ever
/// registered (any publisher).
pub fn instance_uid(user: &str, app_id: &str, persona: Option<&str>) -> Option<u32> {
    portcullis_peer::AppRegistry::load(portcullis_peer::DEFAULT_REGISTRY).ok()?
        .uid_for_persona(user, app_id, persona)
}

/// Why `app_id` has no per-persona instances, if it has none: system apps
/// that span personas (personas.md §3: the file manager, the settings app,
/// apps that reach every workspace) run once and show both sides.
pub fn not_instanced(app_id: &str) -> Option<String> {
    match installed(app_id) {
        Some(m) => not_instanced_manifest(&m),
        None => None, // launching reports the missing app
    }
}

fn not_instanced_manifest(m: &Manifest) -> Option<String> {
    let c = &m.capabilities;
    let spans = [
        (c.home_files == Some(true), "home-files"),
        (c.removable_files == Some(true), "removable-files"),
        (c.app_grants == Some(true), "app-grants"),
        (c.workspaces == Some(true), "workspaces"),
        (c.workspace_exec == Some(true), "workspace-exec"),
    ];
    spans.iter().find(|(on, _)| *on).map(|(_, cap)| format!("it holds {cap}"))
}

/// Does this app's manifest say only one instance may run?
///
/// Absent `[supervision] instances`, treat it as single: that is what every
/// current manifest declares, and quietly allowing a second instance of an app
/// that did not ask for it is the worse default.
pub fn is_single_instance(app_id: &str) -> bool {
    let path = PathBuf::from(APPS_DIR).join(app_id).join("atrium.toml");
    let Ok(text) = fs::read_to_string(path) else { return true };
    let Ok(m) = Manifest::from_str(&text) else { return true };
    !matches!(
        m.supervision.as_ref().and_then(|s| s.instances),
        Some(portcullis_toml::InstancesPolicy::Multi)
    )
}

#[cfg(test)]
mod recovery_name_tests {
    use super::*;

    #[test]
    fn an_old_occupancy_check_still_needs_a_detached_label_check() {
        let old = br#"{"id":[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15],"name":"Proof.csv","confirmed_nonempty":true}"#;
        let marker: WorkbookMarker = serde_json::from_slice(old).unwrap();
        assert!(!marker.checked_detached);
        assert_eq!(marker.book.name, "Proof.csv");
    }

    #[test]
    fn missing_detached_image_preserves_the_legacy_marker() {
        let root = tempfile::tempdir().unwrap();
        let store = workbook_store(root.path(), 0);
        let path = root.path().join("0.pending");
        let marker = br#"{"id":[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15],"name":"Proof.csv"}"#;
        fs::write(&path, marker).unwrap();
        assert!(check_detached_workbook(&store, &path, 0).is_err());
        assert_eq!(fs::read(&path).unwrap(), marker);
        assert!(!store.img.exists());
        assert!(!store.img.with_extension("ufs").exists());
    }

    #[test]
    fn cached_label_does_not_hide_a_missing_image() {
        let root = tempfile::tempdir().unwrap();
        let store = workbook_store(root.path(), 0);
        let path = root.path().join("0.pending");
        let book = write_workbook_marker(&path, "Proof.csv").unwrap();
        store_workbook_marker(&path, &book, true).unwrap();
        let before = fs::read(&path).unwrap();
        let error = check_detached_workbook(&store, &path, 0).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(!store.img.exists());
        assert!(!store.img.with_extension("ufs").exists());
    }

    #[test]
    fn reconciliation_reads_only_one_regular_bounded_name() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("autosave");
        let doc = data.join("doc");
        fs::create_dir_all(&doc).unwrap();
        let name = doc.join("name");
        let marker = root.path().join("0.pending");
        let original = write_workbook_marker(&marker, "Proof.csv").unwrap();
        symlink(&marker, &name).unwrap();
        assert!(closed_workbook_name(&data).is_err());
        fs::remove_file(&name).unwrap();
        fs::write(&name, vec![b'x'; 4096]).unwrap();
        let oversized = closed_workbook_name(&data).unwrap();
        assert_eq!(oversized.len(), 256);
        assert!(rename_workbook_marker(&marker, original.id, oversized).is_err());
        assert_eq!(pending_workbook(&marker, 0).unwrap(), original);
        fs::write(name, "Proof (2).osheet").unwrap();
        assert_eq!(closed_workbook_name(&data).unwrap(), "Proof (2).osheet");
        symlink(&doc, data.join("other")).unwrap();
        assert!(closed_workbook_name(&data).is_err());
    }

    #[test]
    fn a_stale_rename_leaves_the_replacement_marker_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("0.pending");
        let original = write_workbook_marker(&path, "Proof.csv").unwrap();
        let replacement = write_workbook_marker(&path, "Proof.csv").unwrap();
        assert!(rename_workbook_marker(&path, original.id, "Stale.osheet".into()).is_err());
        assert_eq!(pending_workbook(&path, 0).unwrap(), replacement);
        rename_workbook_marker(&path, replacement.id, "Proof (2).osheet".into()).unwrap();
        let renamed = pending_workbook(&path, 0).unwrap();
        assert_eq!(renamed.id, replacement.id);
        assert_eq!(renamed.name, "Proof (2).osheet");
    }
}

#[cfg(test)]
mod fetch_sites_tests {
    use super::fetch_sites_of;

    #[test]
    fn a_site_list_is_checked_before_the_fetcher_starts() {
        assert_eq!(fetch_sites_of(None), Ok(None));
        let l = vec!["Example.com".to_string(), "*.cdn.example.net".into()];
        assert_eq!(fetch_sites_of(Some(&l)), Ok(Some("example.com,*.cdn.example.net".into())));
        assert_eq!(fetch_sites_of(Some(&[])), Ok(Some(String::new())), "an empty list is no sites, not unlimited");
        assert!(fetch_sites_of(Some(&["10.0.0.1".to_string()])).is_err());
        assert!(fetch_sites_of(Some(&["a.com,b.com".to_string()])).is_err());
    }
}

/// Names of console jails start with this (dev-tools-plan.md M0, P1).
pub const CONSOLE_JAIL_PREFIX: &str = "con-";

/// Remove the console jails whose root is `path` (the jail beside an app's,
/// dev-tools-plan.md M0): their shells end, and the root's mounts can go.
/// Only console jails: an app's own jail has a network to release first.
pub fn remove_jails_at(path: &Path) {
    let Ok(o) = Command::new("jls").args(["-N", "name", "path"]).stderr(Stdio::null()).output() else { return };
    let want = path.to_string_lossy();
    let mut removed = Vec::new();
    for line in String::from_utf8_lossy(&o.stdout).lines() {
        let mut f = line.split_whitespace();
        if let (Some(name), Some(p)) = (f.next(), f.next()) {
            if p == want && name.starts_with(CONSOLE_JAIL_PREFIX) {
                let _ = portcullisd::jaild_jail::remove(name);
                removed.push(name.to_string());
            }
        }
    }
    // ★ A dying console jail pins the root as the workspace's own would
    // (portcullis_mounts::wait_jail_gone): wait for each before the caller
    // unmounts, or the root survives the teardown and Delete refuses.
    for name in removed {
        portcullis_mounts::wait_jail_gone(&name, portcullis_mounts::JAIL_GONE_TIMEOUT);
    }
}

#[cfg(test)]
mod reused_account_tests {
    use super::still_member;

    #[test]
    fn a_reused_app_account_left_in_a_group_is_found() {
        let groups = "wheel:*:0:root\nfiles-alice:*:2000:atrium-app-50006,atrium-app-50007\nfiles-bob:*:2002:\n";
        assert_eq!(still_member(groups, "atrium-app-50006"), Some("files-alice"));
        assert_eq!(still_member(groups, "atrium-app-50007"), Some("files-alice"));
        assert_eq!(still_member(groups, "atrium-app-5000"), None, "a prefix is not a member");
        assert_eq!(still_member("files-bob:*:2002:\n", "atrium-app-50006"), None);
    }
}
