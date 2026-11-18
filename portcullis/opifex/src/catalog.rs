//! `opifex catalog …`: publish into a catalog folder and sync a machine's
//! copy from one (opifex.md §6, §10; build step C1). Fetching over the
//! network through Nuntius is C4; here the source is a folder.
//!
//! ```text
//! opifex catalog root-sign <cat> --key <root.pem> --pub <root.pub.pem>
//! opifex catalog add <cat> <pkg-dir> --arch <a> --archive <file> --url <https://…> [--url …]
//! opifex catalog yank <cat> <id> <version> --reason <text> --key <pem> --pub <pem>
//! opifex catalog head <cat> --key <catalog.pem> [--days 14]
//! opifex catalog sync <state> --from <cat> [--root <root.toml>]
//! opifex catalog list <state>
//! ```
//!
//! Running a catalog of your own (opifex.md §6.5; C7):
//!
//! ```text
//! opifex catalog init <cat> --name <name> --keys <key-dir> [--roots 3] [--threshold 2] [--years 5]
//! opifex catalog register <cat> --name <publisher> --domain <d> [--domain …] --key <pub.pem> --sign <key.pem>
//! opifex catalog publisher <cat> rotate|scope|recover --id <pub:…> --key <pub.pem> --sign <pem> [--sign …]
//!                                [--name <n>] [--domain <d> …]
//! opifex catalog publishers <cat>
//! opifex catalog publish <cat> --key <catalog.pem> [--days 14]   (the same as head)
//! ```

use std::path::Path;

use opifex_catalog as oc;
use sha2::{Digest, Sha256};

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn read(p: &str) -> Result<String, String> { std::fs::read_to_string(p).map_err(|e| format!("{p}: {e}")) }

pub fn cmd(args: &[String]) -> Result<(), String> {
    let mut pos: Vec<String> = Vec::new();
    let mut flags: std::collections::HashMap<String, Vec<String>> = Default::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if let Some(f) = a.strip_prefix("--") {
            let v = it.next().ok_or_else(|| format!("--{f} needs a value"))?;
            flags.entry(f.to_string()).or_default().push(v.clone());
        } else {
            pos.push(a.clone());
        }
    }
    let one = |f: &str| flags.get(f).and_then(|v| v.last()).cloned().ok_or_else(|| format!("--{f} is needed"));
    let p: Vec<&str> = pos.iter().map(String::as_str).collect();
    match p.as_slice() {
        ["root-sign", cat] => {
            let text = read(&format!("{cat}/{}", oc::ROOT))?;
            oc::Root::parse(&text)?;
            let line = oc::sign_root(&text, &read(&one("key")?)?, &read(&one("pub")?)?)?;
            let sigp = Path::new(cat).join(oc::ROOT_SIG);
            let mut s = std::fs::read_to_string(&sigp).unwrap_or_default();
            s.push_str(&line);
            std::fs::write(&sigp, s).map_err(|e| e.to_string())?;
            println!("opifex: {} signed by {}", oc::ROOT, line.split(' ').next().unwrap_or(""));
            Ok(())
        }
        ["add", cat, pkg] => {
            let archive = one("archive")?;
            let bytes = std::fs::read(&archive).map_err(|e| format!("{archive}: {e}"))?;
            let sha = oc::hex(&Sha256::digest(&bytes));
            let log = oc::Log::for_dir(Path::new(cat))?;
            let root = log.root()?;
            // The signer: whichever publishing key in root.toml, or publisher's
            // current key in the log, signed the manifest.
            let manifest = read(&format!("{pkg}/atrium.toml"))?;
            let sig = read(&format!("{pkg}/atrium.toml.sig"))?;
            let der = portcullis_sig::sig_from_base64(sig.trim()).map_err(|e| format!("atrium.toml.sig: {e:?}"))?;
            let signs = |pem: &str| portcullis_sig::verify_one(manifest.as_bytes(), &der, pem).is_ok();
            let publishers = log.replay()?.publishers;
            let (pem, publisher) = match root.with_role("platform").find(|k| signs(&k.pem)) {
                Some(k) => (k.pem.clone(), String::new()),
                None => publishers.iter().find(|p| signs(&p.key)).map(|p| (p.key.clone(), p.id.clone()))
                    .ok_or("atrium.toml is not signed by a publishing key in root.toml or a publisher's current key")?,
            };
            let mut e = oc::release_entry(Path::new(pkg), &one("arch")?, &sha, bytes.len() as u64,
                                          flags.get("url").cloned().unwrap_or_default(), &pem, now())?;
            e.release.as_mut().expect("a release").publisher = publisher;
            let n = log.append(&[e])?;
            println!("opifex: added entry {} (archive sha256 {sha}); sign a new head to publish it", n - 1);
            Ok(())
        }
        ["yank", cat, id, version] => {
            let e = oc::yank_entry(id, version, &one("reason")?, &read(&one("key")?)?, &read(&one("pub")?)?, now())?;
            oc::Log::for_dir(Path::new(cat))?.append(&[e])?;
            println!("opifex: yanked {id} {version}; sign a new head to publish it");
            Ok(())
        }
        ["head", cat] | ["publish", cat] => {
            let days = flags.get("days").and_then(|v| v.last()).map(|d| d.parse::<u64>()).transpose()
                .map_err(|_| "--days: a number")?.unwrap_or(14);
            let h = oc::Log::open(Path::new(cat)).sign_head(&read(&one("key")?)?, now(), days)?;
            println!("opifex: head {} entries, root {}, expires in {days} days", h.size, oc::hex(&h.root));
            Ok(())
        }
        ["sync", state] => {
            let from = one("from")?;
            let c = match flags.get("root").and_then(|v| v.last()) {
                Some(r) => oc::Client::init(Path::new(state), &read(r)?)?,
                None => oc::Client::open(Path::new(state)),
            };
            // A catalog with no platform key is checked as a third party's.
            let c = if c.root()?.with_role("platform").next().is_none() { oc::Client::third_party(&c.dir) } else { c };
            let src = std::path::PathBuf::from(&from);
            let fetch = move |p: &str| std::fs::read(src.join(p)).map_err(|e| format!("{p}: {e}"));
            let l = c.update(&fetch, now())?;
            println!("opifex: catalog {:?} checked: {} entries, {} releases", l.name, l.size, l.releases.len());
            Ok(())
        }
        ["list", state] => {
            let l = oc::Client::open(Path::new(state)).local()?;
            for (id, rs) in oc::by_id(&l) {
                let mut v: Vec<String> = rs.iter().map(|r| format!("{} ({})", r.version, r.arch)).collect();
                v.dedup();
                println!("  {id:<32} {:<6} {}", rs[0].kind, v.join(", "));
            }
            Ok(())
        }
        ["init", cat] => init(Path::new(cat), &one("name")?, Path::new(&one("keys")?),
                              num(&flags, "roots", 3)?, num(&flags, "threshold", 2)?, num(&flags, "years", 5)?),
        ["register", cat] => {
            let key = read(&one("key")?)?;
            let id = oc::publisher::publisher_id(&key)?;
            let p = publisher_entry(&flags, &id, "register", &key, None)?;
            let log = oc::Log::for_dir(Path::new(cat))?;
            log.append(&[p.clone().entry(now())])?;
            println!("opifex: registered {} as {id}; sign a new head to publish it.", p.name);
            proofs(&p.id, &p.domains);
            Ok(())
        }
        ["publisher", cat, action] if ["rotate", "scope", "recover"].contains(action) => {
            let log = oc::Log::for_dir(Path::new(cat))?;
            let id = one("id")?;
            let cur = log.replay()?.publishers.into_iter().find(|p| p.id == id).ok_or_else(|| format!("{id}: not registered"))?;
            let key = match flags.get("key").and_then(|v| v.last()) { Some(k) => read(k)?, None => cur.key.clone() };
            let p = publisher_entry(&flags, &id, action, &key, Some(&cur))?;
            log.append(&[p.clone().entry(now())])?;
            println!("opifex: {action} of {id} added; sign a new head to publish it.");
            if *action == "scope" {
                proofs(&p.id, &p.domains);
            }
            Ok(())
        }
        ["publishers", cat] => {
            for p in oc::Log::for_dir(Path::new(cat))?.replay()?.publishers {
                println!("{}  {}  domains {}  kinds {}  key {}{}", p.id, p.name, p.domains.join(","), p.kinds.join(","),
                         &p.key_id[..16], if p.recoveries > 0 { format!("  ({} recoveries)", p.recoveries) } else { String::new() });
            }
            Ok(())
        }
        _ => Err("catalog: root-sign | add | yank | head | publish | sync | list | init | register | publisher | publishers (see opifex help)".into()),
    }
}

type Flags = std::collections::HashMap<String, Vec<String>>;

fn num(flags: &Flags, f: &str, default: u64) -> Result<u64, String> {
    flags.get(f).and_then(|v| v.last()).map(|v| v.parse::<u64>().map_err(|_| format!("--{f}: a number"))).transpose()
        .map(|v| v.unwrap_or(default))
}

/// A publisher record from the flags, signed by every `--sign` key.
fn publisher_entry(flags: &Flags, id: &str, action: &str, key: &str, cur: Option<&oc::publisher::Publisher>)
                   -> Result<oc::publisher::PublisherEntry, String> {
    let name = flags.get("name").and_then(|v| v.last()).cloned().or(cur.map(|c| c.name.clone())).ok_or("--name is needed")?;
    let domains = flags.get("domain").cloned().or(cur.map(|c| c.domains.clone())).ok_or("--domain is needed")?;
    let kinds = flags.get("kind").cloned().or(cur.map(|c| c.kinds.clone())).unwrap_or_else(|| vec!["app".into()]);
    let mut p = oc::publisher::PublisherEntry {
        id: id.into(), action: action.into(), name, domains, kinds, devices: cur.is_some_and(|c| c.devices),
        key: key.into(), sigs: Vec::new() };
    let signers = flags.get("sign").ok_or("--sign <key.pem> is needed (the keys that sign this record)")?;
    for s in signers {
        let sk = read(s)?;
        let pk = portcullis_sig::public_pem(&sk).map_err(|e| format!("{s}: {e:?}"))?;
        p.sign_with(&sk, &pk)?;
    }
    Ok(p)
}

/// What the publisher puts on each domain, for machines to find.
fn proofs(id: &str, domains: &[String]) {
    println!("Each domain proves it once, for every machine to check (either is enough):");
    for d in domains {
        println!("  DNS:   {} TXT \"{}\"", oc::publisher::proof_txt_name(d), oc::publisher::proof_text(id).trim());
        println!("  HTTPS: {} containing: {}", oc::publisher::proof_url(d), oc::publisher::proof_text(id).trim());
    }
}

fn random_seed() -> Result<[u8; 32], String> {
    use std::io::Read;
    let mut b = [0u8; 32];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).map_err(|e| format!("/dev/urandom: {e}"))?;
    Ok(b)
}

fn write_key(dir: &Path, name: &str) -> Result<(String, String), String> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::io::Write;
    let (sk, pk) = portcullis_sig::new_key(random_seed()?).map_err(|e| format!("{e:?}"))?;
    let p = dir.join(format!("{name}.pem"));
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&p)
        .map_err(|e| format!("{}: {e}", p.display()))?;
    f.write_all(sk.as_bytes()).map_err(|e| e.to_string())?;
    std::fs::write(dir.join(format!("{name}.pub.pem")), &pk).map_err(|e| e.to_string())?;
    Ok((sk, pk))
}

/// A new catalog: root keys and a catalog key in `keys`, a root.toml signed
/// by `threshold` of the root keys in `cat`.
fn init(cat: &Path, name: &str, keys: &Path, roots: u64, threshold: u64, years: u64) -> Result<(), String> {
    if cat.join(oc::ROOT).exists() {
        return Err(format!("{}: has a root.toml already", cat.display()));
    }
    if name.eq_ignore_ascii_case("atrium") || name.trim().is_empty() {
        return Err("--name: your catalog's own name".into());
    }
    std::fs::create_dir_all(keys).map_err(|e| format!("{}: {e}", keys.display()))?;
    std::fs::create_dir_all(cat).map_err(|e| format!("{}: {e}", cat.display()))?;
    let rk: Vec<(String, String)> = (1..=roots).map(|i| write_key(keys, &format!("root{i}"))).collect::<Result<_, _>>()?;
    let (_, cpk) = write_key(keys, "catalog")?;
    let mut text = format!("schema = \"{}\"\nname = {:?}\nversion = 1\nexpires = {}\nthreshold = {threshold}\n",
                           oc::ROOT_SCHEMA, name, now() + years * 365 * 86400);
    for (_, pk) in &rk {
        text += &format!("\n[[key]]\npem = \"\"\"{pk}\"\"\"\nroles = [\"root\"]\n");
    }
    text += &format!("\n[[key]]\npem = \"\"\"{cpk}\"\"\"\nroles = [\"catalog\"]\n");
    oc::Root::parse(&text)?;
    let mut sigs = String::new();
    for (sk, pk) in rk.iter().take(threshold as usize) {
        sigs += &oc::sign_root(&text, sk, pk)?;
    }
    std::fs::write(cat.join(oc::ROOT), &text).map_err(|e| e.to_string())?;
    std::fs::write(cat.join(oc::ROOT_SIG), sigs).map_err(|e| e.to_string())?;
    oc::Log::third_party(cat).sign_head(&std::fs::read_to_string(keys.join("catalog.pem")).map_err(|e| e.to_string())?, now(), 14)?;
    println!("opifex: catalog {name:?} in {}; keys in {}.", cat.display(), keys.display());
    println!("  Move root1..root{roots}.pem somewhere offline now: any {threshold} of them replace keys and recover publishers.");
    println!("  catalog.pem signs heads (CI's CATALOG_KEY secret). Next: opifex catalog register.");
    Ok(())
}
