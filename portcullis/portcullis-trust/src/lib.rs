//! The manifest TRUST gate — shared by **every** user-app launch vector so the
//! check is uniform, not copied per path.
//!
//! ★★ IT IS ITS OWN CRATE BECAUSE THAT CLAIM WAS NOT TRUE. While this lived
//! inside the daemon binary, `atrium-launch` carried a second copy that already
//! disagreed with it — that one refused when no publishers were installed,
//! this one warned and allowed — and the CLI's local fallback path had no gate
//! at all. A module that says it is shared has to be reachable by the things
//! that must share it. A manifest's capabilities are honoured
//! only if a trusted publisher signed it (keyed Sigstore, `portcullis-sig`): an
//! unsigned / tampered / untrusted manifest is refused before any cap is granted
//! or any jail created. Applies to *any* app under `/var/lib/atrium/apps`,
//! whichever way it is launched (`Request::Launch`, the session/stdio path, …).
//!
//! System services (`/etc/atrium/services.d`) are a *different* trust root — the
//! operator installed them into the base — and are intentionally out of scope for
//! third-party publisher signatures.
//!
//! ★★ THE UNCONFIGURED CASE IS THE WHOLE DESIGN PROBLEM.
//!
//! With no publisher keys installed, this gate used to warn and allow. That is
//! defensible for a fresh developer machine — enforcement turns on the moment
//! the first key is installed — and indefensible for anything that launches
//! jails continuously, where an unsigned-manifest window stops being a one-off
//! and becomes a standing condition.
//!
//! Two mechanisms, because those are two different questions:
//!
//!   - `require_signatures` is the OPERATOR's answer, read from
//!     `/etc/atrium/trust.toml`, default off so no existing machine changes
//!     behaviour by being upgraded.
//!   - `Demand::Required` is the CALLER's answer, for a lane that must not run
//!     unsigned whatever the machine is set to — the Navigator's jailed document
//!     workers being the case this was built for. A caller can demand more than
//!     the operator configured. It can never demand less.
//!
//! ★ AND A MALFORMED CONFIG FAILS CLOSED. An absent file is a decision (the
//! documented default); an unparsable one is an accident, and reading an
//! accident as permission is the same fail-open bug one level up.

use std::path::{Path, PathBuf};

/// The trusted-publisher set lives here (`*.pem`). Which publishers Portcullis
/// will honour is Atrium policy.
pub const PUBLISHERS_DIR: &str = "/etc/atrium/publishers";

/// Operator trust settings.
pub const TRUST_CONFIG: &str = "/etc/atrium/trust.toml";

/// How badly the caller needs a signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Demand {
    /// Whatever the machine is configured for.
    PolicyDefault,
    /// ★ This launch must not proceed unsigned, whatever the machine says. Used
    /// by lanes that run jails continuously rather than at a person's request.
    Required,
}

#[derive(Debug, Clone)]
pub struct Trust {
    publishers_dir: PathBuf,
    require_signatures: bool,
}

impl Default for Trust {
    fn default() -> Self { Trust::load() }
}

impl Trust {
    /// Explicit settings — the constructor tests use, and the one to reach for
    /// when a caller already knows its policy.
    pub fn new(publishers_dir: impl Into<PathBuf>, require_signatures: bool) -> Self {
        Trust { publishers_dir: publishers_dir.into(), require_signatures }
    }

    /// Read the operator's settings from `TRUST_CONFIG`.
    pub fn load() -> Self { Trust::load_from(Path::new(TRUST_CONFIG), PUBLISHERS_DIR) }

    pub fn load_from(config: &Path, publishers_dir: impl Into<PathBuf>) -> Self {
        let publishers_dir = publishers_dir.into();
        let require_signatures = match std::fs::read_to_string(config) {
            // Absent is the documented default: a machine with no trust
            // configuration behaves as it always has.
            Err(_) => false,
            Ok(text) => match text.parse::<toml::Value>() {
                Ok(v) => v.get("require_signatures").and_then(|b| b.as_bool()).unwrap_or(false),
                // ★ FAIL CLOSED. A security setting whose file cannot be parsed
                // is an operator error, and guessing "permissive" would turn a
                // typo into an open door — silently, since the machine would
                // keep launching exactly as before.
                Err(e) => {
                    eprintln!("portcullis: {} is malformed ({e}); \
                               REQUIRING signatures until it is fixed", config.display());
                    true
                }
            },
        };
        Trust { publishers_dir, require_signatures }
    }

    pub fn require_signatures(&self) -> bool { self.require_signatures }

    /// Verify the app tree's manifest signature.
    ///
    /// `Ok(())` = a trusted publisher signed it, or trust is unconfigured and
    /// neither the operator nor the caller demanded otherwise.
    pub fn verify(&self, tree: &Path, manifest_text: &str, demand: Demand)
        -> Result<(), String>
    {
        let publishers = load_trusted_publishers(
            self.publishers_dir.to_str().unwrap_or(PUBLISHERS_DIR));
        if publishers.is_empty() {
            let demanded = self.require_signatures || demand == Demand::Required;
            if demanded {
                return Err(format!(
                    "manifest trust is required but not configured: no publisher keys in {}",
                    self.publishers_dir.display()));
            }
            eprintln!(
                "portcullisd: WARNING manifest trust not configured ({} empty); allowing UNSIGNED {}",
                self.publishers_dir.display(), tree.display());
            return Ok(());
        }
        let sig = manifest_signature(&tree.join("atrium.toml.sig"));
        portcullis_sig::verify_trusted(manifest_text.as_bytes(), &sig, &publishers)
            .map(|()| eprintln!("portcullis: {} manifest signature verified (trusted publisher)",
                                tree.display()))
            .map_err(|e| format!("manifest not signed by a trusted publisher ({e:?})"))
    }
}

/// Load every trusted-publisher public key (`*.pem`) from `dir`. Empty (missing
/// dir or no keys) = trust not yet configured.
pub fn load_trusted_publishers(dir: &str) -> Vec<String> {
    let mut keys = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("pem") {
                if let Ok(pem) = std::fs::read_to_string(&p) {
                    keys.push(pem);
                }
            }
        }
    }
    keys
}

/// Read the manifest signature, accepting a DER signature (openssl/cosign) or
/// base64 text (cosign's on-disk form), auto-detected.
pub fn manifest_signature(sig_path: &Path) -> Vec<u8> {
    let raw = std::fs::read(sig_path).unwrap_or_default();
    if let Ok(s) = std::str::from_utf8(&raw) {
        if let Ok(der) = portcullis_sig::sig_from_base64(s) {
            return der;
        }
    }
    raw
}

/// The machine's configured gate, at the operator's policy level.
pub fn verify(tree: &Path, manifest_text: &str) -> Result<(), String> {
    Trust::load().verify(tree, manifest_text, Demand::PolicyDefault)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir { tempfile::tempdir().expect("tempdir") }

    /// ★ The default must not change behaviour on an existing machine: no
    /// config file, no publishers, launch proceeds with the warning.
    #[test]
    fn unconfigured_still_allows_by_default() {
        let d = tmp();
        let t = Trust::load_from(&d.path().join("absent.toml"), d.path().join("no-keys"));
        assert!(!t.require_signatures());
        t.verify(d.path(), "[app]\n", Demand::PolicyDefault)
            .expect("an unconfigured machine keeps working");
    }

    /// ★ The operator turns it on and the same launch is refused, naming the
    /// directory an operator has to populate.
    #[test]
    fn require_signatures_refuses_when_trust_is_unconfigured() {
        let d = tmp();
        let cfg = d.path().join("trust.toml");
        std::fs::write(&cfg, "require_signatures = true\n").unwrap();
        let keys = d.path().join("no-keys");
        let t = Trust::load_from(&cfg, &keys);
        assert!(t.require_signatures());
        let why = t.verify(d.path(), "[app]\n", Demand::PolicyDefault)
            .expect_err("must refuse");
        assert!(why.contains(keys.to_str().unwrap()), "{why}");
    }

    /// ★★ A CALLER CAN DEMAND MORE THAN THE MACHINE CONFIGURES. This is the
    /// jailed-worker lane: it runs jails continuously, so it must not run
    /// unsigned even on a dev box that still allows it for ordinary apps.
    #[test]
    fn a_caller_can_require_signatures_the_operator_did_not() {
        let d = tmp();
        let t = Trust::new(d.path().join("no-keys"), false);
        t.verify(d.path(), "[app]\n", Demand::PolicyDefault).expect("ordinary launches proceed");
        t.verify(d.path(), "[app]\n", Demand::Required)
            .expect_err("a demanding caller must be refused");
    }

    /// ★ AND IT CAN NEVER DEMAND LESS. With the operator requiring signatures,
    /// `PolicyDefault` does not relax anything — there is no call that opts out.
    #[test]
    fn no_caller_can_opt_out_of_the_operators_requirement() {
        let d = tmp();
        let t = Trust::new(d.path().join("no-keys"), true);
        for demand in [Demand::PolicyDefault, Demand::Required] {
            t.verify(d.path(), "[app]\n", demand)
                .unwrap_err();
        }
    }

    /// ★★★ A MALFORMED CONFIG FAILS CLOSED. An absent file is a decision; an
    /// unparsable one is an accident, and reading an accident as permission
    /// would leave the machine launching exactly as before with nothing to
    /// show that its security setting never took effect.
    #[test]
    fn a_malformed_config_requires_signatures() {
        let d = tmp();
        let cfg = d.path().join("trust.toml");
        std::fs::write(&cfg, "require_signatures = = = yes\n").unwrap();
        let t = Trust::load_from(&cfg, d.path().join("no-keys"));
        assert!(t.require_signatures(), "a broken config read as permission");
        t.verify(d.path(), "[app]\n", Demand::PolicyDefault).unwrap_err();
    }

    /// A config that parses but says nothing keeps the documented default —
    /// only a malformed file is treated as an accident.
    #[test]
    fn an_empty_config_keeps_the_default() {
        let d = tmp();
        let cfg = d.path().join("trust.toml");
        std::fs::write(&cfg, "# nothing here\n").unwrap();
        assert!(!Trust::load_from(&cfg, d.path().join("no-keys")).require_signatures());
    }

    #[test]
    fn require_signatures_false_is_honoured_when_written_explicitly() {
        let d = tmp();
        let cfg = d.path().join("trust.toml");
        std::fs::write(&cfg, "require_signatures = false\n").unwrap();
        assert!(!Trust::load_from(&cfg, d.path().join("no-keys")).require_signatures());
    }

    /// ★ Once publishers ARE installed, an unsigned manifest is refused whatever
    /// the setting says — the setting governs the unconfigured case only, and
    /// must not become a way to weaken a configured machine.
    #[test]
    fn installed_publishers_refuse_an_unsigned_manifest_regardless_of_the_setting() {
        let d = tmp();
        let keys = d.path().join("keys");
        std::fs::create_dir_all(&keys).unwrap();
        // Not a usable key, but enough to make the set non-empty: the point is
        // that the unsigned path is no longer reachable.
        std::fs::write(keys.join("p.pem"), "-----BEGIN PUBLIC KEY-----\nx\n-----END PUBLIC KEY-----\n").unwrap();
        for require in [false, true] {
            let t = Trust::new(&keys, require);
            t.verify(d.path(), "[app]\n", Demand::PolicyDefault)
                .expect_err("an unsigned manifest must be refused once trust is configured");
        }
    }
}
