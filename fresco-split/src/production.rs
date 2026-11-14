//! What a shipped frescod refuses to run with (officina.md §10.5;
//! docs/spec/fresco-f4-split.md §F4i).
//!
//! Several frescod settings exist for development and tests only, and each
//! puts clients' scenes, or their raster, back into frescod's own process or
//! out of their jail: then one huge or hostile scene could stall or take
//! down compositing for the whole screen. A shipped frescod (the
//! `40-atrium-frescod` service: installed as `atrium-frescod`, run as
//! `_frescod`) exits with the reason instead of starting with any of them.
//! It never falls back to an unsplit mode on its own either: with the
//! Vulkan renderer split mode would stay off, so that renderer is refused.
//!
//! Development runs (`frescod` as root from the VM scripts) are unchanged.

/// `_frescod`, the system user the shipped service runs frescod as.
pub const FRESCOD_UID: u32 = 65010;
/// The name the shipped service installs frescod under.
pub const SHIPPED_NAME: &str = "atrium-frescod";

/// Whether this frescod is a shipped one: run as `_frescod`, or installed
/// as `atrium-frescod`.
pub fn is_shipped(euid: u32, argv0: &str) -> bool {
    let name = argv0.rsplit('/').next().unwrap_or(argv0);
    euid == FRESCOD_UID || name == SHIPPED_NAME
}

/// The development settings in the environment (`get`), each with why a
/// shipped frescod refuses it. Empty: nothing to refuse.
pub fn dev_settings(get: impl Fn(&str) -> Option<String>) -> Vec<String> {
    let mut out = Vec::new();
    let is = |k: &str, v: &str| get(k).is_some_and(|x| x.trim() == v);
    if is("FRESCOD_SPLIT", "0") {
        out.push("FRESCOD_SPLIT=0 parses every client's scene inside frescod".to_string());
    }
    if is("FRESCOD_RENDERER", "vulkan") {
        out.push("FRESCOD_RENDERER=vulkan merges every client's scene into one process (no split mode)".to_string());
    }
    if is("FRESCOD_SPLIT_PIXELS", "0") {
        out.push("FRESCOD_SPLIT_PIXELS=0 rasters clients' frames inside frescod".to_string());
    }
    if get("FRESCOD_RENDERER_LAUNCHER").is_some_and(|v| !v.trim().is_empty()) {
        out.push("FRESCOD_RENDERER_LAUNCHER starts renderers outside portcullisd's jails".to_string());
    }
    for k in ["FRESCOD_GPU_ISOLATION_PROBE", "FRESCOD_GPU_ONLY"] {
        if get(k).is_some() {
            out.push(format!("{k} is a test mode"));
        }
    }
    out
}

/// `Err` with every reason if a shipped frescod (`euid`, `argv0`) was
/// started with development settings.
pub fn check(euid: u32, argv0: &str, get: impl Fn(&str) -> Option<String>) -> Result<(), String> {
    if !is_shipped(euid, argv0) {
        return Ok(());
    }
    let bad = dev_settings(get);
    if bad.is_empty() {
        return Ok(());
    }
    Err(format!("refusing to start a shipped frescod with development settings: {}", bad.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(kv: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| kv.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    #[test]
    fn shipped_frescod_refuses_dev_modes() {
        let shipped = [(FRESCOD_UID, "/usr/local/bin/frescod"), (0, "/usr/local/bin/atrium-frescod"), (0, "atrium-frescod")];
        let bad: &[&[(&str, &str)]] = &[
            &[("FRESCOD_SPLIT", "0")],
            &[("FRESCOD_RENDERER", "vulkan")],
            &[("FRESCOD_SPLIT_PIXELS", "0")],
            &[("FRESCOD_RENDERER_LAUNCHER", "unconfined")],
            &[("FRESCOD_RENDERER_LAUNCHER", "/tmp/x {client}")],
            &[("FRESCOD_GPU_ISOLATION_PROBE", "1")],
            &[("FRESCOD_GPU_ONLY", "1")],
        ];
        for (uid, argv0) in shipped {
            for kv in bad {
                assert!(check(uid, argv0, env(kv)).is_err(), "{uid} {argv0} {kv:?}");
            }
            // The shipped configuration itself.
            for ok in [&[][..], &[("FRESCOD_SPLIT", "1"), ("FRESCOD_RENDERER", "compute")], &[("FRESCOD_RENDERER", "gpu")]] {
                assert!(check(uid, argv0, env(ok)).is_ok(), "{uid} {argv0} {ok:?}");
            }
        }
    }

    #[test]
    fn development_frescod_keeps_its_modes() {
        assert!(check(0, "/root/pp/frescod", env(&[("FRESCOD_SPLIT", "0"), ("FRESCOD_RENDERER", "vulkan")])).is_ok());
        assert!(!is_shipped(0, "/usr/local/bin/frescod-atrium-frescod"));
    }

    #[test]
    fn every_reason_is_named() {
        let e = check(FRESCOD_UID, "frescod", env(&[("FRESCOD_SPLIT", "0"), ("FRESCOD_RENDERER", "vulkan")])).unwrap_err();
        assert!(e.contains("FRESCOD_SPLIT=0") && e.contains("FRESCOD_RENDERER=vulkan"), "{e}");
    }
}
