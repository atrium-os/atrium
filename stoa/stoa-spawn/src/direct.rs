//! `DirectSpawner` — `forkpty` + `execve` a shell as the current user, no
//! jail. The dev/test path (macOS + FreeBSD-without-the-TCB). Rejects a
//! [`Target::Jail`] target: there are no jails off-Atrium, and pretending
//! otherwise would hide a real capability gap behind a silent fallback.

use std::ffi::CString;
use std::io;
use std::ptr;

use crate::{pty_shell_from_raw, resolve_path, PtyShell, ShellSpawner, SpawnSpec, Target};

/// Spawns shells directly on the host with no jail. See module docs.
#[derive(Debug, Default, Clone)]
pub struct DirectSpawner;

impl DirectSpawner {
    pub fn new() -> Self {
        DirectSpawner
    }
}

impl ShellSpawner for DirectSpawner {
    fn spawn(&self, spec: &SpawnSpec) -> io::Result<PtyShell> {
        if let Target::Jail(name) = &spec.target {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "DirectSpawner cannot enter jail {name:?}; \
                     jail targets need the FreeBSD BrokerSpawner (stoa.md §4.5)"
                ),
            ));
        }

        // --- Everything that can allocate or fail is done BEFORE the fork,
        //     so the child only does async-signal-safe calls (chdir, execve).

        // argv: explicit command, or the user's login shell.
        let argv: Vec<String> = if spec.cmd.is_empty() {
            vec![default_shell()]
        } else {
            spec.cmd.clone()
        };
        let exe = resolve_path(&argv[0])?;
        let c_argv: Vec<CString> = argv
            .iter()
            .map(|a| CString::new(a.as_str()))
            .collect::<Result<_, _>>()
            .map_err(|_| io::Error::other("NUL in argv"))?;
        let mut argv_ptrs: Vec<*const libc::c_char> =
            c_argv.iter().map(|c| c.as_ptr()).collect();
        argv_ptrs.push(ptr::null());

        // envp: inherited environment + the spec's overrides.
        let c_env = build_env(&spec.env)?;
        let mut envp_ptrs: Vec<*const libc::c_char> =
            c_env.iter().map(|c| c.as_ptr()).collect();
        envp_ptrs.push(ptr::null());

        let c_cwd = match &spec.cwd {
            Some(p) => Some(CString::new(p.as_str()).map_err(|_| io::Error::other("NUL in cwd"))?),
            None => None,
        };

        let ws = libc::winsize {
            ws_row: spec.rows,
            ws_col: spec.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        let mut master: libc::c_int = -1;
        // SAFETY: forkpty allocates a pty pair and forks; `master` receives
        // the master fd in the parent. termp=NULL inherits sane defaults;
        // winp seeds the initial window size.
        let pid = unsafe {
            libc::forkpty(
                &mut master,
                ptr::null_mut(),
                ptr::null_mut(),
                &ws as *const libc::winsize as *mut libc::winsize,
            )
        };
        if pid < 0 {
            return Err(io::Error::last_os_error());
        }

        if pid == 0 {
            // --- Child. Async-signal-safe only past this point. ---
            if let Some(cwd) = &c_cwd {
                // SAFETY: chdir with a valid C string; ignore failure and
                // let the shell start in the inherited cwd rather than abort.
                unsafe { libc::chdir(cwd.as_ptr()) };
            }
            // SAFETY: execve replaces the image; argv/envp are NULL-terminated
            // arrays of valid pointers that outlive the call (in the child).
            unsafe {
                libc::execve(exe.as_ptr(), argv_ptrs.as_ptr(), envp_ptrs.as_ptr());
                // Only reached if execve failed.
                libc::_exit(127);
            }
        }

        // --- Parent. ---
        Ok(pty_shell_from_raw(master, pid))
    }
}

fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
}

/// Inherited environment with `overrides` applied (replace-or-append).
fn build_env(overrides: &[(String, String)]) -> io::Result<Vec<CString>> {
    let mut pairs: Vec<(String, String)> = std::env::vars().collect();
    for (k, v) in overrides {
        if let Some(slot) = pairs.iter_mut().find(|(ek, _)| ek == k) {
            slot.1 = v.clone();
        } else {
            pairs.push((k.clone(), v.clone()));
        }
    }
    pairs
        .into_iter()
        .map(|(k, v)| {
            CString::new(format!("{k}={v}")).map_err(|_| io::Error::other("NUL in environment"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// Read the whole pty stream to EOF (EIO-as-EOF handled in `PtyShell`).
    fn drain(shell: &mut PtyShell) -> String {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match shell.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    #[test]
    fn runs_a_command_on_a_pty() {
        let spec = SpawnSpec {
            target: Target::SessionJail,
            cmd: vec!["/bin/sh".into(), "-c".into(), "printf STOA_OK".into()],
            cols: 80,
            rows: 24,
            cwd: None,
            env: Vec::new(),
        };
        let mut shell = DirectSpawner::new().spawn(&spec).expect("spawn");
        let output = drain(&mut shell);
        assert!(output.contains("STOA_OK"), "got: {output:?}");
        let code = shell.wait().expect("wait");
        assert_eq!(code, 0);
    }

    #[test]
    fn env_override_reaches_the_child() {
        let spec = SpawnSpec {
            target: Target::SessionJail,
            cmd: vec!["/bin/sh".into(), "-c".into(), "printf %s \"$STOA_TEST\"".into()],
            cols: 80,
            rows: 24,
            cwd: None,
            env: vec![("STOA_TEST".into(), "value42".into())],
        };
        let mut shell = DirectSpawner::new().spawn(&spec).expect("spawn");
        assert!(drain(&mut shell).contains("value42"));
    }

    #[test]
    fn cwd_is_honored() {
        let spec = SpawnSpec {
            target: Target::SessionJail,
            cmd: vec!["/bin/sh".into(), "-c".into(), "pwd".into()],
            cols: 80,
            rows: 24,
            cwd: Some("/tmp".into()),
            env: Vec::new(),
        };
        let mut shell = DirectSpawner::new().spawn(&spec).expect("spawn");
        let out = drain(&mut shell);
        // macOS /tmp is a symlink to /private/tmp; accept either.
        assert!(out.contains("/tmp"), "got: {out:?}");
    }

    #[test]
    fn resize_succeeds() {
        let spec = SpawnSpec {
            target: Target::SessionJail,
            cmd: vec!["/bin/sh".into(), "-c".into(), "sleep 0.2".into()],
            cols: 80,
            rows: 24,
            cwd: None,
            env: Vec::new(),
        };
        let shell = DirectSpawner::new().spawn(&spec).expect("spawn");
        shell.resize(120, 40).expect("resize");
        let _ = shell.wait();
    }

    #[test]
    fn jail_target_is_rejected() {
        let spec = SpawnSpec {
            target: Target::Jail("org.atrium.editor".into()),
            ..SpawnSpec::login_shell(80, 24)
        };
        let err = DirectSpawner::new().spawn(&spec).expect_err("must reject");
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }
}
