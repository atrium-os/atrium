//! Pictures decoded in the app's `convert` worker (officina.md §5.2): the
//! [`officina_text::picture::Decoder`] an app installs so that the process
//! with its window and files never decodes a picture that came from outside.
//!
//! The worker is started once (`officina-convert pictures`) and kept: one
//! jail and its start cost for the session, not one per picture. Requests on
//! its stdin, one at a time per process:
//!
//!   op (u8: 1 size, 2 decode) · target width · target height (u32 LE, 0 0 =
//!   natural size) · length (u32 LE) · the picture's bytes
//!
//! and answers on its stdout: ok (u8 1 or 0) · width · height (u32 LE) and,
//! for a decode, width × height × 4 bytes of RGBA. A worker that dies or
//! answers nonsense is dropped and the request tried once on a fresh one.
//! Several threads can ask at once (Slides decodes on a few): each takes an
//! idle process or starts one, up to [`MAX_PROCS`].

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Condvar, Mutex};

use officina_text::picture::{Decoder, Pixels};

/// Most picture processes at once.
pub const MAX_PROCS: usize = 3;
/// Largest picture sent (navigator-image's own ceiling).
pub const MAX_INPUT: usize = 16 << 20;
/// Largest decode taken back.
pub const MAX_PIXELS: u64 = 1 << 28;

pub const OP_SIZE: u8 = 1;
pub const OP_DECODE: u8 = 2;

enum Guard {
    Worker(#[allow(dead_code)] atrium_worker::Stdio),
    Child(Child),
}

struct Proc {
    tx: Box<dyn Write + Send>,
    rx: Box<dyn Read + Send>,
    _guard: Guard,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Guard::Child(c) = self {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn spawn() -> Option<Proc> {
    if std::env::var_os("OFFICINA_CONVERT").is_none() {
        if let Some(w) = atrium_worker::Workers::get().ok().filter(|w| w.has(crate::launch::WORKER)) {
            let mut p = w.spawn_stdio(crate::launch::WORKER).ok()?;
            let mut tx: Box<dyn Write + Send> = Box::new(p.stdin.take()?);
            let rx: Box<dyn Read + Send> = Box::new(p.stdout.take()?);
            if let Some(mut err) = p.stderr.take() {
                std::thread::spawn(move || { let _ = std::io::copy(&mut err, &mut std::io::sink()); });
            }
            tx.write_all(b"pictures\n").ok()?;
            return Some(Proc { tx, rx, _guard: Guard::Worker(p) });
        }
    }
    let mut c = Command::new(crate::launch::converter())
        .arg("pictures")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let tx: Box<dyn Write + Send> = Box::new(c.stdin.take()?);
    let rx: Box<dyn Read + Send> = Box::new(c.stdout.take()?);
    Some(Proc { tx, rx, _guard: Guard::Child(c) })
}

fn u32_at(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

impl Proc {
    /// One request; None when the process failed (it is then dropped), or
    /// Some(None) when it answered that it couldn't.
    fn ask(&mut self, op: u8, target: (u32, u32), bytes: &[u8]) -> Option<Option<Pixels>> {
        let mut head = vec![op];
        for v in [target.0, target.1, bytes.len() as u32] {
            head.extend_from_slice(&v.to_le_bytes());
        }
        self.tx.write_all(&head).ok()?;
        self.tx.write_all(bytes).ok()?;
        self.tx.flush().ok()?;
        let mut a = [0u8; 9];
        self.rx.read_exact(&mut a).ok()?;
        let (w, h) = (u32_at(&a[1..5]), u32_at(&a[5..9]));
        match a[0] {
            0 => return Some(None),
            1 => {}
            _ => return None,
        }
        if op == OP_SIZE {
            return Some(Some(Pixels { width: w, height: h, rgba: Vec::new() }));
        }
        let n = w as u64 * h as u64 * 4;
        if n > MAX_PIXELS {
            return None;
        }
        let mut rgba = vec![0u8; n as usize];
        self.rx.read_exact(&mut rgba).ok()?;
        Some(Some(Pixels { width: w, height: h, rgba }))
    }
}

/// The decoder that asks the `convert` worker.
pub struct Remote {
    idle: Mutex<(Vec<Proc>, usize)>,
    freed: Condvar,
}

impl Default for Remote {
    fn default() -> Self {
        Remote { idle: Mutex::new((Vec::new(), 0)), freed: Condvar::new() }
    }
}

impl Remote {
    /// An idle process, or a new one while there are fewer than MAX_PROCS.
    fn take(&self) -> Option<Proc> {
        let mut g = self.idle.lock().unwrap();
        loop {
            if let Some(p) = g.0.pop() {
                return Some(p);
            }
            if g.1 < MAX_PROCS {
                g.1 += 1;
                drop(g);
                let p = spawn();
                if p.is_none() {
                    self.idle.lock().unwrap().1 -= 1;
                    self.freed.notify_one();
                }
                return p;
            }
            g = self.freed.wait(g).unwrap();
        }
    }

    fn give(&self, p: Option<Proc>) {
        let mut g = self.idle.lock().unwrap();
        match p {
            Some(p) => g.0.push(p),
            None => g.1 -= 1,
        }
        self.freed.notify_one();
    }

    fn ask(&self, op: u8, target: (u32, u32), bytes: &[u8]) -> Option<Pixels> {
        if bytes.is_empty() || bytes.len() > MAX_INPUT {
            return None;
        }
        for _ in 0..2 {
            let mut p = self.take()?;
            match p.ask(op, target, bytes) {
                Some(got) => {
                    self.give(Some(p));
                    return got;
                }
                None => self.give(None),
            }
        }
        None
    }

    /// Processes running now (tests).
    pub fn running(&self) -> usize {
        self.idle.lock().unwrap().1
    }
}

impl Decoder for Remote {
    fn decode(&self, bytes: &[u8], target: Option<(u32, u32)>) -> Option<Pixels> {
        self.ask(OP_DECODE, target.unwrap_or((0, 0)), bytes)
    }

    fn size(&self, bytes: &[u8]) -> Option<(u32, u32)> {
        self.ask(OP_SIZE, (0, 0), bytes).map(|p| (p.width, p.height))
    }
}

/// Installs [`Remote`] as this process's picture decoder.
pub fn install() -> bool {
    officina_text::picture::install(Box::new(Remote::default()))
}
