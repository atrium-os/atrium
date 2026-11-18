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

/// The machine's own **dev publisher** (homes.md §5.3, W5): portcullisd's
/// key for what a person's workspace builds and Runs. It signs only app ids
/// under [`DEV_PREFIX`], and only it may: a dev bundle is verified against
/// this key alone, and this key verifies nothing else, so a third party's
/// publisher can't pose as a dev build or the reverse.
pub const DEV_PUBLISHER_DIR: &str = "/var/db/atrium/dev-publisher";
/// opifexd's state (opifex.md §7.5, §6.5): `installed.toml` says where each
/// app came from; `catalogs/<slug>/catalog.toml` holds a third-party
/// catalog's publishers as this machine replayed them from its log.
pub const OPIFEX_STATE: &str = "/var/db/atrium/opifex";

/// App ids of dev bundles: `dev.<workspace>.<the manifest's id>`.
pub const DEV_PREFIX: &str = "dev.";

/// Is this app id a dev build's?
pub fn is_dev(app_id: &str) -> bool { app_id.starts_with(DEV_PREFIX) }

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
    dev_dir: PathBuf,
    opifex_state: PathBuf,
}

impl Default for Trust {
    fn default() -> Self { Trust::load() }
}

impl Trust {
    /// Explicit settings — the constructor tests use, and the one to reach for
    /// when a caller already knows its policy.
    pub fn new(publishers_dir: impl Into<PathBuf>, require_signatures: bool) -> Self {
        Trust { publishers_dir: publishers_dir.into(), require_signatures, dev_dir: PathBuf::from(DEV_PUBLISHER_DIR),
                opifex_state: PathBuf::from(OPIFEX_STATE) }
    }

    /// Where opifexd's state is (tests).
    pub fn with_opifex_state(mut self, d: impl Into<PathBuf>) -> Self {
        self.opifex_state = d.into();
        self
    }

    /// Where the dev publisher's key is (tests).
    pub fn with_dev_dir(mut self, d: impl Into<PathBuf>) -> Self {
        self.dev_dir = d.into();
        self
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
        Trust { publishers_dir, require_signatures, dev_dir: PathBuf::from(DEV_PUBLISHER_DIR),
                opifex_state: PathBuf::from(OPIFEX_STATE) }
    }

    pub fn require_signatures(&self) -> bool { self.require_signatures }

    /// Verify the app tree's manifest signature.
    ///
    /// `Ok(())` = a trusted publisher signed it, or trust is unconfigured and
    /// neither the operator nor the caller demanded otherwise.
    pub fn verify(&self, tree: &Path, manifest_text: &str, demand: Demand)
        -> Result<(), String>
    {
        self.verify_publisher(tree, manifest_text, demand).map(|_| ())
    }

    /// As [`Trust::verify`], and name the publisher: `Some(id)` (SHA-256 of
    /// the signing key's SPKI DER) when a trusted publisher signed it, `None`
    /// when trust is unconfigured and an unsigned manifest was allowed. The
    /// launcher records the id in the launch registry; services that bind
    /// data to an app (Arca) treat `None` as "no publisher identity".
    pub fn verify_publisher(&self, tree: &Path, manifest_text: &str, demand: Demand)
        -> Result<Option<[u8; 32]>, String>
    {
        // ★ A dev bundle: the machine's dev key alone, never unsigned.
        if tree.file_name().and_then(|n| n.to_str()).is_some_and(is_dev) {
            let key = std::fs::read_to_string(self.dev_dir.join("public.pem"))
                .map_err(|_| "a dev build, and this machine has no dev publisher".to_string())?;
            let sig = manifest_signature(&tree.join("atrium.toml.sig"));
            return portcullis_sig::verify_trusted_publisher(manifest_text.as_bytes(), &sig, &[key])
                .map(Some)
                .map_err(|e| format!("a dev build not signed by this machine's dev publisher ({e:?})"));
        }
        // ★ An app from a third-party catalog: its publisher's keys in that
        // catalog alone, never unsigned (opifex.md §6.5, C7).
        if let Some(app_id) = tree.file_name().and_then(|n| n.to_str()) {
            if let Some(keys) = self.catalog_publisher_keys(app_id)? {
                let sig = manifest_signature(&tree.join("atrium.toml.sig"));
                return portcullis_sig::verify_trusted_publisher(manifest_text.as_bytes(), &sig, &keys)
                    .map(Some)
                    .map_err(|e| format!("{app_id}: not signed by its catalog publisher's key ({e:?})"));
            }
        }
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
            return Ok(None);
        }
        let sig = manifest_signature(&tree.join("atrium.toml.sig"));
        portcullis_sig::verify_trusted_publisher(manifest_text.as_bytes(), &sig, &publishers)
            .map(|id| {
                eprintln!("portcullis: {} manifest signature verified (trusted publisher)",
                          tree.display());
                Some(id)
            })
            .map_err(|e| format!("manifest not signed by a trusted publisher ({e:?})"))
    }
}

impl Trust {
    /// For an app opifexd installed from a third-party catalog
    /// (`installed.toml`: `source = "catalog:<slug>"` with
    /// `catalogs/<slug>/` present), the keys its publisher signs with: the
    /// current one and its past keys, so a tree installed before a rotation
    /// still checks. `Ok(None)`: not such an app, the ordinary gate decides.
    /// A record naming a publisher the catalog no longer has, or one whose
    /// prefixes don't cover the app, is refused rather than passed on.
    fn catalog_publisher_keys(&self, app_id: &str) -> Result<Option<Vec<String>>, String> {
        let Ok(text) = std::fs::read_to_string(self.opifex_state.join("installed.toml")) else { return Ok(None) };
        let installed: toml::Value = text.parse()
            .map_err(|e| format!("{}/installed.toml is malformed ({e}); refusing catalog apps", self.opifex_state.display()))?;
        let Some(rec) = installed.get("app").and_then(|a| a.as_array())
            .and_then(|a| a.iter().find(|r| r.get("id").and_then(|v| v.as_str()) == Some(app_id))) else { return Ok(None) };
        let field = |k: &str| rec.get(k).and_then(|v| v.as_str()).unwrap_or("");
        let Some(slug) = field("source").strip_prefix("catalog:") else { return Ok(None) };
        if slug.is_empty() || slug.contains('/') || slug.starts_with('.') {
            return Err(format!("{app_id}: installed from {:?}, not a catalog this machine follows", field("source")));
        }
        let dir = self.opifex_state.join("catalogs").join(slug);
        if !dir.is_dir() {
            // The Atrium catalog (`catalog/`): its signers are the system's.
            return Ok(None);
        }
        let local: toml::Value = std::fs::read_to_string(dir.join("catalog.toml"))
            .map_err(|e| format!("{app_id}: catalog {slug}: {e}"))?
            .parse().map_err(|e| format!("{app_id}: catalog {slug}: {e}"))?;
        let want = field("publisher");
        let p = local.get("publisher").and_then(|a| a.as_array())
            .and_then(|a| a.iter().find(|p| !want.is_empty() && p.get("id").and_then(|v| v.as_str()) == Some(want)))
            .ok_or_else(|| format!("{app_id}: its publisher {want:?} is no longer in catalog {slug}"))?;
        let strs = |k: &str| p.get(k).and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect::<Vec<_>>()).unwrap_or_default();
        if !strs("prefixes").iter().any(|pre| !pre.is_empty() && app_id.starts_with(pre.as_str())) {
            return Err(format!("{app_id}: not under a domain its publisher {want} proved in catalog {slug}"));
        }
        let mut keys: Vec<String> = p.get("key").and_then(|v| v.as_str()).map(String::from).into_iter().collect();
        keys.extend(strs("past_keys"));
        Ok(Some(keys))
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

/// As [`verify`], naming the publisher (see [`Trust::verify_publisher`]).
pub fn verify_publisher(tree: &Path, manifest_text: &str) -> Result<Option<[u8; 32]>, String> {
    Trust::load().verify_publisher(tree, manifest_text, Demand::PolicyDefault)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_bundles_take_the_dev_key_alone() {
        let d = tmp();
        let (k, p) = portcullis_sig::new_key([3; 32]).unwrap();
        let (k2, p2) = portcullis_sig::new_key([4; 32]).unwrap();
        std::fs::create_dir_all(d.path().join("dev")).unwrap();
        std::fs::write(d.path().join("dev/public.pem"), &p).unwrap();
        std::fs::create_dir_all(d.path().join("pubs")).unwrap();
        std::fs::write(d.path().join("pubs/a.pem"), &p2).unwrap();
        let t = Trust::new(d.path().join("pubs"), false).with_dev_dir(d.path().join("dev"));
        let text = "[app]\nid = \"dev.scratch.hello\"\n";
        let tree = d.path().join("dev.scratch.hello");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join("atrium.toml.sig"), portcullis_sig::sign(text.as_bytes(), &k).unwrap()).unwrap();
        t.verify(&tree, text, Demand::PolicyDefault).expect("signed by the dev key");
        // A publisher's signature doesn't make a dev build.
        std::fs::write(tree.join("atrium.toml.sig"), portcullis_sig::sign(text.as_bytes(), &k2).unwrap()).unwrap();
        assert!(t.verify(&tree, text, Demand::PolicyDefault).is_err());
        // And the dev key signs nothing else.
        let other = d.path().join("org.x.y");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("atrium.toml.sig"), portcullis_sig::sign(b"x", &k).unwrap()).unwrap();
        assert!(t.verify(&other, "x", Demand::PolicyDefault).is_err());
    }

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

    /// ★ An app from a third-party catalog launches on its publisher's key in
    /// that catalog (current or past), whatever /etc/atrium/publishers holds,
    /// and on nothing else.
    #[test]
    fn third_party_catalog_apps_take_their_publishers_keys() {
        let d = tmp();
        let (old, old_pem) = portcullis_sig::new_key([5; 32]).unwrap();
        let (cur, cur_pem) = portcullis_sig::new_key([6; 32]).unwrap();
        let (other, other_pem) = portcullis_sig::new_key([7; 32]).unwrap();
        let st = d.path().join("opifex");
        std::fs::create_dir_all(st.join("catalogs/acme")).unwrap();
        let q = |s: &str| format!("{s:?}");
        std::fs::write(st.join("catalogs/acme/catalog.toml"), format!(
            "name = \"acme\"\nsize = 1\n[[publisher]]\nid = \"pub:00aa\"\nname = \"Acme\"\ndomains = [\"acme.example\"]\n\
             prefixes = [\"example.acme.\"]\nkinds = [\"app\"]\nkey = {}\nkey_id = \"x\"\npast = [\"y\"]\npast_keys = [{}]\n",
            q(&cur_pem), q(&old_pem))).unwrap();
        let rec = |id: &str, source: &str, publisher: &str| format!(
            "[[app]]\nid = \"{id}\"\nversion = \"1\"\nkind = \"app\"\npublisher = \"{publisher}\"\nsource = \"{source}\"\ninstalled_by = 0\ntime = 0\n");
        std::fs::write(st.join("installed.toml"), [
            rec("example.acme.notes", "catalog:acme", "pub:00aa"),
            rec("org.other.app", "catalog:acme", "pub:00aa"),
            rec("example.acme.gone", "catalog:acme", "pub:ffff"),
            rec("org.side.load", "side-load", "abc"),
        ].concat()).unwrap();
        // A system publisher is configured: it signs nothing of the catalog's.
        std::fs::create_dir_all(d.path().join("pubs")).unwrap();
        std::fs::write(d.path().join("pubs/sys.pem"), &other_pem).unwrap();
        let t = Trust::new(d.path().join("pubs"), true).with_opifex_state(&st);
        let app = |id: &str, key: &str| {
            let tree = d.path().join("apps").join(id);
            std::fs::create_dir_all(&tree).unwrap();
            let text = format!("[app]\nid = \"{id}\"\n");
            std::fs::write(tree.join("atrium.toml.sig"), portcullis_sig::sign(text.as_bytes(), key).unwrap()).unwrap();
            (tree, text)
        };
        let (tree, text) = app("example.acme.notes", &cur);
        let id = t.verify_publisher(&tree, &text, Demand::PolicyDefault).expect("current key");
        assert_eq!(id, Some(portcullis_sig::publisher_id(&cur_pem).unwrap()));
        let (tree, text) = app("example.acme.notes", &old);
        t.verify(&tree, &text, Demand::PolicyDefault).expect("a past key: installed before a rotation");
        let (tree, text) = app("example.acme.notes", &other);
        t.verify(&tree, &text, Demand::PolicyDefault).expect_err("a system publisher doesn't sign for the catalog's");
        let (tree, text) = app("org.other.app", &cur);
        let why = t.verify(&tree, &text, Demand::PolicyDefault).expect_err("outside its proved domains");
        assert!(why.contains("proved"), "{why}");
        let (tree, text) = app("example.acme.gone", &cur);
        t.verify(&tree, &text, Demand::PolicyDefault).expect_err("a publisher the catalog dropped");
        // Not from a third-party catalog: the ordinary gate.
        let (tree, text) = app("org.side.load", &other);
        t.verify(&tree, &text, Demand::PolicyDefault).expect("system publisher");
        let (tree, text) = app("org.side.load", &cur);
        t.verify(&tree, &text, Demand::PolicyDefault).expect_err("the catalog publisher's key isn't a system one");
    }
}
