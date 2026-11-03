//! atrium-rdna4-oracle — oracle cases for the native RDNA4 backend.
//!
//! A [`Case`] is a set of buffers placed at fixed GPU addresses, an indirect
//! buffer to run, and the expected contents of the readback buffers. The same
//! case runs on the R9700 (through `runner/amdgpu_runner.c`), on gpusim (once it
//! executes RDNA4 code), and is checked against Tier-2 where a SPIR-V source
//! exists. Results captured on the R9700 become goldens.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::fs;
use std::io;
use std::path::Path;

pub mod cases;

/// One buffer of a case.
#[derive(Debug, Clone)]
pub struct Buffer {
    /// Name, also the output file stem.
    pub name: String,
    /// GPU virtual address (page aligned).
    pub va: u64,
    /// Size in bytes.
    pub size: u64,
    /// Initial contents (zero-extended to `size`).
    pub init: Vec<u8>,
    /// Expected contents after the run, if this buffer is checked.
    pub expect: Option<Vec<u8>>,
}

/// One oracle case.
#[derive(Debug, Clone)]
pub struct Case {
    /// Case name.
    pub name: String,
    /// Buffers.
    pub buffers: Vec<Buffer>,
    /// Buffer holding the indirect buffer, byte offset and length in dwords.
    pub ib: (String, u64, u64),
    /// Engine: `compute` or `gfx`.
    pub ip: &'static str,
}

impl Case {
    /// Write the job directory the runner consumes.
    pub fn write_job(&self, dir: &Path) -> io::Result<()> {
        fs::create_dir_all(dir)?;
        let mut job = format!("# atrium-rdna4-oracle case {}\nip {}\ntimeout_ms 5000\n", self.name, self.ip);
        for b in &self.buffers {
            let init = if b.init.is_empty() {
                "-".to_string()
            } else {
                let f = format!("{}.in", b.name);
                fs::write(dir.join(&f), &b.init)?;
                f
            };
            job += &format!("bo {} {:x} {} {} {}\n", b.name, b.va, b.size, init, b.expect.is_some() as u8);
            if let Some(e) = &b.expect {
                fs::write(dir.join(format!("{}.expect", b.name)), e)?;
            }
        }
        job += &format!("ib {} {} {}\n", self.ib.0, self.ib.1, self.ib.2);
        fs::write(dir.join("job.txt"), job)
    }

    /// Compare a finished job directory against the expectations.
    pub fn check(&self, dir: &Path) -> Result<(), String> {
        let result = fs::read_to_string(dir.join("result.txt")).map_err(|e| format!("result.txt: {e}"))?;
        if !result.lines().any(|l| l == "status=ok") {
            return Err(format!("run failed:\n{result}"));
        }
        for b in &self.buffers {
            let Some(e) = &b.expect else { continue };
            let got = fs::read(dir.join(format!("{}.out", b.name))).map_err(|x| format!("{}.out: {x}", b.name))?;
            if let Some(i) = (0..e.len()).find(|&i| got.get(i) != e.get(i)) {
                return Err(format!("{}: first difference at byte {i}", b.name));
            }
        }
        Ok(())
    }
}
