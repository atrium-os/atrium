//! What a system worker's bundle may be (app-composition.md C6,
//! dev-tools.md §3.5): one of portcullis_ipc::SYSTEM_WORKERS runs for any
//! app that names it in `decoders`, so its own manifest asks for nothing.

use portcullis_toml::Manifest;

/// How many of one system worker one app (of one person) may hold at once,
/// under the worker's own system-wide cap. One `atrium_assets::Decoder`
/// serves many pictures, so an honest app needs one or two.
pub const PER_CALLER: usize = 4;

/// The longest a system worker's jail lives, whatever its caller does: a
/// caller that keeps the connection open and idle does not keep a jail.
pub const LIFE: std::time::Duration = std::time::Duration::from_secs(300);

/// Refuse a system worker bundle that declares any capability (even one set
/// to false, or a key this schema does not know), `[[workers]]`, `[roles]`
/// or `[ipc]`. Its jail is a one-shot with none of these, and a bundle that
/// asks for them is not the one this path was meant to run.
pub fn check_bundle(m: &Manifest) -> Result<(), String> {
    if !m.capabilities.declares_nothing() {
        return Err(format!("{} declares capabilities: a system worker holds none", m.app.id));
    }
    if !m.workers.is_empty() || m.roles.is_some() || m.ipc.is_some() {
        return Err(format!("{} declares workers, roles or ipc: a system worker has none", m.app.id));
    }
    Ok(())
}

/// The limiter key for one caller's system workers of one bundle.
pub fn caller_key(bundle: &str, person: &str, app: &str) -> String {
    format!("system-worker:{bundle}:{person}:{app}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "[app]\nid = \"org.atrium.image-decode\"\nname = \"Image decoder\"\nversion = \"1\"\nentry = \"bin/d\"\n\n[resources]\nmemory = \"256M\"\ncpu = 100\n";

    fn manifest(extra: &str) -> Manifest {
        Manifest::from_str(&format!("{BASE}{extra}")).expect("manifest parses")
    }

    #[test]
    fn the_installed_decoder_passes() {
        assert_eq!(check_bundle(&manifest("")), Ok(()));
    }

    #[test]
    fn a_bundle_with_any_capability_is_refused() {
        for caps in ["clipboard = false", "network = \"full\"", "filesystem = [\"~/Documents\"]", "decoders = [\"image\"]"] {
            let m = manifest(&format!("\n[capabilities]\n{caps}\n"));
            let e = check_bundle(&m).expect_err(caps);
            assert!(e.contains("declares capabilities"), "{caps}: {e}");
        }
    }

    #[test]
    fn a_bundle_with_workers_is_refused() {
        let m = manifest("\n[[workers]]\nname = \"w\"\nentry = \"bin/w\"\n");
        assert!(check_bundle(&m).is_err());
    }

    #[test]
    fn callers_are_keyed_apart() {
        assert_ne!(caller_key("b", "ann", "x"), caller_key("b", "ann", "y"));
        assert_ne!(caller_key("b", "ann", "x"), caller_key("b", "bob", "x"));
    }
}
