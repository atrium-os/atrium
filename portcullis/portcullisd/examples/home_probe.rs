//! home_probe: a test app for scripts/vm-homes-check.sh (homes.md §8 W1).
//! Run as a jailed app, it reports one line on stderr: its uid and groups,
//! what its overlay already holds (`old-state` from before the move, its own
//! run counter, bumped here), and the person's folders at /atrium/home if
//! it was given them, trying to make a file in Documents.

use std::io::Write;

fn main() {
    let old = std::path::Path::new("/old-state").exists();
    let runs = std::fs::read_to_string("/var/probe-runs").ok().and_then(|s| s.trim().parse::<u32>().ok()).unwrap_or(0) + 1;
    let wrote = std::fs::create_dir_all("/var").and_then(|_| std::fs::write("/var/probe-runs", runs.to_string())).is_ok();
    // Who made the counter file is who runs this.
    use std::os::unix::fs::MetadataExt;
    let uid = std::fs::metadata("/var/probe-runs").map(|m| format!("uid={}", m.uid())).unwrap_or_else(|_| "uid=?".into());
    let home = match std::fs::read_dir("/atrium/home") {
        Ok(rd) => {
            let mut v: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
            v.sort();
            v.join(",")
        }
        Err(_) => "absent".into(),
    };
    let made = if home != "absent" {
        match std::fs::File::create("/atrium/home/Documents/probe.txt").and_then(|mut f| f.write_all(b"from the file manager")) {
            Ok(()) => "ok".to_string(),
            Err(e) => format!("err({})", e.raw_os_error().unwrap_or(0)),
        }
    } else {
        "-".into()
    };
    let group = std::fs::metadata("/atrium/home/Documents/probe.txt").map(|m| format!("{}", m.gid())).unwrap_or_else(|_| "-".into());
    eprintln!("home-probe: {uid} old={old} runs={runs} wrote={wrote} home=[{home}] made={made} made_gid={group}");
}
