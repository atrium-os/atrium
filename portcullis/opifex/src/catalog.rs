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
            let log = oc::Log::open(Path::new(cat));
            let root = log.root()?;
            // The signer: whichever publishing key in root.toml signed the manifest.
            let manifest = read(&format!("{pkg}/atrium.toml"))?;
            let sig = read(&format!("{pkg}/atrium.toml.sig"))?;
            let der = portcullis_sig::sig_from_base64(sig.trim()).map_err(|e| format!("atrium.toml.sig: {e:?}"))?;
            let signer = root.with_role("platform")
                .find(|k| portcullis_sig::verify_one(manifest.as_bytes(), &der, &k.pem).is_ok())
                .ok_or("atrium.toml is not signed by a publishing key in root.toml")?;
            let e = oc::release_entry(Path::new(pkg), &one("arch")?, &sha, bytes.len() as u64,
                                      flags.get("url").cloned().unwrap_or_default(), &signer.pem, now())?;
            let n = log.append(&[e])?;
            println!("opifex: added entry {} (archive sha256 {sha}); sign a new head to publish it", n - 1);
            Ok(())
        }
        ["yank", cat, id, version] => {
            let e = oc::yank_entry(id, version, &one("reason")?, &read(&one("key")?)?, &read(&one("pub")?)?, now())?;
            oc::Log::open(Path::new(cat)).append(&[e])?;
            println!("opifex: yanked {id} {version}; sign a new head to publish it");
            Ok(())
        }
        ["head", cat] => {
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
        _ => Err("catalog: root-sign | add | yank | head | sync | list (see opifex help)".into()),
    }
}
