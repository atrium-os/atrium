//! One sync-and-send pass for a configured account (mail.md §6.1, §9.3):
//! the person's changes and new mail (IMAP or JMAP), drafts copied to the
//! server, and sends that are due. The connections are the caller's (Arca
//! couriers that log in), so the engine still has no network of its own
//! and never sees a secret: it names the Arca item, the courier uses it.

use std::os::unix::net::UnixStream;

use mail_store::{AccountConfig, AccountId, Protocol, Reached, Server};
pub use mail_sync::remote::Imap;

use crate::send::Delivery;
use crate::Engine;

/// How Mail reaches its servers.
pub trait Connect {
    /// A logged-in IMAP or SMTP session to `server` with Arca item `item`
    /// (the mail courier's relay).
    fn mail(&self, server: &Server, item: &[u8; 16]) -> std::io::Result<UnixStream>;
    /// HTTP to a JMAP account whose `Authorization` is item `item`.
    fn jmap(&self, session: &str, item: &[u8; 16]) -> Result<Box<dyn mail_sync::jmap::Http>, String>;
}

/// Seconds a draft rests before its copy goes to the server's Drafts.
pub const DRAFT_REST: i64 = 30;

/// What one pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Pass {
    pub report: mail_sync::Report,
    pub sent: usize,
    /// Sends that failed this pass, with why.
    pub failed: Vec<String>,
}

/// Providers whose submission server files the sent copy in Sent itself
/// (another copy would be a duplicate).
fn files_sent_itself(smtp: &Server) -> bool {
    ["smtp.gmail.com", "smtp.office365.com"].contains(&smtp.host.as_str())
}

fn reached_from(e: &str, now: i64) -> Reached {
    let up = e.to_ascii_uppercase();
    if up.contains("AUTHENTICATIONFAILED") || up.contains("SIGN IN AGAIN") || up.contains("NO SIGN-IN") || up.contains("535 ") {
        Reached::SignInAgain { at: now }
    } else {
        Reached::Failed { at: now, why: e.chars().filter(|c| !c.is_control()).take(200).collect() }
    }
}

/// Mail's `sync` worker (§4), in its manifest.
pub const SYNC_WORKER: &str = "sync";

/// An IMAP session in the `sync` worker, with the worker it runs in.
struct InWorker<I: std::io::Write + std::os::fd::AsFd, O: std::io::Read + std::os::fd::AsFd> {
    remote: mail_sync::remote::Remote<I, O>,
    proc: SyncProc,
}

enum SyncProc {
    Worker(atrium_worker::Stdio),
    Child(std::process::Child),
}

impl<I: std::io::Write + std::os::fd::AsFd, O: std::io::Read + std::os::fd::AsFd> Drop for InWorker<I, O> {
    fn drop(&mut self) {
        match &mut self.proc {
            SyncProc::Worker(w) => w.kill(),
            SyncProc::Child(c) => {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }
}

impl<I: std::io::Write + std::os::fd::AsFd, O: std::io::Read + std::os::fd::AsFd> Imap for InWorker<I, O> {
    fn capabilities(&self) -> &[String] {
        self.remote.capabilities()
    }
    fn sync(&mut self, sink: &mut dyn mail_sync::Sink) -> mail_sync::Result<mail_sync::Report> {
        self.remote.sync(sink)
    }
    fn append(&mut self, folder: &str, flags: &[&str], message: &[u8]) -> mail_sync::Result<Option<u32>> {
        self.remote.append(folder, flags, message)
    }
    fn idle(&mut self, folder: &str, wait: std::time::Duration) -> mail_sync::Result<bool> {
        self.remote.idle(folder, wait)
    }
    fn logout(&mut self) -> mail_sync::Result<()> {
        self.remote.logout()
    }
}

/// An IMAP session over `courier`: in Mail's `sync` worker when the app
/// has one (a jail of its own, no capability, §4), in the program
/// `MAIL_SYNC_WORKER` names (development, tests), else here.
pub fn open_imap(courier: UnixStream) -> Result<Box<dyn Imap>, String> {
    use mail_sync::remote::Remote;
    let named = std::env::var_os("MAIL_SYNC_WORKER");
    if let Some(w) = atrium_worker::Workers::get().ok().filter(|w| named.is_none() && w.has(SYNC_WORKER)) {
        let mut p = w.spawn_stdio(SYNC_WORKER).map_err(|e| format!("start the sync worker: {e}"))?;
        let (i, o) = (p.stdin.take().ok_or("no worker input")?, p.stdout.take().ok_or("no worker output")?);
        drop(p.stderr.take());
        let proc = SyncProc::Worker(p);
        let remote = Remote::start(courier, i, o).map_err(|e| e.to_string())?;
        return Ok(Box::new(InWorker { remote, proc }));
    }
    if let Some(program) = named {
        let mut c = std::process::Command::new(program)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("start the sync worker: {e}"))?;
        let (i, o) = (c.stdin.take().ok_or("no worker input")?, c.stdout.take().ok_or("no worker output")?);
        let proc = SyncProc::Child(c);
        let remote = Remote::start(courier, i, o).map_err(|e| e.to_string())?;
        return Ok(Box::new(InWorker { remote, proc }));
    }
    Ok(Box::new(mail_sync::remote::Local::start(courier).map_err(|e| e.to_string())?))
}

/// Seconds one `IDLE` waits before it is renewed: under the 29 minutes
/// RFC 2177 allows, and the mail courier's 31-minute limit.
pub const IDLE_RENEW: u64 = 25 * 60;

/// How a watch ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Watched {
    /// The server has no push (`IDLE`, or a JMAP event source): the account
    /// is polled instead.
    NoIdle,
    /// The caller said stop.
    Stopped,
}

/// A JMAP stream that ends sooner than this after it began is a failure
/// (backed off), not a renewal.
pub const STREAM_MIN_SECS: u64 = 30;

/// Watch an account for new mail (§6.1): an IMAP inbox with `IDLE` on a
/// connection of its own, or a JMAP account's event source. `on(true)`
/// after each change the server reports, `on(false)` at each sign of life
/// (a renewed `IDLE`, a ping); return `false` to stop. `held` gets a clone
/// of an IMAP connection, so another thread can shut it down to end a wait
/// early. An error is the connection failing.
pub fn watch_inbox(cfg: &AccountConfig, net: &dyn Connect, held: &mut dyn FnMut(UnixStream), on: &mut dyn FnMut(bool) -> bool) -> Result<Watched, String> {
    let item = cfg.incoming_item.ok_or("no sign-in for the incoming server")?;
    let imap = match &cfg.protocol {
        Protocol::Imap { imap, .. } => imap,
        Protocol::Jmap { session } => {
            let h = net.jmap(session, &item)?;
            let mut client = mail_sync::jmap::Client::connect(h, session).map_err(|e| e.to_string())?;
            loop {
                let began = std::time::Instant::now();
                match client.watch(on).map_err(|e| e.to_string())? {
                    mail_sync::jmap::Pushed::NoPush => return Ok(Watched::NoIdle),
                    mail_sync::jmap::Pushed::Stopped => return Ok(Watched::Stopped),
                    mail_sync::jmap::Pushed::Ended if began.elapsed().as_secs() < STREAM_MIN_SECS => return Err("the server ended its event stream at once".into()),
                    mail_sync::jmap::Pushed::Ended => {}
                }
            }
        }
    };
    let s = net.mail(imap, &item).map_err(|e| format!("couldn't reach {}: {e}", imap.host))?;
    held(s.try_clone().map_err(|e| e.to_string())?);
    let mut session = open_imap(s)?;
    if !session.capabilities().iter().any(|c| c.eq_ignore_ascii_case("IDLE")) {
        let _ = session.logout();
        return Ok(Watched::NoIdle);
    }
    loop {
        let changed = session.idle("INBOX", std::time::Duration::from_secs(IDLE_RENEW)).map_err(|e| e.to_string())?;
        if !on(changed) {
            let _ = session.logout();
            return Ok(Watched::Stopped);
        }
    }
}

impl Engine {
    /// The accounts Mail can reach, with their settings.
    pub fn configured(&self) -> Vec<(AccountId, AccountConfig)> {
        self.store.accounts().unwrap_or_default().into_iter().filter_map(|(id, _)| Some((id, self.store.account_config(id).ok()??))).collect()
    }

    /// One pass for `account`; how it went is kept in its settings.
    pub fn pass(&self, account: AccountId, net: &dyn Connect, now: i64) -> Result<Pass, Reached> {
        let Some(cfg) = self.store.account_config(account).ok().flatten() else { return Err(Reached::Failed { at: now, why: "no settings".into() }) };
        let r = match &cfg.protocol {
            Protocol::Imap { imap, smtp } => self.pass_imap(account, &cfg, imap, smtp, net),
            Protocol::Jmap { session } => self.pass_jmap(account, &cfg, session, net),
        };
        let reached = match &r {
            Ok(_) => Reached::Ok { at: now },
            Err(e) => reached_from(e, now),
        };
        let _ = self.store.write(|w| w.set_reached(account, reached.clone()));
        r.map_err(|_| reached)
    }

    fn pass_imap(&self, account: AccountId, cfg: &AccountConfig, imap: &Server, smtp: &Server, net: &dyn Connect) -> Result<Pass, String> {
        let item = cfg.incoming_item.ok_or("no sign-in for the incoming server")?;
        let s = net.mail(imap, &item).map_err(|e| format!("couldn't reach {}: {e}", imap.host))?;
        let mut s = open_imap(s)?;
        let mut pass = Pass::default();
        pass.report = s.sync(&mut self.sink(account)).map_err(|e| e.to_string())?;
        self.copy_drafts(account, s.as_mut(), DRAFT_REST).map_err(|e| e.to_string())?;
        let mut copies = Vec::new();
        if let Some(out) = cfg.outgoing_item {
            for (id, o) in self.due_sends().map_err(|e| e.to_string())? {
                if o.account != account {
                    continue;
                }
                match self.deliver(id, || net.mail(smtp, &out), files_sent_itself(smtp)).map_err(|e| e.to_string())? {
                    Delivery::Sent { copy, .. } => {
                        pass.sent += 1;
                        copies.extend(copy);
                    }
                    Delivery::Failed { why, .. } => pass.failed.push(why),
                    _ => {}
                }
            }
        }
        for c in copies {
            self.file_sent(account, s.as_mut(), &c).map_err(|e| e.to_string())?;
        }
        let _ = s.logout();
        Ok(pass)
    }

    fn pass_jmap(&self, account: AccountId, cfg: &AccountConfig, session: &str, net: &dyn Connect) -> Result<Pass, String> {
        let item = cfg.incoming_item.ok_or("no sign-in for this account")?;
        let h = net.jmap(session, &item)?;
        let mut client = mail_sync::jmap::Client::connect(h, session).map_err(|e| e.to_string())?;
        let mut pass = Pass { report: client.sync(&mut self.sink(account)).map_err(|e| e.to_string())?, ..Default::default() };
        for (id, o) in self.due_sends().map_err(|e| e.to_string())? {
            if o.account != account {
                continue;
            }
            match self.deliver_jmap(id, &mut client).map_err(|e| e.to_string())? {
                Delivery::Sent { .. } => pass.sent += 1,
                Delivery::Failed { why, .. } => pass.failed.push(why),
                _ => {}
            }
        }
        Ok(pass)
    }

    /// When the next pass should run at the latest: the next due send,
    /// else `idle` seconds from now.
    pub fn next_pass(&self, now: i64, idle: i64) -> i64 {
        let soonest = self.store.outbox().ok().into_iter().flatten().map(|(_, o)| o.due).filter(|d| *d > now).min();
        soonest.map_or(now + idle, |d| d.min(now + idle))
    }
}
