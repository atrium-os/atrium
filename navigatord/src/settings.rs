//! navigatord's settings (navigator-ui.md §2.3a, J1): its environment,
//! else its bundle's config file.
//!
//! A jail gets no environment of its caller's, so a jailed navigatord
//! reads `etc/navigatord.conf` beside its `bin/` (or `NAVIGATORD_CONFIG`):
//! `KEY=value` lines with the environment's names, `#` comments. Only
//! navigatord's own keys are read from it; anything else is ignored with a
//! warning, so the file can't set the loader's or another program's.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

/// The keys the file may set.
fn ours(k: &str) -> bool {
    k.starts_with("NAVIGATORD_") || k.starts_with("NAVIGATOR_UI_") || k == "FRESCOD_CHROME_SOCK" || k == "PORTCULLIS_SOCKET"
}

/// `KEY=value` lines; later lines win.
pub fn parse(text: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for l in text.lines() {
        let l = l.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let Some((k, v)) = l.split_once('=') else {
            eprintln!("navigatord: config: not KEY=value: {l:?}");
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        if !ours(k) {
            eprintln!("navigatord: config: {k} is not navigatord's; ignored");
            continue;
        }
        m.insert(k.to_string(), v.to_string());
    }
    m
}

fn file() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("NAVIGATORD_CONFIG") {
        return Some(PathBuf::from(p));
    }
    let exe = std::env::current_exe().ok()?;
    let p = exe.parent()?.parent()?.join("etc/navigatord.conf");
    p.exists().then_some(p)
}

fn loaded() -> &'static HashMap<String, String> {
    static CONF: OnceLock<HashMap<String, String>> = OnceLock::new();
    CONF.get_or_init(|| match file() {
        Some(p) => match std::fs::read_to_string(&p) {
            Ok(t) => {
                eprintln!("navigatord: settings from {}", p.display());
                parse(&t)
            }
            Err(e) => {
                eprintln!("navigatord: config {}: {e}", p.display());
                HashMap::new()
            }
        },
        None => HashMap::new(),
    })
}

/// `k` from the environment, else from the config file; blank is unset.
pub fn get(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.trim().is_empty())
        .or_else(|| loaded().get(k).cloned().filter(|v| !v.trim().is_empty()))
}

/// The `NAVIGATOR_UI_*` settings, for a UI navigatord starts (it gets no
/// environment of ours in its jail): the environment's over the file's.
pub fn ui_pairs() -> Vec<(String, String)> {
    let mut m: std::collections::BTreeMap<String, String> = loaded().iter()
        .filter(|(k, _)| k.starts_with("NAVIGATOR_UI_")).map(|(k, v)| (k.clone(), v.clone())).collect();
    for (k, v) in std::env::vars() {
        if k.starts_with("NAVIGATOR_UI_") {
            m.insert(k, v);
        }
    }
    m.into_iter().take(navigator_control::SETTINGS_MAX).collect()
}

/// The path setting `k`, else `default`.
pub fn path(k: &str, default: &str) -> String {
    get(k).unwrap_or_else(|| default.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_navigatords_own_keys() {
        let m = parse("# c\nNAVIGATORD_CONVERTER = jail:org.atrium.navigator.converter\nLD_PRELOAD=/x.so\nPORTCULLIS_SOCKET=/p\nnonsense\nNAVIGATORD_STORE=/a\nNAVIGATORD_STORE=/b\n");
        assert_eq!(m.get("NAVIGATORD_CONVERTER").map(String::as_str), Some("jail:org.atrium.navigator.converter"));
        assert_eq!(m.get("PORTCULLIS_SOCKET").map(String::as_str), Some("/p"));
        assert_eq!(m.get("NAVIGATORD_STORE").map(String::as_str), Some("/b"), "the later line");
        assert!(!m.contains_key("LD_PRELOAD"));
        assert_eq!(m.len(), 3);
    }
}
