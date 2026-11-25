//! A Debug Adapter Protocol client (dev-tools.md §6, artifex.md §4; plan
//! M12): the adapter (`lldb-dap`) runs in the workspace, started with
//! `WorkspaceExec`, and debugs the program there with the workspace's own
//! reach (its jail's `develop` class allows ptrace of its own processes,
//! plan M0). Framing is LSP's; this module is the protocol only, like
//! [`crate::lsp`].
//!
//! Paths: breakpoints and stack frames name files by the workspace's own
//! `~/work` paths; they are rewritten to and from Artifex's `/work/<name>`,
//! and a frame outside `work` (libc, the toolchain) has no file to show.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::lsp::{frame, Decoder};

/// The adapter, as run in the workspace.
pub const ADAPTER: [&str; 1] = ["lldb-dap"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub id: i64,
    pub name: String,
    /// In `work`, when it is.
    pub file: Option<PathBuf>,
    /// 1-based, as DAP counts by default.
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variable {
    pub name: String,
    pub value: String,
    pub kind: Option<String>,
    /// Its members' handle (a struct, an array, a `Vec`): 0 for none.
    pub reference: i64,
}

/// A breakpoint as sent: its line (1-based) and, when it has one, the
/// condition it stops on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bp {
    pub line: usize,
    pub condition: Option<String>,
}

/// How long lldb waits for each gdb-remote reply, the first one included,
/// in seconds. The first is answered only once portcullisd has made the
/// app's jail and lldb-server has started the app in it, which takes
/// longer than lldb's default on a slow guest; a timed-out handshake drops
/// the connection.
pub const HANDSHAKE_S: u32 = 60;

/// How long lldb-dap waits for an attached program to stop, in seconds.
pub const ATTACH_TIMEOUT_S: u32 = 120;

/// The most members kept for one variable, and the most variables expanded.
pub const MAX_MEMBERS: usize = 200;
pub const MAX_EXPANDED: usize = 64;
/// The most evaluations kept, and the longest expression sent.
pub const MAX_EVALS: usize = 50;
pub const MAX_EXPR: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Run {
    /// Started, not yet running the program.
    #[default]
    Starting,
    Running,
    /// Stopped: why (breakpoint, step, exception, pause).
    Stopped(String),
    Exited(Option<i64>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The state changed (stopped, running, exited).
    State,
    /// The program or the adapter printed something.
    Output(String),
    /// The stack of the stopped thread is in [`Session::frames`].
    Frames,
    /// The shown frame's locals are in [`Session::locals`], or a
    /// variable's members in [`Session::members`].
    Locals,
    /// An evaluation's answer is in [`Session::evals`] or [`Session::hover`].
    Evaluated,
    /// Breakpoints in a file were set: their verified lines.
    Breakpoints(PathBuf, Vec<usize>),
    Error(String),
}

#[derive(Debug, Clone)]
enum Pending {
    Initialize,
    Launch,
    SetBreakpoints(PathBuf),
    ConfigurationDone,
    Threads,
    StackTrace,
    Scopes,
    Variables,
    Members(i64),
    Evaluate { expr: String, hover: bool },
    Step,
    Disconnect,
}

/// One debugging session.
pub struct Session {
    local_root: PathBuf,
    remote_root: String,
    seq: i64,
    pending: BTreeMap<i64, Pending>,
    out: Vec<u8>,
    dec: Decoder,
    program: String,
    args: Vec<String>,
    cwd: String,
    /// Wanted breakpoints, by file (Artifex's path).
    pub breakpoints: BTreeMap<PathBuf, Vec<Bp>>,
    initialized: bool,
    /// Attach to a program already started under lldb-server at this
    /// `gdb-remote` address (a dev app: it runs in its own jail), instead
    /// of launching it.
    attach: Option<String>,
    pub run: Run,
    pub thread: Option<i64>,
    pub frames: Vec<Frame>,
    /// The frame whose locals are shown (and that evaluations run in).
    pub frame: Option<i64>,
    pub locals: Vec<Variable>,
    /// Members of the variables expanded, by their handle.
    pub members: BTreeMap<i64, Vec<Variable>>,
    /// Evaluations from the panel's line: expression, answer or error.
    pub evals: Vec<(String, Result<String, String>)>,
    /// The name under the pointer and its value.
    pub hover: Option<(String, String)>,
}

impl Session {
    /// Debug `program` (relative to `work`, or absolute in the workspace)
    /// with `args`, in folder `cwd` of `work`; queues `initialize`.
    pub fn new(local_root: &Path, remote_root: &str, program: &str, args: &[String], cwd: &str,
               breakpoints: BTreeMap<PathBuf, Vec<Bp>>) -> Session {
        let mut s = Session {
            local_root: local_root.to_path_buf(),
            remote_root: remote_root.trim_end_matches('/').to_string(),
            seq: 1,
            pending: BTreeMap::new(),
            out: Vec::new(),
            dec: Decoder::default(),
            program: program.to_string(),
            args: args.to_vec(),
            cwd: cwd.to_string(),
            breakpoints,
            initialized: false,
            attach: None,
            run: Run::Starting,
            thread: None,
            frames: Vec::new(),
            frame: None,
            locals: Vec::new(),
            members: BTreeMap::new(),
            evals: Vec::new(),
            hover: None,
        };
        s.request(Pending::Initialize, "initialize", json!({
            "clientID": "artifex", "clientName": "Artifex", "adapterID": "lldb-dap",
            "linesStartAt1": true, "columnsStartAt1": true, "pathFormat": "path",
            "supportsVariableType": true, "supportsRunInTerminalRequest": false,
        }));
        s
    }

    /// Attach through `gdb-remote address` rather than launch.
    pub fn attaching(mut self, address: &str) -> Session {
        self.attach = Some(address.to_string());
        self
    }

    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    fn request(&mut self, kind: Pending, command: &str, args: Value) {
        let seq = self.seq;
        self.seq += 1;
        self.pending.insert(seq, kind);
        self.out.extend(frame(&json!({ "seq": seq, "type": "request", "command": command, "arguments": args })));
    }

    fn remote(&self, rel: &str) -> String {
        if rel.is_empty() { self.remote_root.clone() } else { format!("{}/{rel}", self.remote_root) }
    }

    fn remote_of(&self, local: &Path) -> Option<String> {
        crate::tree::relative(&self.local_root, local).map(|r| self.remote(&r))
    }

    fn local_of(&self, remote: &str) -> Option<PathBuf> {
        let rel = remote.strip_prefix(&format!("{}/", self.remote_root))?;
        if rel.split('/').any(|c| c == ".." || c == "." || c.is_empty()) {
            return None;
        }
        Some(self.local_root.join(rel))
    }

    /// Set the breakpoints of `file` (sent now if the adapter is ready).
    pub fn set_breakpoints(&mut self, file: &Path, lines: Vec<Bp>) {
        self.breakpoints.insert(file.to_path_buf(), lines);
        if self.initialized {
            self.send_breakpoints(file);
        }
    }

    fn send_breakpoints(&mut self, file: &Path) {
        let Some(remote) = self.remote_of(file) else { return };
        let wanted = self.breakpoints.get(file).cloned().unwrap_or_default();
        let bps: Vec<Value> = wanted.iter().map(|b| match &b.condition {
            Some(c) => json!({ "line": b.line, "condition": c }),
            None => json!({ "line": b.line }),
        }).collect();
        let lines: Vec<usize> = wanted.iter().map(|b| b.line).collect();
        self.request(Pending::SetBreakpoints(file.to_path_buf()), "setBreakpoints",
            json!({ "source": { "path": remote }, "breakpoints": bps, "lines": lines }));
    }

    fn step(&mut self, command: &str) {
        let Some(t) = self.thread else { return };
        if matches!(self.run, Run::Stopped(_)) {
            self.run = Run::Running;
            self.clear_stop();
            self.request(Pending::Step, command, json!({ "threadId": t }));
        }
    }

    pub fn resume(&mut self) { self.step("continue") }
    pub fn next(&mut self) { self.step("next") }
    pub fn step_in(&mut self) { self.step("stepIn") }
    pub fn step_out(&mut self) { self.step("stepOut") }

    pub fn pause(&mut self) {
        if self.run == Run::Running {
            let t = self.thread.unwrap_or(0);
            self.request(Pending::Step, "pause", json!({ "threadId": t }));
        }
    }

    /// What a stop showed: handles are good only while stopped.
    fn clear_stop(&mut self) {
        self.frames.clear();
        self.frame = None;
        self.locals.clear();
        self.members.clear();
        self.hover = None;
    }

    /// Show the locals of frame `id` (one of [`Session::frames`]).
    pub fn select_frame(&mut self, id: i64) {
        if matches!(self.run, Run::Stopped(_)) && self.frames.iter().any(|f| f.id == id) && self.frame != Some(id) {
            self.frame = Some(id);
            self.members.clear();
            self.request(Pending::Scopes, "scopes", json!({ "frameId": id }));
        }
    }

    /// Fetch the members of a variable (its `reference`).
    pub fn expand(&mut self, reference: i64) {
        if reference > 0 && matches!(self.run, Run::Stopped(_)) && !self.members.contains_key(&reference)
            && self.members.len() < MAX_EXPANDED {
            self.request(Pending::Members(reference), "variables", json!({ "variablesReference": reference }));
        }
    }

    /// Evaluate `expr` in the shown frame: from the panel's line, or for
    /// the name under the pointer (`hover`).
    pub fn evaluate(&mut self, expr: &str, hover: bool) {
        let expr: String = expr.trim().chars().take(MAX_EXPR).collect();
        if expr.is_empty() || !matches!(self.run, Run::Stopped(_)) {
            return;
        }
        // `watch` answers with the value alone (lldb-dap's `hover` and
        // `repl` add the type and a name).
        let mut args = json!({ "expression": expr, "context": "watch" });
        if let Some(f) = self.frame {
            args["frameId"] = json!(f);
        }
        self.request(Pending::Evaluate { expr, hover }, "evaluate", args);
    }

    /// End the program and the session.
    pub fn stop(&mut self) {
        self.request(Pending::Disconnect, "disconnect", json!({ "terminateDebuggee": true }));
    }

    /// Bytes from the adapter.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Event>, String> {
        let mut ev = Vec::new();
        for msg in self.dec.feed(bytes)? {
            match msg.get("type").and_then(Value::as_str) {
                Some("response") => self.response(&msg, &mut ev),
                Some("event") => self.event(&msg, &mut ev),
                // A reverse request (runInTerminal, startDebugging): not
                // offered, so refused.
                Some("request") => {
                    let seq = self.seq;
                    self.seq += 1;
                    let rs = msg.get("seq").cloned().unwrap_or(Value::Null);
                    let command = msg.get("command").cloned().unwrap_or(Value::Null);
                    self.out.extend(frame(&json!({ "seq": seq, "type": "response", "request_seq": rs, "command": command,
                        "success": false, "message": "not supported by Artifex" })));
                }
                _ => {}
            }
        }
        Ok(ev)
    }

    fn response(&mut self, msg: &Value, ev: &mut Vec<Event>) {
        let Some(kind) = msg.get("request_seq").and_then(Value::as_i64).and_then(|s| self.pending.remove(&s)) else { return };
        let ok = msg.get("success").and_then(Value::as_bool).unwrap_or(false);
        let body = msg.get("body").cloned().unwrap_or(Value::Null);
        if !ok {
            let m = msg.get("message").and_then(Value::as_str).unwrap_or("failed").to_string();
            let m: String = body.pointer("/error/format").and_then(Value::as_str).map(str::to_string).unwrap_or(m).chars().take(300).collect();
            match kind {
                Pending::Disconnect => {}
                // Not a name the debugger knows: nothing to show.
                Pending::Evaluate { hover: true, .. } => self.hover = None,
                Pending::Evaluate { expr, hover: false } => {
                    self.push_eval(expr, Err(error_line(&m)));
                    ev.push(Event::Evaluated);
                }
                _ => ev.push(Event::Error(m)),
            }
            return;
        }
        match kind {
            Pending::Initialize => {
                // Launch now; breakpoints follow the `initialized` event.
                let program = if self.program.starts_with('/') { self.program.clone() } else { self.remote(&self.program) };
                let cwd = self.remote(&self.cwd);
                let args = self.args.clone();
                match self.attach.clone() {
                    // The dev app waits under lldb-server; lldb connects to it.
                    // `timeout`: how long lldb-dap waits for it to stop
                    // once connected (30 s by default; a slow guest takes
                    // longer to start it under lldb-server).
                    Some(a) => self.request(Pending::Launch, "attach", json!({
                        "program": program, "attachCommands": [format!("settings set plugin.process.gdb-remote.packet-timeout {HANDSHAKE_S}"), format!("gdb-remote {a}")], "stopOnEntry": false, "timeout": ATTACH_TIMEOUT_S })),
                    None => self.request(Pending::Launch, "launch", json!({ "program": program, "args": args, "cwd": cwd, "stopOnEntry": false })),
                }
            }
            Pending::SetBreakpoints(file) => {
                let lines = body.get("breakpoints").and_then(Value::as_array).map(|a| a.iter()
                    .filter(|b| b.get("verified").and_then(Value::as_bool).unwrap_or(false))
                    .filter_map(|b| b.get("line").and_then(Value::as_u64).map(|l| l as usize)).collect()).unwrap_or_default();
                ev.push(Event::Breakpoints(file, lines));
            }
            Pending::Threads => {
                if self.thread.is_none() {
                    self.thread = body.pointer("/threads/0/id").and_then(Value::as_i64);
                }
                self.stack();
            }
            Pending::StackTrace => {
                self.frames = body.get("stackFrames").and_then(Value::as_array).map(|a| a.iter().take(64).filter_map(|f| {
                    Some(Frame {
                        id: f.get("id")?.as_i64()?,
                        name: f.get("name").and_then(Value::as_str).unwrap_or("?").chars().take(200).collect(),
                        file: f.pointer("/source/path").and_then(Value::as_str).and_then(|p| self.local_of(p)),
                        line: f.get("line").and_then(Value::as_u64).unwrap_or(0) as usize,
                    })
                }).collect()).unwrap_or_default();
                ev.push(Event::Frames);
                // The locals of the frame the editor shows: the first in the
                // project (a crash stops deep in libc and the panic machinery).
                if let Some(top) = self.frames.iter().find(|f| f.file.is_some()).or(self.frames.first()).map(|f| f.id) {
                    self.frame = Some(top);
                    self.request(Pending::Scopes, "scopes", json!({ "frameId": top }));
                }
            }
            Pending::Scopes => {
                // The first scope is the locals for lldb-dap.
                if let Some(r) = body.pointer("/scopes/0/variablesReference").and_then(Value::as_i64) {
                    self.request(Pending::Variables, "variables", json!({ "variablesReference": r }));
                }
            }
            Pending::Variables => {
                self.locals = variables(&body);
                ev.push(Event::Locals);
            }
            Pending::Members(r) => {
                if self.members.len() < MAX_EXPANDED {
                    self.members.insert(r, variables(&body));
                }
                ev.push(Event::Locals);
            }
            Pending::Evaluate { expr, hover } => {
                let v: String = body.get("result").and_then(Value::as_str).unwrap_or("").chars().take(300).collect();
                if hover {
                    self.hover = Some((expr, v));
                } else {
                    self.push_eval(expr, Ok(v));
                }
                ev.push(Event::Evaluated);
            }
            Pending::Launch | Pending::ConfigurationDone | Pending::Step | Pending::Disconnect => {}
        }
    }

    fn push_eval(&mut self, expr: String, r: Result<String, String>) {
        if self.evals.len() >= MAX_EVALS {
            self.evals.drain(..1);
        }
        self.evals.push((expr, r));
    }

    fn stack(&mut self) {
        if let Some(t) = self.thread {
            self.request(Pending::StackTrace, "stackTrace", json!({ "threadId": t, "startFrame": 0, "levels": 64 }));
        }
    }

    fn event(&mut self, msg: &Value, ev: &mut Vec<Event>) {
        let body = msg.get("body").cloned().unwrap_or(Value::Null);
        match msg.get("event").and_then(Value::as_str).unwrap_or("") {
            "initialized" => {
                self.initialized = true;
                let files: Vec<PathBuf> = self.breakpoints.keys().cloned().collect();
                for f in files {
                    self.send_breakpoints(&f);
                }
                self.request(Pending::ConfigurationDone, "configurationDone", json!({}));
                self.run = Run::Running;
                ev.push(Event::State);
            }
            "stopped" => {
                let why: String = body.get("reason").and_then(Value::as_str).unwrap_or("paused").chars().take(40).collect();
                self.clear_stop();
                self.run = Run::Stopped(why);
                if let Some(t) = body.get("threadId").and_then(Value::as_i64) {
                    self.thread = Some(t);
                    self.stack();
                } else {
                    self.request(Pending::Threads, "threads", Value::Null);
                }
                ev.push(Event::State);
            }
            "continued" => {
                self.clear_stop();
                self.run = Run::Running;
                ev.push(Event::State);
            }
            "exited" => {
                self.run = Run::Exited(body.get("exitCode").and_then(Value::as_i64));
                ev.push(Event::State);
            }
            "terminated" => {
                if !matches!(self.run, Run::Exited(_)) {
                    self.run = Run::Exited(None);
                }
                ev.push(Event::State);
            }
            "output" => {
                if body.get("category").and_then(Value::as_str) != Some("telemetry") {
                    if let Some(o) = body.get("output").and_then(Value::as_str) {
                        ev.push(Event::Output(o.to_string()));
                    }
                }
            }
            _ => {}
        }
    }
}

/// A compiler's error as one line: lldb's first, without its
/// `error: <user expression N>:1:1:` place.
fn error_line(m: &str) -> String {
    let first = m.lines().find(|l| !l.trim().is_empty()).unwrap_or(m).trim();
    let first = first.strip_prefix("error:").map(str::trim_start).unwrap_or(first);
    let first = match first.strip_prefix("<user expression") {
        // `<user expression 2>:1:1: use of …`: after the place.
        Some(rest) => rest.splitn(2, ": ").nth(1).unwrap_or(rest),
        None => first,
    };
    first.chars().take(200).collect()
}

/// A `variables` answer, bounded: names and values cut, at most
/// [`MAX_MEMBERS`].
fn variables(body: &Value) -> Vec<Variable> {
    body.get("variables").and_then(Value::as_array).map(|a| a.iter().take(MAX_MEMBERS).filter_map(|v| {
        Some(Variable {
            name: v.get("name")?.as_str()?.chars().take(200).collect(),
            value: v.get("value").and_then(Value::as_str).unwrap_or("").chars().take(200).collect(),
            kind: v.get("type").and_then(Value::as_str).map(|t| t.chars().take(200).collect()),
            reference: v.get("variablesReference").and_then(Value::as_i64).filter(|r| *r > 0).unwrap_or(0),
        })
    }).collect()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sent(s: &mut Session) -> Vec<Value> {
        Decoder::default().feed(&s.take_output()).unwrap()
    }

    fn reply(seq: &Value, body: Value) -> Vec<u8> {
        frame(&json!({ "seq": 100, "type": "response", "request_seq": seq, "success": true, "body": body }))
    }

    fn event(name: &str, body: Value) -> Vec<u8> {
        frame(&json!({ "seq": 101, "type": "event", "event": name, "body": body }))
    }

    #[test]
    fn a_dev_app_is_attached_to_through_gdb_remote() {
        let mut s = Session::new(Path::new("/work/app"), "/home/p/work", "/var/lib/atrium/apps/dev.p.ws.org.x/app", &[], "app",
                                 BTreeMap::new()).attaching("127.0.0.1:4711");
        let m = sent(&mut s);
        s.feed(&reply(&m[0]["seq"], json!({}))).unwrap();
        let m = sent(&mut s);
        assert_eq!(m[0]["command"], "attach");
        assert_eq!(m[0]["arguments"]["program"], "/var/lib/atrium/apps/dev.p.ws.org.x/app");
        assert_eq!(m[0]["arguments"]["attachCommands"], json!([format!("settings set plugin.process.gdb-remote.packet-timeout {HANDSHAKE_S}"), "gdb-remote 127.0.0.1:4711".to_string()]));
        assert_eq!(m[0]["arguments"]["timeout"], ATTACH_TIMEOUT_S);
    }

    #[test]
    fn a_session_to_a_breakpoint() {
        let main = PathBuf::from("/work/proj/hello.c");
        let mut bps = BTreeMap::new();
        bps.insert(main.clone(), vec![Bp { line: 5, condition: None }]);
        let mut s = Session::new(Path::new("/work/proj"), "/home/p/work", "hello", &[], "", bps);
        let m = sent(&mut s);
        assert_eq!(m[0]["command"], "initialize");
        s.feed(&reply(&m[0]["seq"], json!({}))).unwrap();
        let m = sent(&mut s);
        assert_eq!(m[0]["command"], "launch");
        assert_eq!(m[0]["arguments"]["program"], "/home/p/work/hello");
        assert_eq!(m[0]["arguments"]["cwd"], "/home/p/work");

        let ev = s.feed(&event("initialized", json!({}))).unwrap();
        assert_eq!(ev, vec![Event::State]);
        let m = sent(&mut s);
        assert_eq!(m[0]["command"], "setBreakpoints");
        assert_eq!(m[0]["arguments"]["source"]["path"], "/home/p/work/hello.c");
        assert_eq!(m[1]["command"], "configurationDone");
        let ev = s.feed(&reply(&m[0]["seq"], json!({"breakpoints": [{"verified": true, "line": 5}]}))).unwrap();
        assert_eq!(ev, vec![Event::Breakpoints(main.clone(), vec![5])]);

        let ev = s.feed(&event("stopped", json!({"reason": "breakpoint", "threadId": 7}))).unwrap();
        assert_eq!(ev, vec![Event::State]);
        assert_eq!(s.run, Run::Stopped("breakpoint".into()));
        let m = sent(&mut s);
        assert_eq!(m[0]["command"], "stackTrace");
        let ev = s.feed(&reply(&m[0]["seq"], json!({"stackFrames": [
            {"id": 1, "name": "main", "line": 5, "source": {"path": "/home/p/work/hello.c"}},
            {"id": 2, "name": "__libc_start1", "line": 0, "source": {"path": "/usr/src/lib/libc/csu/libc_start1.c"}}]}))).unwrap();
        assert_eq!(ev, vec![Event::Frames]);
        assert_eq!(s.frames[0].file.as_deref(), Some(main.as_path()));
        assert_eq!(s.frames[1].file, None);
        let m = sent(&mut s);
        assert_eq!(m[0]["command"], "scopes");
        s.feed(&reply(&m[0]["seq"], json!({"scopes": [{"name": "Locals", "variablesReference": 9}]}))).unwrap();
        let m = sent(&mut s);
        assert_eq!(m[0]["arguments"]["variablesReference"], 9);
        let ev = s.feed(&reply(&m[0]["seq"], json!({"variables": [{"name": "n", "value": "3", "type": "int"}]}))).unwrap();
        assert_eq!(ev, vec![Event::Locals]);
        assert_eq!(s.locals[0], Variable { name: "n".into(), value: "3".into(), kind: Some("int".into()), reference: 0 });

        s.next();
        let m = sent(&mut s);
        assert_eq!(m[0]["command"], "next");
        assert_eq!(m[0]["arguments"]["threadId"], 7);
        assert_eq!(s.run, Run::Running);
        // Not stopped: no second step.
        s.next();
        assert!(sent(&mut s).is_empty());

        s.feed(&event("output", json!({"category": "stdout", "output": "hello\n"}))).unwrap();
        let ev = s.feed(&event("exited", json!({"exitCode": 0}))).unwrap();
        assert_eq!(ev, vec![Event::State]);
        assert_eq!(s.run, Run::Exited(Some(0)));
    }

    #[test]
    fn refuses_reverse_requests_and_reports_errors() {
        let mut s = Session::new(Path::new("/work/p"), "/w", "a", &[], "", BTreeMap::new());
        let m = sent(&mut s);
        let ev = s.feed(&frame(&json!({"seq": 5, "type": "request", "command": "runInTerminal", "arguments": {}}))).unwrap();
        assert!(ev.is_empty());
        let r = sent(&mut s);
        assert_eq!(r[0]["success"], false);
        assert_eq!(r[0]["request_seq"], 5);
        let ev = s.feed(&frame(&json!({"seq": 6, "type": "response", "request_seq": m[0]["seq"], "success": false, "message": "no lldb"}))).unwrap();
        assert_eq!(ev, vec![Event::Error("no lldb".into())]);
        // Breakpoints outside work aren't sent.
        s.initialized = true;
        s.set_breakpoints(Path::new("/etc/x.c"), vec![Bp { line: 1, condition: None }]);
        assert!(sent(&mut s).is_empty());
    }

    /// A session stopped at a breakpoint in thread 7, frames 1 (main) and 2.
    fn stopped() -> Session {
        let mut s = Session::new(Path::new("/work/p"), "/w", "a", &[], "", BTreeMap::new());
        s.initialized = true;
        sent(&mut s);
        s.feed(&event("stopped", json!({"reason": "breakpoint", "threadId": 7}))).unwrap();
        let m = sent(&mut s);
        s.feed(&reply(&m[0]["seq"], json!({"stackFrames": [
            {"id": 1, "name": "span_of", "line": 9, "source": {"path": "/w/a.c"}},
            {"id": 2, "name": "main", "line": 32, "source": {"path": "/w/a.c"}}]}))).unwrap();
        let m = sent(&mut s);
        assert_eq!((m[0]["command"].as_str(), m[0]["arguments"]["frameId"].as_i64()), (Some("scopes"), Some(1)));
        s.feed(&reply(&m[0]["seq"], json!({"scopes": [{"variablesReference": 9}]}))).unwrap();
        let m = sent(&mut s);
        s.feed(&reply(&m[0]["seq"], json!({"variables": [
            {"name": "w", "value": "480", "type": "float", "variablesReference": 0},
            {"name": "ev", "value": "{...}", "type": "atrium_window_event_t", "variablesReference": 12}]}))).unwrap();
        s
    }

    #[test]
    fn conditions_frames_members_and_evaluation() {
        // A condition goes with its line.
        let mut s = Session::new(Path::new("/work/p"), "/w", "a", &[], "", BTreeMap::new());
        s.initialized = true;
        sent(&mut s);
        s.set_breakpoints(Path::new("/work/p/a.c"), vec![Bp { line: 32, condition: Some("n > 3".into()) }, Bp { line: 40, condition: None }]);
        let m = sent(&mut s);
        assert_eq!(m[0]["arguments"]["breakpoints"], json!([{"line": 32, "condition": "n > 3"}, {"line": 40}]));

        let mut s = stopped();
        assert_eq!((s.frame, s.locals.len(), s.locals[1].reference), (Some(1), 2, 12));
        // A struct's members, once; a leaf has none to fetch.
        s.expand(12);
        let m = sent(&mut s);
        assert_eq!(m[0]["arguments"]["variablesReference"], 12);
        s.feed(&reply(&m[0]["seq"], json!({"variables": [{"name": "kind", "value": "3", "variablesReference": 0}]}))).unwrap();
        assert_eq!(s.members.get(&12).map(|v| v[0].name.as_str()), Some("kind"));
        s.expand(12);
        s.expand(0);
        assert!(sent(&mut s).is_empty());
        // Another frame: its locals, and evaluations run in it.
        s.select_frame(2);
        let m = sent(&mut s);
        assert_eq!((m[0]["command"].as_str(), m[0]["arguments"]["frameId"].as_i64()), (Some("scopes"), Some(2)));
        assert!(s.members.is_empty());
        s.select_frame(99);
        assert!(sent(&mut s).is_empty());
        s.evaluate("  width * 2 ", false);
        let m = sent(&mut s);
        assert_eq!(m[0]["arguments"], json!({"expression": "width * 2", "context": "watch", "frameId": 2}));
        let ev = s.feed(&reply(&m[0]["seq"], json!({"result": "960"}))).unwrap();
        assert_eq!(ev, vec![Event::Evaluated]);
        assert_eq!(s.evals, vec![("width * 2".to_string(), Ok("960".to_string()))]);
        // An error is the answer, not a note; on a hover it is nothing.
        s.evaluate("nope", false);
        let m = sent(&mut s);
        s.feed(&frame(&json!({"seq": 90, "type": "response", "request_seq": m[0]["seq"], "success": false, "message": "use of undeclared identifier"}))).unwrap();
        assert_eq!(s.evals[1], ("nope".to_string(), Err("use of undeclared identifier".to_string())));
        assert_eq!(error_line("error: <user expression 2>:1:1: use of undeclared identifier 'q'\n    1 | q\n      | ^\n"), "use of undeclared identifier 'q'");
        s.evaluate("ev.kind", true);
        let m = sent(&mut s);
        assert_eq!(m[0]["arguments"]["context"], "watch");
        s.feed(&reply(&m[0]["seq"], json!({"result": "3"}))).unwrap();
        assert_eq!(s.hover, Some(("ev.kind".to_string(), "3".to_string())));
        // A step forgets what a stop showed; nothing is evaluated running.
        s.next();
        assert!(s.hover.is_none() && s.frame.is_none() && s.locals.is_empty());
        sent(&mut s);
        s.evaluate("n", false);
        s.expand(12);
        assert!(sent(&mut s).is_empty());
        // The history is bounded.
        let mut s = stopped();
        for i in 0..(MAX_EVALS + 5) {
            s.evaluate(&format!("{i}"), false);
            let m = sent(&mut s);
            s.feed(&reply(&m[0]["seq"], json!({"result": "x"}))).unwrap();
        }
        assert_eq!(s.evals.len(), MAX_EVALS);
        assert_eq!(s.evals[0].0, "5");
    }
}
