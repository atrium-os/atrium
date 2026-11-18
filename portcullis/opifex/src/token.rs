//! `opifex token …`: keys on hardware tokens, for the catalog's root keys
//! (opifex.md §5.1, §10.3). The key stays on the token; a small key file
//! names it, and every `--key`/`--sign` takes that file like a PEM.
//!
//! ```text
//! opifex token list --module <pkcs11 lib>
//! opifex token ref --module <pkcs11 lib> --serial <s> --id <hex> --out <file> [--pub <file>]
//! opifex token pub <key file>
//! ```

use portcullis_token::KeyRef;

fn flag(args: &[String], f: &str) -> Option<String> {
    args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).cloned()
}

pub fn cmd(args: &[String]) -> Result<(), String> {
    let need = |f: &str| flag(args, f).ok_or_else(|| format!("{f} is needed"));
    match args.first().map(String::as_str) {
        Some("list") => {
            let module = need("--module")?;
            let tokens = portcullis_token::list(&module)?;
            if tokens.is_empty() {
                println!("no token plugged in");
            }
            for t in tokens {
                println!("token {:?} serial {} ({})", t.label, t.serial, t.model);
                if t.keys.is_empty() {
                    println!("  no P-256 keys");
                }
                for (id, label, pem) in t.keys {
                    println!("  id {id}  {label}  key {}", &opifex_catalog::key_id(&pem)?[..16]);
                }
            }
            Ok(())
        }
        Some("ref") => {
            let r = KeyRef { format: 1, module: need("--module")?, serial: need("--serial")?, id: need("--id")? };
            let pem = portcullis_token::public_pem(&r)?;
            let out = need("--out")?;
            std::fs::write(&out, r.text()).map_err(|e| format!("{out}: {e}"))?;
            if let Some(p) = flag(args, "--pub") {
                std::fs::write(&p, &pem).map_err(|e| format!("{p}: {e}"))?;
            }
            println!("opifex: {out} names key {} on token {} (key id {})", r.id, r.serial, &opifex_catalog::key_id(&pem)?[..16]);
            Ok(())
        }
        Some("pub") => {
            let f = args.get(1).ok_or("token pub <key file>")?;
            let t = std::fs::read_to_string(f).map_err(|e| format!("{f}: {e}"))?;
            print!("{}", portcullis_sig::public_pem(&t).map_err(|e| format!("{f}: {e:?}"))?);
            Ok(())
        }
        _ => Err("token: list --module <lib> | ref --module <lib> --serial <s> --id <hex> --out <file> [--pub <file>] | pub <key file>".into()),
    }
}
