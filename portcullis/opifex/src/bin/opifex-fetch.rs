//! opifex-fetch: opifexd's network, as the system bundle
//! `org.atrium.opifex.fetch` (opifex.md §7.2, build step C4). Its only
//! capability is `net-fetch`, narrowed by `net-fetch-sites` to the catalog's
//! hosts: every byte comes through its own Nuntius. opifexd starts it with
//! `portcullis launch` and asks on its stdin (`opifex::fetch`); it holds no
//! state and checks nothing that matters, because opifexd trusts nothing it
//! brings.
//!
//! `opifex-fetch --dir <root>` answers from files instead
//! (`https://host/path` is `<root>/host/path`), for tests.

use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use opifex::fetch::serve;

/// `https://host/a/b` under `root`, refusing anything that climbs out.
fn local(root: &Path, url: &str) -> Result<PathBuf, String> {
    let rest = url.strip_prefix("https://").ok_or("not https")?;
    let p = Path::new(rest);
    if p.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err("not a plain path".into());
    }
    Ok(root.join(p))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (stdin, stdout) = (std::io::stdin().lock(), std::io::stdout().lock());
    let r = if let [flag, root] = &args[..] {
        if flag != "--dir" {
            eprintln!("usage: opifex-fetch [--dir <root>]");
            std::process::exit(2);
        }
        let root = PathBuf::from(root);
        serve(stdin, stdout, |url, max| {
            let p = local(&root, url)?;
            let f = std::fs::File::open(&p).map_err(|_| "404".to_string())?;
            let mut b = Vec::new();
            f.take(max + 1).read_to_end(&mut b).map_err(|e| e.to_string())?;
            Ok(b)
        })
    } else {
        let f = match nuntius::launch::open() {
            Ok(f) => f,
            Err(e) => {
                eprintln!("opifex-fetch: no fetcher: {e}");
                std::process::exit(1)
            }
        };
        let (mut c, guard) = f.into_parts();
        let mut id = 0u32;
        serve(stdin, stdout, |url, max| {
            id += 1;
            let mut o = nuntius::Open::get(id, url);
            o.deliver = nuntius::Deliver::Fd;
            o.limits.max_bytes = max;
            c.open(o).map_err(|e| format!("nuntius: {e}"))?;
            let (mut status, mut file) = (0, None);
            loop {
                match c.recv_timeout(Duration::from_secs(300)) {
                    Ok(Some(nuntius::Event::Head { status: s, .. })) => status = s,
                    Ok(Some(nuntius::Event::Fd { fd, .. })) => file = Some(std::fs::File::from(fd)),
                    Ok(Some(nuntius::Event::Done { .. })) => break,
                    Ok(Some(nuntius::Event::Error { code, .. })) => return Err(format!("{code:?}")),
                    Ok(Some(_)) => {}
                    Ok(None) => return Err("timed out".into()),
                    Err(e) => return Err(format!("nuntius: {e}{}", guard.failure().map(|w| format!(" ({w})")).unwrap_or_default())),
                }
            }
            if status != 200 {
                return Err(format!("HTTP {status}"));
            }
            let mut b = Vec::new();
            file.ok_or("no body")?.take(max + 1).read_to_end(&mut b).map_err(|e| e.to_string())?;
            Ok(b)
        })
    };
    if let Err(e) = r {
        eprintln!("opifex-fetch: {e}");
        std::process::exit(1);
    }
}
