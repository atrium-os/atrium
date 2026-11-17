use super::*;

struct Keys { sk: String, pk: String }

fn key(seed: u8) -> Keys {
    let (sk, pk) = portcullis_sig::new_key([seed; 32]).unwrap();
    Keys { sk, pk }
}

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("opifex-catalog-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

const NOW: u64 = 1_800_000_000;

fn root_text(r: &[&Keys], cat: &Keys, plat: &Keys, version: u64) -> String {
    let mut s = format!("schema = \"{ROOT_SCHEMA}\"\nname = \"Atrium\"\nversion = {version}\nexpires = {}\nthreshold = 2\n", NOW + 1_000_000);
    for k in r {
        s += &format!("\n[[key]]\npem = \"\"\"{}\"\"\"\nroles = [\"root\"]\n", k.pk);
    }
    s += &format!("\n[[key]]\npem = \"\"\"{}\"\"\"\nroles = [\"catalog\"]\n", cat.pk);
    s += &format!("\n[[key]]\npem = \"\"\"{}\"\"\"\nroles = [\"platform\"]\nprefixes = [\"org.atrium.\"]\nkinds = [\"app\", \"layer\", \"font\"]\n", plat.pk);
    s
}

struct World { dir: PathBuf, roots: Vec<Keys>, cat: Keys, plat: Keys }

fn world(tag: &str) -> World {
    let dir = tmp(tag);
    let roots = vec![key(1), key(2), key(3)];
    let (cat, plat) = (key(4), key(5));
    let text = root_text(&[&roots[0], &roots[1], &roots[2]], &cat, &plat, 1);
    let sigs = sign_root(&text, &roots[0].sk, &roots[0].pk).unwrap() + &sign_root(&text, &roots[1].sk, &roots[1].pk).unwrap();
    fs::create_dir_all(dir.join("cat")).unwrap();
    fs::write(dir.join("cat").join(ROOT), &text).unwrap();
    fs::write(dir.join("cat").join(ROOT_SIG), sigs).unwrap();
    World { dir, roots, cat, plat }
}

fn app_manifest(id: &str, version: &str) -> String {
    format!("[app]\nid = \"{id}\"\nname = \"X\"\nversion = \"{version}\"\nentry = \"bin/x\"\n\n[bundle]\ncontent = \"sha256:{}\"\n", "ab".repeat(32))
}

fn release(_w: &World, id: &str, version: &str, signer: &Keys) -> Entry {
    let manifest = app_manifest(id, version);
    let sig = b64(&portcullis_sig::sign(manifest.as_bytes(), &signer.sk).unwrap());
    Entry { kind: "release".into(), time: NOW, yank: None, locations: None, release: Some(Release {
        id: id.into(), version: version.into(), arch: "aarch64".into(), kind: "app".into(),
        signer: key_id(&signer.pk).unwrap(), manifest, sig, archive_sha256: "cd".repeat(32), archive_size: 10,
        urls: vec![format!("https://example.org/{id}-{version}.insula")] }) }
}

fn fetcher(dir: PathBuf) -> impl Fn(&str) -> Result<Vec<u8>, String> {
    move |p: &str| fs::read(dir.join(p)).map_err(|e| format!("{p}: {e}"))
}

fn client(w: &World) -> Client {
    let text = fs::read_to_string(w.dir.join("cat").join(ROOT)).unwrap();
    Client::init(&w.dir.join("state"), &text).unwrap()
}

#[test]
fn merkle_known_values() {
    // RFC 6962 shape: a 3-leaf tree is node(node(a,b), c).
    let (a, b, c) = (leaf_hash(b"a"), leaf_hash(b"b"), leaf_hash(b"c"));
    assert_eq!(tree_hash(&[a, b, c]), node(&node(&a, &b), &c));
    assert_eq!(tree_hash(&[]), <[u8; 32]>::from(Sha256::digest(b"")));
}

#[test]
fn publish_and_sync_across_bundles() {
    let w = world("sync");
    let log = Log::open(&w.dir.join("cat"));
    // 300 releases: one full bundle and a partial one.
    let first: Vec<Entry> = (0..200).map(|i| release(&w, "org.atrium.a", &format!("1.{i}"), &w.plat)).collect();
    log.append(&first).unwrap();
    log.sign_head(&w.cat.sk, NOW, 14).unwrap();
    let c = client(&w);
    let l = c.update(&fetcher(w.dir.join("cat")), NOW).unwrap();
    assert_eq!(l.size, 200);
    let more: Vec<Entry> = (200..300).map(|i| release(&w, "org.atrium.a", &format!("1.{i}"), &w.plat)).collect();
    log.append(&more).unwrap();
    assert!(w.dir.join("cat/entries/000000").is_file() && w.dir.join("cat/entries/000001.p44").is_file());
    assert!(!w.dir.join("cat/entries/000000.p200").exists());
    log.sign_head(&w.cat.sk, NOW + 10, 14).unwrap();
    let l = c.update(&fetcher(w.dir.join("cat")), NOW + 10).unwrap();
    assert_eq!(l.size, 300);
    assert_eq!(l.latest("org.atrium.a", "aarch64").unwrap().version, "1.299");
    // Yank the newest; latest steps back.
    log.append(&[yank_entry("org.atrium.a", "1.299", "broken", &w.plat.sk, &w.plat.pk, NOW).unwrap()]).unwrap();
    log.sign_head(&w.cat.sk, NOW + 20, 14).unwrap();
    let l = c.update(&fetcher(w.dir.join("cat")), NOW + 20).unwrap();
    assert_eq!(l.latest("org.atrium.a", "aarch64").unwrap().version, "1.298");
}

#[test]
fn publishing_refuses() {
    let w = world("pubrefuse");
    let log = Log::open(&w.dir.join("cat"));
    let stranger = key(9);
    assert!(log.append(&[release(&w, "org.atrium.a", "1", &stranger)]).unwrap_err().contains("not in"));
    assert!(log.append(&[release(&w, "com.evil.a", "1", &w.plat)]).unwrap_err().contains("prefixes"));
    let mut e = release(&w, "org.atrium.a", "1", &w.plat);
    e.release.as_mut().unwrap().manifest = app_manifest("org.atrium.a", "2");
    assert!(log.append(&[e]).unwrap_err().contains("not signed"));
    log.append(&[release(&w, "org.atrium.a", "1", &w.plat)]).unwrap();
    assert!(log.append(&[release(&w, "org.atrium.a", "1", &w.plat)]).unwrap_err().contains("already"));
    assert!(log.sign_head(&w.plat.sk, NOW, 14).unwrap_err().contains("catalog key"));
}

#[test]
fn client_refuses_tampering() {
    let w = world("tamper");
    let cat = w.dir.join("cat");
    let log = Log::open(&cat);
    log.append(&[release(&w, "org.atrium.a", "1", &w.plat), release(&w, "org.atrium.b", "1", &w.plat)]).unwrap();
    log.sign_head(&w.cat.sk, NOW, 14).unwrap();
    let c = client(&w);
    c.update(&fetcher(cat.clone()), NOW).unwrap();

    // Expired head (a frozen catalog).
    assert!(c.update(&fetcher(cat.clone()), NOW + 15 * 86400).unwrap_err().contains("expired"));
    // A head signed by someone else.
    fs::write(cat.join(HEAD_SIG), b64(&portcullis_sig::sign(fs::read(cat.join(HEAD)).unwrap().as_slice(), &w.plat.sk).unwrap())).unwrap();
    assert!(c.update(&fetcher(cat.clone()), NOW).unwrap_err().contains("catalog key"));
    log.sign_head(&w.cat.sk, NOW, 14).unwrap();

    // History rewritten: replace entry 0 and re-sign the head (a stolen catalog key).
    let bytes = encode_bundle(&[release(&w, "org.atrium.a", "9", &w.plat).bytes(), release(&w, "org.atrium.b", "1", &w.plat).bytes()]);
    fs::write(cat.join("entries/000000.p2"), bytes).unwrap();
    log.sign_head(&w.cat.sk, NOW, 14).unwrap();
    assert!(c.update(&fetcher(cat.clone()), NOW).unwrap_err().contains("history rewritten"));

    // A forged release slipped in with a valid head (stolen catalog key, no platform key).
    let w2 = world("tamper2");
    let cat2 = w2.dir.join("cat");
    let log2 = Log::open(&cat2);
    log2.append(&[release(&w2, "org.atrium.a", "1", &w2.plat)]).unwrap();
    log2.sign_head(&w2.cat.sk, NOW, 14).unwrap();
    let c2 = client(&w2);
    c2.update(&fetcher(cat2.clone()), NOW).unwrap();
    let mut forged = release(&w2, "org.atrium.a", "2", &w2.plat);
    forged.release.as_mut().unwrap().manifest = app_manifest("org.atrium.a", "3");
    let all = vec![release(&w2, "org.atrium.a", "1", &w2.plat).bytes(), forged.bytes()];
    fs::write(cat2.join("entries/000000.p2"), encode_bundle(&all)).unwrap();
    let _ = fs::remove_file(cat2.join("entries/000000.p1"));
    log2.sign_head(&w2.cat.sk, NOW, 14).unwrap();
    assert!(c2.update(&fetcher(cat2.clone()), NOW).unwrap_err().contains("not signed"));
    // State unchanged after every refusal.
    assert_eq!(c2.local().unwrap().size, 1);
}

#[test]
fn root_rotation_needs_old_and_new_thresholds() {
    let w = world("rotate");
    let cat = w.dir.join("cat");
    let log = Log::open(&cat);
    log.append(&[release(&w, "org.atrium.a", "1", &w.plat)]).unwrap();
    log.sign_head(&w.cat.sk, NOW, 14).unwrap();
    let c = client(&w);
    c.update(&fetcher(cat.clone()), NOW).unwrap();
    // A new catalog key, v2, signed by only one old root key: refused.
    let cat2 = key(6);
    let text = root_text(&[&w.roots[0], &w.roots[1], &w.roots[2]], &cat2, &w.plat, 2);
    fs::write(cat.join(ROOT), &text).unwrap();
    fs::write(cat.join(ROOT_SIG), sign_root(&text, &w.roots[0].sk, &w.roots[0].pk).unwrap()).unwrap();
    log.sign_head(&cat2.sk, NOW, 14).unwrap();
    assert!(c.update(&fetcher(cat.clone()), NOW).unwrap_err().contains("current root keys"));
    // Two of them: accepted, and pinned.
    let sigs = sign_root(&text, &w.roots[0].sk, &w.roots[0].pk).unwrap() + &sign_root(&text, &w.roots[2].sk, &w.roots[2].pk).unwrap();
    fs::write(cat.join(ROOT_SIG), sigs).unwrap();
    c.update(&fetcher(cat.clone()), NOW).unwrap();
    assert_eq!(c.root().unwrap().version, 2);
    // Rolling back to v1 isn't possible: the pinned root is v2 now.
    assert!(Root::parse(&root_text(&[&w.roots[0], &w.roots[1], &w.roots[2]], &w.cat, &w.plat, 1)).is_ok());
    assert!(c.root().unwrap().accept_next(&fs::read_to_string(cat.join(ROOT)).unwrap(), "", NOW).is_err());
}

#[test]
fn versions() {
    use std::cmp::Ordering::*;
    assert_eq!(version_cmp("1.10", "1.9"), Greater);
    assert_eq!(version_cmp("1.0", "1"), Equal);
    assert_eq!(version_cmp("2.0.1", "2.0.0"), Greater);
}
