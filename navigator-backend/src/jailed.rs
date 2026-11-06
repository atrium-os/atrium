//! A `DocumentHost` that runs each document in its own process.
//!
//! ★★ THIS IS THE MECHANISM SPEC §2 DESCRIBES: one worker per document, with
//! a pipe as its only channel. The broker holds capabilities and never
//! parses; the worker parses and holds nothing. Documents never share an
//! address space, so cross-document disclosure is structurally absent rather
//! than mitigated.
//!
//! ★★★ WHAT IS AND IS NOT CONFINED, because "it is jailed" is not a uniform
//! claim — a jail is a capability SET, and a host that said "jailed" while
//! running a bare subprocess would be the most dangerous kind of comment in
//! this tree.
//!
//! - `Confinement::None` runs the worker as an ordinary child process. It is
//!   process isolation and nothing more: separate address space, separate
//!   crash domain, no ambient capability of its own beyond what the broker's
//!   user already has. It is NOT a jail. The constructor is named
//!   `unconfined_for_testing` so choosing it is a sentence someone has to
//!   write on purpose.
//! - `Confinement::Launcher` wraps the worker in a command that confines it.
//!   This is where a jail goes.
//!
//! ★ AND THE JAIL IS NOT WIRED, for a reason worth recording rather than
//! working around: Portcullis today launches *applications* — `portcullis
//! launch <app-tree>` reads a signed `atrium.toml`, builds a jail.conf
//! section and starts a long-lived jail. It has no "run this executable
//! confined and hand me its stdin/stdout" mode, which is exactly what a
//! process-per-document host needs. That mode is a Portcullis-side change.
//! Inventing an invocation here that does not exist would produce a host that
//! claims confinement and silently provides none, which is worse than having
//! neither.
//!
//! So `Launcher` takes the command explicitly, this crate asserts nothing
//! about what any particular launcher confines, and `require_confinement`
//! exists for a caller that must not start without one.

use crate::navigatord::DocumentHost;
use crate::reverse::{Back, Forward};
use crate::session::{Expiry, Millis, SessionId, Status};
use crate::wire::*;
use std::collections::HashMap;
use std::io::BufReader;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use crate::carried::Carried;
use crate::image::{Ceilings, Pixels};

/// How a worker process is launched.
#[derive(Debug, Clone)]
pub enum Confinement {
    /// A bare child process. Process isolation only — NOT a jail.
    None,
    /// Wrap the worker in a confining command: the worker's path is appended
    /// to `program` + `args`.
    ///
    /// ★ An `{instance}` token in `args` is replaced with this session's id.
    /// A confining launcher that creates one jail per unit of work needs a
    /// DISTINCT name per spawn — `portcullis exec --instance {instance}` —
    /// and a static argument list cannot provide one. Without it every worker
    /// would ask for the same jail name, and `jail -c` on an existing name
    /// reconfigures the running jail rather than failing, so two documents
    /// would quietly share one.
    Launcher { program: String, args: Vec<String> },
}

#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub worker: String,
    pub confinement: Confinement,
    /// ★ Every request is bounded. A worker that hangs — parsing something
    /// pathological, or compromised and stalling on purpose — must not hang
    /// the broker, which is the process every other session depends on.
    pub deadline: Duration,
    /// How long a retiring worker gets to exit on end-of-input before it is
    /// killed. A launcher needs this window to tear its jail down.
    pub shutdown_grace: Duration,
}

impl WorkerConfig {
    /// ★ The only way to get an unconfined host, and it says so in its name.
    /// A default that quietly produced this would make "is it jailed?"
    /// unanswerable by reading the call site.
    pub fn unconfined_for_testing(worker: impl Into<String>) -> Self {
        WorkerConfig {
            worker: worker.into(),
            confinement: Confinement::None,
            deadline: Duration::from_secs(10),
            shutdown_grace: Duration::from_secs(5),
        }
    }

    pub fn confined(worker: impl Into<String>, program: impl Into<String>, args: Vec<String>)
        -> Self
    {
        WorkerConfig {
            worker: worker.into(),
            confinement: Confinement::Launcher { program: program.into(), args },
            deadline: Duration::from_secs(10),
            shutdown_grace: Duration::from_secs(5),
        }
    }

    /// For a caller that must refuse to start rather than run unconfined.
    /// Deployment decides this; the library only makes it askable.
    pub fn require_confinement(&self) -> Result<(), String> {
        match self.confinement {
            Confinement::Launcher { .. } => Ok(()),
            Confinement::None =>
                Err("worker confinement is None: process isolation only, not a jail".into()),
        }
    }

    pub fn is_confined(&self) -> bool {
        matches!(self.confinement, Confinement::Launcher { .. })
    }

    fn command(&self, instance: SessionId) -> Command {
        match &self.confinement {
            Confinement::None => Command::new(&self.worker),
            Confinement::Launcher { program, args } => {
                let mut c = Command::new(program);
                c.args(args.iter().map(|a| a.replace("{instance}", &instance.to_string())));
                c.arg(&self.worker);
                c
            }
        }
    }
}

struct Worker {
    child: Child,
    /// `Option` so it can be DROPPED to signal end-of-input; see `kill`.
    stdin: Option<ChildStdin>,
    frames: Receiver<Result<Frame, String>>,
    /// ★ Charged from what the BROKER measured, never from what the worker
    /// reports. A compromised worker asked for its size would answer zero,
    /// and the budget that protects the broker would be set by the thing it
    /// is protecting itself from.
    charged: usize,
    opened_at: Millis,
    last_seen: Millis,
    /// ★ Cached from the worker's answer to OPEN. The broker asks for these
    /// through a shared reference, and a worker can only be asked through a
    /// mutable one — so the first draft of this returned an empty list with
    /// an apologetic comment, which would have shown every reader a document
    /// with nothing to click and reported no error at all. The cache is the
    /// fix; the empty vec was the bug.
    triggers: Vec<String>,
    /// ★ The largest reply the reader thread will accept, set before each
    /// request. `MAX_FRAME` for everything except an image, whose reply may
    /// be as large as the budget the broker sent with it and no larger: a
    /// worker announcing a 256 MiB frame unasked is refused before the
    /// allocation, as any oversized frame is.
    reply_limit: Arc<AtomicUsize>,
    /// What the recording carries, from the bytes the broker was handed.
    carried: Carried,
    /// ★ The broker's own copy of what progressive display handed the
    /// worker (legacy-web §5.4.5.7), so that what the session CARRIES — the
    /// link and image bounds — follows the shown document from bytes the
    /// broker holds, never from the worker's word: the held scripted
    /// recording and its table, and one kept out as not different.
    staged: Option<Vec<u8>>,
    staged_table: Option<Vec<u8>>,
    /// The document generation the worker's content source was built for.
    bands_generation: Option<u64>,
    kept_out: Option<Vec<u8>>,
    /// The first document's images, as they arrive. Broker-side only: the
    /// worker's document does not change, and the worker is never asked
    /// what its recording names.
    first_images: crate::progressive::FirstImages,
}

impl Worker {
    fn request(&mut self, f: &Frame, deadline: Duration) -> Result<Frame, String> {
        self.request_limited(f, deadline, MAX_FRAME)
    }

    fn request_limited(&mut self, f: &Frame, deadline: Duration, reply: usize) -> Result<Frame, String> {
        self.reply_limit.store(reply, Ordering::SeqCst);
        let stdin = self.stdin.as_mut().ok_or("worker input is closed")?;
        write_frame(stdin, f).map_err(|e| e.to_string())?;
        match self.frames.recv_timeout(deadline) {
            Ok(Ok(frame)) => Ok(frame),
            Ok(Err(e)) => Err(e),
            Err(RecvTimeoutError::Timeout) =>
                Err(format!("worker did not answer within {deadline:?}")),
            Err(RecvTimeoutError::Disconnected) => Err("worker exited".into()),
        }
    }
}

pub struct JailedHost {
    config: WorkerConfig,
    limits: crate::session::SessionLimits,
    workers: HashMap<SessionId, Worker>,
    /// Sessions that ended, and why — so `status` can tell a reader whose
    /// worker crashed from one whose id never existed.
    gone: Vec<(SessionId, Status)>,
    next: SessionId,
    /// Where recordings and saved sessions go, if anywhere. ★ The STORE is
    /// the broker's capability, never a worker's: a worker answers its trail
    /// and replays one, and only this side reads or writes the store.
    store: Option<Box<dyn crate::store::Store>>,
    /// Each open session's recording address, in `store`.
    recordings: HashMap<SessionId, String>,
}

impl JailedHost {
    pub fn new(config: WorkerConfig, limits: crate::session::SessionLimits) -> Self {
        JailedHost { config, limits, workers: HashMap::new(), gone: vec![], next: 0, store: None, recordings: HashMap::new() }
    }

    /// With a store, so sessions can be saved and restored.
    pub fn with_store(mut self, store: Box<dyn crate::store::Store>) -> Self {
        self.store = Some(store);
        self
    }

    pub fn is_confined(&self) -> bool { self.config.is_confined() }

    /// The process id serving a session, for an operator looking at a stuck
    /// worker — and for tests that need to kill exactly their own child
    /// rather than every worker on the machine.
    pub fn worker_pid(&self, id: SessionId) -> Option<u32> {
        self.workers.get(&id).map(|w| w.child.id())
    }

    /// Stop a worker. ★ Killed, not merely dropped: dropping the pipes leaves
    /// a worker stuck mid-parse running forever, and the whole point of a
    /// deadline is that something dies at the end of it.
    /// ★★ CLOSE FIRST, KILL SECOND.
    ///
    /// SIGKILLing the child was wrong the moment the child became a
    /// *launcher*. `portcullis exec` creates a jail and tears it down when
    /// its work finishes; killed outright, its teardown never runs, and
    /// because a jail is created with `persist = true` the jail object and
    /// its mounts outlive it. Measured: one retired worker left a named,
    /// process-less jail behind, and the next session with that instance tag
    /// was refused because the husk still answered to the name.
    ///
    /// Closing stdin is end-of-input, which the worker already treats as "the
    /// host went away" and exits on. That lets the launcher finish normally.
    /// The kill stays as the backstop for a worker that ignores EOF — a
    /// compromised one will — so this is a shutdown with a deadline, not a
    /// request and a hope.
    fn kill(&mut self, id: SessionId) -> bool {
        self.recordings.remove(&id);
        let Some(mut w) = self.workers.remove(&id) else { return false };
        drop(w.stdin.take());
        let grace = std::time::Instant::now();
        // ★ Backoff from 1 ms, not a fixed 20. The broker is single-threaded,
        // so this wait stalls every other session; a fixed first sleep made
        // EVERY close cost at least 20 ms (measured: unconfined close p50 =
        // 20.0 ms, a worker that had exited within the first millisecond;
        // 1.0 ms after).
        let mut nap = Duration::from_millis(1);
        loop {
            match w.child.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) if grace.elapsed() < self.config.shutdown_grace => {
                    std::thread::sleep(nap);
                    // Capped low: a jailed teardown takes ~25-30 ms, and a
                    // doubling that ran on to 16 ms added up to 15 ms to it.
                    nap = (nap * 2).min(Duration::from_millis(5));
                }
                _ => break,
            }
        }
        let _ = w.child.kill();
        let _ = w.child.wait();
        true
    }

    /// Stop a worker AND record why, for a reader who comes back to it.
    fn retire(&mut self, id: SessionId, status: Status) {
        self.kill(id);
        self.gone.push((id, status));
        while self.gone.len() > self.limits.remember_expired { self.gone.remove(0); }
    }

    /// Ask a worker something, retiring it if the exchange fails.
    fn ask(&mut self, id: SessionId, f: Frame, now: Millis) -> Result<String, String> {
        let deadline = self.config.deadline;
        let Some(w) = self.workers.get_mut(&id) else {
            return Err("no such session".into());
        };
        w.last_seen = w.last_seen.max(now);
        match w.request(&f, deadline) {
            Ok(reply) if reply.tag == RSP_OK => Ok(reply.text()),
            // ★ An ERR is the worker doing its job: it refused something, and
            // it is still healthy. Only a broken exchange retires it.
            Ok(reply) if reply.tag == RSP_ERR => Err(reply.text()),
            Ok(other) => {
                let why = format!("worker sent an unknown reply {:?}", other.tag);
                self.retire(id, Status::Failed);
                Err(why)
            }
            Err(why) => {
                self.retire(id, Status::Failed);
                Err(why)
            }
        }
    }
}

impl DocumentHost for JailedHost {
    fn is_confined(&self) -> bool { self.config.is_confined() }

    fn open(&mut self, recording: &[u8], now: Millis) -> Result<SessionId, String> {
        if self.workers.len() >= self.limits.max_sessions {
            return Err(format!("{} sessions open, at the limit of {}",
                self.workers.len(), self.limits.max_sessions));
        }
        // ★ Charged BEFORE the worker exists, from bytes the broker holds.
        let charged = recording.len() * 2;
        let used: usize = self.workers.values().map(|w| w.charged).sum();
        let available = self.limits.max_total_bytes.saturating_sub(used);
        if charged > available {
            return Err(format!("session needs {charged} bytes, {available} left in the budget"));
        }

        let id = self.next + 1;
        let mut child = self.config.command(id)
            // ★ stderr is INHERITED, not piped. Piping it and never reading
            // it threw away every word a failing worker said — and worse,
            // a worker chatty enough to fill the pipe buffer would block
            // forever on a write nobody was draining, which reads from the
            // broker's side as a hang with no explanation. The worker's
            // stderr is diagnostics for whoever is running the broker, so it
            // goes where the broker's own stderr goes.
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("cannot start worker {:?}: {e}", self.config.worker))?;
        let stdin = child.stdin.take().ok_or("worker has no stdin")?;
        let stdout = child.stdout.take().ok_or("worker has no stdout")?;

        // ★ A reader THREAD, because a pipe has no read timeout. Without one,
        // "bound every request" would be a comment rather than a mechanism:
        // the broker would block in `read` with no way back out.
        let (tx, frames) = channel();
        let reply_limit = Arc::new(AtomicUsize::new(MAX_FRAME));
        let limit = reply_limit.clone();
        std::thread::spawn(move || {
            let mut r = BufReader::new(stdout);
            loop {
                match read_frame_limited(&mut r, limit.load(Ordering::SeqCst)) {
                    Ok(f) => if tx.send(Ok(f)).is_err() { return },
                    Err(WireError::Closed) => return,
                    Err(e) => { let _ = tx.send(Err(e.to_string())); return }
                }
            }
        });

        self.next = id;
        self.workers.insert(id, Worker {
            child, stdin: Some(stdin), frames, charged, opened_at: now, last_seen: now,
            triggers: vec![], reply_limit, carried: Carried::new(recording),
            staged: None, staged_table: None, kept_out: None, bands_generation: None,
            first_images: crate::progressive::FirstImages::opened(recording),
        });

        match self.ask(id, Frame::new(REQ_OPEN, recording), now) {
            Ok(list) => {
                let triggers: Vec<String> = list.lines()
                    .filter(|l| !l.is_empty()).map(str::to_string).collect();
                if let Some(w) = self.workers.get_mut(&id) { w.triggers = triggers }
                // ★ Stored at OPEN, from the bytes the broker holds, as the
                // in-process host does: at save time they are gone.
                if let Some(store) = &mut self.store {
                    match store.put(recording) {
                        Ok(a) => { self.recordings.insert(id, a); }
                        Err(why) => { self.kill(id); return Err(format!("could not store the recording: {why}")) }
                    }
                }
                Ok(id)
            }
            Err(why) => {
                // ★ The worker refused the document, so no session was ever
                // created — `kill`, not `retire`. Recording a tombstone here
                // would let a failed open evict a real expiry from the
                // bounded memory of why sessions ended.
                self.kill(id);
                Err(why)
            }
        }
    }

    fn navigate(&mut self, id: SessionId, trigger: &str, now: Millis) -> Result<(), String> {
        self.ask(id, Frame::new(REQ_NAVIGATE, trigger.as_bytes()), now).map(|_| ())
    }

    /// ★ The worker answers its TRAIL, which the broker parses strictly (it
    /// is a possibly compromised process talking) and completes with the
    /// recording's address, which only the broker knows. A lying worker can
    /// at worst save a trail that will not replay: restore checks every step
    /// and the final digest, and refuses.
    fn save(&mut self, id: SessionId) -> Result<String, String> {
        if self.store.is_none() { return Err("this host has no store to save sessions to".into()) }
        let recording = self.recordings.get(&id).cloned().ok_or("this session's recording was not stored")?;
        let reply = self.ask(id, Frame::new(REQ_SAVE, ""), 0)?;
        let trail = crate::session::Trail::from_json(reply.as_bytes())
            .map_err(|e| format!("worker sent a bad trail: {e}"))?;
        let saved = trail.into_saved(recording);
        self.store.as_mut().expect("checked").put(&saved.to_json())
    }

    /// Open a fresh worker on the stored recording, then hand it the trail
    /// to replay. Any failure closes that worker: a half-restored session is
    /// never handed to a reader.
    fn restore(&mut self, saved: &str, now: Millis) -> Result<SessionId, String> {
        let store = self.store.as_ref().ok_or("this host has no store to restore sessions from")?;
        let record = crate::session::Saved::from_json(&store.get(saved)?)?;
        let bytes = store.get(&record.recording)?;
        let id = self.open(&bytes, now)?;
        match self.ask(id, Frame::new(REQ_RESTORE, record.trail().to_json()), now) {
            Ok(_) => Ok(id),
            Err(why) => { self.kill(id); Err(why) }
        }
    }

    fn forward(&mut self, id: SessionId, now: Millis) -> Result<Forward, String> {
        match self.ask(id, Frame::new(REQ_FORWARD, ""), now)?.as_str() {
            "stepped" => Ok(Forward::Stepped),
            "at-end" => Ok(Forward::AtEnd),
            other => {
                let why = format!("worker sent an unknown forward result {other:?}");
                self.retire(id, Status::Failed);
                Err(why)
            }
        }
    }

    fn back(&mut self, id: SessionId, now: Millis) -> Result<Back, String> {
        match self.ask(id, Frame::new(REQ_BACK, ""), now)?.as_str() {
            "stepped" => Ok(Back::Stepped),
            "at-start" => Ok(Back::AtStart),
            "forgotten" => Ok(Back::Forgotten),
            // ★ Untrusted input again: the worker could answer anything. An
            // unrecognised answer is a failure, not a default.
            other => {
                let why = format!("worker sent an unknown rewind result {other:?}");
                self.retire(id, Status::Failed);
                Err(why)
            }
        }
    }

    /// A deliberate close leaves no tombstone: the caller knows it closed
    /// this, and `Unknown` is the right answer to a later request for it.
    fn close(&mut self, id: SessionId) -> bool { self.kill(id) }

    fn expire(&mut self, now: Millis) -> Vec<(SessionId, Expiry)> {
        let limits = self.limits;
        let due: Vec<(SessionId, Expiry)> = self.workers.iter().filter_map(|(&id, w)| {
            if now.saturating_sub(w.opened_at) >= limits.max_age_ms { Some((id, Expiry::Age)) }
            else if now.saturating_sub(w.last_seen) >= limits.max_idle_ms { Some((id, Expiry::Idle)) }
            else { None }
        }).collect();
        for &(id, why) in &due { self.retire(id, Status::Expired(why)) }
        due
    }

    fn status(&self, id: SessionId) -> Status {
        if self.workers.contains_key(&id) { return Status::Open }
        self.gone.iter().find(|(g, _)| *g == id).map(|(_, s)| *s).unwrap_or(Status::Unknown)
    }

    fn bytes(&self) -> usize { self.workers.values().map(|w| w.charged).sum() }
    fn open_count(&self) -> usize { self.workers.len() }

    fn carried(&self, id: SessionId) -> Option<&Carried> { self.workers.get(&id).map(|w| &w.carried) }

    /// ★ Decoded IN THE WORKER, the process that already holds this
    /// document's hostile bytes, and validated here on the way back. A reply
    /// of the wrong shape is a lie, and a worker that lies is retired — the
    /// same rule as every other answer it gives.
    fn decode_image(&mut self, id: SessionId, address: &str, bytes: &[u8], c: Ceilings, now: Millis)
        -> Result<Pixels, String>
    {
        let deadline = self.config.deadline;
        let w = self.workers.get_mut(&id).ok_or("no such session")?;
        w.last_seen = w.last_seen.max(now);
        let (tw, th) = c.target.unwrap_or((0, 0));
        let mut payload = format!("{address} {} {tw} {th}\n", c.budget).into_bytes();
        payload.extend_from_slice(bytes);
        let header = 64;
        let reply = match w.request_limited(&Frame::new(REQ_IMAGE, payload), deadline, c.budget + header) {
            Ok(r) => r,
            Err(why) => { self.retire(id, Status::Failed); return Err(why) }
        };
        if reply.tag == RSP_ERR { return Err(reply.text()) }
        let parsed = if reply.tag == RSP_OK { parse_pixels(reply.payload, c) }
                     else { Err(format!("worker sent an unknown reply {:?}", reply.tag)) };
        match parsed {
            Ok(p) => Ok(p),
            Err(why) => { self.retire(id, Status::Failed); Err(format!("worker sent a bad image: {why}")) }
        }
    }

    /// The content source (`display::Bands`), asked of the worker; an answer
    /// of the wrong shape retires it, as a bad scene does.
    fn bands(&mut self, id: SessionId, ask: crate::display::BandsAsk, initial: bool, generation: u64,
             cached: &mut dyn FnMut() -> Option<Vec<u8>>, now: Millis) -> Result<crate::display::BandsAnswer, String>
    {
        let deadline = self.config.deadline;
        let w = self.workers.get_mut(&id).ok_or("no such session")?;
        w.last_seen = w.last_seen.max(now);
        // The stored document goes only to a worker building its source.
        let c = if w.bands_generation != Some(generation) && initial { cached() } else { None };
        let c = c.unwrap_or_default();
        let a = match ask {
            crate::display::BandsAsk::Want { range, zoom_64, prefetch } => format!("want {} {} {zoom_64} {prefetch}", range.0, range.1),
            crate::display::BandsAsk::Idle { bands } => format!("idle {bands} 0 0 0"),
        };
        let mut payload = format!("{} {generation} {} {a}\n", initial as u8, c.len()).into_bytes();
        payload.extend_from_slice(&c);
        let reply = match w.request_limited(&Frame::new(REQ_BANDS, payload), deadline, MAX_FRAME) {
            Ok(r) => r,
            Err(why) => { self.retire(id, Status::Failed); return Err(why) }
        };
        if reply.tag == RSP_ERR { return Err(reply.text()) }
        let parsed = if reply.tag == RSP_OK { crate::display::decode_answer(&reply.payload) }
                     else { Err(format!("worker sent an unknown reply {:?}", reply.tag)) };
        match parsed.and_then(|a| crate::navigatord::check_bands(&a).map(|_| a)) {
            Ok(a) => {
                if let Some(w) = self.workers.get_mut(&id) { w.bands_generation = Some(generation) }
                Ok(a)
            }
            Err(why) => { self.retire(id, Status::Failed); Err(format!("worker sent bad bands: {why}")) }
        }
    }

    /// The first screen (`display`), asked of the worker; an answer of the
    /// wrong shape retires it, as a bad image does.
    fn scene(&mut self, id: SessionId, cached: Option<&[u8]>, initial: bool, limit_px: u32, now: Millis)
        -> Result<crate::display::SceneAnswer, String>
    {
        let deadline = self.config.deadline;
        let w = self.workers.get_mut(&id).ok_or("no such session")?;
        w.last_seen = w.last_seen.max(now);
        let c = cached.unwrap_or(&[]);
        let mut payload = format!("{} {limit_px} {}\n", initial as u8, c.len()).into_bytes();
        payload.extend_from_slice(c);
        let reply = match w.request_limited(&Frame::new(REQ_SCENE, payload), deadline, MAX_FRAME) {
            Ok(r) => r,
            Err(why) => { self.retire(id, Status::Failed); return Err(why) }
        };
        if reply.tag == RSP_ERR { return Err(reply.text()) }
        let parsed = if reply.tag == RSP_OK { parse_scene(reply.payload) }
                     else { Err(format!("worker sent an unknown reply {:?}", reply.tag)) };
        match parsed.and_then(|a| crate::navigatord::check_scene(&a).map(|_| a)) {
            Ok(a) => Ok(a),
            Err(why) => { self.retire(id, Status::Failed); Err(format!("worker sent a bad scene: {why}")) }
        }
    }

    /// ★ Asked of the worker, whose answer is believed only as far as the
    /// broker's own bound allows (`Navigatord::carries_link` checks the
    /// recording first). An answer other than yes or no is a lie.
    fn links_to(&mut self, id: SessionId, href: &str, now: Millis) -> Result<bool, String> {
        match self.ask(id, Frame::new(REQ_HREF, href.as_bytes()), now)?.as_str() {
            "yes" => Ok(true),
            "no" => Ok(false),
            other => {
                let why = format!("worker sent an unknown link answer {other:?}");
                self.retire(id, Status::Failed);
                Err(why)
            }
        }
    }

    fn triggers(&self, id: SessionId) -> Vec<String> {
        self.workers.get(&id).map(|w| w.triggers.clone()).unwrap_or_default()
    }

    fn upgrade(&mut self, id: SessionId, recording: &[u8], now: Millis) -> Result<crate::progressive::Upgrade, String> {
        use crate::progressive::{Difference, Upgrade};
        // ★ Charged before the worker sees it, as `open` charges: the held
        // document will replace this session's, so the budget is checked
        // with this session's own charge given back.
        let mine = self.workers.get(&id).map(|w| w.charged).ok_or("no such session")?;
        let used: usize = self.workers.values().map(|w| w.charged).sum::<usize>() - mine;
        let need = recording.len() * 2;
        if need > self.limits.max_total_bytes.saturating_sub(used) {
            return Err(format!("the scripted document needs {need} bytes, over the budget"));
        }
        let reply = self.ask(id, Frame::new(REQ_UPGRADE, recording), now)?;
        // The difference itself stays in the worker, which parsed both; the
        // broker hears the decision and a sentence for a person.
        let d = |why: &str| Difference { words_old: 0, words_new: 0, words_changed: 0, images_changed: 0,
                                         meaningful: false, said: None }.with_why(why);
        let w = self.workers.get_mut(&id).ok_or("no such session")?;
        if let Some(why) = reply.strip_prefix("ready ") { w.staged = Some(recording.to_vec()); return Ok(Upgrade::Ready(d(why))) }
        if let Some(why) = reply.strip_prefix("kept ") { w.kept_out = Some(recording.to_vec()); return Ok(Upgrade::Kept(d(why))) }
        let why = format!("worker sent an unknown upgrade result {:?}", reply.chars().take(40).collect::<String>());
        self.retire(id, Status::Failed);
        Err(why)
    }

    fn swap(&mut self, id: SessionId, anchor: &crate::progressive::Anchor, now: Millis) -> Result<crate::progressive::Mapped, String> {
        use crate::progressive::{Anchor, How, Mapped};
        if anchor.path.contains('\n') || anchor.path.len() > crate::links::MAX_HREF {
            return Err("an anchor path must be one line and bounded".into());
        }
        if self.workers.get(&id).ok_or("no such session")?.staged.is_none() {
            return Err("there is no scripted document to swap in".into());
        }
        let reply = self.ask(id, Frame::new(REQ_SWAP, format!("{}\n{}", anchor.offset, anchor.path)), now)?;
        // ★ The answer is untrusted: a shape that does not parse retires the
        // worker. A path that parses but is wrong can only misplace the
        // reader in their own document.
        let mut lines = reply.lines();
        let parsed = (|| {
            let (how, offset) = lines.next()?.split_once(' ')?;
            let how = match how { "same" => How::Same, "nearest" => How::Nearest, "top" => How::Top, _ => return None };
            let offset: i64 = offset.parse().ok()?;
            let path = lines.next()?.to_string();
            (path.len() <= crate::links::MAX_HREF).then_some(Mapped { anchor: Anchor { path, offset }, how })
        })();
        let Some(mapped) = parsed else {
            self.retire(id, Status::Failed);
            return Err("worker sent a malformed swap answer".into());
        };
        let triggers: Vec<String> = lines.filter(|l| !l.is_empty()).map(str::to_string).collect();
        let w = self.workers.get_mut(&id).expect("asked");
        let rec = w.staged.take().expect("checked");
        let mut c = Carried::new(&rec);
        let table = w.staged_table.take();
        if let Some(t) = &table { c.extend(t) }
        w.carried = c;
        w.charged = rec.len() * 2;
        w.triggers = triggers;
        w.first_images.retire();
        if let Some(store) = &mut self.store {
            let whole = match &table { Some(t) => crate::progressive::compose(&rec, t)?, None => rec };
            let a = store.put(&whole).map_err(|e| format!("could not store the recording: {e}"))?;
            self.recordings.insert(id, a);
        }
        Ok(mapped)
    }

    fn attach(&mut self, id: SessionId, transitions: &[u8], now: Millis) -> Result<crate::progressive::Attached, String> {
        use crate::progressive::Attached;
        let reply = self.ask(id, Frame::new(REQ_ATTACH, transitions), now)?;
        let mut lines = reply.lines();
        let head = lines.next().unwrap_or("");
        let w = self.workers.get_mut(&id).ok_or("no such session")?;
        // ★ Each answer must be one the broker's own copy allows: a worker
        // that claims a table went with a swap it never staged is lying.
        let ok = match head {
            "now" => {
                w.carried.extend(transitions);
                w.triggers = lines.filter(|l| !l.is_empty()).map(str::to_string).collect();
                Some(Attached::Now)
            }
            "with-swap" if w.staged.is_some() && w.staged_table.is_none() => {
                w.staged_table = Some(transitions.to_vec());
                Some(Attached::WithSwap)
            }
            "needs-swap" if w.kept_out.is_some() && w.staged.is_none() => {
                w.staged = w.kept_out.take();
                w.staged_table = Some(transitions.to_vec());
                Some(Attached::NeedsSwap)
            }
            _ => None,
        };
        match ok {
            Some(Attached::Now) => {
                if let (Some(store), Some(addr)) = (&mut self.store, self.recordings.get(&id).cloned()) {
                    let whole = crate::progressive::compose(&store.get(&addr)?, transitions)?;
                    let a = store.put(&whole).map_err(|e| format!("could not store the recording: {e}"))?;
                    self.recordings.insert(id, a);
                }
                Ok(Attached::Now)
            }
            Some(a) => Ok(a),
            None => {
                let why = format!("worker sent an attach result the broker cannot accept: {:?}", head.chars().take(40).collect::<String>());
                self.retire(id, Status::Failed);
                Err(why)
            }
        }
    }

    fn images(&mut self, id: SessionId, images: &[u8], now: Millis) -> Result<crate::progressive::ImagesAttached, String> {
        let used: usize = self.workers.values().map(|w| w.charged).sum();
        let w = self.workers.get_mut(&id).ok_or("no such session")?;
        // ★ JSON only, the converter's envelope and the images block, and
        // the merged recording's bounds: the broker parses no HTML here, as
        // nowhere (the document is unchanged, and stays the worker's).
        let a = w.first_images.attach(images, &crate::Limits::default())?;
        // ★ Charged as `open` charges, with this session's own charge given
        // back: the merged recording replaces the first one.
        let need = a.merged.len() * 2;
        if need > self.limits.max_total_bytes.saturating_sub(used - w.charged) {
            return Err(format!("the document with its images needs {need} bytes, over the budget"));
        }
        w.charged = need;
        w.carried = Carried::new(&a.merged);
        w.last_seen = now;
        if let Some(store) = &mut self.store {
            let addr = store.put(&a.merged).map_err(|e| format!("could not store the recording: {e}"))?;
            self.recordings.insert(id, addr);
        }
        Ok(a)
    }

    fn document_url(&mut self, id: SessionId, now: Millis) -> Result<String, String> {
        self.ask(id, Frame::new(REQ_URL, &b""[..]), now)
    }

    fn report(&self) -> Vec<(SessionId, usize)> {
        let mut v: Vec<_> = self.workers.iter().map(|(&id, w)| (id, w.charged)).collect();
        v.sort_by_key(|(id, _)| *id);
        v
    }
}

/// `<normalized_len>\n`, that many bytes, then NSG text: digits only, the
/// length within its bound and the payload, the rest UTF-8.
fn parse_scene(p: Vec<u8>) -> Result<crate::display::SceneAnswer, String> {
    let nl = p.iter().position(|&b| b == b'\n').filter(|&i| i <= 12).ok_or("no length line")?;
    let head = std::str::from_utf8(&p[..nl]).map_err(|_| "length is not text")?;
    if head.is_empty() || !head.bytes().all(|b| b.is_ascii_digit()) { return Err("length is not a number".into()) }
    let n: usize = head.parse().map_err(|_| "length is not a number")?;
    if n > crate::navigatord::MAX_NORMALIZED_BYTES || nl + 1 + n > p.len() { return Err("normalized length out of bounds".into()) }
    let normalized = (n > 0).then(|| p[nl + 1..nl + 1 + n].to_vec());
    let nsg = String::from_utf8(p[nl + 1 + n..].to_vec()).map_err(|_| "scene is not UTF-8")?;
    Ok(crate::display::SceneAnswer { nsg, normalized })
}

/// A worker's pixel reply: `<width> <height>\n` and exactly the RGBA8 bytes
/// those imply, within the ceilings the request carried.
pub fn parse_pixels(mut payload: Vec<u8>, c: Ceilings) -> Result<Pixels, String> {
    let nl = payload.iter().take(32).position(|&b| b == b'\n').ok_or("no header")?;
    let head = std::str::from_utf8(&payload[..nl]).map_err(|_| "header is not text")?;
    let (w, h) = head.split_once(' ').ok_or("header has no height")?;
    let parse = |v: &str| -> Result<u32, String> {
        // Digits only: `u32::from_str` takes a leading `+`, and a header
        // has one spelling.
        if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) { return Err(format!("{v:?} is not a dimension")) }
        v.parse().map_err(|_| format!("{v:?} is not a dimension"))
    };
    let (width, height) = (parse(w)?, parse(h)?);
    crate::image::validate(width, height, payload.len() - nl - 1, c)?;
    // In place: a copy would hold the frame twice.
    payload.drain(..=nl);
    Ok(Pixels { width, height, rgba: payload })
}
