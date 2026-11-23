//! The Calendar window sees one socket. Its Fasti jail keeps the writable
//! store in the owner's persona data and the listener survives service restarts.
use jaild::client::Client;
use portcullisd::{
    listeners,
    supervisor::{Pending, Supervisor},
    system_services::{ManifestMount, ManifestSocket, MountKindStr, ServiceManifest},
};
use std::{
    collections::HashMap,
    fs, io,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, LazyLock, Mutex,
    },
    thread,
};

pub const APP: &str = "org.atrium.calendar";
const TEMPLATE: &str = include_str!("../../../atrium-calendar/fasti-service.toml");
const ENDPOINTS: &str = "/var/run/atrium/fasti";

struct SessionService {
    owner: String,
    persona: Option<String>,
    stop: Arc<AtomicBool>,
    worker: thread::JoinHandle<()>,
}

static SERVICES: LazyLock<Mutex<HashMap<u32, SessionService>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn reserve_window(
    publisher: Option<&[u8; 32]>,
    persona: Option<&str>,
) -> io::Result<(String, fs::File)> {
    let signer = hex::encode(publisher.ok_or_else(|| io::Error::other("Calendar must be signed"))?);
    let expected = fs::read_to_string("/etc/atrium/calendar-publisher")?;
    if expected.trim() != signer {
        return Err(io::Error::other(
            "Calendar publisher differs from the installed Fasti grant",
        ));
    }
    // The normal single-instance check sees a jail only once it exists.
    // Serialize opening and teardown of this UI root across that interval.
    directory(Path::new(ENDPOINTS), 0, 0o700)?;
    let path = Path::new(ENDPOINTS).join(format!("window-{}", persona.unwrap_or("personal")));
    let lease = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    lease
        .try_lock()
        .map_err(|e| io::Error::other(format!("Calendar window is in use: {e}")))?;
    Ok((signer, lease))
}

// Called only under a trusted parent. Never adopt an existing link, an
// unexpected owner or a group-writable service directory.
fn directory(path: &Path, uid: u32, mode: u32) -> io::Result<()> {
    match fs::DirBuilder::new().mode(mode).create(path) {
        Ok(()) => std::os::unix::fs::chown(path, Some(uid), Some(uid))?,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let m = fs::symlink_metadata(path)?;
    if !m.is_dir() || m.uid() != uid || m.mode() & 0o777 != mode {
        return Err(io::Error::other(format!(
            "unexpected ownership or permissions on {}",
            path.display()
        )));
    }
    Ok(())
}

fn store_dir(
    owner: &str,
    persona: Option<&str>,
    signer: &str,
    service_uid: u32,
) -> io::Result<PathBuf> {
    let home = portcullis_overlay::home_of(owner)
        .ok_or_else(|| io::Error::other("missing Calendar owner home"))?;
    let home = fs::canonicalize(home)?;
    let parent = match persona {
        None => portcullis_overlay::apps_root(&home).map_err(io::Error::other)?,
        Some(p) if portcullis_peer::valid_person_persona(p) => {
            portcullis_overlay::persona_apps_dir(&home, p)
        }
        Some(_) => return Err(io::Error::other("invalid Calendar persona")),
    };
    // The ordinary app launch already prepared the persona data directory.
    let metadata = fs::symlink_metadata(&parent)?;
    if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        return Err(io::Error::other("insecure Calendar persona directory"));
    }
    let stores = parent.join(".fasti");
    directory(&stores, 0, 0o700)?;
    let store = stores.join(signer);
    directory(&store, service_uid, 0o700)?;
    Ok(store)
}

/// The app's overlay persists across launches. Refuse links in any component
/// before setting up a host mount under the overlay.
pub fn socket_mountpoint(root: &Path) -> io::Result<PathBuf> {
    let mut path = root.to_owned();
    for part in ["atrium", "sockets", "calendar"] {
        path.push(part);
        match fs::create_dir(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        if !fs::symlink_metadata(&path)?.is_dir() {
            return Err(io::Error::other("Calendar mountpoint is not a directory"));
        }
    }
    Ok(path)
}

pub fn endpoint(
    owner: &str,
    persona: Option<&str>,
    signer: &str,
    app_uid: u32,
) -> io::Result<PathBuf> {
    let mut services = SERVICES.lock().unwrap();
    let endpoint = Path::new(ENDPOINTS).join(app_uid.to_string());
    if let Some(service) = services.get(&app_uid) {
        if !service.worker.is_finished() {
            return Ok(endpoint);
        }
    }
    let mut manifest: ServiceManifest = toml::from_str(TEMPLATE).map_err(io::Error::other)?;
    // A doubled hyphen cannot occur in an installed app ID. The root and
    // jail name must never collide with an ordinary app's overlay.
    manifest.name = format!("system-fasti--{app_uid}");
    let root = Path::new(crate::launch::JAILS_DIR).join(&manifest.name);
    manifest.path = root.to_string_lossy().into_owned();
    directory(&root, 0, 0o755)?;
    let lease = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(".supervisor"))?;
    lease.try_lock()?;
    // A broker crash closes its procdesc. Retire leftover nullfs mounts
    // before preparing a new instance under the same dedicated root.
    if !portcullis_mounts::converge(&[&root], portcullis_mounts::Force::No).is_empty() {
        return Err(io::Error::other("Fasti's previous jail is busy"));
    }
    fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join("registry"))?;
    let exec = manifest.exec.as_mut().unwrap();
    let store = store_dir(owner, persona, signer, exec.uid)?;
    exec.argv.extend([
        owner.into(),
        persona.unwrap_or("personal").into(),
        signer.into(),
    ]);
    // Only this leaf is mounted into the UI. Fasti receives the already
    // bound descriptor (its UID cannot write the directory).
    directory(&endpoint, 0, 0o755)?;
    for (source, dest) in [(&store, "store"), (&endpoint, "endpoint")] {
        manifest.mounts.push(ManifestMount {
            source: source.to_string_lossy().into_owned(),
            dest: dest.into(),
            kind: MountKindStr::RwNullfs,
        });
    }
    manifest.sockets.push(ManifestSocket {
        path: endpoint
            .join("calendar.sock")
            .to_string_lossy()
            .into_owned(),
        owner: app_uid,
        group: app_uid,
        mode: "0600".into(),
    });
    let fds = listeners::bind_all(&manifest.sockets)?;
    let mut create_req = manifest.to_create_request();
    create_req.syscall_profile = Some("calendar-store".into());
    let broker = Path::new(crate::JAILD_SOCK);
    let mut supervisor = Supervisor::new(Client::connect(broker)?, broker.to_owned())?;
    supervisor
        .start_session_service(Pending {
            manifest,
            create_req,
            listeners: fds,
        })
        .map_err(io::Error::other)?;
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = Arc::clone(&stop);
    let worker = thread::spawn(move || {
        if let Err(e) = supervisor.run_until(&stopping) {
            eprintln!("portcullisd: Fasti supervisor: {e}");
        }
        drop(lease);
    });
    services.insert(
        app_uid,
        SessionService {
            owner: owner.into(),
            persona: persona.map(str::to_owned),
            stop,
            worker,
        },
    );
    Ok(endpoint)
}

/// Stop all services for a logout, or just the removed named persona.
pub fn stop(owner: &str, persona: Option<&str>) {
    let mut services = SERVICES.lock().unwrap();
    let uids: Vec<_> = services
        .iter()
        .filter(|(_, s)| {
            s.owner == owner && persona.is_none_or(|p| s.persona.as_deref() == Some(p))
        })
        .map(|(&uid, s)| {
            s.stop.store(true, Ordering::SeqCst);
            uid
        })
        .collect();
    for uid in uids {
        let s = services.remove(&uid).unwrap();
        if s.worker.join().is_err() {
            eprintln!("portcullisd: Fasti supervisor for uid {uid} exited unexpectedly");
        }
    }
}
