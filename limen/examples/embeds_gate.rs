//! The parent of scripts/vm-embeds-check.sh: asks limend for roles and
//! prints `embeds-gate <role> <result>` (to portcullisd's log).
use limen::client::{Error, Parent};

fn main() {
    let have = std::path::Path::new(limen::client::DEFAULT_SOCKET).exists();
    println!("embeds-gate socket {}", if have { "yes" } else { "no" });
    println!("embeds-gate arca {}", if std::path::Path::new("/atrium/sockets/arca").exists() { "yes" } else { "no" });
    if !have { return }
    for role in std::env::args().skip(1).collect::<Vec<_>>().iter().map(String::as_str)
        .chain(["doc-viewer", "editor-extension", "share-target", "map", "picker"].into_iter()) {
        let r = match Parent::at(limen::client::DEFAULT_SOCKET).and_then(|mut p| p.request(role, [7u8; 16])) {
            Ok(e) => format!("embed {e}"),
            Err(Error::Refused(n)) => format!("refused {n}"),
            Err(e) => format!("error {e:?}"),
        };
        println!("embeds-gate {role} {r}");
    }
    // editor-extension as a plugin (dev-tools.md §3.8): limend lists the
    // gate's Share (a dev build of the same person), starts it when named,
    // and refuses an app that isn't a plugin (18, NO_PLUGIN).
    let listed = match Parent::at(limen::client::DEFAULT_SOCKET).and_then(|mut p| p.list_plugins()) {
        Ok(v) => v.iter().map(|i| format!("{}:{}", i.app, i.languages.join("+"))).collect::<Vec<_>>().join(","),
        Err(Error::Refused(n)) => format!("refused {n}"),
        Err(e) => format!("error {e:?}"),
    };
    println!("embeds-gate plugins {listed}");
    for (what, app) in [("plugin", "dev.alice.scratch.org.example.share"), ("not-a-plugin", "dev.alice.scratch.org.example.bare")] {
        let r = match Parent::at(limen::client::DEFAULT_SOCKET).and_then(|mut p| p.request_plugin([7u8; 16], app)) {
            Ok(e) => format!("embed {e}"),
            Err(Error::Refused(n)) => format!("refused {n}"),
            Err(e) => format!("error {e:?}"),
        };
        println!("embeds-gate {what} {r}");
    }
    println!("embeds-gate done 1");
}
