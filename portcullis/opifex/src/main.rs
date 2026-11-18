//! `opifex` — Atrium's binary app installer for FreeBSD.
//!
//! The pkg-style counterpart to `insula` (the ports/Homebrew-style
//! compile-from-source tool): opifex takes a *pre-built*, signed Insula bundle and
//! installs it into the Portcullis app tree that the jail launcher (`portcullisd`)
//! reads — `/var/lib/atrium/apps/<id>/`.
//!
//! It is jail-aware. A Portcullis jail's rootfs is ONLY the app tree (a nullfs+
//! unionfs of `apps/<id>` + an overlay) — there is no shared `/lib` mounted in. So
//! a dynamically-linked app must carry its own runtime. opifex walks the entry
//! binary's DT_NEEDED closure and copies those libraries plus the rtld into the
//! app tree at install time, making the bundle self-contained for its jail. (A
//! bundle that already ships its libs is fine too — those are copied first and
//! left alone, though they are still walked for their own dependencies.)
//!
//! ```text
//! opifex pack <bundle-dir> [--sign <key.pem>]
//! opifex install <bundle-dir> [--allow-unsigned] [--root <dir>] [--sysroot <dir>]
//! opifex list                 [--root <dir>]
//! opifex uninstall <app-id>   [--root <dir>]
//! opifex rollback <app-id>    [--root <dir>]
//! ```
//! `pack` writes `content.index` and names it in `[bundle] content`, so the
//! manifest's signature covers every file; install then checks each file
//! (opifex.md §4.2). See `app.rs`.
//! `--root` is the Atrium state root (default `/var/lib/atrium`); apps install
//! under `<root>/apps/<id>/`, with the writable overlay at `<root>/overlays/<id>/`.
//!
//! `--sysroot` is where runtime libraries are READ FROM (default `/`, i.e. the
//! running system). It exists so opifex can run on a cross-build host: point it
//! at an extracted FreeBSD sysroot and a bundle can be installed without a target
//! machine. That is why the library closure is computed by parsing ELF DT_NEEDED
//! rather than by shelling out to `ldd(1)`, which only exists on the target and
//! only ever resolves against the running system.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

mod catalog;
mod token;

use opifex::{app, layer, runtime::resolve_runtime};

const DEFAULT_ROOT: &str = "/var/lib/atrium";
/// Where runtime libraries are read from. "/" = the running system (the native,
/// on-target case). Point it at an extracted FreeBSD sysroot to install from a
/// cross-build host.
const DEFAULT_SYSROOT: &str = "/";
fn main() -> ExitCode {
    // Key files may name a key on a hardware token (opifex token ref).
    portcullis_token::install();
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
        return ExitCode::from(2);
    }
    let res = match args[1].as_str() {
        "pack"      => cmd_pack(&args[2..]),
        "archive"   => (|| {
            let (d, o) = (args.get(2).ok_or("archive <packed-dir> <out.insula>")?, args.get(3).ok_or("archive <packed-dir> <out.insula>")?);
            let b = opifex::archive::pack(Path::new(d))?;
            std::fs::write(o, &b).map_err(|e| format!("{o}: {e}"))?;
            println!("opifex: {o}: {} bytes", b.len());
            Ok(())
        })(),
        // The publishing pipeline unpacks a release to check it (not sandboxed:
        // opifexd uses opifex-check for that).
        "unarchive" => (|| {
            let (a, d) = (args.get(2).ok_or("unarchive <file.insula> <new-dir>")?, args.get(3).ok_or("unarchive <file.insula> <new-dir>")?);
            let f = std::fs::File::open(a).map_err(|e| format!("{a}: {e}"))?;
            let size = f.metadata().map_err(|e| e.to_string())?.len();
            std::fs::create_dir(d).map_err(|e| format!("{d}: {e}"))?;
            let n = opifex::archive::unpack(f, size, Path::new(d))?;
            println!("opifex: {d}: {n} bytes unpacked");
            Ok(())
        })(),
        "install"   => cmd_install(&args[2..]),
        "rollback"  => cmd_rollback(&args[2..]),
        "list"      => cmd_list(&args[2..]),
        "uninstall" => cmd_uninstall(&args[2..]),
        "layer"     => cmd_layer(&args[2..]),
        "font" | "driver" | "firmware" => cmd_pack_kind(&args[1], &args[2..]),
        "driver-for" => driver_for(&args[2..]),
        "update-all" => update_all(),
        "catalog"   => catalog::cmd(&args[2..]),
        "token"     => token::cmd(&args[2..]),
        "sign"      => cmd_sign(&args[2..]),
        "describe" | "search" | "installed" | "get" | "sync" | "remove" | "side-load"
        | "updates" | "update-care" | "tick" | "catalogs" | "follow" | "unfollow" => ask_daemon(&args[1..]),
        "help" | "-h" | "--help" => { usage(); Ok(()) }
        other => { eprintln!("opifex: unknown command: {other}"); usage(); return ExitCode::from(2); }
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => { eprintln!("opifex: {e}"); ExitCode::FAILURE }
    }
}

fn usage() {
    eprintln!(
        "Usage: opifex <command> [args]\n\n\
         Commands:\n  \
           pack <bundle-dir> [--sign <key.pem>]\n      \
             Index every file, name the index in atrium.toml, sign it.\n  \
           archive <packed-dir> <out.insula>   Write the .insula archive (INSB v2).\n  \
           unarchive <file.insula> <new-dir>   Unpack one (the publishing pipeline; not sandboxed).\n  \
           install <bundle-dir> [--allow-unsigned] [--root <dir>] [--sysroot <dir>]\n      \
             Install a pre-built signed bundle into the Portcullis app tree.\n  \
           rollback <app-id> [--root <dir>]    Swap back to the version before the last install.\n  \
           list [--root <dir>]                 Show installed apps.\n  \
           uninstall <app-id> [--root <dir>]   Remove an app + its overlay.\n  \
           layer pack <tree> --manifest <toml> [--sign <key.pem>]\n  \
           layer install <dir> [--root <dir>] [--keys <dir>] [--arch <a>] [--allow-unsigned]\n  \
           layer list [--root <dir>]\n  \
           layer remove <name@version> [--root <dir>]\n      \
             Toolchain layers (dev-tools-plan.md M3), signed by the platform key.\n  \
           catalog root-new <cat> --name <n> --root <key>… --catalog <key> [--platform <key>] [--threshold 2] [--next yes [--retire-at <entry>]]\n  \
           catalog root-sign <cat> --key <key> [--pub <pem>]\n  \
           catalog add <cat> <pkg-dir> --arch <a> --archive <file> --url <https://...>\n  \
           catalog yank <cat> <id> <version> --reason <text> --key <pem> [--pub <pem>]\n  \
           catalog head <cat> --key <catalog.pem> [--days 14]\n  \
           catalog sync <state> --from <cat> [--root <root.toml>]\n  \
           catalog list <state>\n      \
             The signed catalog (opifex.md §6): publish into a folder, check a copy.\n  \
           catalog init <cat> --name <n> --keys <key-dir> [--roots 3] [--threshold 2] [--years 5]\n  \
           catalog register <cat> --name <n> --domain <d> --key <pub.pem> --sign <key.pem>\n  \
           catalog publisher <cat> rotate|scope|recover --id <pub:…> [--key <pub.pem>] --sign <pem>…\n  \
           catalog publishers <cat> | catalog publish <cat> --key <catalog.pem>\n      \
             A catalog of your own (§6.5): apps only, under domains its publishers prove.\n  \
           sign <dir>… --key <key> [--expect <list>] | sign --list <dir>…\n      \
             Sign packed manifests built elsewhere; --expect: only if a second build matches.\n  \
           token list --module <pkcs11 lib> | token pub <key file>\n  \
           token ref --module <lib> --serial <s> --id <hex> --out <key file> [--pub <pem>]\n      \
             Keys on hardware tokens (YubiKey PIV): a key file names one; any --key/--sign takes it.\n\n\
         Through opifexd (OPIFEX_SOCKET, default /atrium/sockets/opifex/opifex.sock):\n  \
           describe <id> | search <text> | installed\n  \
           get <id>            install the newest catalog release (root)\n  \
           side-load <path>    install a bundle folder or .insula file (root)\n  \
           remove <id> | sync [https://… | <catalog-dir>]   (root; no argument: the network)\n  \
           updates             what is waiting, and why\n  \
           update-care <id> notify|auto   wait for me / the publisher's timing\n  \
           tick                the daily round now (root)\n  \
           update-all          apply every waiting update (root)\n  \
           catalogs            the catalogs followed: the Atrium one and those people added\n  \
           follow <https://…> [<root sha256> [--everyone]]   show, then follow, a catalog (root)\n  \
           unfollow <slug>     stop following one; its apps stay, their updates stop\n\n\
         font|driver|firmware pack <tree> --manifest <atrium.toml> [--sign <key.pem>]   (opifex.md §8)\n  \
           driver-for usb <vvvv:pppp> [<cc/ss/pp>] | hid <page>   look a device up in the synced catalog\n\n\
         --root:    Atrium state root, installed INTO (default {DEFAULT_ROOT}).\n\
         --sysroot: where runtime libs are READ FROM (default {DEFAULT_SYSROOT});\n      \
           point at a FreeBSD sysroot to install from a cross-build host."
    );
}

/// Pull `--root <dir>` and `--sysroot <dir>` (leaving positionals) out of an arg
/// slice. `--sysroot` is where runtime libraries are read FROM; `--root` is the
/// Atrium state root they are installed INTO. They are independent: installing
/// into a mounted target root while reading libs from a cross sysroot is valid.
fn take_root(args: &[String]) -> (PathBuf, PathBuf, Vec<String>) {
    let mut root = PathBuf::from(DEFAULT_ROOT);
    let mut sysroot = PathBuf::from(DEFAULT_SYSROOT);
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--root" => { if let Some(v) = it.next() { root = PathBuf::from(v); } }
            "--sysroot" => { if let Some(v) = it.next() { sysroot = PathBuf::from(v); } }
            _ => rest.push(a.clone()),
        }
    }
    (root, sysroot, rest)
}

fn cmd_pack(args: &[String]) -> Result<(), String> {
    let mut tree = None;
    let mut sign = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--sign" => sign = Some(it.next().ok_or("--sign needs a key file")?.clone()),
            o if o.starts_with("--") => return Err(format!("pack: unknown flag {o}")),
            o => tree = Some(o.to_string()),
        }
    }
    let tree = tree.ok_or("pack: missing <bundle-dir>")?;
    let key = match sign { Some(k) => Some(std::fs::read_to_string(&k).map_err(|e| format!("{k}: {e}"))?), None => None };
    let m = app::pack(Path::new(&tree), key.as_deref())?;
    println!("opifex: packed {} ({}){}", m.app.id,
             m.bundle.as_ref().and_then(|b| b.content.clone()).unwrap_or_default(),
             if key.is_some() { ", signed" } else { ", NOT signed" });
    Ok(())
}

/// `opifex sign`: see `opifex::sign`.
fn cmd_sign(args: &[String]) -> Result<(), String> {
    let (mut key, mut expect, mut list, mut dirs) = (None::<String>, None::<String>, false, Vec::new());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--key" => key = Some(it.next().ok_or("--key needs a key file")?.clone()),
            "--expect" => expect = Some(it.next().ok_or("--expect needs a file")?.clone()),
            "--list" => list = true,
            o if o.starts_with("--") => return Err(format!("sign: unknown flag {o}")),
            o => dirs.push(Path::new(o).to_path_buf()),
        }
    }
    if dirs.is_empty() {
        return Err("sign --list <dir>… | sign <dir>… --key <key> [--expect <list>]".into());
    }
    if list {
        for d in &dirs {
            println!("{}", opifex::sign::read(d)?.line());
        }
        return Ok(());
    }
    let k = key.ok_or("sign: --key <key file> (a PEM or a token key file)")?;
    let key = std::fs::read_to_string(&k).map_err(|e| format!("{k}: {e}"))?;
    let want = match expect { Some(f) => Some(opifex::sign::expected(&std::fs::read_to_string(&f).map_err(|e| format!("{f}: {e}"))?)), None => None };
    let refs: Vec<&Path> = dirs.iter().map(|d| d.as_path()).collect();
    for p in opifex::sign::sign(&refs, &key, want.as_deref())? {
        println!("opifex: signed {} {} ({})", p.id, p.version, p.content);
    }
    Ok(())
}

fn cmd_install(args: &[String]) -> Result<(), String> {
    let (root, sysroot, rest) = take_root(args);
    let mut allow_unsigned = false;
    let mut src: Option<&str> = None;
    for a in &rest {
        match a.as_str() {
            "--allow-unsigned" => allow_unsigned = true,
            other if other.starts_with("--") => return Err(format!("install: unknown flag '{other}'")),
            other => src = Some(other),
        }
    }
    let bundle = PathBuf::from(src.ok_or("install: missing <bundle-dir>")?);

    // 1–2. Manifest, trust gate (the same one portcullisd applies at launch),
    //      and every file against the signed content index, into staging.
    let trust = portcullis_trust::Trust::load();
    let opts = app::Opts {
        root: &root,
        trust: &trust,
        allow_unsigned,
        require_content: app::require_content(Path::new(portcullis_trust::TRUST_CONFIG)),
        catalog_key: None,
    };
    let (manifest, stage) = app::stage(&bundle, &opts)?;
    let id = manifest.app.id.clone();
    let entry = manifest.entry();

    // 3. Make it self-contained for its jail: resolve every program's lib closure +
    //    the rtld into the tree (the jail rootfs has no shared /lib). Bring-up
    //    exception for side-loads (atrium-bundle-format.md); catalog packages
    //    will have to ship their closure.
    let entry_path = stage.join(entry);
    let added = entry_path.exists()
        .then(|| resolve_runtime(&entry_path, &stage, &sysroot))
        .unwrap_or_else(|| Err(format!("manifest entry {entry:?} not found in bundle")));
    let added = match added {
        Ok(n) => n,
        Err(e) => { let _ = std::fs::remove_dir_all(&stage); return Err(e); }
    };

    // 4. Swap it in; the previous version is kept for `opifex rollback`.
    let dest = app::commit(&root, &id, &stage)?;
    println!("opifex: installed {id} -> {}", dest.display());
    println!("opifex:   entry {entry}, {added} runtime file(s) resolved into the app tree");
    Ok(())
}

fn ask_daemon(args: &[String]) -> Result<(), String> {
    use opifex::proto::{Request, Response};

    let arg = |i: usize| args.get(i).cloned().ok_or_else(|| format!("{}: missing argument", args[0]));
    let req = match args[0].as_str() {
        "describe" => Request::Describe { id: arg(1)?, for_uid: None },
        "search" => Request::Search { text: arg(1)? },
        "installed" => Request::Installed,
        "get" => Request::Install { id: arg(1)? },
        "side-load" => Request::InstallLocal { path: std::fs::canonicalize(arg(1)?).map_err(|e| e.to_string())?.display().to_string() },
        "remove" => Request::Uninstall { id: arg(1)? },
        // `opifex sync` (the network), `opifex sync https://…`, or a folder.
        "sync" => Request::Sync { from: match args.get(1) {
            None => String::new(),
            Some(a) if a.starts_with("https://") => a.clone(),
            Some(_) => std::fs::canonicalize(arg(1)?).map_err(|e| e.to_string())?.display().to_string(),
        } },
        "updates" => Request::Updates,
        "update-care" => Request::SetUpdates { id: arg(1)?, care: arg(2)? },
        "tick" => Request::Tick,
        "catalogs" => Request::Catalogs,
        // `opifex follow <url>` shows what it is; `opifex follow <url> <sha256>
        // [--everyone]` follows it (root; a person adds one on the sheet).
        "follow" => match args.get(2) {
            None => Request::PreviewCatalog { url: arg(1)? },
            Some(sha) => Request::AddCatalog { url: arg(1)?, root_sha256: sha.clone(),
                                               machine: args.iter().any(|a| a == "--everyone"), for_uid: 0 },
        },
        "unfollow" => Request::RemoveCatalog { slug: arg(1)? },
        _ => unreachable!(),
    };
    match opifex::proto::ask(&req)? {
        Response::Error { message } => Err(message),
        Response::Installed { apps } => {
            for a in apps { println!("  {:<32} {:<10} {}", a.id, a.version, a.source); }
            Ok(())
        }
        Response::Described { app } => {
            println!("{} ({}) {}", app.name, app.id, app.latest);
            println!("  {}", app.description);
            println!("  catalog {}, publisher {}", app.catalog, app.publisher);
            println!("  may use: {}", if app.capabilities.is_empty() { "nothing beyond its own window".into() } else { app.capabilities.join(", ") });
            println!("  download {} bytes; installed: {}", app.archive_size, app.installed.as_deref().unwrap_or("no"));
            Ok(())
        }
        Response::Found { ids } => { for i in ids { println!("  {i}"); } Ok(()) }
        Response::Done { id, version } => { println!("opifex: {id} {version}: done"); Ok(()) }
        Response::Synced { entries, releases, others } => {
            println!("opifex: catalog checked: {entries} entries, {releases} releases");
            for o in others {
                println!("  {o}");
            }
            Ok(())
        }
        Response::Catalogs { items } => {
            for c in items {
                let whose = if !c.third_party { "Atrium".to_string() } else if c.machine { "everyone".into() } else { format!("uid {}", c.added_by) };
                println!("{:<24} {:<20} {:>5} releases  for {whose}  {}", if c.slug.is_empty() { "-" } else { &c.slug }, c.name, c.releases, c.url);
                for p in c.publishers {
                    println!("    {p}");
                }
            }
            Ok(())
        }
        Response::CatalogPreview { url, name, root_sha256, warning } => {
            println!("{name} at {url}\nroot.toml sha256 {root_sha256}\n{warning}");
            Ok(())
        }
        Response::CatalogAdded { slug, name, summary } => { println!("opifex: following {name} as {slug}: {summary}"); Ok(()) }
        Response::Updates { items, last_sync, last_error } => {
            println!("last checked: {}{}", if last_sync == 0 { "never".into() } else { format!("{last_sync} (unix time)") },
                     if last_error.is_empty() { String::new() } else { format!(", failed: {last_error}") });
            if items.is_empty() { println!("  everything is up to date"); }
            for u in items {
                let what = match u.action {
                    opifex::updates::Action::Background => "goes in on its own".to_string(),
                    opifex::updates::Action::Notify => "waits for you".to_string(),
                    opifex::updates::Action::Ask => format!("asks for more: {}", u.added.join(", ")),
                    opifex::updates::Action::Refused => format!("refused: {}", u.why),
                };
                println!("  {:<32} {} -> {}  {what}", u.id, u.from, u.to);
            }
            Ok(())
        }
        Response::Ticked { installed, waiting } => {
            for i in &installed { println!("opifex: updated {i}"); }
            println!("opifex: {} updated, {waiting} waiting", installed.len());
            Ok(())
        }
        Response::Ok => Ok(()),
        Response::LocalPreview { .. } => Err("opifexd: a preview, which only the sheet asks for".into()),
    }
}

fn cmd_rollback(args: &[String]) -> Result<(), String> {
    let (root, _sysroot, rest) = take_root(args);
    let id = rest.first().ok_or("rollback: missing <app-id>")?;
    let dest = app::rollback(&root, id)?;
    println!("opifex: {id} rolled back -> {}", dest.display());
    Ok(())
}

/// `opifex driver-for usb <vvvv:pppp> [<cc/ss/pp>] | hid <page>`: what the
/// synced catalog has for a device (devices.idx; nothing leaves the machine).
fn driver_for(args: &[String]) -> Result<(), String> {
    let state = std::env::var("OPIFEX_STATE").unwrap_or_else(|_| "/var/db/atrium/opifex".into());
    let idx = std::fs::read_to_string(Path::new(&state).join(opifex::driver::IDX)).map_err(|e| format!("{state}/devices.idx: {e} (sync first)"))?;
    let hex = |s: &str| u16::from_str_radix(s, 16).map_err(|_| format!("{s}: not hex"));
    let hits = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["usb", ids, ref rest @ ..] => {
            let id = opifex_catalog::driver::usb_id(ids).ok_or("usb: vvvv:pppp")?;
            let mut c = [0u8; 3];
            if let Some(cl) = rest.first() {
                for (i, p) in cl.split('/').take(3).enumerate() { c[i] = hex(p)? as u8; }
            }
            opifex::driver::lookup(&idx, Some((id, c)), &[])
        }
        ["hid", page] => opifex::driver::lookup(&idx, None, &[hex(page)?]),
        _ => return Err("driver-for usb <vvvv:pppp> [<cc/ss/pp>] | hid <page>".into()),
    };
    if hits.is_empty() { println!("opifex: no driver in the catalog for it"); }
    for h in hits { println!("  {h}"); }
    Ok(())
}

/// `opifex font|driver|firmware pack <tree> --manifest <toml> [--sign <key.pem>]`.
fn cmd_pack_kind(kind: &str, args: &[String]) -> Result<(), String> {
    let (mut manifest, mut sign, mut pos) = (None::<String>, None::<String>, Vec::new());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--manifest" => manifest = it.next().cloned(),
            "--sign" => sign = it.next().cloned(),
            o if o.starts_with("--") => return Err(format!("font: unknown flag {o}")),
            o => pos.push(o.to_string()),
        }
    }
    let ["pack", tree] = pos.iter().map(String::as_str).collect::<Vec<_>>()[..] else {
        return Err(format!("{kind} pack <tree> --manifest <atrium.toml> [--sign <key.pem>]"));
    };
    let t = std::fs::read_to_string(manifest.ok_or(format!("{kind} pack: --manifest <toml>"))?).map_err(|e| e.to_string())?;
    let key = match sign { Some(k) => Some(std::fs::read_to_string(&k).map_err(|e| format!("{k}: {e}"))?), None => None };
    let tree = Path::new(tree);
    let (id, version, content) = match kind {
        "font" => { let m = opifex::font::pack(tree, &t, key.as_deref())?; (m.font.id, m.font.version, m.font.content) }
        "driver" => { let m = opifex::driver::pack_driver(tree, &t, key.as_deref())?; (m.driver.id, m.driver.version, m.driver.content) }
        _ => { let m = opifex::driver::pack_firmware(tree, &t, key.as_deref())?; (m.firmware.id, m.firmware.version, m.firmware.content) }
    };
    println!("opifex: packed {kind} {id} {version} (content {content}){}", if key.is_some() { ", signed" } else { ", NOT signed" });
    Ok(())
}

/// `opifex update-all` (root): apply every waiting update that isn't
/// refused, the administrator's batched approval (§11).
fn update_all() -> Result<(), String> {
    use opifex::proto::{Request, Response};
    use opifex::updates::Action;
    let Response::Updates { items, .. } = opifex::proto::ask(&Request::Updates)? else { return Err("opifexd: unexpected answer".into()) };
    let mut failed = 0;
    for u in items.iter().filter(|u| u.action != Action::Refused) {
        match opifex::proto::ask(&Request::Install { id: u.id.clone() })? {
            Response::Done { id, version } => println!("opifex: {id} {version}: done"),
            Response::Error { message } => { eprintln!("opifex: {}: {message}", u.id); failed += 1; }
            other => { eprintln!("opifex: {}: {other:?}", u.id); failed += 1; }
        }
    }
    if failed > 0 { Err(format!("{failed} update(s) failed")) } else { Ok(()) }
}

fn cmd_layer(args: &[String]) -> Result<(), String> {
    let (root, _sysroot, rest) = take_root(args);
    let mut pos: Vec<&str> = Vec::new();
    let (mut manifest, mut sign, mut keys_dir, mut arch, mut unsigned) =
        (None::<String>, None::<String>, layer::PLATFORM_KEYS.to_string(), portcullis_toml::layer::host_arch().to_string(), false);
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().ok_or_else(|| format!("{a} needs a value"));
        match a.as_str() {
            "--manifest" => manifest = Some(val()?),
            "--sign" => sign = Some(val()?),
            "--keys" => keys_dir = val()?,
            "--arch" => arch = val()?,
            "--allow-unsigned" => unsigned = true,
            o if o.starts_with("--") => return Err(format!("layer: unknown flag {o}")),
            o => pos.push(o),
        }
    }
    match pos.as_slice() {
        ["pack", tree] => {
            let t = std::fs::read_to_string(manifest.ok_or("layer pack: --manifest <toml>")?).map_err(|e| e.to_string())?;
            let key = match sign { Some(k) => Some(std::fs::read_to_string(&k).map_err(|e| format!("{k}: {e}"))?), None => None };
            let m = layer::pack(Path::new(tree), &t, key.as_deref())?;
            println!("opifex: packed {} ({}, content {}){}", m.reference(), m.layer.arch, m.layer.content,
                     if key.is_some() { ", signed" } else { ", NOT signed" });
            Ok(())
        }
        ["install", dir] => {
            let keys = load_publishers(&keys_dir);
            let (m, dest) = layer::install(Path::new(dir), &layer::InstallOpts { root: &root, keys: &keys, arch: &arch, allow_unsigned: unsigned })?;
            println!("opifex: layer {} installed -> {}", m.reference(), dest.display());
            Ok(())
        }
        ["list"] => {
            let v = layer::list(&root);
            if v.is_empty() {
                println!("opifex: no layers under {}", root.join("layers").display());
            }
            for (r, m) in v {
                println!("  {r:<24} {:<8} {}", m.layer.arch, m.layer.summary);
            }
            Ok(())
        }
        ["remove", r] => {
            let d = layer::remove(&root, r)?;
            println!("opifex: removed {}", d.display());
            Ok(())
        }
        _ => Err("layer: pack <tree> | install <dir> | list | remove <name@version>".into()),
    }
}

fn cmd_list(args: &[String]) -> Result<(), String> {
    let (root, _sysroot, _rest) = take_root(args);
    let apps = root.join("apps");
    let Ok(entries) = std::fs::read_dir(&apps) else {
        println!("opifex: no apps installed under {}", apps.display());
        return Ok(());
    };
    let mut rows: Vec<(String, String, String)> = Vec::new();
    for e in entries.flatten() {
        let m = e.path().join("atrium.toml");
        if let Ok(text) = std::fs::read_to_string(&m) {
            if let Ok(man) = portcullis_toml::Manifest::from_str(&text) {
                rows.push((man.app.id, man.app.name, man.app.entry));
            }
        }
    }
    rows.sort();
    if rows.is_empty() {
        println!("opifex: no apps installed under {}", apps.display());
    }
    for (id, name, entry) in rows {
        println!("  {id:<28} {name:<20} {entry}");
    }
    Ok(())
}

fn cmd_uninstall(args: &[String]) -> Result<(), String> {
    let (root, _sysroot, rest) = take_root(args);
    let id = rest.first().ok_or("uninstall: missing <app-id>")?;
    let mut removed = false;
    for p in [root.join("apps").join(id), root.join("overlays").join(id), app::state(&root).join("prev").join(id)] {
        if p.exists() {
            std::fs::remove_dir_all(&p).map_err(|e| format!("remove {}: {e}", p.display()))?;
            println!("opifex: removed {}", p.display());
            removed = true;
        }
    }
    if !removed {
        return Err(format!("{id} is not installed under {}", root.display()));
    }
    Ok(())
}

// ── trust ─────────────────────────────────────────────────────────────────────

fn load_publishers(dir: &str) -> Vec<String> {
    let mut keys = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("pem") {
                if let Ok(pem) = std::fs::read_to_string(&p) { keys.push(pem); }
            }
        }
    }
    keys
}

