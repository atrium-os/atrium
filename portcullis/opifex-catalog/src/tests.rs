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
    Entry { kind: "release".into(), time: NOW, yank: None, locations: None, publisher: None, release: Some(Release {
        id: id.into(), version: version.into(), arch: "aarch64".into(), kind: "app".into(),
        signer: key_id(&signer.pk).unwrap(), manifest, sig, archive_sha256: "cd".repeat(32), archive_size: 10,
        urls: vec![format!("https://example.org/{id}-{version}.insula")], publisher: String::new() }) }
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

// ── C7: third-party catalogs and publisher records ──────────────────────────

/// A third-party catalog: root keys and a catalog key, no platform key.
fn third_world(tag: &str) -> World {
    let w = world(tag);
    let text = fs::read_to_string(w.dir.join("cat").join(ROOT)).unwrap();
    // Drop the platform key (the last [[key]] block) and rename.
    let last = text.rfind("\n[[key]]").unwrap();
    let text = text[..last].replace("name = \"Atrium\"", "name = \"Example\"") + "\n";
    let sigs = sign_root(&text, &w.roots[0].sk, &w.roots[0].pk).unwrap() + &sign_root(&text, &w.roots[1].sk, &w.roots[1].pk).unwrap();
    fs::write(w.dir.join("cat").join(ROOT), &text).unwrap();
    fs::write(w.dir.join("cat").join(ROOT_SIG), sigs).unwrap();
    w
}

fn pub_entry(action: &str, id: &str, domains: &[&str], kinds: &[&str], key: &Keys, signers: &[&Keys]) -> Entry {
    let mut p = publisher::PublisherEntry {
        id: id.into(), action: action.into(), name: "Example Ltd".into(),
        domains: domains.iter().map(|d| d.to_string()).collect(), kinds: kinds.iter().map(|d| d.to_string()).collect(),
        devices: false, key: key.pk.clone(), sigs: Vec::new() };
    for s in signers {
        p.sign_with(&s.sk, &s.pk).unwrap();
    }
    p.entry(NOW)
}

fn pub_release(w: &World, id: &str, version: &str, signer: &Keys, publisher: &str) -> Entry {
    let mut e = release(w, id, version, signer);
    e.release.as_mut().unwrap().publisher = publisher.into();
    e
}

#[test]
fn third_party_publisher_lifecycle() {
    let w = third_world("pubs");
    let cat = w.dir.join("cat");
    let log = Log::third_party(&cat);
    let (k1, k2, k3) = (key(20), key(21), key(22));
    let id = publisher::publisher_id(&k1.pk).unwrap();
    assert!(id.starts_with("pub:") && id.len() == 20);

    // Register, publish under the proven prefix.
    log.append(&[pub_entry("register", &id, &["example.com"], &["app"], &k1, &[&k1])]).unwrap();
    log.append(&[pub_release(&w, "com.example.weather", "1.0", &k1, &id)]).unwrap();
    log.sign_head(&w.cat.sk, NOW, 14).unwrap();
    let text = fs::read_to_string(cat.join(ROOT)).unwrap();
    let c = Client::init(&w.dir.join("state"), &text).unwrap();
    let c = Client::third_party(&c.dir);
    let l = c.update(&fetcher(cat.clone()), NOW).unwrap();
    assert_eq!(l.publishers.len(), 1);
    assert_eq!(l.publishers[0].prefixes, vec!["com.example.".to_string()]);
    assert_eq!(l.latest("com.example.weather", "aarch64").unwrap().publisher, id);

    // Outside its prefix, unknown publisher, not its key, no publisher at all.
    assert!(log.append(&[pub_release(&w, "com.other.x", "1", &k1, &id)]).unwrap_err().contains("prefixes"));
    assert!(log.append(&[pub_release(&w, "com.example.x", "1", &k1, "pub:0000000000000000")]).unwrap_err().contains("not registered"));
    assert!(log.append(&[pub_release(&w, "com.example.x", "1", &k2, &id)]).unwrap_err().contains("current key"));
    assert!(log.append(&[release(&w, "com.example.x", "1", &k1)]).unwrap_err().contains("publisher records"));

    // Rotation needs the old and the new key; afterwards the old key is refused.
    assert!(log.append(&[pub_entry("rotate", &id, &["example.com"], &["app"], &k2, &[&k2])]).unwrap_err().contains("old and the new"));
    log.append(&[pub_entry("rotate", &id, &["example.com"], &["app"], &k2, &[&k1, &k2])]).unwrap();
    assert!(log.append(&[pub_release(&w, "com.example.weather", "1.1", &k1, &id)]).unwrap_err().contains("current key"));
    log.append(&[pub_release(&w, "com.example.weather", "1.1", &k2, &id)]).unwrap();
    // A past key can't come back.
    assert!(log.append(&[pub_entry("rotate", &id, &["example.com"], &["app"], &k1, &[&k2, &k1])]).unwrap_err().contains("past key"));

    // Recovery: two root keys and the new key; one root key is not enough.
    assert!(log.append(&[pub_entry("recover", &id, &["example.com"], &["app"], &k3, &[&w.roots[0], &k3])]).unwrap_err().contains("root keys"));
    log.append(&[pub_entry("recover", &id, &["example.com"], &["app"], &k3, &[&w.roots[0], &w.roots[2], &k3])]).unwrap();

    // Scope change by the current key adds a domain.
    log.append(&[pub_entry("scope", &id, &["example.com", "example.net"], &["app"], &k3, &[&k3])]).unwrap();
    log.append(&[pub_release(&w, "net.example.notes", "1", &k3, &id)]).unwrap();

    // The current key yanks a release a past key signed.
    let mut y = yank_entry("com.example.weather", "1.0", "bad", &k3.sk, &k3.pk, NOW).unwrap();
    log.append(&[y.clone()]).unwrap();
    y.yank.as_mut().unwrap().signer = key_id(&k1.pk).unwrap();
    assert!(log.append(&[y]).is_err());

    log.sign_head(&w.cat.sk, NOW + 5, 14).unwrap();
    let l = c.update(&fetcher(cat.clone()), NOW + 5).unwrap();
    let p = &l.publishers[0];
    assert_eq!((p.recoveries, p.past.len(), p.key_id.clone()), (1, 2, key_id(&k3.pk).unwrap()));
    assert_eq!(l.latest("com.example.weather", "aarch64").unwrap().version, "1.1");
    assert!(l.latest("net.example.notes", "aarch64").is_some());
}

#[test]
fn third_party_catalogs_refuse() {
    let w = third_world("pubrefuse3");
    let log = Log::third_party(&w.dir.join("cat"));
    let (k1, k2) = (key(30), key(31));
    let id = publisher::publisher_id(&k1.pk).unwrap();
    let id2 = publisher::publisher_id(&k2.pk).unwrap();
    // Id must come from the key; signed by it; apps only; never org.atrium.
    assert!(log.append(&[pub_entry("register", &id2, &["example.com"], &["app"], &k1, &[&k1])]).unwrap_err().contains("derived"));
    assert!(log.append(&[pub_entry("register", &id, &["example.com"], &["app"], &k1, &[&k2])]).unwrap_err().contains("its key"));
    assert!(log.append(&[pub_entry("register", &id, &["example.com"], &["font"], &k1, &[&k1])]).unwrap_err().contains("apps only"));
    assert!(log.append(&[pub_entry("register", &id, &["atrium.org"], &["app"], &k1, &[&k1])]).unwrap_err().contains("org.atrium"));
    assert!(log.append(&[pub_entry("register", &id, &["Example.com"], &["app"], &k1, &[&k1])]).unwrap_err().contains("lowercase"));
    log.append(&[pub_entry("register", &id, &["example.com"], &["app"], &k1, &[&k1])]).unwrap();
    // Twice, or a second publisher claiming the same domain.
    assert!(log.append(&[pub_entry("register", &id, &["example.com"], &["app"], &k1, &[&k1])]).unwrap_err().contains("already"));
    assert!(log.append(&[pub_entry("register", &id2, &["example.com"], &["app"], &k2, &[&k2])]).unwrap_err().contains("belongs to"));

    // The same records in the Atrium catalog may carry other kinds.
    let w = world("pubfirst");
    let log = Log::open(&w.dir.join("cat"));
    log.append(&[pub_entry("register", &id, &["example.com"], &["app", "font"], &k1, &[&k1])]).unwrap();
}

#[test]
fn proofs() {
    let id = "pub:0123456789abcdef";
    assert!(publisher::proof_ok(&publisher::proof_text(id), id));
    assert!(publisher::proof_ok("v=spf1 -all\n\"atrium-publisher pub:0123456789abcdef\"", id));
    assert!(!publisher::proof_ok("atrium-publisher pub:0123456789abcdee", id));
    assert_eq!(publisher::prefix_of("shop.example.co.uk"), "uk.co.example.shop.");
}

/// A platform key stolen after entry 2: root.toml version 2 names a new
/// platform key and keeps the old one retired at 2. Machines already synced
/// and fresh machines end with the same catalog: the old key's first two
/// releases count, the thief's don't (and are listed as revoked), and the
/// new key publishes.
#[test]
fn retired_platform_key() {
    let w = world("retire");
    let cat = w.dir.join("cat");
    let log = Log::open(&cat);
    log.append(&[release(&w, "org.atrium.a", "1", &w.plat), release(&w, "org.atrium.b", "1", &w.plat)]).unwrap();
    // The thief, with the stolen platform key (and a way past CI).
    log.append(&[release(&w, "org.atrium.a", "2", &w.plat),
                 yank_entry("org.atrium.b", "1", "gone", &w.plat.sk, &w.plat.pk, NOW).unwrap()]).unwrap();
    log.sign_head(&w.cat.sk, NOW, 14).unwrap();
    let old_machine = client(&w);
    let l = old_machine.update(&fetcher(cat.clone()), NOW).unwrap();
    assert_eq!(l.latest("org.atrium.a", "aarch64").unwrap().version, "2");

    // Root version 2: new platform key, the stolen one retired at entry 2.
    let new_plat = key(6);
    let mut text = root_text(&[&w.roots[0], &w.roots[1], &w.roots[2]], &w.cat, &new_plat, 2);
    text += &format!("\n[[key]]\npem = \"\"\"{}\"\"\"\nroles = [\"platform\"]\nprefixes = [\"org.atrium.\"]\nkinds = [\"app\"]\nuntil = 2\n", w.plat.pk);
    let sigs = sign_root(&text, &w.roots[0].sk, &w.roots[0].pk).unwrap() + &sign_root(&text, &w.roots[2].sk, &w.roots[2].pk).unwrap();
    fs::write(cat.join(ROOT), &text).unwrap();
    fs::write(cat.join(ROOT_SIG), sigs).unwrap();
    // The publishing side refuses the retired key, takes the new one.
    assert!(log.append(&[release(&w, "org.atrium.c", "1", &w.plat)]).unwrap_err().contains("retired"));
    log.append(&[release(&w, "org.atrium.a", "3", &new_plat)]).unwrap();
    log.sign_head(&w.cat.sk, NOW + 10, 14).unwrap();

    let check = |l: &Local| {
        assert_eq!(l.size, 5);
        assert_eq!(l.latest("org.atrium.a", "aarch64").unwrap().version, "3");
        assert!(l.releases.iter().all(|r| !(r.id == "org.atrium.a" && r.version == "2")));
        assert!(l.is_revoked("org.atrium.a", "2") && !l.is_revoked("org.atrium.a", "1"));
        // The thief's yank didn't count.
        assert_eq!(l.latest("org.atrium.b", "aarch64").unwrap().version, "1");
    };
    check(&old_machine.update(&fetcher(cat.clone()), NOW + 10).unwrap());
    let w_fresh = World { dir: w.dir.join("fresh"), roots: vec![], cat: key(4), plat: key(5) };
    fs::create_dir_all(&w_fresh.dir).unwrap();
    let fresh = Client::init(&w_fresh.dir.join("state"), &text).unwrap();
    check(&fresh.update(&fetcher(cat.clone()), NOW + 10).unwrap());
    check(&log.replay().unwrap());

    // root.toml rules: only a platform key is retired, and not all of them.
    let bad = root_text(&[&w.roots[0], &w.roots[1], &w.roots[2]], &w.cat, &new_plat, 3).replace("kinds = [\"app\", \"layer\", \"font\"]\n", "kinds = [\"app\"]\nuntil = 0\n");
    assert!(Root::parse(&bad).unwrap_err().contains("every platform key"));
    let bad = text.replacen("roles = [\"catalog\"]\n", "roles = [\"catalog\"]\nuntil = 1\n", 1);
    assert!(Root::parse(&bad).unwrap_err().contains("only a platform key"));
}
