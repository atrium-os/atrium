//! Workbook timing probe for a private test volume. Arguments: path, MiB, seed,
//! and an optional mode. The default measures Open and one Save of a fresh CSV.
//! `published-pair` times two saves on one grant. `publish-only` prepares the
//! fixture for a later `open-only`, which first touches the file inside the timer.
use atrium_sheets::doc::Doc;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let [_, path, mib, seed, rest @ ..] = args.as_slice() else {
        return Err("expected a CSV path, size in MiB, seed and optional mode".into());
    };
    let mode = match rest {
        [] => "fresh",
        [mode]
            if matches!(
                mode.as_str(),
                "published-pair" | "publish-only" | "open-only"
            ) =>
        {
            mode
        }
        _ => return Err("invalid probe mode".into()),
    };
    let path = std::path::absolute(path)?;
    let rows = mib
        .parse::<usize>()?
        .checked_mul(1024)
        .ok_or("size overflow")?;
    let mut state = seed.parse::<u64>()?;
    assert!(state > 0 && rows > 0);
    // Each row is 1,020 letters and one number, with comma and CRLF.
    // Distinct text keeps the file from collapsing into repeated blocks.
    let mut expected = Vec::with_capacity(rows * 1024);
    for _ in 0..rows {
        expected.extend_from_slice(b"1,");
        for _ in 0..1020 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            expected.push(b'a' + (state % 26) as u8);
        }
        expected.extend_from_slice(b"\r\n");
    }
    if matches!(mode, "published-pair" | "publish-only") {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or("missing CSV name")?;
        let snapshot = File::from(limen::fdpass::sealed_file(name, &expected)?);
        let (_, chosen) = scrinium::fs::publish_workbook(path.parent().unwrap(), name, &snapshot)?;
        assert_eq!(chosen, name);
    } else if mode == "fresh" {
        let mut original = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)?;
        original.write_all(&expected)?;
        original.sync_all()?;
    }
    if mode != "open-only" {
        assert_eq!(fs::read(&path)?, expected);
    }
    if mode == "publish-only" {
        return Ok(());
    }

    let start = Instant::now();
    let mut file = File::options().read(true).write(true).open(&path)?;
    let token = scrinium::workbook::grant(&path, &file)?;
    let granted = start.elapsed();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let loaded = start.elapsed();
    let (mut doc, _) = Doc::from_csv("Book.csv", &bytes)?;
    let opened = start.elapsed();
    if mode == "open-only" {
        assert_eq!(bytes, expected);
        assert_eq!(doc.write_csv(Vec::new())?, expected);
        println!(
            "{mib} {seed} {} {} {} {}",
            granted.as_micros(),
            (loaded - granted).as_micros(),
            (opened - loaded).as_micros(),
            opened.as_micros()
        );
        return Ok(());
    }
    let revisions = if mode == "published-pair" {
        &b"23"[..]
    } else {
        &b"2"[..]
    };
    let mut times = vec![granted.as_micros(), opened.as_micros()];
    for &value in revisions {
        assert!(doc.type_into(0, 0, &char::from(value).to_string()));
        let start = Instant::now();
        let encoded = doc.write_csv(Vec::new())?;
        let snapshot = File::from(limen::fdpass::sealed_file("Book.csv", &encoded)?);
        let staged = start.elapsed();
        scrinium::workbook::commit(token, &snapshot)?;
        let saved = start.elapsed();
        expected[0] = value;
        assert_eq!(fs::read(&path)?, expected);
        times.extend([(saved - staged).as_micros(), saved.as_micros()]);
    }
    print!("{mib} {seed}");
    for t in times {
        print!(" {t}");
    }
    println!();
    Ok(())
}
