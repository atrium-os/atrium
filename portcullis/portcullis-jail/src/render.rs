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
    ///
    /// Refuses, rather than renders, any name, key or value that could end
    /// its token early or be expanded by jail(8): a value reaches here from a
    /// manifest (the entry in `exec.start`), and one `"` in it would let the
    /// manifest add parameters such as `exec.prestart`, which jail(8) runs on
    /// the host as root.
    pub fn render_jail_conf(&self) -> Result<String, RenderError> {
        let mut out = String::new();
        if !is_plain_token(&self.name) {
            return Err(RenderError::new("jail name", &self.name));
        }
        let _ = writeln!(out, "{} {{", self.name);
        let _ = writeln!(out, "    path = {};", quote("path", &self.root_path.display().to_string())?);
        for (k, v) in &self.params {
            if !is_plain_token(k) {
                return Err(RenderError::new("parameter name", k));
            }
            let _ = match v {
                Value::String(s)   => writeln!(out, "    {k} = {};", quote(k, s)?),
                Value::Bool(true)  => writeln!(out, "    {k} = true;"),
                Value::Bool(false) => writeln!(out, "    {k} = false;"),
                Value::Number(n)   => writeln!(out, "    {k} = {n};"),
                Value::Symbolic(s) if is_plain_token(s) => writeln!(out, "    {k} = {s};"),
                Value::Symbolic(s) => return Err(RenderError::new(k, s)),
            };
        }
        for m in &self.mounts {
            /* fstab format: src dst type opts 0 0. Fields are split on
             * whitespace, so none may contain any. */
            let opts = if m.opts.is_empty() { "rw".into() } else { m.opts.join(",") };
            let fields = [m.src.display().to_string(), m.dst.display().to_string(), m.fstype.clone(), opts];
            for f in &fields {
                if f.is_empty() || f.chars().any(char::is_whitespace) {
                    return Err(RenderError::new("mount field", f));
                }
            }
            let _ = writeln!(out, "    mount += {};",
                quote("mount", &format!("{} {} {} {} 0 0", fields[0], fields[1], fields[2], fields[3]))?);
        }
        let _ = writeln!(out, "}}");
        Ok(out)
    }

    /// Render this config and write it to a new file for `jail -c -f`, in
    /// the temporary directory, named `<prefix>-<pid>-<nanos>-<n>.conf`.
    ///
    /// The file is created with `O_CREAT|O_EXCL` and mode 0600, never
    /// opened if something is already at that name: the directory is the
    /// world-writable `/tmp`, and a file or symlink planted at a predictable
    /// name would let its owner rewrite the config root is about to hand to
    /// jail(8), or point root's write at another file.
    pub fn write_jail_conf(&self, prefix: &str) -> Result<std::path::PathBuf, String> {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt;
        let text = self.render_jail_conf().map_err(|e| e.to_string())?;
        let dir = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        for n in 0..16 {
            let path = dir.join(format!("{prefix}-{}-{nanos}-{n}.conf", std::process::id()));
            match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path) {
                Ok(mut f) => {
                    return f.write_all(text.as_bytes()).map(|()| path.clone()).map_err(|e| {
                        let _ = std::fs::remove_file(&path);
                        format!("write {}: {e}", path.display())
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("create {}: {e}", path.display())),
            }
        }
        Err(format!("no free jail.conf name for {prefix} in {}", dir.display()))
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

/// A value jail.conf would read differently from how it is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderError {
    pub what:  String,
    pub value: String,
}

impl RenderError {
    fn new(what: &str, value: &str) -> Self {
        RenderError { what: what.into(), value: value.into() }
    }
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "refusing to render {} {:?} into jail.conf: it holds a quote, `$`, `\\` or a control character", self.what, self.value)
    }
}

impl std::error::Error for RenderError {}

/// A double-quoted jail.conf string. Inside double quotes jail(8) processes
/// backslash escapes and expands `$name` / `${name}`, and a `"` ends the
/// string, so a value holding any of those, or a control character (a
/// newline ends the line), is refused instead of escaped: nothing Portcullis
/// builds legitimately needs them.
fn quote(what: &str, s: &str) -> Result<String, RenderError> {
    if s.chars().any(|c| matches!(c, '"' | '\\' | '$') || c.is_control()) {
        return Err(RenderError::new(what, s));
    }
    Ok(format!("\"{s}\""))
}

/// An unquoted jail.conf token: a jail name, a parameter name, or a
/// symbolic value (`new`, `inherit`, `disable`).
fn is_plain_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
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
        let s = j.render_jail_conf().unwrap();
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
        let s = j.render_jail_conf().unwrap();
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

    #[test]
    fn a_quote_in_a_value_is_refused_not_rendered() {
        /* The proof of concept from the 2026-11-24 review: an entry that
         * closes exec.start and adds an exec.prestart (run on the host as
         * root by jail(8)). */
        for evil in ["/bin/y\"; exec.prestart = \"/usr/bin/touch /root/pwned",
                     "/bin/y$HOME", "/bin/y\\", "/bin/y\nexec.prestart=x"] {
            let mut j = JailConfig::new("t".into(), PathBuf::from("/j"));
            j.set("exec.start", Value::String(evil.into()));
            let e = j.render_jail_conf().expect_err(evil);
            assert_eq!((e.what.as_str(), e.value.as_str()), ("exec.start", evil));
        }
    }

    #[test]
    fn names_keys_symbols_and_mount_fields_are_plain_tokens() {
        let bad_name = JailConfig::new("t { x".into(), PathBuf::from("/j"));
        assert!(bad_name.render_jail_conf().is_err());
        let mut j = JailConfig::new("t".into(), PathBuf::from("/j"));
        j.set("exec.prestart = x; a", Value::Bool(true));
        assert!(j.render_jail_conf().is_err());
        let mut j = JailConfig::new("t".into(), PathBuf::from("/j"));
        j.set("vnet", Value::Symbolic("new; exec.prestart = x".into()));
        assert!(j.render_jail_conf().is_err());
        let mut j = JailConfig::new("t".into(), PathBuf::from("/j"));
        j.add_mount(std::path::Path::new("/a b"), std::path::Path::new("/j/a"), "nullfs", &["ro"]);
        assert!(j.render_jail_conf().is_err());
        let mut j = JailConfig::new("app-org_x_y__1".into(), PathBuf::from("/j"));
        j.set("vnet", Value::Symbolic("new".into()));
        j.set("exec.start", Value::String("/bin/y --persona work".into()));
        assert!(j.render_jail_conf().is_ok());
    }

    #[test]
    fn the_conf_file_is_new_and_private() {
        use std::os::unix::fs::PermissionsExt;
        let mut j = JailConfig::new("t".into(), PathBuf::from("/j"));
        j.set("persist", Value::Bool(true));
        let a = j.write_jail_conf("render-test").unwrap();
        let b = j.write_jail_conf("render-test").unwrap();
        assert_ne!(a, b, "each write gets its own file");
        assert_eq!(std::fs::metadata(&a).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), j.render_jail_conf().unwrap());
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
        let mut evil = j.clone();
        evil.set("exec.start", Value::String("x\"; exec.prestart = \"y".into()));
        assert!(evil.write_jail_conf("render-test").is_err());
    }
}
