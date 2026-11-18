//! A test bundle's entry: Portcullis starts an app's entry with no arguments and no
//! environment of its choosing, so a bundle that should run the player on its own media
//! (scripts/hospes-player-jail-vm.sh) starts this instead. It reads `../play.conf` beside
//! its own directory and replaces itself with `atrium-player` from the same directory:
//!
//! ```text
//! env ATRIUM_PLAYER_CDM=system:com.widevine.alpha
//! arg --headless
//! arg /media/clip.mp4
//! out /tmp/player.out
//! ```
//!
//! `out` sends the player's standard output and error to that file (an app
//! launched by portcullisd has neither).
//!
//! Test only: nothing here is part of how the player is launched for real (Limen's
//! media-player role hands it what to play).
use std::os::unix::process::CommandExt;
use std::process::Command;

fn main() {
    let exe = std::env::current_exe().expect("own path");
    let dir = exe.parent().expect("own directory");
    let conf = dir.join("../play.conf");
    let text = std::fs::read_to_string(&conf).unwrap_or_else(|e| panic!("{}: {e}", conf.display()));
    let mut c = Command::new(dir.join("atrium-player"));
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        match line.split_once(' ') {
            Some(("env", kv)) => {
                let (k, v) = kv.split_once('=').unwrap_or_else(|| panic!("play.conf: {line}"));
                c.env(k, v);
            }
            Some(("arg", a)) => {
                c.arg(a);
            }
            Some(("out", path)) => {
                let f = std::fs::File::create(path).unwrap_or_else(|e| panic!("{path}: {e}"));
                c.stderr(f.try_clone().expect("dup"));
                c.stdout(f);
            }
            _ => panic!("play.conf: {line}"),
        }
    }
    let e = c.exec();
    eprintln!("bundle-launch: atrium-player: {e}");
    std::process::exit(127);
}
