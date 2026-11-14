//! The O2 crash-recovery gate, with a real crash: a child process types into a sheet
//! as fast as it can, its journal syncing every 50 actions, and is killed with SIGKILL
//! at an arbitrary moment. What comes back must be exactly the first N actions in
//! order (no gap, no half action), with N at least what the child last reported as
//! durable.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use atrium_sheets::doc::{self, Doc};
use officina_grid::addr::CellAddr;
use officina_grid::Value;

const CHILD: &str = "ATRIUM_SHEETS_CRASH_CHILD";

fn child(dir: &str) -> ! {
    let mut d = Doc::new("crash");
    d.autosave_to(std::path::Path::new(dir)).unwrap();
    let mut i: u32 = 0;
    loop {
        // A formula every tenth action, so replay recalculates as it goes.
        let text = if i % 10 == 9 { format!("=A{}+1", i) } else { i.to_string() };
        assert!(d.type_into(i, 0, &text));
        i += 1;
        if i % 50 == 0 {
            d.autosave().unwrap().sync().unwrap();
            println!("durable {i}");
        }
    }
}

#[test]
fn a_killed_process_comes_back_in_order() {
    if let Ok(dir) = std::env::var(CHILD) {
        child(&dir);
    }
    let me = std::env::current_exe().unwrap();
    for round in 0..5u32 {
        let dir: PathBuf = std::env::temp_dir().join(format!("atrium-sheets-kill-{}-{round}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut c = Command::new(&me)
            .args(["--exact", "a_killed_process_comes_back_in_order", "--nocapture", "--test-threads=1"])
            .env(CHILD, &dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut out = BufReader::new(c.stdout.take().unwrap());
        let mut durable = 0;
        let mut seen = 0;
        // A different moment each round.
        let after = 3 + round * 7;
        let mut line = String::new();
        while out.read_line(&mut line).unwrap() > 0 {
            if let Some(n) = line.trim().strip_prefix("durable ") {
                durable = n.parse().unwrap();
                seen += 1;
                if seen == after {
                    break;
                }
            }
            line.clear();
        }
        // SAFETY: kill(2) on our own child.
        unsafe { libc::kill(c.id() as i32, libc::SIGKILL) };
        c.wait().unwrap();
        assert!(durable > 0, "the child never got going");

        let (d, rec) = doc::recover(&dir).unwrap();
        let n = rec.actions as u32;
        assert!(n >= durable, "round {round}: {n} actions back, {durable} were durable");
        for i in 0..n {
            let want = i as f64; // the formulas compute their row too
            assert_eq!(d.wb.get(CellAddr::new(0, i, 0)), Value::Num(want), "round {round}, row {i}");
        }
        assert_eq!(d.wb.get(CellAddr::new(0, n, 0)), Value::Empty, "round {round}: nothing past action {n}");
        assert!(d.dirty);
        eprintln!("round {round}: killed after {durable} durable, {n} came back, {} torn bytes", rec.torn_bytes);
        d.close().unwrap();
        assert!(!dir.exists());
    }
}
