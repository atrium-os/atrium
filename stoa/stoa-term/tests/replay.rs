//! Replay recorded terminal-program sessions (dev-tools-plan.md M1) and
//! compare the screen with the reference recorded alongside them.
//!
//! `scripts/terminal-record.sh` ran each program (vi, vim, Neovim, Helix,
//! `less`, `top`) in a tmux pane of a fixed size as xterm-256color and kept
//! every byte it wrote (`<case>.out`), the screen tmux showed at the end
//! (`<case>.screen`) and `cols rows cursor_x cursor_y alternate_on`
//! (`<case>.meta`). Feeding the bytes into a `Terminal` of that size must
//! give the same text, cursor and screen.

use std::path::{Path, PathBuf};

use stoa_term::{mode, Terminal};

fn assets() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-assets/terminal")
}

fn cases() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(assets())
        .expect("test-assets/terminal")
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str()?.strip_suffix(".out").map(str::to_string))
        .collect();
    v.sort();
    v
}

/// The differences between our screen and the reference, if any.
fn check(name: &str) -> Result<(), String> {
    let dir = assets();
    let out = std::fs::read(dir.join(format!("{name}.out"))).map_err(|e| e.to_string())?;
    let screen = std::fs::read_to_string(dir.join(format!("{name}.screen"))).map_err(|e| e.to_string())?;
    let meta = std::fs::read_to_string(dir.join(format!("{name}.meta"))).map_err(|e| e.to_string())?;
    let m: Vec<u16> = meta.split_whitespace().map(|v| v.parse().unwrap()).collect();
    let (cols, rows, cx, cy, alt) = (m[0], m[1], m[2], m[3], m[4] != 0);

    let mut t = Terminal::new(cols, rows);
    t.feed(&out);
    let got = t.rows_text();
    let want: Vec<String> = screen.lines().map(|l| l.trim_end().to_string())
        .chain(std::iter::repeat(String::new())).take(rows as usize).collect();
    let mut diffs = Vec::new();
    for (r, (g, w)) in got.iter().zip(&want).enumerate() {
        if g != w {
            diffs.push(format!("  row {r:2}: got  {g:?}\n          want {w:?}"));
        }
    }
    // tmux shows a cursor parked past the last column (a wrap pending) at
    // column `cols`; ours stays on the last column with the wrap pending.
    let cx = cx.min(cols - 1);
    if t.grid().cursor() != (cy, cx) {
        diffs.push(format!("  cursor: got {:?}, want ({cy}, {cx})", t.grid().cursor()));
    }
    if t.grid().mode(mode::ALT_SCREEN) != alt {
        diffs.push(format!("  alternate screen: got {}, want {alt}", t.grid().mode(mode::ALT_SCREEN)));
    }
    if diffs.is_empty() { Ok(()) } else { Err(format!("{name}:\n{}", diffs.join("\n"))) }
}

#[test]
fn recorded_sessions_match_the_reference() {
    let all = cases();
    assert!(!all.is_empty(), "no recordings in test-assets/terminal");
    let failures: Vec<String> = all.iter().filter_map(|c| check(c).err()).collect();
    for c in &all {
        eprintln!("{c}: {}", if failures.iter().any(|f| f.starts_with(&format!("{c}:"))) { "DIFFERS" } else { "ok" });
    }
    assert!(failures.is_empty(), "{} of {} differ:\n{}", failures.len(), all.len(), failures.join("\n"));
}
