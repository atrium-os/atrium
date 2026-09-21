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
use crate::reverse::Back;
use crate::session::{Expiry, Millis, SessionId, Status};
use crate::wire::*;
use std::collections::HashMap;
use std::io::BufReader;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::time::Duration;

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
}

impl Worker {
    fn request(&mut self, f: &Frame, deadline: Duration) -> Result<Frame, String> {
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
}

impl JailedHost {
    pub fn new(config: WorkerConfig, limits: crate::session::SessionLimits) -> Self {
        JailedHost { config, limits, workers: HashMap::new(), gone: vec![], next: 0 }
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
        let Some(mut w) = self.workers.remove(&id) else { return false };
        drop(w.stdin.take());
        let grace = std::time::Instant::now();
        loop {
            match w.child.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) if grace.elapsed() < self.config.shutdown_grace => {
                    std::thread::sleep(Duration::from_millis(20));
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
        std::thread::spawn(move || {
            let mut r = BufReader::new(stdout);
            loop {
                match read_frame(&mut r) {
                    Ok(f) => if tx.send(Ok(f)).is_err() { return },
                    Err(WireError::Closed) => return,
                    Err(e) => { let _ = tx.send(Err(e.to_string())); return }
                }
            }
        });

        self.next = id;
        self.workers.insert(id, Worker {
            child, stdin: Some(stdin), frames, charged, opened_at: now, last_seen: now,
            triggers: vec![],
        });

        match self.ask(id, Frame::new(REQ_OPEN, recording), now) {
            Ok(list) => {
                let triggers: Vec<String> = list.lines()
                    .filter(|l| !l.is_empty()).map(str::to_string).collect();
                if let Some(w) = self.workers.get_mut(&id) { w.triggers = triggers }
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

    fn triggers(&self, id: SessionId) -> Vec<String> {
        self.workers.get(&id).map(|w| w.triggers.clone()).unwrap_or_default()
    }

    fn report(&self) -> Vec<(SessionId, usize)> {
        let mut v: Vec<_> = self.workers.iter().map(|(&id, w)| (id, w.charged)).collect();
        v.sort_by_key(|(id, _)| *id);
        v
    }
}
