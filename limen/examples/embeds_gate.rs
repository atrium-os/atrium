//! The parent of scripts/vm-embeds-check.sh: asks limend for roles and
//! prints `embeds-gate <role> <result>` (to portcullisd's log).
use limen::client::{Error, Parent};

fn main() {
    let have = std::path::Path::new(limen::client::DEFAULT_SOCKET).exists();
    println!("embeds-gate socket {}", if have { "yes" } else { "no" });
    println!("embeds-gate arca {}", if std::path::Path::new("/atrium/sockets/arca").exists() { "yes" } else { "no" });
    if !have { return }
    for role in std::env::args().skip(1).collect::<Vec<_>>().iter().map(String::as_str)
        .chain(["doc-viewer", "share-target", "map", "picker"].into_iter()) {
        let r = match Parent::at(limen::client::DEFAULT_SOCKET).and_then(|mut p| p.request(role, [7u8; 16])) {
            Ok(e) => format!("embed {e}"),
            Err(Error::Refused(n)) => format!("refused {n}"),
            Err(e) => format!("error {e:?}"),
        };
        println!("embeds-gate {role} {r}");
    }
    println!("embeds-gate done 1");
}
