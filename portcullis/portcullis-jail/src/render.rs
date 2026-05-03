//! Render a `JailConfig` to:
//!  - a jail.conf section (for `jail -c -f <file>`)
//!  - a devfs.rules ruleset (for `devfs rule -s <id>`)
//!
//! Two outputs because they live in different files / interfaces in
//! FreeBSD; both are pure text.

use std::fmt::Write;

use crate::config::{JailConfig, Value};

impl JailConfig {
    /// Render this config as a jail.conf section.
    ///
    /// Output is deterministic (insertion-ordered params; mounts in
    /// the order they were added). Suitable for golden-file diffing.
    pub fn render_jail_conf(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "{} {{", self.name);
        let _ = writeln!(out, "    path = {};", quote(&self.root_path.display().to_string()));
        for (k, v) in &self.params {
            match v {
                Value::String(s)   => writeln!(out, "    {k} = {};", quote(s)),
                Value::Bool(true)  => writeln!(out, "    {k} = true;"),
                Value::Bool(false) => writeln!(out, "    {k} = false;"),
                Value::Number(n)   => writeln!(out, "    {k} = {n};"),
                Value::Symbolic(s) => writeln!(out, "    {k} = {s};"),
            }.unwrap();
        }
        for m in &self.mounts {
            /* fstab format: src dst type opts 0 0 */
            let _ = writeln!(out,
                "    mount += {};",
                quote(&format!("{} {} {} {} 0 0",
                    m.src.display(),
                    m.dst.display(),
                    m.fstype,
                    if m.opts.is_empty() { "rw".into() } else { m.opts.join(",") })));
        }
        let _ = writeln!(out, "}}");
        out
    }

    /// Render the devfs.rules ruleset for this jail.
    /// Output is the lines that go after `[<rulename>=<id>]` in
    /// /etc/devfs.rules.
    pub fn render_devfs_rules(&self) -> String {
        let mut out = String::new();
        /* Standard atrium ruleset baseline: hide everything, then
         * unhide the per-cap nodes. */
        let _ = writeln!(out, "add include $devfsrules_hide_all");
        let _ = writeln!(out, "add include $devfsrules_unhide_basic");
        let _ = writeln!(out, "add include $devfsrules_unhide_login");
        for d in &self.devfs_actions {
            let _ = writeln!(out, "add {}", d.line);
        }
        out
    }
}

fn quote(s: &str) -> String {
    /* jail.conf double-quotes. We don't have to handle escapes for
     * the values our builders emit (paths, identifiers); guard with
     * an assert in case someone passes a quote-bearing string later. */
    debug_assert!(!s.contains('"'), "value contains quote: {s:?}");
    format!("\"{s}\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use crate::config::JailConfig;

    #[test]
    fn empty_jail_renders_minimal() {
        let mut j = JailConfig::new("test".into(), PathBuf::from("/jail"));
        j.set("persist", Value::Bool(true));
        let s = j.render_jail_conf();
        assert!(s.contains("test {"));
        assert!(s.contains("path = \"/jail\""));
        assert!(s.contains("persist = true;"));
    }

    #[test]
    fn mount_renders_fstab_format() {
        let mut j = JailConfig::new("t".into(), PathBuf::from("/j"));
        j.add_mount(
            std::path::Path::new("/atrium/sockets/foo.sock"),
            std::path::Path::new("/j/atrium/sockets/foo.sock"),
            "nullfs", &["ro"],
        );
        let s = j.render_jail_conf();
        assert!(s.contains("mount += "));
        assert!(s.contains("/atrium/sockets/foo.sock"));
        assert!(s.contains("nullfs"));
        assert!(s.contains(" ro "));
    }

    #[test]
    fn devfs_rules_baseline_then_unhides() {
        let mut j = JailConfig::new("t".into(), PathBuf::from("/j"));
        j.add_devfs_action("path 'fresco0' unhide");
        let s = j.render_devfs_rules();
        assert!(s.contains("hide_all"));
        assert!(s.contains("path 'fresco0' unhide"));
    }
}
