//! `cargo atrium cross <arch>`: build for Atrium on the other arch, or from
//! Linux or macOS (dev-tools.md §5.4, §5.5).
//!
//! What it needs: a sysroot (the SDK's archive for that arch, unpacked; or
//! `--sysroot`), clang and ld.lld, and a Rust std for the target (rustup's
//! for amd64, the Rust layer's or the SDK's for aarch64). It writes small
//! cc/c++ wrappers into `target/atrium-cross/` that call clang with the
//! FreeBSD target and the sysroot, points cargo, the cc crate and
//! pkg-config at them, and runs `cargo build --target <triple>`.

use std::path::{Path, PathBuf};

pub struct Target {
    pub arch:   &'static str,
    pub cpu:    &'static str,
    pub triple: &'static str,
}

pub const TARGETS: &[Target] = &[
    Target { arch: "amd64", cpu: "x86_64", triple: "x86_64-unknown-freebsd" },
    Target { arch: "aarch64", cpu: "aarch64", triple: "aarch64-unknown-freebsd" },
];

/// `amd64`, `x86_64`, `aarch64`, `arm64` or a full triple.
pub fn target(name: &str) -> Option<&'static Target> {
    let n = match name {
        "x86_64" => "amd64",
        "arm64" => "aarch64",
        o => o,
    };
    TARGETS.iter().find(|t| t.arch == n || t.triple == n)
}

/// The FreeBSD major version the SDK targets (clang's `--target` suffix).
pub const FREEBSD_MAJOR: u32 = 16;

/// Where the sysroot for `t` is: `--sysroot`, then `$ATRIUM_SYSROOT/<arch>`,
/// then `~/.atrium/sysroot/<arch>`.
pub fn sysroot(t: &Target, given: Option<&Path>, env: Option<&str>, home: Option<&Path>) -> Result<PathBuf, String> {
    let mut tried = Vec::new();
    let candidates: Vec<PathBuf> = match given {
        Some(p) => vec![p.to_path_buf()],
        None => env
            .map(|e| Path::new(e).join(t.arch))
            .into_iter()
            .chain(home.map(|h| h.join(".atrium/sysroot").join(t.arch)))
            .collect(),
    };
    for c in candidates {
        if c.join("usr/lib/Scrt1.o").is_file() && c.join("usr/include/stdio.h").is_file() {
            return Ok(c);
        }
        tried.push(c.display().to_string());
    }
    Err(format!(
        "no {} sysroot (looked in {}); unpack the SDK's atrium-sysroot-*-{}.tar.xz there, or pass --sysroot",
        t.arch,
        if tried.is_empty() { "nowhere: set ATRIUM_SYSROOT or HOME".into() } else { tried.join(", ") },
        t.arch
    ))
}

/// The wrapper script for `kind` ("cc" or "c++").
pub fn wrapper(t: &Target, clang: &Path, sysroot: &Path, kind: &str) -> String {
    let drv = if kind == "c++" { clangxx(clang) } else { clang.to_path_buf() };
    format!(
        "#!/bin/sh\n# Written by cargo atrium cross.\nexec {} --target={}-unknown-freebsd{} --sysroot={} -fuse-ld=lld -Wno-unused-command-line-argument \"$@\"\n",
        sh_quote(&drv.display().to_string()),
        t.cpu,
        FREEBSD_MAJOR,
        sh_quote(&sysroot.display().to_string()),
    )
}

/// clang++ next to clang (`clang-18` → `clang++-18`).
fn clangxx(clang: &Path) -> PathBuf {
    let name = clang.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let xx = match name.strip_prefix("clang") {
        Some(rest) => format!("clang++{rest}"),
        None => "clang++".into(),
    };
    clang.with_file_name(xx)
}

fn sh_quote(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+=:".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// The environment for cargo: linker, cc-crate compilers, archiver and
/// pkg-config, all for `t` only, so host build scripts are untouched.
pub fn env(t: &Target, cc: &Path, cxx: &Path, ar: Option<&Path>, sysroot: &Path) -> Vec<(String, String)> {
    let up = t.triple.to_uppercase().replace('-', "_");
    let low = t.triple.replace('-', "_");
    let s = sysroot.display().to_string();
    let mut v = vec![
        (format!("CARGO_TARGET_{up}_LINKER"), cc.display().to_string()),
        (format!("CC_{low}"), cc.display().to_string()),
        (format!("CXX_{low}"), cxx.display().to_string()),
        (format!("PKG_CONFIG_SYSROOT_DIR_{low}"), s.clone()),
        (format!("PKG_CONFIG_LIBDIR_{low}"), format!("{s}/usr/libdata/pkgconfig:{s}/usr/local/libdata/pkgconfig")),
        ("PKG_CONFIG_ALLOW_CROSS".into(), "1".into()),
    ];
    if let Some(ar) = ar {
        v.push((format!("AR_{low}"), ar.display().to_string()));
    }
    v
}

/// The first of `names` found on `path` (a PATH-style list), or in the
/// usual LLVM install folders.
pub fn find_tool(names: &[&str], path: &str) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::split_paths(path).collect();
    for extra in ["/opt/homebrew/opt/llvm/bin", "/usr/local/opt/llvm/bin", "/opt/homebrew/opt/lld/bin"] {
        dirs.push(PathBuf::from(extra));
    }
    for n in names {
        for d in &dirs {
            let p = d.join(n);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// Whether the active rustc has std for `t`; the hint if not.
pub fn std_hint(t: &Target) -> String {
    match t.arch {
        "amd64" => format!("rustup target add {}", t.triple),
        _ => format!(
            "rustup has no std for {} (tier 3): install the SDK's rust-std-<version>-{} component for this rustc version, \
             or use a nightly with -Zbuild-std",
            t.triple, t.triple
        ),
    }
}

/// Whether `rustc` can build std itself: a nightly with rust-src.
pub fn can_build_std(rustc: &str) -> bool {
    let out = |a: &[&str]| std::process::Command::new(rustc).args(a).output().ok()
        .filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let nightly = out(&["-V"]).is_some_and(|v| v.contains("-nightly") || v.contains("-dev"));
    nightly && out(&["--print", "sysroot"]).is_some_and(|s| Path::new(&s).join("lib/rustlib/src/rust/library/std").is_dir())
}

/// The target's rustflags come from the wrappers, not from a
/// `.cargo/config.toml`: a config written for another linker (rust-lld's
/// `--dynamic-linker=`, CRT objects on a Mac path) breaks clang's link. An
/// empty RUSTFLAGS overrides `target.<triple>.rustflags` and
/// `build.rustflags`; flags the caller set in the environment stay.
pub fn rustflags_env(caller_set: bool) -> Vec<(String, String)> {
    if caller_set { Vec::new() } else { vec![("RUSTFLAGS".into(), String::new())] }
}

pub fn has_std(libdir: &Path) -> bool {
    std::fs::read_dir(libdir)
        .map(|d| d.flatten().any(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n.starts_with("libstd-") && n.ends_with(".rlib")
        }))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_wrapper() {
        assert_eq!(target("arm64").unwrap().triple, "aarch64-unknown-freebsd");
        assert_eq!(target("x86_64-unknown-freebsd").unwrap().arch, "amd64");
        assert!(target("riscv64").is_none());
        let t = target("aarch64").unwrap();
        let w = wrapper(t, Path::new("/usr/bin/clang-18"), Path::new("/s r/aarch64"), "c++");
        assert!(w.contains("exec /usr/bin/clang++-18 --target=aarch64-unknown-freebsd16 --sysroot='/s r/aarch64' -fuse-ld=lld -Wno-unused-command-line-argument \"$@\""), "{w}");
        let e = env(t, Path::new("/w/cc"), Path::new("/w/c++"), None, Path::new("/s"));
        assert!(e.contains(&("CARGO_TARGET_AARCH64_UNKNOWN_FREEBSD_LINKER".into(), "/w/cc".into())));
        assert!(e.contains(&("CC_aarch64_unknown_freebsd".into(), "/w/cc".into())));
        // A config's target rustflags are overridden; the caller's stay.
        assert_eq!(rustflags_env(false), [("RUSTFLAGS".to_string(), String::new())]);
        assert!(rustflags_env(true).is_empty());
    }

    #[test]
    fn sysroot_lookup() {
        let d = std::env::temp_dir().join(format!("atrium-cross-{}", std::process::id()));
        let s = d.join("amd64");
        std::fs::create_dir_all(s.join("usr/lib")).unwrap();
        std::fs::create_dir_all(s.join("usr/include")).unwrap();
        let t = target("amd64").unwrap();
        assert!(sysroot(t, None, Some(d.to_str().unwrap()), None).is_err());
        std::fs::write(s.join("usr/lib/Scrt1.o"), b"").unwrap();
        std::fs::write(s.join("usr/include/stdio.h"), b"").unwrap();
        assert_eq!(sysroot(t, None, Some(d.to_str().unwrap()), None).unwrap(), s);
        let e = sysroot(target("aarch64").unwrap(), None, Some(d.to_str().unwrap()), None).unwrap_err();
        assert!(e.contains("atrium-sysroot-*-aarch64.tar.xz"), "{e}");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
