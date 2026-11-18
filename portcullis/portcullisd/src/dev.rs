//! Run what a workspace built (docs/spec/homes.md §5.3, W5). A workspace
//! has no windows: its build runs as a regular app, in its own jail, with
//! only what its manifest declares.
//!
//! **Run** names a workspace and a project folder in its `work` holding an
//! `atrium.toml` and the entry it names. portcullisd reads both from the
//! host side, never through a link out of `work` (each open is
//! `O_RESOLVE_BENEATH` the project folder, which is itself opened beneath
//! `work`), and installs them as the app `dev.<person>.<workspace>.<id>`
//! (the person too: two people's Scratch builds are two apps):
//!
//! - signed by this machine's **dev publisher** (portcullis-trust), whose
//!   key signs only `dev.` ids and is the only key that may;
//! - refused if the manifest asks for any system-only capability;
//! - never in the Applications grid (the catalog leaves `dev.` out);
//! - its windows framed in amber and tagged "dev build: <id> from <ws>"
//!   by frescod;
//! - granted what it declares on the Run press, as the person's own
//!   decision for that manifest (a changed manifest is a new grant);
//! - gone, with its grant, when its workspace is deleted.
//!
//! The manifest, the entry and the entry's libraries are copied
//! (dev-tools-plan.md M4). Each library is found the way the workspace's
//! loader would find it (`DT_RPATH`/`DT_RUNPATH` with `$ORIGIN`, then
//! `/lib`, `/usr/lib`, `/usr/local/lib`), in the workspace's own view:
//! the base is the host's, `/usr/local` its root's, `~/work` its `work`,
//! `/workspaces/<A>` A's `work`. In a folder the workspace can write, every
//! open is `O_RESOLVE_BENEATH` that folder, so a link out of it is refused.
//! Base libraries keep their paths in the bundle; the rest go to
//! `/usr/lib`, where the app's loader looks. Anything else a build needs
//! (data files, plugins it `dlopen`s) it ships in its entry for now.

use std::fs;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::workspace;

const APPS_DIR: &str = "/var/lib/atrium/apps";
const MANIFEST_MAX: u64 = 64 * 1024;
const ENTRY_MAX: u64 = 512 * 1024 * 1024;

/// The dev bundle's id.
pub fn dev_id(owner: &str, ws: &str, id: &str) -> String {
    format!("{}{owner}.{ws}.{id}", portcullisd::manifest_trust::DEV_PREFIX)
}

/// Open `rel` beneath the directory `dir` (no `..`, no absolute path, no
/// link that leaves it).
fn open_beneath(dir: &OwnedFd, rel: &str, flags: i32) -> Result<OwnedFd, String> {
    if rel.is_empty() || rel.starts_with('/') || rel.split('/').any(|c| c == "..") || rel.contains('\0') {
        return Err(format!("{rel:?}: not a path inside the project"));
    }
    let c = std::ffi::CString::new(rel).map_err(|_| "a NUL".to_string())?;
    // SAFETY: a valid directory descriptor and a NUL-terminated path.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), flags | libc::O_RESOLVE_BENEATH | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(format!("{rel}: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: just opened, ours alone.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Read a regular file through its descriptor, at most `max` bytes.
fn read_file(fd: OwnedFd, what: &str, max: u64) -> Result<Vec<u8>, String> {
    let f = fs::File::from(fd);
    let m = f.metadata().map_err(|e| format!("{what}: {e}"))?;
    if !m.is_file() {
        return Err(format!("{what} is not a file"));
    }
    if m.len() > max {
        return Err(format!("{what} is larger than {max} bytes"));
    }
    let mut out = Vec::with_capacity(m.len() as usize);
    f.take(max + 1).read_to_end(&mut out).map_err(|e| format!("{what}: {e}"))?;
    if out.len() as u64 > max {
        return Err(format!("{what} grew past {max} bytes"));
    }
    Ok(out)
}

/// This machine's dev publisher key (made once): the private key, root's.
fn dev_key() -> Result<String, String> {
    let dir = Path::new(portcullisd::manifest_trust::DEV_PUBLISHER_DIR);
    let private = dir.join("private.pem");
    if let Ok(k) = fs::read_to_string(&private) {
        return Ok(k);
    }
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    let mut seed = [0u8; 32];
    fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut seed)).map_err(|e| format!("random: {e}"))?;
    let (k, p) = portcullis_sig::new_key(seed).map_err(|e| format!("dev key: {e:?}"))?;
    let tmp = dir.join("private.pem.new");
    fs::write(&tmp, &k).map_err(|e| e.to_string())?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    fs::write(dir.join("public.pem"), &p).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &private).map_err(|e| e.to_string())?;
    eprintln!("portcullisd: made this machine's dev publisher key");
    Ok(k)
}

/// What a Run reads and checks before anything is installed or the press
/// spent: (dev id, manifest text to install, entry path in the bundle,
/// entry bytes).
pub struct Prepared {
    pub id:       String,
    pub manifest: String,
    /// The entry, then each `[[workers]]` entry: (path in the bundle, bytes).
    entries:      Vec<(String, Vec<u8>)>,
    /// Their libraries and the loader: (path in the bundle, bytes).
    libs:         Vec<(PathBuf, Vec<u8>)>,
}

pub fn prepare(owner: &str, ws: &str, project: &str) -> Result<Prepared, String> {
    if !workspace::valid_name(ws) {
        return Err(format!("{ws:?} is not a workspace name"));
    }
    let (home, gid) = portcullisd::home::ensure_person(owner)?;
    let wsdir = workspace::root_dir(&home, gid)?.join(ws);
    if !workspace::list(owner)?.iter().any(|(r, _)| r.name == ws) {
        return Err(format!("{owner} has no workspace {ws}"));
    }
    let work_path = wsdir.join(workspace::WORK);
    let c = std::ffi::CString::new(work_path.as_os_str().as_encoded_bytes()).map_err(|_| "a NUL".to_string())?;
    // SAFETY: a NUL-terminated path. `work` itself must not be a link.
    let wfd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    if wfd < 0 {
        return Err(format!("{}: {}", work_path.display(), std::io::Error::last_os_error()));
    }
    // SAFETY: just opened, ours alone.
    let work = unsafe { OwnedFd::from_raw_fd(wfd) };
    let project = project.trim_matches('/');
    let proj = if project.is_empty() || project == "." { work } else { open_beneath(&work, project, libc::O_RDONLY | libc::O_DIRECTORY)? };
    let text = read_file(open_beneath(&proj, "atrium.toml", libc::O_RDONLY)?, "atrium.toml", MANIFEST_MAX)?;
    let text = String::from_utf8(text).map_err(|_| "atrium.toml is not UTF-8".to_string())?;
    let m = portcullis_toml::Manifest::from_str(&text).map_err(|e| format!("atrium.toml: {e}"))?;
    let report = portcullis_toml::validate(&m);
    if !report.is_ok() {
        return Err(format!("atrium.toml: {}", report.errors.join("; ")));
    }
    let id = m.app.id.clone();
    if id.is_empty() || id.contains(['/', '\0']) || id.starts_with('.') || portcullisd::manifest_trust::is_dev(&id) {
        return Err(format!("{id:?} can't be a dev build's id"));
    }
    // ★ Nothing a dev build may not have: system-only capabilities are the
    // system's to give, never a Run's.
    let mut asks: Vec<&str> = m.capabilities.system_only();
    asks.extend(m.capabilities.restricted());
    asks.sort();
    asks.dedup();
    if !asks.is_empty() {
        return Err(format!("a dev build can't have {}", asks.join(", ")));
    }
    // The entry and its workers' (app-composition.md §4.1: the same bundle,
    // the same signature), each with its libraries, before the press is
    // spent: a refusal costs nothing.
    let (view, home_in) = workspace::view(owner, ws)?;
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    let mut libs: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    let names = std::iter::once(m.entry().to_string()).chain(m.workers.iter().map(|w| w.entry.clone()));
    for entry in names {
        let entry = entry.trim_start_matches('/').to_string();
        if entries.iter().any(|(e, _)| *e == entry) { continue }
        let bytes = read_file(open_beneath(&proj, &entry, libc::O_RDONLY)?, &entry, ENTRY_MAX)?;
        let in_jail = format!("{home_in}/{}/{}{entry}", workspace::WORK,
                              if project.is_empty() || project == "." { String::new() } else { format!("{project}/") });
        for (at, b) in closure(&bytes, &in_jail, &view)? {
            if !libs.iter().any(|(p, _)| *p == at) { libs.push((at, b)) }
        }
        if libs.len() > LIBS_MAX {
            return Err(format!("more than {LIBS_MAX} libraries"));
        }
        entries.push((entry, bytes));
    }
    let dev = dev_id(owner, ws, &id);
    // The installed manifest: the same, under the dev id.
    let mut v: toml::Value = text.parse().map_err(|e| format!("atrium.toml: {e}"))?;
    v.get_mut("app").and_then(|a| a.as_table_mut()).ok_or("atrium.toml has no [app]")?
        .insert("id".into(), toml::Value::String(dev.clone()));
    let manifest = toml::to_string(&v).map_err(|e| e.to_string())?;
    portcullis_toml::Manifest::from_str(&manifest).map_err(|e| format!("the dev manifest: {e}"))?;
    Ok(Prepared { id: dev, manifest, entries, libs })
}

/// Install a prepared bundle (replacing the last Run's), signed by the dev
/// publisher.
pub fn install(p: &Prepared) -> Result<PathBuf, String> {
    let key = dev_key()?;
    let tree = Path::new(APPS_DIR).join(&p.id);
    let tmp = Path::new(APPS_DIR).join(format!(".{}.new", p.id));
    let _ = fs::remove_dir_all(&tmp);
    let e = |e: std::io::Error| e.to_string();
    fs::create_dir_all(&tmp).map_err(e)?;
    fs::write(tmp.join("atrium.toml"), &p.manifest).map_err(e)?;
    let sig = portcullis_sig::sign(p.manifest.as_bytes(), &key).map_err(|e| format!("sign: {e:?}"))?;
    fs::write(tmp.join("atrium.toml.sig"), sig).map_err(e)?;
    for (entry, bytes) in &p.entries {
        let ep = tmp.join(entry);
        if let Some(d) = ep.parent() {
            fs::create_dir_all(d).map_err(e)?;
        }
        fs::write(&ep, bytes).map_err(e)?;
        fs::set_permissions(&ep, fs::Permissions::from_mode(0o555)).map_err(e)?;
    }
    // An app's jail is its bundle: the entry's libraries and the loader go
    // in too, as the workspace's loader found them.
    for (at, bytes) in &p.libs {
        let to = tmp.join(at);
        if let Some(d) = to.parent() {
            fs::create_dir_all(d).map_err(e)?;
        }
        fs::write(&to, bytes).map_err(e)?;
        fs::set_permissions(&to, fs::Permissions::from_mode(0o555)).map_err(e)?;
    }
    let _ = fs::remove_dir_all(&tree);
    fs::rename(&tmp, &tree).map_err(e)?;
    Ok(tree)
}

/// The dev bundles of `owner`'s workspace `ws`.
pub fn bundles_of(owner: &str, ws: &str) -> Vec<String> {
    let pre = dev_id(owner, ws, "");
    fs::read_dir(APPS_DIR).map(|d| d.flatten().filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| n.starts_with(&pre)).collect()).unwrap_or_default()
}

/// Remove a dev bundle.
pub fn remove(id: &str) {
    if portcullisd::manifest_trust::is_dev(id) && !id.contains('/') {
        let _ = fs::remove_dir_all(Path::new(APPS_DIR).join(id));
    }
}

/// Where the loader looks after an object's own paths: FreeBSD's default
/// path, and `/usr/local/lib` as ldconfig's hints have it.
const DEFAULT_LIB_DIRS: [&str; 3] = ["/lib", "/usr/lib", "/usr/local/lib"];
/// Libraries a bundle may carry, and their bytes in all.
const LIBS_MAX: usize = 512;
const LIBS_BYTES_MAX: u64 = 2 * 1024 * 1024 * 1024;

/// Open `path` (a path in the workspace's jail) through its view, or
/// `None` if it isn't there. A link out of a folder the workspace can
/// write is an error, as is a path outside every folder of the view.
fn open_in_view(view: &[workspace::ViewMount], path: &str) -> Result<Option<(OwnedFd, bool)>, String> {
    let m = view.iter().find(|m| m.in_jail == "/" || path == m.in_jail || path.starts_with(&format!("{}/", m.in_jail)))
        .ok_or_else(|| format!("{path}: not in the workspace"))?;
    let rel = path[m.in_jail.len()..].trim_start_matches('/');
    let host = m.host.join(rel);
    if !m.untrusted {
        return match fs::File::open(&host) {
            Ok(f) => Ok(Some((OwnedFd::from(f), false))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{path}: {e}")),
        };
    }
    let c = std::ffi::CString::new(m.host.as_os_str().as_encoded_bytes()).map_err(|_| "a NUL".to_string())?;
    // SAFETY: a NUL-terminated path. The folder itself must not be a link.
    let dfd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    if dfd < 0 {
        return Ok(None);
    }
    // SAFETY: just opened, ours alone.
    let dir = unsafe { OwnedFd::from_raw_fd(dfd) };
    if rel.is_empty() || rel.split('/').any(|c| c == "..") {
        return Err(format!("{path}: not a library path"));
    }
    let r = std::ffi::CString::new(rel).map_err(|_| "a NUL".to_string())?;
    // SAFETY: a valid directory descriptor and a NUL-terminated path.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), r.as_ptr(), libc::O_RDONLY | libc::O_RESOLVE_BENEATH | libc::O_CLOEXEC) };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        return match err.raw_os_error() {
            Some(libc::ENOENT) | Some(libc::ENOTDIR) => Ok(None),
            Some(libc::ENOTCAPABLE) | Some(libc::EXDEV) =>
                Err(format!("{path} is reached through a link out of {}: refused", m.in_jail)),
            _ => Err(format!("{path}: {err}")),
        };
    }
    // SAFETY: just opened, ours alone.
    Ok(Some((unsafe { OwnedFd::from_raw_fd(fd) }, true)))
}

/// The folder part of a path in the jail.
fn dir_of(p: &str) -> &str {
    match p.rfind('/') {
        Some(0) => "/",
        Some(i) => &p[..i],
        None => "/",
    }
}

/// The loader and the entry's `DT_NEEDED` closure, found as the workspace's
/// loader would, read now: (path in the bundle, bytes). `entry_in_jail` is
/// where the entry is in the workspace, for `$ORIGIN`. Parsed, never run.
fn closure(entry: &[u8], entry_in_jail: &str, view: &[workspace::ViewMount]) -> Result<Vec<(PathBuf, Vec<u8>)>, String> {
    let first = elf_dynamic(entry, Path::new("the entry"))?;
    if first.needed.is_empty() {
        return Ok(Vec::new()); // static
    }
    let mut out: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    let mut total: u64 = 0;
    let ld = fs::read("/libexec/ld-elf.so.1").map_err(|e| format!("the loader: {e}"))?;
    out.push((PathBuf::from("libexec/ld-elf.so.1"), ld));
    let mut seen = std::collections::BTreeSet::new();
    // (soname, who needs it, that object's search path)
    let paths = |d: &Dynamic, origin: &str| -> Vec<String> {
        let own = if d.runpath.is_some() { &d.runpath } else { &d.rpath };
        let mut v: Vec<String> = own.as_deref().unwrap_or("").split(':').filter(|p| !p.is_empty())
            .filter_map(|p| {
                let p = p.replace("${ORIGIN}", origin).replace("$ORIGIN", origin);
                (p.starts_with('/') && !p.contains('$')).then_some(p)
            }).collect();
        v.extend(DEFAULT_LIB_DIRS.iter().map(|s| s.to_string()));
        v
    };
    let mut queue: Vec<(String, String, Vec<String>)> = first.needed.iter()
        .map(|n| (n.clone(), entry_in_jail.to_string(), paths(&first, dir_of(entry_in_jail)))).collect();
    while let Some((so, by, dirs)) = queue.pop() {
        if !seen.insert(so.clone()) {
            continue;
        }
        if so.contains('/') || so.starts_with('.') || so.is_empty() {
            return Err(format!("{by} needs {so:?}: not a library name"));
        }
        if seen.len() > LIBS_MAX {
            return Err(format!("more than {LIBS_MAX} libraries"));
        }
        let mut found = None;
        for d in &dirs {
            let at = format!("{}/{so}", d.trim_end_matches('/'));
            if let Some((fd, untrusted)) = open_in_view(view, &at)? {
                found = Some((fd, untrusted, at));
                break;
            }
        }
        let (fd, untrusted, at) = found.ok_or_else(|| format!("{by} needs {so}, which isn't in {}", dirs.join(", ")))?;
        let bytes = read_file(fd, &at, ENTRY_MAX)?;
        total += bytes.len() as u64;
        if total > LIBS_BYTES_MAX {
            return Err(format!("its libraries pass {LIBS_BYTES_MAX} bytes"));
        }
        let d = elf_dynamic(&bytes, Path::new(&at))?;
        for n in &d.needed {
            queue.push((n.clone(), at.clone(), paths(&d, dir_of(&at))));
        }
        // The base's keep their place; the workspace's go where the app's
        // loader looks without hints or the workspace's paths.
        let dest = if untrusted { PathBuf::from("usr/lib").join(&so) } else { PathBuf::from(at.trim_start_matches('/')) };
        out.push((dest, bytes));
    }
    Ok(out)
}

// A minimal ELF64 reader, as opifex's (opifex/src/main.rs), with every
// offset checked: the entry is the workspace's, so untrusted.

fn rd_u16(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}
fn rd_u64(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

/// What the loader reads from an object's dynamic section.
#[derive(Default)]
struct Dynamic {
    needed:  Vec<String>,
    rpath:   Option<String>,
    runpath: Option<String>,
}

/// The DT_NEEDED sonames and DT_RPATH/DT_RUNPATH of an ELF64 little-endian
/// object. A static binary (no PT_DYNAMIC) has none, which is not an error.
fn elf_dynamic(b: &[u8], what: &Path) -> Result<Dynamic, String> {
    let bad = |m: &str| format!("{}: {m}", what.display());
    if b.len() < 64 || &b[0..4] != b"\x7fELF" {
        return Err(bad("not an ELF file"));
    }
    if b[4] != 2 || b[5] != 1 {
        return Err(bad("not ELF64 little-endian"));
    }
    let phoff = rd_u64(b, 0x20).ok_or_else(|| bad("truncated e_phoff"))? as usize;
    let phentsize = rd_u16(b, 0x36).ok_or_else(|| bad("truncated e_phentsize"))? as usize;
    let phnum = rd_u16(b, 0x38).ok_or_else(|| bad("truncated e_phnum"))? as usize;

    // Collect PT_LOAD segments so a vaddr can be mapped back to a file offset,
    // and locate PT_DYNAMIC.
    let mut loads: Vec<(u64, u64, u64)> = Vec::new(); // (vaddr, filesz, offset)
    let mut dynamic: Option<(usize, usize)> = None; // (offset, filesz)
    for i in 0..phnum {
        let ph = phoff.checked_add(i.checked_mul(phentsize).ok_or_else(|| bad("phdr out of range"))?).ok_or_else(|| bad("phdr out of range"))?;
        let p_type = rd_u16(b, ph).ok_or_else(|| bad("truncated phdr"))? as u32
            | (rd_u16(b, ph + 2).ok_or_else(|| bad("truncated phdr"))? as u32) << 16;
        let p_offset = rd_u64(b, ph + 0x08).ok_or_else(|| bad("truncated p_offset"))?;
        let p_vaddr = rd_u64(b, ph + 0x10).ok_or_else(|| bad("truncated p_vaddr"))?;
        let p_filesz = rd_u64(b, ph + 0x20).ok_or_else(|| bad("truncated p_filesz"))?;
        match p_type {
            1 => loads.push((p_vaddr, p_filesz, p_offset)), // PT_LOAD
            2 => dynamic = Some((p_offset as usize, p_filesz as usize)), // PT_DYNAMIC
            _ => {}
        }
    }
    let Some((dyn_off, dyn_sz)) = dynamic else {
        return Ok(Dynamic::default()); // static binary
    };

    let vaddr_to_off = |v: u64| -> Option<usize> {
        for (vaddr, filesz, offset) in &loads {
            if v >= *vaddr && v < vaddr.saturating_add(*filesz) {
                return Some(offset.checked_add(v - vaddr)? as usize);
            }
        }
        None
    };

    // First pass for DT_STRTAB (a vaddr), second for the DT_NEEDED offsets.
    let mut strtab: Option<usize> = None;
    let mut needed_offs: Vec<u64> = Vec::new();
    let (mut rpath_off, mut runpath_off) = (None, None);
    let mut i = dyn_off;
    while i + 16 <= dyn_off.saturating_add(dyn_sz) {
        let tag = rd_u64(b, i).ok_or_else(|| bad("truncated Elf64_Dyn"))?;
        let val = rd_u64(b, i + 8).ok_or_else(|| bad("truncated Elf64_Dyn"))?;
        match tag {
            0 => break,                       // DT_NULL
            1 => needed_offs.push(val),       // DT_NEEDED
            5 => strtab = vaddr_to_off(val),  // DT_STRTAB
            15 => rpath_off = Some(val),      // DT_RPATH
            29 => runpath_off = Some(val),    // DT_RUNPATH
            _ => {}
        }
        i += 16;
    }
    let Some(strtab) = strtab else {
        if needed_offs.is_empty() {
            return Ok(Dynamic::default());
        }
        return Err(bad("has DT_NEEDED but no resolvable DT_STRTAB"));
    };
    let string = |n: u64| -> Result<String, String> {
        let start = strtab.checked_add(n as usize).ok_or_else(|| bad("string out of range"))?;
        let rest = b.get(start..).ok_or_else(|| bad("string out of range"))?;
        let end = rest.iter().position(|&c| c == 0).ok_or_else(|| bad("unterminated string"))?;
        Ok(String::from_utf8_lossy(&rest[..end]).into_owned())
    };
    Ok(Dynamic {
        needed:  needed_offs.into_iter().map(string).collect::<Result<_, _>>()?,
        rpath:   rpath_off.map(string).transpose()?,
        runpath: runpath_off.map(string).transpose()?,
    })
}
