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
        }, |name| {
            // `<root>/dns/<name>`: one TXT record per line.
            let p = local(&root, &format!("https://dns/{name}"))?;
            Ok(std::fs::read_to_string(p).map(|t| t.lines().map(str::to_string).collect()).unwrap_or_default())
        })
    } else {
        let f = match nuntius::launch::open() {
            Ok(f) => f,
            Err(e) => {
                eprintln!("opifex-fetch: no fetcher: {e}");
                std::process::exit(1)
            }
        };
        let (c, guard) = f.into_parts();
        let (c, id) = (std::cell::RefCell::new(c), std::cell::Cell::new(0u32));
        let next = || { id.set(id.get().wrapping_add(1)); id.get() };
        serve(stdin, stdout, |url, max| {
            let mut c = c.borrow_mut();
            let id = next();
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
        }, |name| {
            let mut c = c.borrow_mut();
            let id = next();
            c.lookup(nuntius::Lookup { id, name: name.into(), kind: nuntius::RecordKind::Txt }).map_err(|e| format!("nuntius: {e}"))?;
            loop {
                match c.recv_timeout(Duration::from_secs(10)) {
                    Ok(Some(nuntius::Event::Records { id: i, records, .. })) if i == id => {
                        return Ok(records.into_iter().filter_map(|r| match r { nuntius::Record::Txt(t) => Some(t), _ => None }).collect());
                    }
                    Ok(Some(nuntius::Event::Error { id: i, code })) if i == id => return Err(format!("DNS {code:?}")),
                    Ok(Some(_)) => {}
                    Ok(None) => return Err("DNS timed out".into()),
                    Err(e) => return Err(format!("nuntius: {e}")),
                }
            }
        })
    };
    if let Err(e) = r {
        eprintln!("opifex-fetch: {e}");
        std::process::exit(1);
    }
}
