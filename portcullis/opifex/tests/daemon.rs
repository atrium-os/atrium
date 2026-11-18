//! opifexd's work end to end on the host: a catalog with the platform key, a
//! packed and signed app, its archive, sync, describe, install through the
//! worker (unsandboxed here), update, rollback, uninstall, and refusals.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use opifex::daemon::Daemon;
use opifex::proto::{Request, Response};
use opifex::updates::Action;
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
         [[key]]\npem = \"\"\"{}\"\"\"\nroles = [\"platform\"]\nprefixes = [\"org.atrium.\"]\nkinds = [\"app\", \"font\", \"layer\", \"driver\", \"firmware\"]\n",
        oc::ROOT_SCHEMA, r1.pk, r2.pk, cat.pk, plat.pk);
    fs::create_dir_all(d.join("cat")).unwrap();
    fs::write(d.join("cat/root.toml"), &text).unwrap();
    fs::write(d.join("root.toml"), &text).unwrap();
    let daemon = Daemon {
        root: d.join("root"), state: d.join("state"), root_toml: d.join("root.toml"),
        check_bin: PathBuf::from(env!("CARGO_BIN_EXE_opifex-check")), sandbox: false, worker_uid: 0, sheet_uid: Some(65042),
        trust: portcullis_trust::Trust::new(d.join("pubs"), true), require_content: true, lock: Mutex::new(()),
        fetch_cmd: vec![env!("CARGO_BIN_EXE_opifex-fetch").into(), "--dir".into(), d.join("web").display().to_string()],
        fetch_any_cmd: vec![env!("CARGO_BIN_EXE_opifex-fetch").into(), "--dir".into(), d.join("web").display().to_string()],
        catalog_url: "https://example.org/cat/".into(),
        praeco: None, fonts: d.join("fonts"), system: d.join("sys"),
        sysroot: { let s = d.join("sysroot"); fs::create_dir_all(s.join("libexec")).unwrap(); fs::write(s.join("libexec/ld-elf.so.1"), b"rtld").unwrap(); s },
    };
    W { d, plat, cat, daemon }
}

/// Build, pack, sign, archive and publish version `v`; returns the archive's sha.
fn publish(w: &W, v: &str, body: &[u8]) -> String { publish_m(w, v, body, "", "clipboard = true\n") }

/// The same, with extra `[app]` lines and the `[capabilities]` body.
fn publish_m(w: &W, v: &str, body: &[u8], app_extra: &str, caps: &str) -> String {
    let b = w.d.join(format!("b{v}"));
    let _ = fs::remove_dir_all(&b);
    fs::create_dir_all(b.join("bin")).unwrap();
    fs::write(b.join("bin/hello"), body).unwrap();
    fs::write(b.join("atrium.toml"), format!(
        "[app]\nid = \"{ID}\"\nname = \"Hello\"\nversion = \"{v}\"\nentry = \"bin/hello\"\ndescription = \"Says hello\"\n{app_extra}\n[capabilities]\n{caps}")).unwrap();
    opifex::app::pack(&b, Some(&w.plat.sk)).unwrap();
    publish_dir(w, &b, opifex_arch(), v)
}

/// Archive a packed, signed folder and publish it; returns the archive's sha.
fn publish_dir(w: &W, b: &Path, arch: &str, v: &str) -> String {
    let bytes = opifex::archive::pack(b).unwrap();
    let sha = oc::hex(&Sha256::digest(&bytes));
    fs::create_dir_all(w.daemon.downloads()).unwrap();
    fs::write(w.daemon.downloads().join(format!("{sha}.insula")), &bytes).unwrap();
    // The same archive "on the web", for the fetcher's --dir.
    fs::create_dir_all(w.d.join("web/example.org")).unwrap();
    fs::write(w.d.join(format!("web/example.org/{v}.insula")), &bytes).unwrap();
    let e = oc::release_entry(b, arch, &sha, bytes.len() as u64,
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
    match ask(&w, 1001, Request::Describe { id: ID.into(), for_uid: None }) {
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
    let shown = match ask(&w, 1001, Request::Describe { id: ID.into(), for_uid: None }) {
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
    let now = match ask(&w, 1001, Request::Describe { id: ID.into(), for_uid: None }) {
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

#[test]
fn updates_and_the_daily_round() {
    let w = world("updates");
    publish(&w, "1.0", &elf(1));
    sync(&w);
    assert!(matches!(ask(&w, 0, Request::Install { id: ID.into() }), Response::Done { .. }));
    let pending = |w: &W| match ask(w, 1001, Request::Updates) {
        Response::Updates { items, .. } => items,
        other => panic!("{other:?}"),
    };
    let tick = |w: &W| match w.daemon.tick(false).unwrap() {
        Response::Ticked { installed, waiting } => (installed, waiting),
        other => panic!("{other:?}"),
    };
    let version = |w: &W| w.daemon.store().get(ID).unwrap().unwrap().version;
    assert!(pending(&w).is_empty());

    // The publisher's default: in the background.
    publish(&w, "1.1", &elf(2));
    sync(&w);
    let p = pending(&w);
    assert_eq!((p.len(), p[0].action, p[0].name.as_str()), (1, Action::Background, "Hello"));
    assert_eq!(tick(&w), (vec![format!("{ID} 1.1")], 0));
    assert_eq!(version(&w), "1.1");

    // The publisher asks to be waited on: nothing installed, one notice.
    publish_m(&w, "1.2", &elf(3), "updates = \"notify\"\n", "clipboard = true\n");
    sync(&w);
    assert_eq!(pending(&w)[0].action, Action::Notify);
    assert_eq!(tick(&w), (vec![], 1));
    assert_eq!(version(&w), "1.1");
    let announced = || opifex::updates::PrefStore::new(&w.daemon.state).load().announced;
    assert_eq!(announced(), vec![format!("{ID} 1.2")]);
    tick(&w);
    assert_eq!(announced().len(), 1, "announced once per version");
    assert!(matches!(ask(&w, 0, Request::Install { id: ID.into() }), Response::Done { .. }));

    // More capabilities: asked, whatever the class; the sheet sees the additions.
    publish_m(&w, "1.3", &elf(4), "", "clipboard = true\ncamera = true\n");
    sync(&w);
    let p = pending(&w);
    assert_eq!((p[0].action, p[0].added.clone()), (Action::Ask, vec!["camera".to_string()]));
    match ask(&w, 1001, Request::Describe { id: ID.into(), for_uid: None }) {
        Response::Described { app } => assert_eq!(app.added, vec!["camera".to_string()]),
        other => panic!("{other:?}"),
    }
    assert_eq!(tick(&w), (vec![], 1));
    assert_eq!(version(&w), "1.2");
    assert!(matches!(ask(&w, 0, Request::Install { id: ID.into() }), Response::Done { .. }));

    // A person may ask for more care, not less; only the installer or root.
    publish(&w, "1.4", &elf(5));
    sync(&w);
    assert!(err(ask(&w, 1001, Request::SetUpdates { id: ID.into(), care: "notify".into() })).contains("administrator"));
    assert_eq!(ask(&w, 0, Request::SetUpdates { id: ID.into(), care: "notify".into() }), Response::Ok);
    assert_eq!(pending(&w)[0].action, Action::Notify);
    assert!(err(ask(&w, 0, Request::SetUpdates { id: ID.into(), care: "never".into() })).contains("care"));
    assert_eq!(ask(&w, 0, Request::SetUpdates { id: ID.into(), care: "auto".into() }), Response::Ok);
    assert_eq!(pending(&w)[0].action, Action::Background);
    // The daily round is root's.
    assert!(err(ask(&w, 1001, Request::Tick)).contains("administrator"));
}

const FONT: &str = "org.atrium.font.test-jp";

fn font_package(w: &W, v: &str, tag: u8) -> PathBuf {
    let t = w.d.join(format!("font{v}"));
    let _ = fs::remove_dir_all(&t);
    fs::create_dir_all(&t).unwrap();
    for f in ["TestJP-Regular.ttf", "TestJP-Bold.ttf"] {
        fs::write(t.join(f), [&[0u8, 1, 0, 0][..], &[tag; 60]].concat()).unwrap();
    }
    fs::write(t.join("OFL.txt"), "SIL Open Font License 1.1\n").unwrap();
    let m = format!("kind = \"font\"\n[font]\nid = \"{FONT}\"\nname = \"Test JP\"\nversion = \"{v}\"\nlicense = \"OFL-1.1\"\n\
                     description = \"Japanese, sans serif\"\nlanguages = [\"ja\"]\n\n\
                     [[face]]\nfamily = \"Test Sans JP\"\nfile = \"TestJP-Regular.ttf\"\nweight = 400\nranges = \"3040-30FF,4E00-9FFF\"\n\n\
                     [[face]]\nfamily = \"Test Sans JP\"\nfile = \"TestJP-Bold.ttf\"\nweight = 700\nranges = \"3040-30FF,4E00-9FFF\"\n");
    opifex::font::pack(&t, &m, Some(&w.plat.sk)).unwrap();
    t
}

fn layer_package(w: &W, v: &str) -> PathBuf {
    let t = w.d.join(format!("layer{v}"));
    let _ = fs::remove_dir_all(&t);
    fs::create_dir_all(t.join("bin")).unwrap();
    fs::write(t.join("bin/rustc"), elf(9)).unwrap();
    let m = format!("kind = \"layer\"\n[layer]\nname = \"rust\"\nversion = \"{v}\"\narch = \"{}\"\nmount = \"/atrium/toolchains/rust\"\npath = [\"bin\"]\n",
                    opifex_arch());
    opifex::layer::pack(&t, &m, Some(&w.plat.sk)).unwrap();
    t
}

#[test]
fn fonts_and_layers_from_the_catalog() {
    let w = world("kinds");
    let f = font_package(&w, "1.0", 1);
    publish_dir(&w, &f, "any", "f1.0");
    let l = layer_package(&w, "1.100.0");
    publish_dir(&w, &l, opifex_arch(), "l1.100.0");
    assert!(matches!(sync(&w), Response::Synced { entries: 2, .. }));
    assert_eq!(ask(&w, 1001, Request::Search { text: "japanese".into() }), Response::Found { ids: vec![FONT.into()] });

    // A font: any person, through the sheet.
    let sha = match ask(&w, 1001, Request::Describe { id: FONT.into(), for_uid: None }) {
        Response::Described { app } => {
            assert_eq!((app.kind.as_str(), app.name.as_str(), app.description.as_str()), ("font", "Test JP", "Japanese, sans serif"));
            assert!(app.capabilities.is_empty());
            app.manifest_sha256
        }
        other => panic!("{other:?}"),
    };
    let approved = |id: &str, sha: &str| Request::InstallApproved { id: id.into(), manifest_sha256: sha.into(), for_uid: 1001 };
    assert_eq!(ask(&w, 65042, approved(FONT, &sha)), Response::Done { id: FONT.into(), version: "1.0".into() });
    let pins = fs::read_to_string(w.daemon.fonts.join("pins")).unwrap();
    let faces = atrium_fonts::extra::parse(&pins);
    assert_eq!(faces.len(), 2);
    assert_eq!(faces[0].0.file, format!("{FONT}/TestJP-Regular.ttf"));
    let rec = w.daemon.store().get(FONT).unwrap().unwrap();
    assert_eq!((rec.kind.as_str(), rec.installed_by), ("font", 1001));
    assert!(err(ask(&w, 0, Request::Rollback { id: FONT.into() })).contains("only apps"));

    // Its update goes in on the daily round.
    let f2 = font_package(&w, "1.1", 2);
    publish_dir(&w, &f2, "any", "f1.1");
    sync(&w);
    match w.daemon.tick(false).unwrap() {
        Response::Ticked { installed, waiting } => assert_eq!((installed, waiting), (vec![format!("{FONT} 1.1")], 0)),
        other => panic!("{other:?}"),
    }
    assert_ne!(fs::read_to_string(w.daemon.fonts.join("pins")).unwrap(), pins);

    // A layer: an administrator's.
    let lsha = match ask(&w, 1001, Request::Describe { id: "rust".into(), for_uid: None }) {
        Response::Described { app } => { assert_eq!(app.kind, "layer"); app.manifest_sha256 }
        other => panic!("{other:?}"),
    };
    assert!(err(ask(&w, 65042, approved("rust", &lsha))).contains("administrator"));
    assert_eq!(ask(&w, 0, Request::Install { id: "rust".into() }), Response::Done { id: "rust".into(), version: "1.100.0".into() });
    assert!(w.daemon.root.join("layers/rust@1.100.0/bin/rustc").is_file());
    let l2 = layer_package(&w, "1.101.0");
    publish_dir(&w, &l2, opifex_arch(), "l1.101.0");
    sync(&w);
    match ask(&w, 1001, Request::Updates) {
        Response::Updates { items, .. } => {
            assert_eq!(items.len(), 1);
            assert_eq!((items[0].id.as_str(), items[0].action), ("rust", Action::Notify));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(ask(&w, 0, Request::Install { id: "rust".into() }), Response::Done { id: "rust".into(), version: "1.101.0".into() });
    // The version before stays for workspaces that name it.
    assert!(w.daemon.root.join("layers/rust@1.100.0").is_dir());

    // Removing.
    assert!(matches!(ask(&w, 0, Request::Uninstall { id: FONT.into() }), Response::Done { .. }));
    assert_eq!(fs::read_to_string(w.daemon.fonts.join("pins")).unwrap(), "# atrium-fonts pins 1\n");
    assert!(!w.daemon.fonts.join(FONT).exists());
    assert!(matches!(ask(&w, 0, Request::Uninstall { id: "rust".into() }), Response::Done { .. }));
    assert!(!w.daemon.root.join("layers/rust@1.101.0").exists());
}

#[test]
fn a_driver_with_its_firmware() {
    let w = world("driver");
    let drv = "org.atrium.driver.rtl";
    let fw = w.d.join("fwpkg");
    fs::create_dir_all(&fw).unwrap();
    fs::write(fw.join("rtl8153a-4.fw"), b"blob").unwrap();
    opifex::driver::pack_firmware(&fw, "kind = \"firmware\"\n[firmware]\nid = \"org.atrium.firmware.rtl\"\nname = \"RTL firmware\"\n\
        version = \"1\"\nlicense = \"LicenseRef-Realtek\"\nplace = \"jailed\"\ndir = \"rtl8153\"\n", Some(&w.plat.sk)).unwrap();
    publish_dir(&w, &fw, "any", "fw1");
    let t = w.d.join("drvpkg");
    fs::create_dir_all(&t).unwrap();
    fs::write(t.join("rtl"), elf(3)).unwrap();
    fs::write(t.join("devices.toml"), format!("[[rule]]\nname = \"rtl\"\nclass = \"usb\"\nusb_ids = [\"0bda:8153\"]\nnodes = [\"ugen{{bus}}.{{addr}}\"]\n\
        owner = \"driver:rtl\"\n\n[driver.rtl]\nexec = \"/usr/local/libexec/atrium/drivers/{drv}/rtl\"\nargv = [\"rtl\"]\nuid = 61060\ngid = 61060\n\
        jail = \"atrium-driver-rtl-{{usb}}\"\nroot = \"/var/lib/atrium/jails/driver-rtl-{{usb}}\"\ndevfs_ruleset = 33\nsysfil = \"device\"\n")).unwrap();
    opifex::driver::pack_driver(&t, &format!("kind = \"driver\"\n[driver]\nid = \"{drv}\"\nname = \"Realtek USB Ethernet\"\nversion = \"1.0\"\n\
        license = \"BSD-2-Clause\"\nfirmware = [\"org.atrium.firmware.rtl\"]\n\n[[match]]\nclass = \"usb\"\nusb_ids = [\"0bda:8153\"]\n"), Some(&w.plat.sk)).unwrap();
    publish_dir(&w, &t, opifex_arch(), "d1");
    assert!(matches!(sync(&w), Response::Synced { entries: 2, .. }));

    // The lookup is local: devices.idx after the sync.
    let idx = fs::read_to_string(w.daemon.state.join("devices.idx")).unwrap();
    assert_eq!(opifex::driver::lookup(&idx, Some(((0x0bda, 0x8153), [0xff, 0, 0])), &[]), vec![drv.to_string()]);

    // An administrator's: not through the sheet.
    let sha = match ask(&w, 1001, Request::Describe { id: drv.into(), for_uid: None }) {
        Response::Described { app } => { assert_eq!((app.kind.as_str(), app.name.as_str()), ("driver", "Realtek USB Ethernet")); app.manifest_sha256 }
        other => panic!("{other:?}"),
    };
    assert!(err(ask(&w, 65042, Request::InstallApproved { id: drv.into(), manifest_sha256: sha, for_uid: 1001 })).contains("administrator"));
    assert_eq!(ask(&w, 0, Request::Install { id: drv.into() }), Response::Done { id: drv.into(), version: "1.0".into() });
    let sys = &w.daemon.system;
    assert!(sys.join(format!("usr/local/etc/atrium/devices.d/60-{drv}.toml")).is_file());
    assert!(sys.join(format!("usr/local/libexec/atrium/drivers/{drv}/rtl")).is_file());
    assert_eq!(fs::read(sys.join("usr/local/share/atrium/firmware/rtl8153/rtl8153a-4.fw")).unwrap(), b"blob");
    assert_eq!(w.daemon.store().get("org.atrium.firmware.rtl").unwrap().unwrap().kind, "firmware");

    assert!(matches!(ask(&w, 0, Request::Uninstall { id: drv.into() }), Response::Done { .. }));
    assert!(!sys.join(format!("usr/local/etc/atrium/devices.d/60-{drv}.toml")).exists());
    assert!(matches!(ask(&w, 0, Request::Uninstall { id: "org.atrium.firmware.rtl".into() }), Response::Done { .. }));
    assert!(!sys.join("usr/local/share/atrium/firmware/rtl8153").exists());
}

// ── C7: a catalog someone added ─────────────────────────────────────────────

const TP: &str = "com.example.hello";
const SHEET: u32 = 65042;

struct Third { base: PathBuf, cat: K, roots: Vec<K> }

/// A third-party catalog served at https://apps.example.com/cat/.
fn third(w: &W) -> Third {
    let base = w.d.join("web/apps.example.com/cat");
    fs::create_dir_all(&base).unwrap();
    let (r1, r2, cat) = (key(30), key(31), key(32));
    let text = format!(
        "schema = \"{}\"\nname = \"Example Apps\"\nversion = 1\nexpires = 4000000000\nthreshold = 2\n\n\
         [[key]]\npem = \"\"\"{}\"\"\"\nroles = [\"root\"]\n\n[[key]]\npem = \"\"\"{}\"\"\"\nroles = [\"root\"]\n\n\
         [[key]]\npem = \"\"\"{}\"\"\"\nroles = [\"catalog\"]\n", oc::ROOT_SCHEMA, r1.pk, r2.pk, cat.pk);
    fs::write(base.join("root.toml"), text).unwrap();
    // The Atrium catalog "on the web" too, so a full sync reaches it.
    let _ = std::os::unix::fs::symlink(w.d.join("cat"), w.d.join("web/example.org/cat"));
    Third { base, cat, roots: vec![r1, r2] }
}

fn pub_entry(t: &Third, action: &str, id: &str, domains: &[&str], k: &K, signers: &[&K]) {
    let mut p = oc::publisher::PublisherEntry {
        id: id.into(), action: action.into(), name: "Example Ltd".into(), domains: domains.iter().map(|d| d.to_string()).collect(),
        kinds: vec!["app".into()], devices: false, key: k.pk.clone(), sigs: vec![] };
    for s in signers {
        p.sign_with(&s.sk, &s.pk).unwrap();
    }
    let log = oc::Log::third_party(&t.base);
    log.append(&[p.entry(now())]).unwrap();
    log.sign_head(&t.cat.sk, now(), 14).unwrap();
}

fn pub_app(w: &W, t: &Third, id: &str, v: &str, k: &K, publisher: &str) {
    let b = w.d.join(format!("tp-{id}-{v}"));
    let _ = fs::remove_dir_all(&b);
    fs::create_dir_all(b.join("bin")).unwrap();
    fs::write(b.join("bin/hello"), elf(v.len() as u8)).unwrap();
    fs::write(b.join("atrium.toml"), format!(
        "[app]\nid = \"{id}\"\nname = \"Example Hello\"\nversion = \"{v}\"\nentry = \"bin/hello\"\n\n[capabilities]\nclipboard = true\n")).unwrap();
    opifex::app::pack(&b, Some(&k.sk)).unwrap();
    let bytes = opifex::archive::pack(&b).unwrap();
    let sha = oc::hex(&Sha256::digest(&bytes));
    fs::create_dir_all(w.d.join("web/dl.example.com")).unwrap();
    fs::write(w.d.join(format!("web/dl.example.com/{id}-{v}.insula")), &bytes).unwrap();
    let mut e = oc::release_entry(&b, opifex_arch(), &sha, bytes.len() as u64,
                                  vec![format!("https://dl.example.com/{id}-{v}.insula")], &k.pk, now()).unwrap();
    e.release.as_mut().unwrap().publisher = publisher.into();
    let log = oc::Log::third_party(&t.base);
    log.append(&[e]).unwrap();
    log.sign_head(&t.cat.sk, now(), 14).unwrap();
}

#[test]
fn a_catalog_someone_added() {
    let w = world("third");
    publish(&w, "1.0", &elf(1));
    let t = third(&w);
    let (k1, k2, k3, evil) = (key(40), key(41), key(42), key(43));
    let pid = oc::publisher::publisher_id(&k1.pk).unwrap();
    let eid = oc::publisher::publisher_id(&evil.pk).unwrap();
    pub_entry(&t, "register", &pid, &["example.com"], &k1, &[&k1]);
    pub_app(&w, &t, TP, "1.0", &k1, &pid);
    // A publisher claiming a bank's domain, with no proof there.
    pub_entry(&t, "register", &eid, &["bank.test"], &evil, &[&evil]);
    pub_app(&w, &t, "test.bank.app", "1.0", &evil, &eid);
    // example.com proves its publisher (the .well-known file).
    fs::create_dir_all(w.d.join("web/example.com/.well-known")).unwrap();
    fs::write(w.d.join("web/example.com/.well-known/atrium-publisher"), oc::publisher::proof_text(&pid)).unwrap();

    let url = "https://apps.example.com/cat/";
    let sha = match ask(&w, 1001, Request::PreviewCatalog { url: url.into() }) {
        Response::CatalogPreview { name, root_sha256, warning, .. } => {
            assert_eq!(name, "Example Apps");
            assert!(warning.contains("nobody but its publisher"));
            root_sha256
        }
        other => panic!("{other:?}"),
    };
    let add = |uid, machine, sha: &str| ask(&w, uid, Request::AddCatalog { url: url.into(), root_sha256: sha.into(), machine, for_uid: 1001 });
    assert!(err(add(1001, false, &sha)).contains("administrator"), "a person adds one through the sheet");
    assert!(err(add(SHEET, true, &sha)).contains("administrator"), "everyone's: an administrator's");
    assert!(err(add(SHEET, false, &"00".repeat(32))).contains("changed since"));
    match add(SHEET, false, &sha) {
        Response::CatalogAdded { slug, summary, .. } => {
            assert_eq!(slug, "apps.example.com");
            assert!(summary.contains("1 releases offered") && summary.contains("not proven: bank.test"), "{summary}");
        }
        other => panic!("{other:?}"),
    }
    assert!(err(add(SHEET, false, &sha)).contains("added already"));

    // Only the person who added it sees it; the unproven publisher's app is offered to nobody.
    let described = |uid, for_uid, id: &str| ask(&w, uid, Request::Describe { id: id.into(), for_uid });
    match described(1001, None, TP) {
        Response::Described { app } => {
            assert!(app.third_party);
            assert_eq!((app.publisher.as_str(), app.publisher_name.as_str(), app.catalog.as_str()), (pid.as_str(), "Example Ltd", "Example Apps"));
            assert_eq!(app.publisher_domains, vec!["example.com".to_string()]);
        }
        other => panic!("{other:?}"),
    }
    assert!(err(described(1002, None, TP)).contains("not in the catalog"));
    assert!(err(described(1002, Some(1001), TP)).contains("not in the catalog"), "for_uid only from the sheet");
    assert!(matches!(described(SHEET, Some(1001), TP), Response::Described { .. }));
    assert!(err(described(1001, None, "test.bank.app")).contains("not in the catalog"));
    assert_eq!(ask(&w, 1001, Request::Search { text: "example hello".into() }), Response::Found { ids: vec![TP.into()] });
    assert_eq!(ask(&w, 1002, Request::Search { text: "example hello".into() }), Response::Found { ids: vec![] });
    match ask(&w, 1002, Request::Catalogs) { Response::Catalogs { items } => assert_eq!(items.len(), 1), o => panic!("{o:?}") }
    match ask(&w, 1001, Request::Catalogs) {
        Response::Catalogs { items } => {
            assert_eq!(items.len(), 2);
            assert!(items[1].third_party && !items[1].machine && items[1].added_by == 1001);
            assert!(items[1].publishers.iter().any(|p| p.contains("proven: example.com")));
            assert!(items[1].publishers.iter().any(|p| p.contains("proven: none")));
        }
        o => panic!("{o:?}"),
    }

    // Installed through the sheet, for that person.
    let msha = match described(SHEET, Some(1001), TP) { Response::Described { app } => app.manifest_sha256, o => panic!("{o:?}") };
    assert_eq!(ask(&w, SHEET, Request::InstallApproved { id: TP.into(), manifest_sha256: msha, for_uid: 1001 }),
               Response::Done { id: TP.into(), version: "1.0".into() });
    let rec = w.daemon.store().get(TP).unwrap().unwrap();
    assert_eq!((rec.source.as_str(), rec.publisher.as_str(), rec.installed_by), ("catalog:apps.example.com", pid.as_str(), 1001));

    // A rotated key is the same publisher: the update goes in the background.
    pub_entry(&t, "rotate", &pid, &["example.com"], &k2, &[&k1, &k2]);
    pub_app(&w, &t, TP, "1.1", &k2, &pid);
    match ask(&w, 0, Request::Sync { from: String::new() }) {
        Response::Synced { others, .. } => assert!(others.iter().any(|o| o.starts_with("apps.example.com: ")), "{others:?}"),
        o => panic!("{o:?}"),
    }
    let ups = match ask(&w, 1001, Request::Updates) { Response::Updates { items, .. } => items, o => panic!("{o:?}") };
    let u = ups.iter().find(|u| u.id == TP).unwrap();
    assert_eq!((u.to.as_str(), u.action), ("1.1", Action::Background));
    assert!(matches!(ask(&w, 0, Request::Tick), Response::Ticked { .. }));
    assert_eq!(w.daemon.store().get(TP).unwrap().unwrap().version, "1.1");

    // A recovery (two root keys): the next update asks the person again.
    pub_entry(&t, "recover", &pid, &["example.com"], &k3, &[&t.roots[0], &t.roots[1], &k3]);
    pub_app(&w, &t, TP, "1.2", &k3, &pid);
    ask(&w, 0, Request::Sync { from: String::new() });
    let ups = match ask(&w, 1001, Request::Updates) { Response::Updates { items, .. } => items, o => panic!("{o:?}") };
    let u = ups.iter().find(|u| u.id == TP).unwrap();
    assert_eq!(u.action, Action::Ask);
    assert!(u.why.contains("recovery"), "{}", u.why);
    match described(SHEET, Some(1001), TP) { Response::Described { app } => assert!(app.recovered), o => panic!("{o:?}") }

    // Removing it: another person may not; the one who added it may. The app stays; its updates stop.
    assert!(err(ask(&w, 1002, Request::RemoveCatalog { slug: "apps.example.com".into() })).contains("only an administrator"));
    assert_eq!(ask(&w, 1001, Request::RemoveCatalog { slug: "apps.example.com".into() }), Response::Ok);
    assert!(w.daemon.root.join("apps").join(TP).is_dir());
    let ups = match ask(&w, 1001, Request::Updates) { Response::Updates { items, .. } => items, o => panic!("{o:?}") };
    assert!(!ups.iter().any(|u| u.id == TP));
}
