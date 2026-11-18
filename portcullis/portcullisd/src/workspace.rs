//! Workspaces (docs/spec/homes.md §5): a person's own development jails.
//!
//! A workspace is a jail on the base system (read-only), with a writable
//! root of its own, the person's `work` folder at `~/work`, and the `work`
//! of each workspace it uses, read-only, at `/workspaces/<name>`. It runs as
//! its own uid (per person and workspace, as an app's), which is in the
//! person's files group, so the file manager and the workspace can hand
//! files to each other through `work`.
//!
//! On disk, under the person's home (root's `~/.atrium`):
//!
//! ```text
//! .atrium/workspaces/              root:files-<person> 0750
//!   <name>/                        root:files-<person> 0750
//!     work/                        <person>:files-<person> 2770   ~/work
//!     .state/                      root 0700
//!       workspace.toml             the record
//!       root/                      the jail's writable root
//! ```
//!
//! The file manager is given `.atrium/workspaces` and sees each `work`,
//! never `.state`. Its jail is created with `persist` and stays until
//! stopped (or the machine stops); a console joins it.
//!
//! Toolchains: the base system has C (cc, ld, make) and the shell; the
//! others come as toolchain layers (dev-tools-plan.md M3): signed,
//! read-only trees Opifex installs at `/var/lib/atrium/layers/<name>@<ver>`,
//! mounted read-only at `/atrium/toolchains/<name>` and put first on the
//! workspace's `PATH`. The record names them (`layers = ["rust@1.100.0"]`);
//! a toolchain is available once its layer is installed. Personas (personas.md, a proposal) will key a workspace by
//! persona: the record carries one, "personal".

use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::{egress, launch};

/// The one made for everyone at the first ask.
pub const SCRATCH: &str = "scratch";
/// Where the jails' roots are assembled.
pub const JAILS: &str = "/var/lib/atrium/workspaces";
/// Where a workspace's `work` is, under its uid's home in the jail.
pub const WORK: &str = "work";
/// Where the file manager (`home-files`) finds them.
pub const IN_FILE_MANAGER: &str = "atrium/workspaces";
/// Where the workspaces it uses appear.
pub const LINKS: &str = "workspaces";

/// (id, name, the layer it needs: None = the base system has it).
pub const TOOLCHAINS: &[(&str, &str, Option<&str>)] = &[
    ("c", "C (cc, make)", None),
    ("shell", "Plain shell", None),
    ("rust", "Rust", Some("rust")),
    ("python", "Python", Some("python")),
    ("go", "Go", Some("go")),
    ("node", "Node", Some("node")),
];

/// Layers a new workspace of a toolchain gets besides its own, when
/// installed: the C tools (CMake, Meson, pkgconf, git) for C and Rust.
const EXTRA_LAYERS: &[(&str, &str)] = &[("c", "c-dev"), ("rust", "c-dev")];

/// Where installed layers are (`<state root>/layers`).
pub const LAYERS_ROOT: &str = "/var/lib/atrium";
/// The base system's `PATH` in a workspace; layers go before it.
pub const BASE_PATH: &str = "/usr/local/bin:/usr/bin:/bin:/usr/local/sbin:/usr/sbin:/sbin";

/// The installed layers for this machine's arch: (reference, manifest).
pub fn installed_layers() -> Vec<(String, portcullis_toml::layer::LayerManifest)> {
    installed_layers_in(Path::new(LAYERS_ROOT))
}

fn installed_layers_in(root: &Path) -> Vec<(String, portcullis_toml::layer::LayerManifest)> {
    use portcullis_toml::layer as tl;
    let mut v = Vec::new();
    let Ok(rd) = fs::read_dir(root.join(tl::INSTALLED)) else { return v };
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if let Ok(m) = load_layer_in(root, &n) {
            v.push((n, m));
        }
    }
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

/// One installed layer's manifest, checked: its folder is root's and
/// not a link, it is packed, it is for this arch, and its name matches.
fn load_layer_in(root: &Path, reference: &str) -> Result<portcullis_toml::layer::LayerManifest, String> {
    use portcullis_toml::layer as tl;
    let d = tl::installed_dir(root, reference).ok_or_else(|| format!("{reference:?}: name@version"))?;
    let md = fs::symlink_metadata(&d).map_err(|_| format!("layer {reference} is not installed"))?;
    if !md.is_dir() || (cfg!(target_os = "freebsd") && md.uid() != 0) {
        return Err(format!("{} is not a root-owned folder", d.display()));
    }
    let t = fs::read_to_string(d.join("atrium.toml")).map_err(|e| format!("layer {reference}: {e}"))?;
    let m = tl::LayerManifest::from_str(&t).map_err(|e| format!("layer {reference}: {e}"))?;
    m.require_content()?;
    if m.reference() != reference {
        return Err(format!("layer {reference}: its manifest says {}", m.reference()));
    }
    if m.layer.arch != tl::host_arch() {
        return Err(format!("layer {reference} is for {}", m.layer.arch));
    }
    Ok(m)
}

/// The newest installed version of layer `name` (by version order).
fn newest_layer(installed: &[(String, portcullis_toml::layer::LayerManifest)], name: &str) -> Option<String> {
    installed.iter().filter(|(_, m)| m.layer.name == name)
        .max_by(|a, b| version_cmp(&a.1.layer.version, &b.1.layer.version))
        .map(|(r, _)| r.clone())
}

/// Dotted versions compared number by number ("1.100.0" > "1.99.1").
fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let parts = |s: &str| s.split(['.', '-', '+']).map(|p| p.parse::<u64>().map_err(|_| p.to_string())).collect::<Vec<_>>();
    let (pa, pb) = (parts(a), parts(b));
    for (x, y) in pa.iter().zip(pb.iter()) {
        let o = match (x, y) {
            (Ok(x), Ok(y)) => x.cmp(y),
            (Ok(_), Err(_)) => std::cmp::Ordering::Greater,
            (Err(_), Ok(_)) => std::cmp::Ordering::Less,
            (Err(x), Err(y)) => x.cmp(y),
        };
        if o.is_ne() {
            return o;
        }
    }
    pa.len().cmp(&pb.len())
}

/// The toolchains a New workspace offers: (id, name, available now).
pub fn toolchains() -> Vec<(&'static str, &'static str, bool)> {
    let installed = installed_layers();
    TOOLCHAINS.iter().map(|(id, n, layer)| (*id, *n, layer.is_none_or(|l| newest_layer(&installed, l).is_some()))).collect()
}

/// A workspace's layers composed: PATH, environment and mounts.
fn compose_layers(r: &Record) -> Result<(portcullis_toml::layer::Composed, Vec<(PathBuf, String)>), String> {
    use portcullis_toml::layer as tl;
    let root = Path::new(LAYERS_ROOT);
    let mut ms = Vec::new();
    let mut dirs = Vec::new();
    for l in &r.layers {
        let m = load_layer_in(root, l)?;
        dirs.push((tl::installed_dir(root, l).unwrap_or_default(), m.layer.mount.clone()));
        ms.push(m);
    }
    Ok((tl::compose(&ms, BASE_PATH)?, dirs))
}

/// The base system, read-only in every workspace.
const BASE: &[&str] = &[
    "bin", "lib", "libexec", "sbin",
    "usr/bin", "usr/include", "usr/lib", "usr/lib32", "usr/libdata", "usr/libexec", "usr/sbin", "usr/share",
];

/// pkg's repository in every workspace.
const PKG_REPO: &str = "FreeBSD: {\n  url: \"pkg+https://pkg.FreeBSD.org/${ABI}/latest\",\n  mirror_type: \"srv\",\n  signature_type: \"fingerprints\",\n  fingerprints: \"/usr/share/keys/pkg\",\n  enabled: yes\n}\n";

/// Where the workspace's own tools are on the host, and in the jail, first
/// on `PATH`: the `pkg` that asks for a press (dev-tools-plan.md M2).
pub const WS_BIN: &str = "/var/db/atrium/workspace-bin";
pub const WS_BIN_IN_JAIL: &str = "atrium/bin";

/// `pkg` in a workspace. Installing needs the jail's root, which only
/// portcullisd gives, after a press: this asks the terminal it runs in
/// (an OSC 7770 request, homes.md §5.5). The terminal shows what it asks,
/// and on the person's press runs it (`WorkspaceAdmin`), shows pkg's output,
/// and answers `ESC ] 7770 ; ok ; <status> BEL CR` or `ESC ] 7770 ; cancel BEL CR`.
const PKG_WRAPPER: &str = r#"#!/bin/sh
# pkg in an Atrium workspace (docs/spec/dev-tools.md §6.1). Installing runs as
# this workspace's root, never the computer's, and only after you press OK.
real=/usr/local/sbin/pkg
case "$1" in
info|search|query|which|version|audit|stats)
    [ -x "$real" ] && exec "$real" "$@" ;;
esac
for a in "$@"; do
    case "$a" in
    ""|*[!A-Za-z0-9._+@/:=,~^*-]*) echo "pkg: "$a" isn't a package name or option" >&2; exit 64 ;;
    esac
done
if [ ! -t 0 ] || [ ! -t 1 ]; then
    echo "pkg: installing needs your OK; run pkg in a Terminal window" >&2
    exit 1
fi
old=$(stty -g)
trap 'stty "$old"; echo; exit 130' INT TERM
echo "pkg: asking for your OK in this window (Ctrl-C to cancel)"
stty -echo
printf ']7770;pkg;%s' "$*"
IFS= read -r reply
stty "$old"
printf '\033[1A\r\033[2K'    # the "asking" line, answered
case "$reply" in
*"]7770;ok;"*) s=${reply##*;}; s=${s%%[!0-9]*}; exit "${s:-1}" ;;
*"]7770;cancel"*) echo "pkg: cancelled"; exit 1 ;;
*) echo "pkg: this terminal can't ask for your OK" >&2; exit 1 ;;
esac
"#;

/// `atrium-run [folder]` (and `make run`, atrium.app.mk): Run what this
/// workspace built in `~/work/<folder>`, asked of the terminal as `pkg` is
/// (OSC 7770 `run`, homes.md §5.5): the terminal shows what it asks, and
/// on the person's press sends `WorkspaceRun` for its tab's workspace.
const RUN_WRAPPER: &str = r#"#!/bin/sh
# Run what this workspace built, as an app (docs/spec/homes.md §5.3), after
# you press Run in the Terminal. The folder is in ~/work (default: ~/work).
p=${1:-.}
case "$p" in
""|/*|*..*|*[!A-Za-z0-9._/-]*) echo "atrium-run: "$p" isn't a folder in ~/work" >&2; exit 64 ;;
esac
# A terminal asks the person; so does an IDE that runs this as a task
# (ATRIUM_ASK=1: Artifex reads the request in the output and answers).
tty=
if [ -t 0 ] && [ -t 1 ]; then
    tty=1
elif [ "${ATRIUM_ASK:-}" != 1 ]; then
    echo "atrium-run: Run needs your OK; run it in a Terminal window" >&2
    exit 1
fi
if [ -n "$tty" ]; then
    old=$(stty -g)
    trap 'stty "$old"; echo; exit 130' INT TERM
    echo "atrium-run: asking for your OK in this window (Ctrl-C to cancel)"
    stty -echo
fi
printf ']7770;run;%s' "$p"
IFS= read -r reply
[ -z "$tty" ] || stty "$old"
[ -z "$tty" ] || printf '[1A
[2K'
case "$reply" in
*"]7770;ok;"*) s=${reply##*;}; s=${s%%[!0-9]*}; exit "${s:-1}" ;;
*"]7770;cancel"*) echo "atrium-run: cancelled"; exit 1 ;;
*) echo "atrium-run: this terminal can't ask for your OK" >&2; exit 1 ;;
esac
"#;

/// Write the workspace tools (once per change), root's and read-only.
fn ensure_ws_bin() -> Result<PathBuf, String> {
    let dir = PathBuf::from(WS_BIN);
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o755));
    for (name, text) in [("pkg", PKG_WRAPPER), ("atrium-run", RUN_WRAPPER)] {
        let f = dir.join(name);
        if fs::read_to_string(&f).ok().as_deref() != Some(text) {
            let tmp = dir.join(format!(".{name}.new"));
            fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o555)).map_err(|e| e.to_string())?;
            fs::rename(&tmp, &f).map_err(|e| e.to_string())?;
        }
    }
    // git's credential helper for Arca's HTTPS tokens (M9), from the host's
    // install; none installed, none offered.
    let src = Path::new(GIT_CREDENTIAL_ARCA);
    let f = dir.join("git-credential-arca");
    match fs::read(src) {
        Ok(b) if fs::read(&f).ok().as_deref() != Some(&b[..]) => {
            let tmp = dir.join(".git-credential-arca.new");
            fs::write(&tmp, &b).map_err(|e| format!("{}: {e}", tmp.display()))?;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o555)).map_err(|e| e.to_string())?;
            fs::rename(&tmp, &f).map_err(|e| e.to_string())?;
        }
        Ok(_) => {}
        Err(_) => {
            let _ = fs::remove_file(&f);
        }
    }
    Ok(dir)
}

/// Where the host has git-credential-arca (scripts/ssh-agent-service.sh).
const GIT_CREDENTIAL_ARCA: &str = "/usr/local/bin/git-credential-arca";

/// Files the jail's `/etc` starts with, from the host's.
const ETC: &[&str] = &[
    "login.conf", "login.conf.db", "resolv.conf", "hosts", "services", "protocols",
    "shells", "nsswitch.conf", "libmap.conf", "networks",
    // Package scripts source these (pkg's own post-install does).
    "rc.subr", "defaults/rc.conf",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub name:      String,
    pub toolchain: String,
    #[serde(default = "personal")]
    pub persona:   String,
    /// "outbound", "registries" (only the hosts below, through the proxy:
    /// dev-tools.md §5.2) or "none".
    pub network:   String,
    #[serde(default)]
    pub uses:      Vec<String>,
    /// Share compiled objects (ccache, sccache) with the workspaces it is
    /// linked to by uses that share them too (homes.md §5.4, W9). Off by
    /// default.
    #[serde(default, rename = "object-cache")]
    pub object_cache: bool,
    /// Toolchain layers it mounts, `name@version` (dev-tools-plan.md M3).
    #[serde(default)]
    pub layers: Vec<String>,
    /// What a `registries` workspace may reach (port 443, names matched
    /// exactly). Empty: [`DEFAULT_HOSTS`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<String>,
}

/// The registries a `registries` workspace reaches when it lists none:
/// crates.io, GitHub (git and release downloads), rustup's and FreeBSD's
/// package servers.
pub const DEFAULT_HOSTS: &[&str] = &[
    "crates.io", "index.crates.io", "static.crates.io",
    "github.com", "codeload.github.com", "objects.githubusercontent.com",
    "static.rust-lang.org", "pkg.freebsd.org",
];

impl Record {
    /// The hosts its proxy admits.
    pub fn egress_hosts(&self) -> Vec<String> {
        if self.hosts.is_empty() { DEFAULT_HOSTS.iter().map(|h| h.to_string()).collect() } else { self.hosts.clone() }
    }
}

/// A host name as the list takes it: letters, digits, dots and dashes, no
/// wildcard, no address literal.
pub fn valid_host(h: &str) -> bool {
    !h.is_empty() && h.len() <= 253 && !h.starts_with(['.', '-']) && !h.ends_with('.')
        && h.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
        && h.bytes().any(|b| b.is_ascii_lowercase())
}

/// Where a workspace's group cache is in its jail.
pub const OBJECTS_IN_JAIL: &str = "cache/objects";

/// The group `name` shares compiled objects with (itself included):
/// the workspaces with the cache on, joined by uses links (either way)
/// through workspaces that have it on too. Empty when its cache is off.
pub fn object_group(all: &[Record], name: &str) -> Vec<String> {
    let on: Vec<&Record> = all.iter().filter(|r| r.object_cache).collect();
    if !on.iter().any(|r| r.name == name) {
        return Vec::new();
    }
    let linked = |a: &Record, b: &Record| a.uses.contains(&b.name) || b.uses.contains(&a.name);
    let mut group = vec![name.to_string()];
    let mut i = 0;
    while i < group.len() {
        let cur = on.iter().find(|r| r.name == group[i]).unwrap();
        for r in &on {
            if !group.contains(&r.name) && linked(cur, r) {
                group.push(r.name.clone());
            }
        }
        i += 1;
    }
    group.sort();
    group
}

/// The folder a group's objects are in, named by its members: a group
/// that changes gets a new, empty one, so what an old member put there
/// never reaches a new group.
pub fn group_dir_name(group: &[String]) -> String {
    let joined = group.join("+");
    if joined.len() <= 200 {
        return joined;
    }
    // Names are lowercase letters, digits and dashes: `+` never in one.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in joined.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    format!("{}+{:016x}", &group[0], h)
}

fn personal() -> String { "personal".into() }

/// A name: what a folder, a jail and a prompt can all carry.
pub fn valid_name(n: &str) -> bool {
    let b = n.as_bytes();
    !b.is_empty() && b.len() <= 32
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

pub fn valid_network(n: &str) -> bool { matches!(n, "outbound" | "registries" | "none") }

/// The console target of a workspace.
pub fn target(name: &str) -> String { format!("{}{name}", crate::console::WORKSPACE_PREFIX) }

/// The id its uid is registered under (and its host identity derived
/// from): `/` is in no app id, so it never names an app.
fn reg_id(name: &str) -> String { target(name) }

/// Its synthetic manifest id; the jail is named after it (`.` → `_`).
fn manifest_id(owner: &str, name: &str) -> String { format!("ws.{owner}.{name}") }

pub fn jail_name(owner: &str, name: &str) -> String {
    portcullis_jail::jail_name_from_app_id(&manifest_id(owner, name))
}

pub fn jail_path(owner: &str, name: &str) -> PathBuf { Path::new(JAILS).join(owner).join(name) }

/// `.atrium/workspaces`, made root's with the files group.
pub fn root_dir(home: &Path, files_gid: u32) -> Result<PathBuf, String> {
    let d = home.join(".atrium").join("workspaces");
    own_dir(&d, 0, files_gid, 0o750)?;
    Ok(d)
}

/// Make `d` (one level) or put it right: a folder, not a link, ours.
fn own_dir(d: &Path, uid: u32, gid: u32, mode: u32) -> Result<(), String> {
    match fs::symlink_metadata(d) {
        Ok(m) if !m.is_dir() => return Err(format!("{} is not a folder", d.display())),
        Ok(m) if m.uid() != uid && m.uid() != 0 => return Err(format!("{} is someone else's", d.display())),
        Ok(_) => {}
        Err(_) => fs::DirBuilder::new().mode(0o700).create(d).map_err(|e| format!("create {}: {e}", d.display()))?,
    }
    std::os::unix::fs::chown(d, Some(uid), Some(gid)).map_err(|e| format!("chown {}: {e}", d.display()))?;
    fs::set_permissions(d, fs::Permissions::from_mode(mode)).map_err(|e| format!("chmod {}: {e}", d.display()))
}

fn person_uid(person: &str) -> Option<u32> {
    let o = Command::new("id").args(["-u", person]).output().ok()?;
    if !o.status.success() {
        return None;
    }
    String::from_utf8_lossy(&o.stdout).trim().parse().ok()
}

/// The person's home and files group (made if missing).
fn person(owner: &str) -> Result<(PathBuf, u32), String> {
    crate_home::ensure_person(owner)
}

use portcullisd::home as crate_home;

fn record_path(dir: &Path) -> PathBuf { dir.join(".state").join("workspace.toml") }

fn load(dir: &Path) -> Option<Record> {
    let t = fs::read_to_string(record_path(dir)).ok()?;
    toml::from_str(&t).ok()
}

fn save(dir: &Path, r: &Record) -> Result<(), String> {
    let p = record_path(dir);
    let t = toml::to_string(r).map_err(|e| e.to_string())?;
    let tmp = p.with_extension("new");
    fs::write(&tmp, t).and_then(|_| fs::rename(&tmp, &p)).map_err(|e| format!("write {}: {e}", p.display()))
}

/// Make a workspace's folders (work, .state) for `owner`.
fn make_dirs(owner: &str, home: &Path, gid: u32, name: &str) -> Result<PathBuf, String> {
    let root = root_dir(home, gid)?;
    let dir = root.join(name);
    own_dir(&dir, 0, gid, 0o750)?;
    let uid = person_uid(owner).ok_or_else(|| format!("no such person {owner}"))?;
    own_dir(&dir.join(WORK), uid, gid, 0o2770)?;
    own_dir(&dir.join(".state"), 0, 0, 0o700)?;
    Ok(dir)
}

/// The person's workspaces; Scratch is made the first time.
pub fn list(owner: &str) -> Result<Vec<(Record, bool)>, String> {
    let (home, gid) = person(owner)?;
    let root = root_dir(&home, gid)?;
    if load(&root.join(SCRATCH)).is_none() {
        create(owner, Record {
            name: SCRATCH.into(), toolchain: "c".into(), persona: personal(),
            network: "outbound".into(), uses: vec![], object_cache: false, layers: vec![], hosts: vec![],
        })?;
    }
    let mut v = Vec::new();
    for e in fs::read_dir(&root).map_err(|e| format!("{}: {e}", root.display()))?.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if !valid_name(&n) {
            continue;
        }
        if let Some(r) = load(&e.path()) {
            let running = running_jid(owner, &r.name).is_some();
            v.push((r, running));
        }
    }
    // Scratch first, then by name.
    v.sort_by(|a, b| (a.0.name != SCRATCH, &a.0.name).cmp(&(b.0.name != SCRATCH, &b.0.name)));
    Ok(v)
}

fn check(owner: &str, home: &Path, r: &Record) -> Result<(), String> {
    if !valid_name(&r.name) {
        return Err(format!("{:?}: a name is up to 32 lowercase letters, digits and dashes", r.name));
    }
    match TOOLCHAINS.iter().find(|t| t.0 == r.toolchain) {
        None => return Err(format!("no toolchain {:?}", r.toolchain)),
        Some((_, n, Some(l))) if !r.layers.iter().any(|x| x.split('@').next() == Some(*l)) =>
            return Err(format!("{n} needs its toolchain layer ({l}); none is installed")),
        Some(_) => {}
    }
    compose_layers(r)?;
    if !valid_network(&r.network) {
        return Err(format!("network {:?}: outbound, registries or none", r.network));
    }
    if let Some(h) = r.hosts.iter().find(|h| !valid_host(h)) {
        return Err(format!("host {h:?}: a name in lowercase, no wildcard or address"));
    }
    for u in &r.uses {
        if *u == r.name {
            return Err("a workspace can't use itself".into());
        }
        let Some(other) = load(&home.join(".atrium/workspaces").join(u)).filter(|_| valid_name(u)) else {
            return Err(format!("no workspace {u:?} of {owner}'s to use"));
        };
        // personas.md §6: crossing personas is off by default.
        if other.persona != r.persona {
            return Err(format!("{u} is {}'s, {} is {}'s: links don't cross personas", other.persona, r.name, r.persona));
        }
    }
    Ok(())
}

pub fn create(owner: &str, mut r: Record) -> Result<Record, String> {
    let (home, gid) = person(owner)?;
    // Its toolchain's layer and the extras, newest installed, unless named.
    if r.layers.is_empty() {
        let installed = installed_layers();
        let own = TOOLCHAINS.iter().find(|t| t.0 == r.toolchain).and_then(|t| t.2);
        for name in own.into_iter().chain(EXTRA_LAYERS.iter().filter(|(t, _)| *t == r.toolchain).map(|(_, l)| *l)) {
            if let Some(v) = newest_layer(&installed, name) {
                r.layers.push(v);
            }
        }
    }
    check(owner, &home, &r)?;
    let dir = root_dir(&home, gid)?.join(&r.name);
    if load(&dir).is_some() {
        return Err(format!("{owner} has a workspace {} already", r.name));
    }
    let dir = make_dirs(owner, &home, gid, &r.name)?;
    save(&dir, &r)?;
    // §3: a project carries what reproduces its workspace.
    let uid = person_uid(owner).unwrap_or(0);
    let meta = dir.join(WORK).join(".atrium");
    if fs::symlink_metadata(&meta).is_err() && fs::create_dir(&meta).is_ok() {
        let _ = std::os::unix::fs::chown(&meta, Some(uid), Some(gid));
        let _ = fs::set_permissions(&meta, fs::Permissions::from_mode(0o2770));
        let text = format!("# Made by New workspace (homes.md §5; atrium-devjail.md §3).\nname = {:?}\n\n[toolchain]\nlanguage = {:?}\nlayers = {:?}\n\n[network]\noutbound = {:?}\n",
                           r.name, r.toolchain, r.layers, match r.network.as_str() { "none" => "none", "registries" => "registries", _ => "any" });
        let f = meta.join("devjail.toml");
        if fs::write(&f, text).is_ok() {
            let _ = std::os::unix::fs::chown(&f, Some(uid), Some(gid));
            let _ = fs::set_permissions(&f, fs::Permissions::from_mode(0o660));
        }
    }
    eprintln!("portcullisd: workspace {} made for {owner} ({}, network {})", r.name, r.toolchain, r.network);
    Ok(r)
}

/// Duplicate `from` as `to` (homes.md §5.4, W8): its record (named `to`),
/// its `work` and its writable root, build output and all, so a project
/// split out or a risky try costs no rebuild. The copy runs as its own uid:
/// what `from`'s uid owned is re-owned as it is copied. A plain copy here;
/// on a Tessera volume this becomes a clone (copy-on-write).
pub fn duplicate(owner: &str, from: &str, to: &str) -> Result<Record, String> {
    let (home, gid) = person(owner)?;
    let root = root_dir(&home, gid)?;
    let src = root.join(from);
    let r = load(&src).filter(|_| valid_name(from)).ok_or_else(|| format!("no workspace {from:?}"))?;
    let new = Record { name: to.to_string(), ..r };
    check(owner, &home, &new)?;
    if load(&root.join(to)).is_some() || fs::symlink_metadata(root.join(to)).is_ok() {
        return Err(format!("{owner} has a workspace {to} already"));
    }
    let (old_uid, old_user) = launch::resolve_app_uid(owner, &reg_id(from), None).map_err(|e| format!("{e:?}"))?;
    let (new_uid, new_user) = launch::resolve_app_uid(owner, &reg_id(to), None).map_err(|e| format!("{e:?}"))?;
    let new_gid = crate::console::gid_of(new_uid).ok_or_else(|| format!("no group for uid {new_uid}"))?;
    let old_gid = crate::console::gid_of(old_uid);
    let dst = make_dirs(owner, &home, gid, to)?;
    let fail = |m: String| {
        let _ = fs::remove_dir_all(&dst);
        m
    };
    // ★ Copied as links, never through one (cp -R without -L), owners kept.
    for part in [WORK, ".state/root"] {
        let (a, b) = (src.join(part), dst.join(part));
        if fs::symlink_metadata(&a).map(|m| !m.is_dir()).unwrap_or(true) {
            continue;
        }
        fs::create_dir_all(&b).map_err(|e| fail(format!("{}: {e}", b.display())))?;
        let st = Command::new("cp").arg("-Rp").arg(format!("{}/.", a.display())).arg(&b).status()
            .map_err(|e| fail(format!("cp: {e}")))?;
        if !st.success() {
            return Err(fail(format!("copy {part}: {st}")));
        }
    }
    // Its own account's home in its root.
    let oh = dst.join(".state/root/home").join(&old_user);
    if fs::symlink_metadata(&oh).is_ok_and(|m| m.is_dir()) {
        let _ = fs::rename(&oh, dst.join(".state/root/home").join(&new_user));
    }
    // What the old uid owned is the new one's (links themselves, -h).
    let mut find = Command::new("find");
    find.arg(&dst).args(["-user", &old_uid.to_string(), "-exec", "chown", "-h", &format!("{new_uid}:{new_gid}"), "{}", "+"]);
    let st = find.status().map_err(|e| fail(format!("find: {e}")))?;
    if !st.success() {
        return Err(fail(format!("re-own: {st}")));
    }
    if let Some(g) = old_gid {
        let _ = Command::new("find").arg(&dst).args(["-group", &g.to_string(), "-exec", "chgrp", "-h", &new_gid.to_string(), "{}", "+"]).status();
    }
    save(&dst, &new).map_err(fail)?;
    eprintln!("portcullisd: workspace {from} of {owner} duplicated as {to} (uid {old_uid} -> {new_uid})");
    Ok(new)
}

/// Change its settings; a running one is stopped so the change applies.
pub fn set(owner: &str, name: &str, network: Option<String>, uses: Option<Vec<String>>, object_cache: Option<bool>,
           hosts: Option<Vec<String>>) -> Result<Record, String> {
    let (home, gid) = person(owner)?;
    let dir = root_dir(&home, gid)?.join(name);
    let mut r = load(&dir).filter(|_| valid_name(name)).ok_or_else(|| format!("no workspace {name:?}"))?;
    let before: Vec<Record> = list(owner)?.into_iter().map(|(r, _)| r).collect();
    if let Some(o) = object_cache {
        r.object_cache = o;
    }
    if let Some(n) = network {
        r.network = n;
    }
    if let Some(u) = uses {
        r.uses = u;
    }
    if let Some(h) = hosts {
        r.hosts = h.into_iter().map(|h| h.trim().to_ascii_lowercase()).filter(|h| !h.is_empty()).collect();
    }
    check(owner, &home, &r)?;
    stop(owner, name);
    save(&dir, &r)?;
    // Every workspace whose object group changed starts again with the
    // new one (its old group's folder is then removed).
    let after: Vec<Record> = list(owner)?.into_iter().map(|(r, _)| r).collect();
    for w in &after {
        if w.name != name && object_group(&before, &w.name) != object_group(&after, &w.name) {
            stop(owner, &w.name);
        }
    }
    sweep_objects(owner);
    eprintln!("portcullisd: workspace {name} of {owner}: network {}, uses {:?}, object cache {}", r.network, r.uses,
              if r.object_cache { "on" } else { "off" });
    Ok(r)
}

/// `~/.atrium/cache/objects/<group>`: root's folders above it, the group's
/// own the files group's, setgid, 2770 (every workspace's uid is in the
/// files group, and its umask is 002). Folders of groups that no longer
/// exist are removed.
fn objects_dir(home: &Path, files: u32, group: &[String]) -> Result<PathBuf, String> {
    let cache = home.join(".atrium").join("cache");
    own_dir(&cache, 0, files, 0o750)?;
    let objects = cache.join("objects");
    own_dir(&objects, 0, files, 0o750)?;
    let d = objects.join(group_dir_name(group));
    own_dir(&d, 0, files, 0o2770)?;
    Ok(d)
}

/// Remove the object folders of groups that are gone (none of their
/// members is running: the mount would hold it).
fn sweep_objects(owner: &str) {
    let Ok((home, files)) = person(owner) else { return };
    let objects = home.join(".atrium/cache/objects");
    let Ok(rd) = fs::read_dir(&objects) else { return };
    let all: Vec<Record> = list(owner).map(|l| l.into_iter().map(|(r, _)| r).collect()).unwrap_or_default();
    let live: Vec<String> = all.iter().map(|r| object_group(&all, &r.name)).filter(|g| !g.is_empty()).map(|g| group_dir_name(&g)).collect();
    let mounted: Vec<String> = Command::new("mount").arg("-p").output()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().filter_map(|l| l.split_whitespace().next().map(str::to_string)).collect())
        .unwrap_or_default();
    let _ = files;
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        let p = e.path();
        if live.contains(&n) || mounted.iter().any(|m| Path::new(m) == p) {
            continue;
        }
        if e.file_type().map(|t| t.is_dir() && !t.is_symlink()).unwrap_or(false) {
            let _ = fs::remove_dir_all(&p);
            eprintln!("portcullisd: object cache {n} of {owner} removed (its group is gone)");
        }
    }
}

/// May `name` be deleted? Not while another workspace uses it.
pub fn check_delete(owner: &str, name: &str) -> Result<(), String> {
    if !valid_name(name) {
        return Err(format!("{name:?} is not a workspace name"));
    }
    let list = list(owner)?;
    if !list.iter().any(|(r, _)| r.name == name) {
        return Err(format!("no workspace {name:?}"));
    }
    let users: Vec<&str> = list.iter().filter(|(r, _)| r.uses.iter().any(|u| u == name)).map(|(r, _)| r.name.as_str()).collect();
    if !users.is_empty() {
        return Err(format!("{} uses {name}: take that link away first", users.join(", ")));
    }
    Ok(())
}

/// Delete it: stopped, its mounts gone (checked), then its folder, work
/// and all. Scratch comes back, empty, at the next list.
/// A workspace's `work` on the host (it must exist, and be a folder).
pub fn work_dir(owner: &str, name: &str) -> Result<PathBuf, String> {
    if !valid_name(name) {
        return Err(format!("{name:?} is not a workspace name"));
    }
    let (home, gid) = person(owner)?;
    let d = root_dir(&home, gid)?.join(name);
    load(&d).ok_or_else(|| format!("{owner} has no workspace {name}"))?;
    let w = d.join(WORK);
    match fs::symlink_metadata(&w) {
        Ok(m) if m.is_dir() => Ok(w),
        _ => Err(format!("{} is not a folder", w.display())),
    }
}

pub fn delete(owner: &str, name: &str) -> Result<(), String> {
    check_delete(owner, name)?;
    let persona = person(owner).ok()
        .and_then(|(home, gid)| root_dir(&home, gid).ok())
        .and_then(|d| load(&d.join(name)))
        .map(|r| r.persona);
    stop(owner, name);
    if running_jid(owner, name).is_some() {
        return Err(format!("{name} is still running"));
    }
    // Out of every IDE first (homes.md W6).
    crate::ide::forget(owner, name)?;
    let (home, gid) = person(owner)?;
    let dir = root_dir(&home, gid)?.join(name);
    // ★ Never remove through a mount: whatever is mounted under it would go too.
    let jp = jail_path(owner, name);
    let mounted = Command::new("mount").arg("-p").output()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().any(|l| {
            let at = l.split_whitespace().nth(1).unwrap_or("");
            Path::new(at).starts_with(&dir) || Path::new(at).starts_with(&jp)
        }))
        .unwrap_or(true);
    if mounted {
        return Err(format!("{name} still has something mounted; not deleted"));
    }
    if fs::symlink_metadata(&dir).map(|m| m.file_type().is_symlink()).unwrap_or(false) {
        return Err(format!("{} is a link; not deleted", dir.display()));
    }
    fs::remove_dir_all(&dir).map_err(|e| format!("delete {}: {e}", dir.display()))?;
    let _ = fs::remove_dir(&jp);
    if let Some(p) = persona {
        crate::ssh_agent::forget(owner, name, &p);
    }
    eprintln!("portcullisd: workspace {name} of {owner} deleted");
    Ok(())
}

pub fn running_jid(owner: &str, name: &str) -> Option<i32> {
    let out = Command::new("jls").args(["-j", &jail_name(owner, name), "jid"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// What a console needs to start a shell in a running workspace.
pub struct Running {
    pub jid:    i32,
    pub uid:    u32,
    pub gid:    u32,
    /// The person's files group.
    pub files:  u32,
    /// `$HOME` in the jail; `work` is under it.
    pub home:   String,
    /// What its commands get in their environment (the object cache's
    /// folders, when it shares one).
    pub env:    Vec<String>,
}

/// Seed the jail's `/etc`: its own accounts (root and the workspace's) and
/// what the base system's tools read.
fn seed_etc(root: &Path, user: &str, uid: u32, gid: u32, files: u32, home_in: &str) -> Result<(), String> {
    let etc = root.join("etc");
    fs::create_dir_all(&etc).map_err(|e| format!("{}: {e}", etc.display()))?;
    for f in ETC {
        let src = Path::new("/etc").join(f);
        if src.is_file() {
            if let Some(d) = etc.join(f).parent() {
                let _ = fs::create_dir_all(d);
            }
            let _ = fs::copy(&src, etc.join(f));
        }
    }
    // The trusted CAs, so cargo, git and pkg can check TLS: the bundle
    // (read through its link) and certctl's hashed links, which point into
    // /usr/share/certs (mounted read-only from the base).
    let ssl = etc.join("ssl");
    let _ = fs::create_dir_all(ssl.join("certs"));
    if let Ok(b) = fs::read("/etc/ssl/cert.pem") {
        let _ = fs::write(ssl.join("cert.pem"), b);
    }
    if let Ok(rd) = fs::read_dir("/etc/ssl/certs") {
        for e in rd.flatten() {
            if let Ok(t) = fs::read_link(e.path()) {
                if t.starts_with("/usr/share/certs") || t.is_relative() {
                    let _ = fs::remove_file(ssl.join("certs").join(e.file_name()));
                    let _ = std::os::unix::fs::symlink(&t, ssl.join("certs").join(e.file_name()));
                }
            }
        }
    }
    // pkg's repository (dev-tools-plan.md M2): ports from pkg.FreeBSD.org
    // for the base's ABI, rewritten at each start. Not the host's file: no
    // base or kernel-module repositories in a workspace. (`nobody`, in
    // passwd above, is who pkg fetches and checks signatures as.)
    let _ = fs::create_dir_all(etc.join("pkg"));
    fs::write(etc.join("pkg/FreeBSD.conf"), PKG_REPO).map_err(|e| format!("pkg repository: {e}"))?;
    let _ = fs::remove_file(etc.join("termcap"));
    let _ = std::os::unix::fs::symlink("/usr/share/misc/termcap", etc.join("termcap"));
    let mp = etc.join("master.passwd");
    fs::write(&mp, format!(
        "root:*:0:0::0:0:Charlie &:/root:/usr/sbin/nologin\n{user}:*:{uid}:{gid}::0:0:Workspace:{home_in}:/bin/sh\n\
         nobody:*:65534:65534::0:0:Unprivileged user:/nonexistent:/usr/sbin/nologin\n"))
        .map_err(|e| format!("{}: {e}", mp.display()))?;
    fs::set_permissions(&mp, fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    fs::write(etc.join("group"), format!("wheel:*:0:root\n{user}:*:{gid}:\nfiles:*:{files}:{user}\nnobody:*:65534:\n"))
        .map_err(|e| e.to_string())?;
    let st = Command::new("pwd_mkdb").arg("-d").arg(&etc).arg(&mp).status().map_err(|e| format!("pwd_mkdb: {e}"))?;
    if !st.success() {
        return Err(format!("pwd_mkdb: {st}"));
    }
    Ok(())
}

/// One folder of a workspace's file system as its jail sees it, and where
/// it is on the host (dev-tools-plan.md M4: Run resolves a build's
/// libraries the way the workspace's loader would). `untrusted` folders
/// are the workspace's to write: opened only beneath their host folder.
pub struct ViewMount {
    pub in_jail:   String,
    pub host:      PathBuf,
    pub untrusted: bool,
}

/// The workspace's file system, longest folder first: the base (the
/// host's, read-only), its toolchain layers, `~/work`, what it uses at
/// `/workspaces/<name>`, and its own root (`/usr/local` and the rest).
/// Also the home folder in the jail (`/home/<its user>`).
pub fn view(owner: &str, name: &str) -> Result<(Vec<ViewMount>, String), String> {
    let (home, files) = person(owner)?;
    let dir = root_dir(&home, files)?.join(name);
    let r = load(&dir).filter(|_| valid_name(name)).ok_or_else(|| format!("no workspace {name:?}"))?;
    let (_, user) = launch::resolve_app_uid(owner, &reg_id(name), None).map_err(|e| format!("{e:?}"))?;
    let home_in = format!("/home/{user}");
    let mut v: Vec<ViewMount> = BASE.iter()
        .map(|b| ViewMount { in_jail: format!("/{b}"), host: Path::new("/").join(b), untrusted: false }).collect();
    let (_, layer_dirs) = compose_layers(&r)?;
    for (src, at) in layer_dirs {
        v.push(ViewMount { in_jail: format!("/{}", at.trim_start_matches('/')), host: src, untrusted: false });
    }
    v.push(ViewMount { in_jail: format!("{home_in}/{WORK}"), host: dir.join(WORK), untrusted: true });
    for u in &r.uses {
        v.push(ViewMount { in_jail: format!("/{LINKS}/{u}"), host: home.join(".atrium/workspaces").join(u).join(WORK), untrusted: true });
    }
    v.push(ViewMount { in_jail: "/".into(), host: dir.join(".state").join("root"), untrusted: true });
    v.sort_by_key(|m| std::cmp::Reverse(m.in_jail.len()));
    Ok((v, home_in))
}

/// Start the workspace if it isn't running, and say where it is.
pub fn start(owner: &str, name: &str) -> Result<Running, String> {
    let (home, files) = person(owner)?;
    let dir = root_dir(&home, files)?.join(name);
    let r = load(&dir).filter(|_| valid_name(name)).ok_or_else(|| format!("no workspace {name:?}"))?;
    check(owner, &home, &r)?;
    let (uid, user) = launch::resolve_app_uid(owner, &reg_id(name), None).map_err(|e| format!("{e:?}"))?;
    let gid = crate::console::gid_of(uid).ok_or_else(|| format!("no group for uid {uid}"))?;
    crate_home::grant_group(owner, &user)?;
    let home_in = format!("/home/{user}");
    // Its group's object cache (W9): ~/.atrium/cache/objects/<group>, the
    // files group's, at /cache/objects.
    let all: Vec<Record> = list(owner)?.into_iter().map(|(r, _)| r).collect();
    let group = object_group(&all, name);
    let objects = if group.is_empty() { None } else { Some(objects_dir(&home, files, &group)?) };
    let mut env = if objects.is_some() {
        vec![format!("CCACHE_DIR=/{OBJECTS_IN_JAIL}/ccache"), format!("SCCACHE_DIR=/{OBJECTS_IN_JAIL}/sccache")]
    } else {
        Vec::new()
    };
    // Its toolchain layers: PATH first, then what they set.
    let (layers, layer_dirs) = compose_layers(&r)?;
    env.insert(0, format!("PATH=/{WS_BIN_IN_JAIL}:{}", layers.path));
    // What it uses, for pkg-config (dev-tools.md §4.3): each one's prefix.
    if !r.uses.is_empty() {
        let pc: Vec<String> = r.uses.iter().map(|u| format!("/{LINKS}/{u}/.atrium/prefix/lib/pkgconfig")).collect();
        env.push(format!("PKG_CONFIG_PATH={}", pc.join(":")));
    }
    env.extend(layers.env);
    // registries (dev-tools.md §5.2): its only way out is the proxy on its
    // own loopback.
    if r.network == "registries" {
        env.extend(egress::proxy_env());
    }
    // Arca's SSH agent (M9): its socket for this workspace, mounted below
    // when the jail is made; a jail made before the agent ran has none.
    let agent = crate::ssh_agent::open(owner, name, &r.persona, uid);
    let jp = jail_path(owner, name);
    let mounted = jp.join(crate::ssh_agent::IN_JAIL).join("agent.sock").exists();
    if agent.is_some() && (running_jid(owner, name).is_none() || mounted) {
        env.push(crate::ssh_agent::AUTH_SOCK.into());
        // git asks the agent for HTTPS tokens too (git-credential-arca).
        if Path::new(GIT_CREDENTIAL_ARCA).exists() {
            env.extend(["GIT_CONFIG_COUNT=1", "GIT_CONFIG_KEY_0=credential.helper", "GIT_CONFIG_VALUE_0=arca"].map(String::from));
        }
    }
    let running = Running { jid: 0, uid, gid, files, home: home_in.clone(), env };
    if let Some(jid) = running_jid(owner, name) {
        return Ok(Running { jid, ..running });
    }
    egress::stop(&jail_name(owner, name));

    // The writable root, kept between starts (what `pkg` or `make install`
    // put in /usr/local stays).
    let root = dir.join(".state").join("root");
    for (d, mode, o) in [("", 0o755, 0), ("tmp", 0o1777, 0), ("var", 0o755, 0), ("var/tmp", 0o1777, 0),
                         ("usr", 0o755, 0), ("usr/local", 0o755, uid), ("root", 0o700, 0), ("dev", 0o555, 0),
                         ("home", 0o755, 0), (&home_in[1..], 0o755, uid), (LINKS, 0o755, 0),
                         // pkg's, as the jail's root (M2).
                         ("var/db", 0o755, 0), ("var/db/pkg", 0o755, 0), ("var/run", 0o755, 0), ("var/cache", 0o755, 0), ("var/cache/pkg", 0o755, 0),
                         ("atrium", 0o755, 0), (WS_BIN_IN_JAIL, 0o755, 0)] {
        let p = root.join(d);
        fs::create_dir_all(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        let _ = std::os::unix::fs::chown(&p, Some(o), Some(if o == 0 { 0 } else { gid }));
        let _ = fs::set_permissions(&p, fs::Permissions::from_mode(mode));
    }
    seed_etc(&root, &user, uid, gid, files, &home_in)?;

    fs::create_dir_all(&jp).map_err(|e| format!("{}: {e}", jp.display()))?;
    let _ = launch::umount_silent(&jp);
    launch::run("mount", &["-t", "nullfs", "-o", "rw", root.to_str().unwrap_or(""), jp.to_str().unwrap_or("")])
        .map_err(|e| format!("mount its root: {e}"))?;

    let mid = manifest_id(owner, name);
    let text = format!("[app]\nid = {mid:?}\nname = {name:?}\nversion = \"1\"\nentry = \"usr/bin/true\"\n\n[capabilities]\nnetwork = {:?}\n",
                       match r.network.as_str() { "outbound" => "full", "registries" => "loopback", _ => "none" });
    let manifest = portcullis_toml::Manifest::from_str(&text).map_err(|e| format!("its manifest: {e}"))?;
    let fail = |m: String| {
        launch::full_teardown(&jp, None);
        m
    };
    // `develop`: a shell, builds, and debugging the workspace's own
    // processes (dev-tools-plan.md M0). No app class has `fork` or `ptrace`.
    let sysfil = launch::sysfil_for_class(launch::DEVELOP_CLASS).map_err(|e| fail(format!("{e:?}")))?;
    let host_identity = portcullis_identity::for_app(&mid).map_err(|e| fail(format!("identity: {e}")))?;
    let opts = portcullis_jail::BuildOpts {
        root_path: jp.clone(),
        host_sockets: PathBuf::from("/atrium/sockets"),
        user_home: PathBuf::from(&home_in),
        user_name: user.clone(),
        devfs_ruleset: portcullis_jail::APP_DEVFS_RULESET,
        instance: None,
        persist: true,
        host_identity,
        sysfil,
    };
    let mut jc = portcullis_jail::build(&manifest, &opts).map_err(|e| fail(format!("build: {e}")))?;
    // Nothing runs at creation (the entry is `true`), and the workspace's
    // account has no home on the host for jail(8) to change to: it runs as
    // the jail's root, in /root.
    jc.set("exec.jail_user", portcullis_jail::Value::String("root".into()));
    // ★ Debugging its own processes (lldb on a test binary): this jail only;
    // the host keeps security.bsd.unprivileged_proc_debug=0, and with
    // see_other_uids=0 a workspace reaches nothing but its own uid.
    jc.set("allow.unprivileged_proc_debug", portcullis_jail::Value::Bool(true));
    // ★ What its root may not do, said outright (M2: `pkg` and package
    // scripts run as the jail's root): no mounts, no raw sockets, no file
    // flags, and it sees only its own mounts.
    for p in ["allow.mount", "allow.raw_sockets", "allow.chflags", "allow.set_hostname"] {
        jc.set(p, portcullis_jail::Value::Bool(false));
    }
    jc.set("enforce_statfs", portcullis_jail::Value::Number(2));
    jc.set("children.max", portcullis_jail::Value::Number(0));
    let ws_bin = ensure_ws_bin().map_err(|e| fail(e))?;
    jc.add_mount(&ws_bin, &jp.join(WS_BIN_IN_JAIL), "nullfs", &["ro"]);
    for b in BASE {
        let src = Path::new("/").join(b);
        if src.is_dir() {
            jc.add_mount(&src, &jp.join(b), "nullfs", &["ro"]);
        }
    }
    jc.add_mount(&dir.join(WORK), &jp.join(&home_in[1..]).join(WORK), "nullfs", &["rw"]);
    for u in &r.uses {
        jc.add_mount(&home.join(".atrium/workspaces").join(u).join(WORK), &jp.join(LINKS).join(u), "nullfs", &["ro"]);
    }
    if let Some(o) = &objects {
        jc.add_mount(o, &jp.join(OBJECTS_IN_JAIL), "nullfs", &["rw"]);
    }
    // ★ Read-only: the workspace connects to its socket and can put
    // nothing beside it.
    if let Some(d) = &agent {
        jc.add_mount(d, &jp.join(crate::ssh_agent::IN_JAIL), "nullfs", &["ro"]);
    }
    // ★ Read-only, shared by every workspace that names the same version.
    for (src, at) in &layer_dirs {
        jc.add_mount(src, &jp.join(at.trim_start_matches('/')), "nullfs", &["ro"]);
    }
    launch::ensure_mountpoints(&jc).map_err(|e| fail(format!("{e:?}")))?;
    launch::create_persistent(&jc, &jp).map_err(|e| fail(format!("{e:?}")))?;
    let jid = running_jid(owner, name).ok_or_else(|| fail(format!("workspace {name} did not start")))?;
    for id in crate::dev::bundles_of(owner, name) {
        show_bundle(owner, name, &id);
    }
    if r.network == "registries" {
        // ★ Before anything runs in it: the port is the proxy's, not
        // something the workspace could bind first.
        if let Err(e) = egress::start(&jail_name(owner, name), jid, &r.egress_hosts()) {
            launch::remove_persistent(&jail_name(owner, name), &jp, false);
            return Err(format!("its registries proxy: {e}"));
        }
    }
    eprintln!("portcullisd: workspace {name} of {owner} started (jid {jid}, uid {uid}, network {}, uses {:?}, layers {:?}, objects {:?})", r.network, r.uses,
              r.layers, objects.as_ref().map(|o| o.display().to_string()));
    Ok(Running { jid, ..running })
}

/// Where the workspace sees its own dev bundles: at the path they have on
/// the host, which is the path the kernel gives for what they map (a
/// jail's nullfs root hides nothing from it), so lldb finds the files a
/// debugged process or a core names (dev-tools-plan.md M10).
const BUNDLES_IN_JAIL: &str = "var/lib/atrium/apps";

/// Show dev bundle `id` (one of this workspace's) in it, read-only, if it
/// runs; again after each Run, which replaces the bundle.
pub fn show_bundle(owner: &str, name: &str, id: &str) {
    if running_jid(owner, name).is_none() || !id.starts_with(&crate::dev::dev_id(owner, name, "")) || id.contains('/') {
        return;
    }
    let at = jail_path(owner, name).join(BUNDLES_IN_JAIL).join(id);
    let _ = launch::umount_silent(&at);
    let src = Path::new(crate::dev::APPS_DIR).join(id);
    if !src.is_dir() {
        return;
    }
    if let Err(e) = fs::create_dir_all(&at) {
        eprintln!("portcullisd: workspace {name}: {}: {e}", at.display());
        return;
    }
    if let Err(e) = launch::run("mount", &["-t", "nullfs", "-o", "ro", src.to_str().unwrap_or(""), at.to_str().unwrap_or("")]) {
        eprintln!("portcullisd: workspace {name}: show {id}: {e}");
    }
}

/// Take it away again (before a Run replaces it).
pub fn hide_bundle(owner: &str, name: &str, id: &str) {
    if running_jid(owner, name).is_some() && !id.contains('/') {
        let _ = launch::umount_silent(&jail_path(owner, name).join(BUNDLES_IN_JAIL).join(id));
    }
}

/// Stop it: its processes end, its mounts go; its files stay.
pub fn stop(owner: &str, name: &str) {
    if running_jid(owner, name).is_none() {
        return;
    }
    let (home, gid) = match person(owner) {
        Ok(v) => v,
        Err(_) => return,
    };
    let networked = root_dir(&home, gid).ok()
        .and_then(|d| load(&d.join(name)))
        .is_some_and(|r| r.network == "outbound");
    launch::remove_persistent(&jail_name(owner, name), &jail_path(owner, name), networked);
    egress::stop(&jail_name(owner, name));
    crate::ssh_agent::close(owner, name);
    eprintln!("portcullisd: workspace {name} of {owner} stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(valid_name("scratch") && valid_name("lib-a") && valid_name("2d"));
        for bad in ["", "A", "a b", "a/b", "../x", ".x", "-x", "a.b", &"a".repeat(33)] {
            assert!(!valid_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn versions_and_layers() {
        use std::cmp::Ordering::*;
        assert_eq!(version_cmp("1.100.0", "1.99.1"), Greater);
        assert_eq!(version_cmp("1.91", "1.91.0"), Less);
        assert_eq!(version_cmp("2", "2"), Equal);
        let d = std::env::temp_dir().join(format!("ws-layers-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let arch = portcullis_toml::layer::host_arch();
        for (n, v) in [("rust", "1.99.1"), ("rust", "1.100.0"), ("c-dev", "1")] {
            let l = d.join("layers").join(format!("{n}@{v}"));
            fs::create_dir_all(&l).unwrap();
            fs::write(l.join("atrium.toml"), format!("kind = \"layer\"\n[layer]\nname = \"{n}\"\nversion = \"{v}\"\narch = \"{arch}\"\nmount = \"/atrium/toolchains/{n}\"\npath = [\"bin\"]\ncontent = \"sha256:{}\"\n", "0".repeat(64))).unwrap();
        }
        // Named under another reference than its manifest's: not a layer.
        let l = d.join("layers/rust@9");
        fs::create_dir_all(&l).unwrap();
        fs::copy(d.join("layers/rust@1.99.1/atrium.toml"), l.join("atrium.toml")).unwrap();
        let inst = installed_layers_in(&d);
        assert_eq!(inst.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), ["c-dev@1", "rust@1.100.0", "rust@1.99.1"]);
        assert_eq!(newest_layer(&inst, "rust").as_deref(), Some("rust@1.100.0"));
        assert_eq!(newest_layer(&inst, "go"), None);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn targets_and_jails() {
        assert_eq!(target("scratch"), "workspace/scratch");
        assert_eq!(jail_name("alice", "lib-a"), "ws_alice_lib-a");
        assert_ne!(jail_name("alice", "b"), jail_name("bob", "b"));
        // An app id never has a '/', so a workspace's uid is never an app's.
        assert!(reg_id("x").contains('/'));
    }
}
