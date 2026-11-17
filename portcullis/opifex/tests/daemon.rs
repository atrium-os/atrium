//! opifexd's work end to end on the host: a catalog with the platform key, a
//! packed and signed app, its archive, sync, describe, install through the
//! worker (unsandboxed here), update, rollback, uninstall, and refusals.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use opifex::daemon::Daemon;
use opifex::proto::{Request, Response};
use opifex_catalog as oc;
use sha2::{Digest, Sha256};

const ID: &str = "org.atrium.hello";

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("opifexd-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

struct K { sk: String, pk: String }
fn key(s: u8) -> K { let (sk, pk) = portcullis_sig::new_key([s; 32]).unwrap(); K { sk, pk } }

struct W { d: PathBuf, plat: K, cat: K, daemon: Daemon }

fn world(tag: &str) -> W {
    let d = tmp(tag);
    let (r1, r2, cat, plat) = (key(1), key(2), key(3), key(4));
    let text = format!(
        "schema = \"{}\"\nname = \"Atrium\"\nversion = 1\nexpires = 4000000000\nthreshold = 1\n\n\
         [[key]]\npem = \"\"\"{}\"\"\"\nroles = [\"root\"]\n\n[[key]]\npem = \"\"\"{}\"\"\"\nroles = [\"root\"]\n\n\
         [[key]]\npem = \"\"\"{}\"\"\"\nroles = [\"catalog\"]\n\n\
         [[key]]\npem = \"\"\"{}\"\"\"\nroles = [\"platform\"]\nprefixes = [\"org.atrium.\"]\nkinds = [\"app\"]\n",
        oc::ROOT_SCHEMA, r1.pk, r2.pk, cat.pk, plat.pk);
    fs::create_dir_all(d.join("cat")).unwrap();
    fs::write(d.join("cat/root.toml"), &text).unwrap();
    fs::write(d.join("root.toml"), &text).unwrap();
    let daemon = Daemon {
        root: d.join("root"), state: d.join("state"), root_toml: d.join("root.toml"),
        check_bin: PathBuf::from(env!("CARGO_BIN_EXE_opifex-check")), sandbox: false, worker_uid: 0, sheet_uid: Some(65042),
        trust: portcullis_trust::Trust::new(d.join("pubs"), true), require_content: true, lock: Mutex::new(()),
        fetch_cmd: vec![env!("CARGO_BIN_EXE_opifex-fetch").into(), "--dir".into(), d.join("web").display().to_string()],
        catalog_url: "https://example.org/cat/".into(),
        sysroot: { let s = d.join("sysroot"); fs::create_dir_all(s.join("libexec")).unwrap(); fs::write(s.join("libexec/ld-elf.so.1"), b"rtld").unwrap(); s },
    };
    W { d, plat, cat, daemon }
}

/// Build, pack, sign, archive and publish version `v`; returns the archive's sha.
fn publish(w: &W, v: &str, body: &[u8]) -> String {
    let b = w.d.join(format!("b{v}"));
    let _ = fs::remove_dir_all(&b);
    fs::create_dir_all(b.join("bin")).unwrap();
    fs::write(b.join("bin/hello"), body).unwrap();
    fs::write(b.join("atrium.toml"), format!(
        "[app]\nid = \"{ID}\"\nname = \"Hello\"\nversion = \"{v}\"\nentry = \"bin/hello\"\ndescription = \"Says hello\"\n\n[capabilities]\nclipboard = true\n")).unwrap();
    opifex::app::pack(&b, Some(&w.plat.sk)).unwrap();
    let bytes = opifex::archive::pack(&b).unwrap();
    let sha = oc::hex(&Sha256::digest(&bytes));
    fs::create_dir_all(w.daemon.downloads()).unwrap();
    fs::write(w.daemon.downloads().join(format!("{sha}.insula")), &bytes).unwrap();
    // The same archive "on the web", for the fetcher's --dir.
    fs::create_dir_all(w.d.join("web/example.org")).unwrap();
    fs::write(w.d.join(format!("web/example.org/{v}.insula")), &bytes).unwrap();
    let e = oc::release_entry(&b, opifex_arch(), &sha, bytes.len() as u64,
                              vec![format!("https://example.org/{v}.insula")], &w.plat.pk, 1_800_000_000).unwrap();
    let log = oc::Log::open(&w.d.join("cat"));
    log.append(&[e]).unwrap();
    log.sign_head(&w.cat.sk, now(), 14).unwrap();
    sha
}

/// A minimal static ELF64 header (no program headers): enough for the
/// side-load closure walk, which finds nothing to resolve.
fn elf(tag: u8) -> Vec<u8> { let mut b = b"\x7fELF\x02\x01\x01".to_vec(); b.resize(64, 0); b.push(tag); b }

fn opifex_arch() -> &'static str { portcullis_toml::layer::host_arch() }
fn now() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() }

fn ask(w: &W, uid: u32, r: Request) -> Response { w.daemon.handle(uid, r) }
fn sync(w: &W) -> Response { ask(w, 0, Request::Sync { from: w.d.join("cat").display().to_string() }) }
fn err(r: Response) -> String { match r { Response::Error { message } => message, other => panic!("not an error: {other:?}") } }

#[test]
fn catalog_install_update_rollback() {
    let w = world("cycle");
    publish(&w, "1.0", &elf(1));
    assert!(matches!(sync(&w), Response::Synced { entries: 1, .. }));
    match ask(&w, 1001, Request::Describe { id: ID.into() }) {
        Response::Described { app } => {
            assert_eq!((app.name.as_str(), app.latest.as_str(), app.installed.as_deref()), ("Hello", "1.0", None));
            assert_eq!(app.capabilities, vec!["clipboard".to_string()]);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(ask(&w, 1001, Request::Search { text: "says hello".into() }), Response::Found { ids: vec![ID.into()] });
    // A person asks through the sheet, not directly.
    assert!(err(ask(&w, 1001, Request::Install { id: ID.into() })).contains("administrator"));
    assert_eq!(ask(&w, 0, Request::Install { id: ID.into() }), Response::Done { id: ID.into(), version: "1.0".into() });
    let app = w.daemon.root.join("apps").join(ID);
    assert_eq!(fs::read(app.join("bin/hello")).unwrap(), elf(1));
    let rec = w.daemon.store().get(ID).unwrap().unwrap();
    assert_eq!(rec.source, "catalog:Atrium");
    assert_eq!(rec.publisher, oc::key_id(&w.plat.pk).unwrap());
    // Update, rollback.
    publish(&w, "1.1", &elf(2));
    sync(&w);
    assert_eq!(ask(&w, 0, Request::Install { id: ID.into() }), Response::Done { id: ID.into(), version: "1.1".into() });
    assert_eq!(fs::read(app.join("bin/hello")).unwrap(), elf(2));
    assert_eq!(ask(&w, 0, Request::Rollback { id: ID.into() }), Response::Done { id: ID.into(), version: "1.0".into() });
    assert_eq!(w.daemon.store().get(ID).unwrap().unwrap().version, "1.0");
    assert!(matches!(ask(&w, 0, Request::Uninstall { id: ID.into() }), Response::Done { .. }));
    assert!(!app.exists());
    assert!(w.daemon.store().get(ID).unwrap().is_none());
    // Nothing left behind in unpack/ or staging/.
    assert_eq!(fs::read_dir(w.daemon.state.join("unpack")).unwrap().count(), 0);
    assert_eq!(fs::read_dir(w.daemon.root.join("opifex/staging")).unwrap().count(), 0);
}

#[test]
fn refusals() {
    let w = world("refuse");
    let sha = publish(&w, "1.0", &elf(1));
    sync(&w);
    // Not downloaded, and not on the web either.
    let dl = w.daemon.downloads().join(format!("{sha}.insula"));
    let good = fs::read(&dl).unwrap();
    fs::remove_file(&dl).unwrap();
    fs::remove_file(w.d.join("web/example.org/1.0.insula")).unwrap();
    assert!(err(ask(&w, 0, Request::Install { id: ID.into() })).contains("download failed"));
    // A different archive under the right name: refused and removed.
    let mut bad = good.clone();
    let n = bad.len();
    bad[n - 1] ^= 1;
    fs::write(&dl, &bad).unwrap();
    assert!(err(ask(&w, 0, Request::Install { id: ID.into() })).contains("not the one the catalog names"));
    assert!(!dl.exists());
    // Not in the catalog.
    assert!(err(ask(&w, 0, Request::Install { id: "org.atrium.nope".into() })).contains("not in the catalog"));
    // A side-loaded archive signed by nobody the machine trusts.
    fs::write(w.d.join("x.insula"), &good).unwrap();
    assert!(err(ask(&w, 0, Request::InstallLocal { path: w.d.join("x.insula").display().to_string() })).contains("trust"));
    // ...and with the platform key on the side-load list it installs.
    fs::create_dir_all(w.d.join("pubs")).unwrap();
    fs::write(w.d.join("pubs/p.pem"), &w.plat.pk).unwrap();
    let r = ask(&w, 0, Request::InstallLocal { path: w.d.join("x.insula").display().to_string() });
    assert!(matches!(r, Response::Done { .. }), "{r:?}");
    assert_eq!(w.daemon.store().get(ID).unwrap().unwrap().source, "side-load");
    // A garbage archive: the worker refuses, nothing installed changes.
    fs::write(w.d.join("junk.insula"), b"INSB\x02\0\0\0\xff\xff\xff\xff\xff\xff\xff\xff").unwrap();
    assert!(err(ask(&w, 0, Request::InstallLocal { path: w.d.join("junk.insula").display().to_string() })).contains("too many"));
    assert_eq!(fs::read(w.daemon.root.join("apps").join(ID).join("bin/hello")).unwrap(), elf(1));
    let _ = Path::new("");
}

#[test]
fn the_sheet_installs_what_the_person_saw() {
    let w = world("sheet");
    publish(&w, "1.0", &elf(1));
    sync(&w);
    let shown = match ask(&w, 1001, Request::Describe { id: ID.into() }) {
        Response::Described { app } => app.manifest_sha256,
        other => panic!("{other:?}"),
    };
    assert_eq!(shown.len(), 64);
    let approved = |m: &str| Request::InstallApproved { id: ID.into(), manifest_sha256: m.into(), for_uid: 1001 };
    // Only the sheet's uid (or root) may send it.
    assert!(err(ask(&w, 1001, approved(&shown))).contains("install sheet"));
    // A release published after the sheet was shown is not what was approved.
    publish(&w, "1.1", &elf(2));
    sync(&w);
    assert!(err(ask(&w, 65042, approved(&shown))).contains("catalog changed"));
    assert!(w.daemon.store().get(ID).unwrap().is_none());
    let now = match ask(&w, 1001, Request::Describe { id: ID.into() }) {
        Response::Described { app } => app.manifest_sha256,
        other => panic!("{other:?}"),
    };
    assert_eq!(ask(&w, 65042, approved(&now)), Response::Done { id: ID.into(), version: "1.1".into() });
    let rec = w.daemon.store().get(ID).unwrap().unwrap();
    assert_eq!(rec.installed_by, 1001, "recorded for the person, not the sheet");
    // The sheet may not remove, roll back, side-load or sync.
    assert!(err(ask(&w, 65042, Request::Uninstall { id: ID.into() })).contains("administrator"));
    assert!(err(ask(&w, 65042, Request::Install { id: ID.into() })).contains("administrator"));
}

#[test]
fn over_the_network() {
    let w = world("net");
    let sha = publish(&w, "1.0", &elf(1));
    // The catalog "served" at https://example.org/cat/.
    std::os::unix::fs::symlink(w.d.join("cat"), w.d.join("web/example.org/cat")).unwrap();
    // Nothing downloaded yet: the archive comes through the fetcher.
    fs::remove_file(w.daemon.downloads().join(format!("{sha}.insula"))).unwrap();
    assert!(matches!(ask(&w, 0, Request::Sync { from: String::new() }), Response::Synced { entries: 1, .. }));
    assert_eq!(ask(&w, 0, Request::Install { id: ID.into() }), Response::Done { id: ID.into(), version: "1.0".into() });
    assert_eq!(fs::read(w.daemon.root.join("apps").join(ID).join("bin/hello")).unwrap(), elf(1));
    // A server that hands out other bytes: refused, nothing kept.
    let sha2 = publish(&w, "1.1", &elf(2));
    fs::remove_file(w.daemon.downloads().join(format!("{sha2}.insula"))).unwrap();
    fs::write(w.d.join("web/example.org/1.1.insula"), b"INSB not the archive").unwrap();
    assert!(matches!(ask(&w, 0, Request::Sync { from: "https://example.org/cat".into() }), Response::Synced { entries: 2, .. }));
    let e = err(ask(&w, 0, Request::Install { id: ID.into() }));
    assert!(e.contains("not the archive the catalog names"), "{e}");
    assert!(!w.daemon.downloads().join(format!("{sha2}.insula")).exists());
    assert_eq!(w.daemon.store().get(ID).unwrap().unwrap().version, "1.0");
    // A missing file and a catalog that isn't there.
    fs::remove_file(w.d.join("web/example.org/1.1.insula")).unwrap();
    assert!(err(ask(&w, 0, Request::Install { id: ID.into() })).contains("404"));
    assert!(err(ask(&w, 0, Request::Sync { from: "https://example.org/nothing/".into() })).contains("404"));
    // Only https.
    assert!(err(ask(&w, 0, Request::Sync { from: "https://someone@example.org/cat/".into() })).contains("not a plain https URL"));
}
