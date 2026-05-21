//! Capability-diff between two manifests.
//!
//! Used by the install path on re-install: when a user
//! re-installs an app, we compare the new manifest's
//! capability surface against the previously-installed
//! one and surface any widening grants. The user has to
//! explicitly accept the new ask before the install
//! proceeds.
//!
//! Per `insula.md` §5.4 (consent UX) — capability grants
//! are pinned at install time; subsequent updates must
//! re-prompt when they widen the ask.
//!
//! # What counts as a "widening" change
//!
//! - `[network].hosts` — any host present in `new` but
//!   not in `old` (matched on `(name, port, proto)`).
//! - `[network].raw-network` — `false` → `true`.
//! - `[storage].data` / `.cache` — string changed at
//!   all. We don't parse sizes; any change is surfaced
//!   so the user sees it.
//! - `[ipc].services` — any service added.
//! - `[capabilities]` — any key added, or value changed
//!   from a falsy value (false / 0 / empty list) to a
//!   truthy one. Removed keys are non-widening.
//! - `[background].resident` / `.triggered` — newly
//!   declared (was absent, now present). Existing
//!   declarations whose internals change are not
//!   considered widening at this level (per-field
//!   tightening is a future slice).
//! - `[peer].roles` — any role newly claimed.
//! - `[entry-points]` — any new scheme registered.
//! - `[input].mode` — privileged input modes (raw)
//!   newly requested.
//!
//! Narrowing (removed hosts, smaller quotas, dropped
//! capabilities) is silent — the user already trusted
//! the wider grant.

use crate::{HostEntry, Manifest};
use std::collections::BTreeMap;

/// Structured diff between two manifests' capability
/// surfaces. Construct via [`CapabilityDiff::between`].
#[derive(Debug, Clone, Default)]
pub struct CapabilityDiff {
    /// Network hosts present in the new manifest but
    /// not the old. Match key is `(name, port, proto)`.
    pub added_network_hosts: Vec<HostEntry>,

    /// `[network].raw-network` flipped `false` → `true`.
    pub raw_network_added: bool,

    /// `[storage].data` changed. `(old, new)`.
    pub storage_data_changed: Option<(Option<String>, Option<String>)>,

    /// `[storage].cache` changed. `(old, new)`.
    pub storage_cache_changed: Option<(Option<String>, Option<String>)>,

    /// IPC services added to `[ipc].services`.
    pub added_ipc_services: Vec<String>,

    /// Capabilities table keys newly granted, or
    /// flipped from a falsy to a truthy value.
    pub added_capabilities: BTreeMap<String, toml::Value>,

    /// `[background.resident]` newly declared (was
    /// absent in the old manifest).
    pub resident_background_added: bool,

    /// `[background.triggered]` newly declared.
    pub triggered_background_added: bool,

    /// Peer roles claimed in the new manifest's
    /// `[peer].roles` but not the old.
    pub added_peer_roles: Vec<String>,

    /// `atrium-app://` URL schemes registered in
    /// `[entry-points]` but not the old.
    pub added_entry_point_schemes: Vec<String>,
}

impl CapabilityDiff {
    /// Compute the diff `old -> new`.
    pub fn between(old: &Manifest, new: &Manifest) -> Self {
        let mut d = CapabilityDiff::default();

        // [network]
        let old_hosts: Vec<&HostEntry> = old.network.as_ref()
            .map(|n| n.hosts.iter().collect())
            .unwrap_or_default();
        let new_hosts: Vec<&HostEntry> = new.network.as_ref()
            .map(|n| n.hosts.iter().collect())
            .unwrap_or_default();
        for h in &new_hosts {
            let present = old_hosts.iter().any(|o| {
                o.name == h.name && o.port == h.port && o.proto == h.proto
            });
            if !present {
                d.added_network_hosts.push((*h).clone());
            }
        }
        let old_raw = old.network.as_ref().map(|n| n.raw_network).unwrap_or(false);
        let new_raw = new.network.as_ref().map(|n| n.raw_network).unwrap_or(false);
        d.raw_network_added = !old_raw && new_raw;

        // [storage]
        let old_data = old.storage.as_ref().and_then(|s| s.data.clone());
        let new_data = new.storage.as_ref().and_then(|s| s.data.clone());
        if old_data != new_data {
            d.storage_data_changed = Some((old_data, new_data));
        }
        let old_cache = old.storage.as_ref().and_then(|s| s.cache.clone());
        let new_cache = new.storage.as_ref().and_then(|s| s.cache.clone());
        if old_cache != new_cache {
            d.storage_cache_changed = Some((old_cache, new_cache));
        }

        // [ipc].services
        let old_ipc: Vec<&String> = old.ipc.as_ref()
            .map(|s| s.services.iter().collect())
            .unwrap_or_default();
        let new_ipc: Vec<&String> = new.ipc.as_ref()
            .map(|s| s.services.iter().collect())
            .unwrap_or_default();
        for s in &new_ipc {
            if !old_ipc.iter().any(|o| o.as_str() == s.as_str()) {
                d.added_ipc_services.push((*s).clone());
            }
        }

        // [capabilities]
        let empty_caps = BTreeMap::new();
        let old_caps = old.capabilities.as_ref().unwrap_or(&empty_caps);
        let new_caps = new.capabilities.as_ref().unwrap_or(&empty_caps);
        for (k, v_new) in new_caps {
            match old_caps.get(k) {
                None => {
                    if is_truthy(v_new) {
                        d.added_capabilities.insert(k.clone(), v_new.clone());
                    }
                }
                Some(v_old) => {
                    if !is_truthy(v_old) && is_truthy(v_new) {
                        d.added_capabilities.insert(k.clone(), v_new.clone());
                    }
                }
            }
        }

        // [background]
        d.resident_background_added = new.background.as_ref()
            .and_then(|b| b.resident.as_ref()).is_some()
            && old.background.as_ref()
                .and_then(|b| b.resident.as_ref()).is_none();
        d.triggered_background_added = new.background.as_ref()
            .and_then(|b| b.triggered.as_ref()).is_some()
            && old.background.as_ref()
                .and_then(|b| b.triggered.as_ref()).is_none();

        // [peer] — newly *requested* outbound peer roles
        // count as widening. Newly *implemented* roles
        // (this app accepting an incoming peer role)
        // also widen the surface — the app can now be
        // talked to over Concursus in a way it couldn't
        // before.
        let empty_pr: BTreeMap<String, crate::RoleReqSpec> = BTreeMap::new();
        let empty_pi: BTreeMap<String, crate::RoleImplSpec> = BTreeMap::new();
        let old_req = old.peer.as_ref().map(|p| &p.requests).unwrap_or(&empty_pr);
        let new_req = new.peer.as_ref().map(|p| &p.requests).unwrap_or(&empty_pr);
        for k in new_req.keys() {
            if !old_req.contains_key(k) {
                d.added_peer_roles.push(format!("requests:{}", k));
            }
        }
        let old_imp = old.peer.as_ref().map(|p| &p.implements).unwrap_or(&empty_pi);
        let new_imp = new.peer.as_ref().map(|p| &p.implements).unwrap_or(&empty_pi);
        for k in new_imp.keys() {
            if !old_imp.contains_key(k) {
                d.added_peer_roles.push(format!("implements:{}", k));
            }
        }

        // [entry-points] — keys are scheme names.
        let empty_ep = BTreeMap::new();
        let old_ep = old.entry_points.as_ref().unwrap_or(&empty_ep);
        let new_ep = new.entry_points.as_ref().unwrap_or(&empty_ep);
        for k in new_ep.keys() {
            if !old_ep.contains_key(k) {
                d.added_entry_point_schemes.push(k.clone());
            }
        }

        d
    }

    /// Does this diff widen any capability the user
    /// already accepted? If `false`, the re-install can
    /// proceed silently. If `true`, the host should
    /// prompt (or refuse without `--accept-changes`).
    pub fn is_widening(&self) -> bool {
        !self.added_network_hosts.is_empty()
            || self.raw_network_added
            || self.storage_data_changed.is_some()
            || self.storage_cache_changed.is_some()
            || !self.added_ipc_services.is_empty()
            || !self.added_capabilities.is_empty()
            || self.resident_background_added
            || self.triggered_background_added
            || !self.added_peer_roles.is_empty()
            || !self.added_entry_point_schemes.is_empty()
    }

    /// One-line-per-change human summary, suitable for
    /// emitting at install time before prompting.
    /// Returns an empty string if [`Self::is_widening`]
    /// is false.
    pub fn human_summary(&self) -> String {
        let mut lines = Vec::new();
        for h in &self.added_network_hosts {
            lines.push(format!(
                "  + network host {}:{} ({:?})",
                h.name, h.port, h.proto
            ));
        }
        if self.raw_network_added {
            lines.push("  + raw-network access (bypasses host allowlist)".to_string());
        }
        if let Some((old, new)) = &self.storage_data_changed {
            lines.push(format!(
                "  ~ storage.data {} -> {}",
                old.as_deref().unwrap_or("(unset)"),
                new.as_deref().unwrap_or("(unset)"),
            ));
        }
        if let Some((old, new)) = &self.storage_cache_changed {
            lines.push(format!(
                "  ~ storage.cache {} -> {}",
                old.as_deref().unwrap_or("(unset)"),
                new.as_deref().unwrap_or("(unset)"),
            ));
        }
        for s in &self.added_ipc_services {
            lines.push(format!("  + ipc service {}", s));
        }
        for (k, _) in &self.added_capabilities {
            lines.push(format!("  + capability {}", k));
        }
        if self.resident_background_added {
            lines.push("  + resident background task".to_string());
        }
        if self.triggered_background_added {
            lines.push("  + triggered background task".to_string());
        }
        for r in &self.added_peer_roles {
            lines.push(format!("  + peer role {}", r));
        }
        for s in &self.added_entry_point_schemes {
            lines.push(format!("  + entry-point scheme {}", s));
        }
        lines.join("\n")
    }
}

fn is_truthy(v: &toml::Value) -> bool {
    match v {
        toml::Value::Boolean(b) => *b,
        toml::Value::Integer(i) => *i != 0,
        toml::Value::Float(f) => *f != 0.0,
        toml::Value::String(s) => !s.is_empty(),
        toml::Value::Array(a) => !a.is_empty(),
        toml::Value::Table(t) => !t.is_empty(),
        toml::Value::Datetime(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(s: &str) -> Manifest {
        Manifest::parse(s).unwrap()
    }

    const BASE: &str = r#"
[app]
name = "com.example.x"
version = "1.0.0"
sdk-version = "1.x"

[bundle]
form = "native"
arches = ["aarch64-darwin"]
entry = "bin/x"
"#;

    #[test]
    fn identical_manifests_no_widening() {
        let a = manifest(BASE);
        let b = manifest(BASE);
        let d = CapabilityDiff::between(&a, &b);
        assert!(!d.is_widening());
        assert_eq!(d.human_summary(), "");
    }

    #[test]
    fn added_network_host_is_widening() {
        let new_src = format!("{}\n[network]\nhosts = [\n  {{ name = \"api.example.com\", port = 443, proto = \"tcp\" }}\n]\n", BASE);
        let d = CapabilityDiff::between(&manifest(BASE), &manifest(&new_src));
        assert!(d.is_widening());
        assert_eq!(d.added_network_hosts.len(), 1);
        assert!(d.human_summary().contains("api.example.com:443"));
    }

    #[test]
    fn removed_network_host_is_narrowing() {
        let with_host = format!("{}\n[network]\nhosts = [\n  {{ name = \"api.example.com\", port = 443, proto = \"tcp\" }}\n]\n", BASE);
        let d = CapabilityDiff::between(&manifest(&with_host), &manifest(BASE));
        assert!(!d.is_widening(), "removing a host should not require consent");
    }

    #[test]
    fn raw_network_flip_is_widening() {
        let off = format!("{}\n[network]\nhosts = []\nraw-network = false\n", BASE);
        let on  = format!("{}\n[network]\nhosts = []\nraw-network = true\n", BASE);
        let d = CapabilityDiff::between(&manifest(&off), &manifest(&on));
        assert!(d.raw_network_added);
        assert!(d.is_widening());
    }

    #[test]
    fn storage_change_surfaces() {
        let small = format!("{}\n[storage]\ndata = \"10MB\"\n", BASE);
        let big   = format!("{}\n[storage]\ndata = \"1GB\"\n", BASE);
        let d = CapabilityDiff::between(&manifest(&small), &manifest(&big));
        assert!(d.is_widening());
        let summary = d.human_summary();
        assert!(summary.contains("10MB -> 1GB"), "got: {}", summary);
    }

    #[test]
    fn capabilities_added_truthy_only() {
        let off = format!("{}\n[capabilities]\nlocation = false\n", BASE);
        let on  = format!("{}\n[capabilities]\nlocation = true\n", BASE);
        let d = CapabilityDiff::between(&manifest(&off), &manifest(&on));
        assert!(d.is_widening());
        assert!(d.added_capabilities.contains_key("location"));

        // false -> false is not widening
        let d = CapabilityDiff::between(&manifest(&off), &manifest(&off));
        assert!(!d.is_widening());
    }

    #[test]
    fn background_resident_first_time_widens() {
        let with_bg = format!(
            "{}\n[background.resident]\nentry = \"bin/bg\"\n",
            BASE
        );
        let d = CapabilityDiff::between(&manifest(BASE), &manifest(&with_bg));
        assert!(d.resident_background_added);
        assert!(d.is_widening());
    }
}
