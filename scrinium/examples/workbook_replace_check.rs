//! Compare a direct staged replacement with Scrinium's production Save.
//! Run on a disposable volume with two different, similarly sized revisions.
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

const BLOCK: usize = 64 * 1024;

fn digest(path: &Path) -> io::Result<blake3::Hash> {
    let mut file = File::open(path)?;
    let mut buffer = vec![0; BLOCK];
    let mut hash = blake3::Hasher::new();
    let mut offset = 0;
    loop {
        let n = file.read(&mut buffer).map_err(|err| {
            io::Error::other(format!("read at {offset} in {}: {err}", path.display()))
        })?;
        if n == 0 {
            return Ok(hash.finalize());
        }
        hash.update(&buffer[..n]);
        offset += n;
    }
}

fn direct(path: &Path, mut source: &File) -> io::Result<()> {
    let folder = path.parent().unwrap();
    let stage = folder.join("direct-stage");
    fs::create_dir(&stage)?;
    let temporary = stage.join("snapshot");
    let mut file = File::create_new(&temporary)?;
    let mut buffer = vec![0; BLOCK];
    loop {
        let n = source.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        file.write_all(&buffer[..n])?;
    }
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    File::open(folder)?.sync_all()?;
    File::open(&stage)?.sync_all()?;
    fs::remove_dir(stage)?;
    file.sync_all()?;
    File::open(folder)?.sync_all()
}

fn verify(path: &Path, expected: blake3::Hash, round: u32, phase: &str) -> bool {
    match digest(path) {
        Ok(actual) if actual == expected => {
            println!(
                "PASS round={round} phase={phase} {} {actual}",
                path.display()
            );
            true
        }
        result => {
            eprintln!(
                "FAIL round={round} phase={phase} {} expected {expected}, got {result:?}",
                path.display()
            );
            false
        }
    }
}

fn scan_count() -> io::Result<u64> {
    let out = Command::new("sysctl")
        .args(["-n", "kern.tessera.pinscan_swaps"])
        .output()?;
    if !out.status.success() {
        return Err(io::Error::other("Tessera pinscan counter unavailable"));
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .map_err(io::Error::other)
}

fn churn(folder: &Path) -> io::Result<()> {
    let payload = vec![0; BLOCK];
    for i in 0u32..256 {
        let stage = folder.join("churn");
        let mut file = File::create_new(&stage)?;
        file.write_all(&i.to_le_bytes())?;
        file.write_all(&payload)?;
        file.sync_all()?;
        fs::remove_file(stage)?;
    }
    File::open(folder)?.sync_all()
}

fn main() -> io::Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let [folder, base, edited] = args.as_slice() else {
        return Err(io::Error::other(
            "usage: workbook_replace_check NEW_TESSERA_DIR BASE EDITED",
        ));
    };
    let folder = Path::new(folder);
    let base = Path::new(base);
    let edited = Path::new(edited);
    let wanted = digest(edited)?;
    if wanted == digest(base)? || fs::metadata(base)?.len() != fs::metadata(edited)?.len() {
        return Err(io::Error::other("use distinct revisions of equal length"));
    }
    fs::create_dir(folder)?;
    let arm_direct = folder.join("direct.osheet");
    let arm_scrinium = folder.join("scrinium.osheet");
    fs::copy(base, &arm_direct)?;
    fs::copy(base, &arm_scrinium)?;
    let mut complete = true;
    for round in 1..=3 {
        let (source, expected) = if round % 2 == 0 {
            (base, digest(base)?)
        } else {
            (edited, wanted)
        };
        // Hold both previous descriptors through the comparison, as the
        // workbook grant retains the previous published vnode for history.
        let previous = File::open(&arm_direct)?;
        let old = File::options().read(true).write(true).open(&arm_scrinium)?;
        let grant = scrinium::workbook::grant(&arm_scrinium, &old)?;
        direct(&arm_direct, &File::open(source)?)?;
        scrinium::workbook::commit(grant, &File::open(source)?)?;
        complete &= verify(&arm_direct, expected, round, "immediate");
        complete &= verify(&arm_scrinium, expected, round, "immediate");
        let before = scan_count()?;
        churn(folder)?;
        thread::sleep(Duration::from_secs(15));
        let after = scan_count()?;
        println!("round={round} pinscan_swaps before={before} after={after}");
        complete &= after > before;
        complete &= verify(&arm_scrinium, expected, round, "delayed");
        complete &= verify(&arm_direct, expected, round, "delayed");
        drop(previous);
    }
    if !complete {
        return Err(io::Error::other("incomplete or incorrect delayed read"));
    }
    Ok(())
}
