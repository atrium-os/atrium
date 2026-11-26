//! Session recording for the Fenestra stream gate (fenestra.md §19, R1;
//! `fresco_render_core::record`). Only a build with the
//! `fenestra-record` feature records, and only when `FRESCO_RECORD` names
//! a file: a recording holds everything the apps drew, so production
//! builds cannot make one. Each call is made with the frontend's lock
//! held, so the recording has the frontend's own order.

#[cfg(feature = "fenestra-record")]
mod imp {
    use std::fs::File;
    use std::io::BufWriter;
    use std::sync::{Mutex, OnceLock};

    use fresco_render_core::record::Recorder;

    static REC: OnceLock<Option<Mutex<Recorder<BufWriter<File>>>>> = OnceLock::new();

    fn with(f: impl FnOnce(&mut Recorder<BufWriter<File>>) -> std::io::Result<()>) {
        let rec = REC.get_or_init(|| {
            let path = std::env::var_os("FRESCO_RECORD")?;
            // A fresh file only, never one that is there (or a link).
            let file = std::fs::OpenOptions::new().write(true).create_new(true).open(&path);
            match file.and_then(|f| Recorder::new(BufWriter::new(f))) {
                Ok(r) => {
                    eprintln!("frescod: WARNING: recording every client's scene to {}", path.to_string_lossy());
                    Some(Mutex::new(r))
                }
                Err(e) => {
                    eprintln!("frescod: FRESCO_RECORD {}: {e}; not recording", path.to_string_lossy());
                    None
                }
            }
        });
        if let Some(m) = rec {
            let mut r = m.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(e) = f(&mut r) {
                eprintln!("frescod: recording: {e}");
            }
        }
    }

    pub fn msg(client: u8, m: &aqueduct::Message) {
        with(|r| r.msg(client, m));
    }
    pub fn blob(b: &[u8]) {
        with(|r| r.blob(b));
    }
    pub fn tick(now_ns: u64) {
        with(|r| r.tick(now_ns));
    }
    /// The frame's uploads were taken; flushed, so a recording cut short
    /// ends at a draw.
    pub fn draw() {
        with(|r| r.draw().and_then(|_| r.flush()));
    }
}

#[cfg(not(feature = "fenestra-record"))]
#[allow(dead_code)]
mod imp {
    #[inline(always)]
    pub fn msg(_client: u8, _m: &aqueduct::Message) {}
    #[inline(always)]
    pub fn blob(_b: &[u8]) {}
    #[inline(always)]
    pub fn tick(_now_ns: u64) {}
    #[inline(always)]
    pub fn draw() {}
}

#[allow(unused_imports)]
pub use imp::*;
