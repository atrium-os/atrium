//! Calendar returns a choice. Only the final Send button enqueues it.
use mail_engine::{
    invitations::{LatestInvitation, Selected},
    Engine,
};
use pergola::{
    view::Ctx, window::StepResult, App, Button, Label, SizeSpec, View, Window, WindowDesc,
};
use portcullis_ipc::{
    calendar::{self, Reply},
    Request, Response,
};
use std::{
    io,
    os::{fd::AsRawFd, unix::net::UnixStream},
    sync::mpsc,
    time::{Duration, Instant},
};

/// Drain durable inbox work independently of opening a message or a window
/// activation. Calendar never learns an unrelated folder or message ID.
pub fn start_updates(store: mail_store::Store, parser: mail_engine::Parser) {
    std::thread::spawn(move || {
        let engine = Engine::new(store, parser);
        let mut reported = false;
        let mut after = 0;
        loop {
            let result = (|| -> Result<bool, String> {
                let more = engine
                    .store
                    .index_calendar_page()
                    .map_err(|e| e.to_string())?;
                let pending = engine
                    .store
                    .pending_calendar(after)
                    .map_err(|e| e.to_string())?;
                let full = pending.len() == 16;
                if pending.is_empty() {
                    after = 0;
                }
                for (msg, generation) in pending {
                    after = msg;
                    if let Some(update) = engine.unopened_update(msg).map_err(|e| e.to_string())? {
                        deposit(&update).map_err(|e| e.to_string())?;
                    }
                    engine
                        .store
                        .write(|w| w.finish_calendar_delivery(msg, generation))
                        .map_err(|e| e.to_string())?;
                }
                Ok(more || full)
            })();
            let delay = match result {
                Ok(more) => {
                    reported = false;
                    if more {
                        1
                    } else {
                        5
                    }
                }
                Err(_) => {
                    if !reported {
                        eprintln!("atrium-mail: background calendar delivery deferred");
                        reported = true;
                    }
                    30
                }
            };
            std::thread::sleep(Duration::from_secs(delay));
        }
    });
}

fn deposit(update: &calendar::BackgroundUpdate) -> io::Result<()> {
    let mut broker = UnixStream::connect(atrium_mail::files::portcullis_socket())?;
    let timeout = Some(Duration::from_secs(8));
    broker.set_read_timeout(timeout)?;
    broker.set_write_timeout(timeout)?;
    if !matches!(
        portcullis_ipc::round_trip(
            &mut broker,
            &Request::Hello {
                version: portcullis_ipc::PROTO_VERSION
            }
        )?,
        Response::Hello { .. }
    ) || !matches!(
        portcullis_ipc::round_trip(&mut broker, &Request::CalendarUpdate)?,
        Response::Ok
    ) {
        return Err(io::Error::other("Calendar updates unavailable"));
    }
    calendar::write(&mut broker, update)?;
    if !matches!(
        portcullis_ipc::read_response_unbuffered(&mut broker)?,
        Response::Ok
    ) {
        return Err(io::Error::other("Calendar receipt unavailable"));
    }
    Ok(())
}

fn open_session(activation: [u8; 16]) -> io::Result<UnixStream> {
    let mut broker = UnixStream::connect(atrium_mail::files::portcullis_socket())?;
    broker.set_read_timeout(Some(Duration::from_secs(10)))?;
    if !matches!(
        portcullis_ipc::round_trip(
            &mut broker,
            &Request::Hello {
                version: portcullis_ipc::PROTO_VERSION
            }
        )?,
        Response::Hello { .. }
    ) {
        return Err(io::Error::other("Calendar broker unavailable"));
    }
    portcullis_ipc::write_request(&mut broker, &Request::CalendarInvitation { activation })?;
    match portcullis_ipc::read_response_unbuffered(&mut broker)? {
        Response::ReadyForFds => (),
        Response::Error { message } => return Err(io::Error::other(message)),
        _ => return Err(io::Error::other("Calendar did not open")),
    }
    let fd = portcullis_ipc::recv_fds(&broker, 1)?
        .pop()
        .ok_or_else(|| io::Error::other("Missing Calendar session"))?;
    Ok(UnixStream::from(fd))
}

fn offer(selected: Selected, mut session: UnixStream) -> io::Result<Option<Reply>> {
    session.set_read_timeout(Some(Duration::from_secs(calendar::REVIEW_SECONDS)))?;
    session.set_write_timeout(Some(Duration::from_secs(5)))?;
    let part = limen::fdpass::sealed_file("invitation", &selected.bytes)?;
    calendar::write(&mut session, &selected.source)?;
    portcullis_ipc::send_fds(&session, &[part.as_raw_fd()])?;
    match calendar::read(&mut session) {
        Ok(reply) => Ok(Some(reply)),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(e),
    }
}

pub type Reviewed = (u64, mail_store::Hash, Result<Option<Reply>, String>);

pub fn start_review(
    engine: &Engine,
    msg: u64,
    press: [u8; 16],
    tx: mpsc::Sender<Reviewed>,
) -> Result<(), String> {
    let selected = engine.selected_invitation(msg).map_err(|e| e.to_string())?;
    // Spend the activation while the window that received the press is alive.
    let session = open_session(press).map_err(|e| e.to_string())?;
    let store = engine.store.clone();
    let blob = selected.blob;
    std::thread::spawn(move || {
        let result = offer(selected, session).map_err(|e| e.to_string());
        if result.is_ok() && store.write(|w| w.replay_calendar_revisions(msg)).is_err() {
            eprintln!("atrium-mail: could not requeue later meeting updates");
        }
        let _ = tx.send((msg, blob, result));
    });
    Ok(())
}

enum Action {
    Cancel,
    Send,
    ReviewLatest(u64, [u8; 16]),
}
pub enum Finished {
    Note(String),
    Review(u64, [u8; 16]),
}
struct Confirmation {
    changed: pergola::Mutable<Option<LatestInvitation>>,
    reply: Reply,
    title: String,
    proposed: String,
    scope: Option<String>,
    verified: bool,
    actions: mpsc::Sender<Action>,
    height: u32,
}
impl View for Confirmation {
    fn render(&self, ctx: &mut Ctx) {
        ctx.add(pergola::node::Node::Rect {
            rect: pergola::geom::Rect::new(0., 0., 600., self.height as f32),
            fill: ctx.theme.bg_canvas(),
            radius: 0.,
        });
        let r = &self.reply;
        let changed = self.changed.get_cloned();
        Label::new(if changed.is_some() {
            "Invitation changed before sending"
        } else {
            "Review your invitation response"
        })
        .at(18., 24.)
        .size(22.)
        .render(ctx);
        let mut y = 79.;
        let mut lines = vec![
            self.title.clone(),
            format!("Proposed · {}", self.proposed),
            format!("From · {}", r.attendee),
            format!("To organizer · {}", r.organizer),
            format!("Response · {}", r.answer.partstat()),
            if self.verified {
                "Mail verified the sender domain."
            } else {
                "Mail has not verified the sender."
            }
            .into(),
        ];
        if let Some(scope) = &self.scope {
            lines.insert(1, scope.clone());
        }
        if let Some(change) = &changed {
            lines = vec![
                self.title.clone(),
                format!("You reviewed · {}", self.proposed),
            ];
            if let Some(scope) = &self.scope {
                lines.insert(1, scope.clone());
            }
            match change {
                LatestInvitation::Update {
                    sequence,
                    when,
                    location,
                    ..
                } => {
                    lines.push(format!("Latest · {when}"));
                    if !location.is_empty() {
                        let mut venue: String = location.chars().take(53).collect();
                        if venue.len() < location.len() {
                            venue.push('…');
                        }
                        lines.push(venue);
                    }
                    lines.push(format!("Organizer revision {sequence}"));
                }
                LatestInvitation::Conflict { sequence } => {
                    lines.push(format!("Conflicting updates for revision {sequence}."));
                    lines.push("Wait for the organizer to clarify the meeting.".into());
                }
            }
            lines.push("Your response has not been sent.".into());
        }
        for line in lines {
            let chars: Vec<_> = line.chars().collect();
            for chunk in chars.chunks(54) {
                Label::new(chunk.iter().collect::<String>())
                    .at(18., y)
                    .render(ctx);
                y += 23.;
            }
            y += 9.;
        }
        let back = self.actions.clone();
        Button::secondary("Cancel")
            .at(18., self.height as f32 - 58.)
            .width(110.)
            .on_click(move || {
                let _ = back.send(Action::Cancel);
            })
            .render(ctx);
        let tx = self.actions.clone();
        if let Some(change) = changed {
            if let LatestInvitation::Update { msg, .. } = change {
                Button::primary("Review latest invitation")
                    .at(348., self.height as f32 - 58.)
                    .width(225.)
                    .on_click(move || {
                        if let Some(press) = pergola::clipboard::current_activation() {
                            let _ = tx.send(Action::ReviewLatest(msg, press));
                        }
                    })
                    .render(ctx);
            }
            return;
        }
        Button::primary("Send response")
            .at(398., self.height as f32 - 58.)
            .width(174.)
            .on_click(move || {
                let _ = tx.send(Action::Send);
            })
            .render(ctx);
    }
}
pub struct ResponseWindow {
    win: Window,
    app: App<Confirmation>,
    rx: mpsc::Receiver<Action>,
    changed: pergola::Mutable<Option<LatestInvitation>>,
    reply: Reply,
    msg: u64,
    blob: mail_store::Hash,
    until: Instant,
}
impl ResponseWindow {
    pub fn new(
        engine: &Engine,
        msg: u64,
        blob: mail_store::Hash,
        reply: Reply,
    ) -> Result<Self, String> {
        let m = engine.store.message(msg).map_err(|e| e.to_string())?;
        let source = m
            .summary
            .invite
            .as_ref()
            .ok_or("Invitation no longer available")?;
        if source.method != mail_schema::InviteMethod::Request
            || source.cancelled
            || m.full != Some(blob)
            || reply.uid != source.uid
            || reply.sequence != source.sequence
            || !reply.organizer.eq_ignore_ascii_case(&source.organizer)
            || !portcullis_ipc::calendar::address(&reply.attendee)
        {
            return Err("Invitation identity changed. Review it again.".into());
        }
        let proposed = mail_engine::invitations::proposed_time(source)
            .ok_or("The proposed time could not be verified")?;
        let title = source.title.clone();
        let scope = engine
            .invitation_scope(msg, &reply)
            .map_err(|e| e.to_string())?;
        let verified = matches!(
            m.summary.sender_check,
            mail_schema::SenderCheck::Verified { .. }
        );
        let extra = (title.chars().count().saturating_sub(1) / 54
            + (reply.attendee.len() + 7) / 54
            + (reply.organizer.len() + 14) / 54
            + (proposed.len() + 11) / 54) as u32;
        let height = 365
            + extra * 23
            + scope.as_ref().map_or(0, |s| {
                32 + 23 * (s.chars().count().saturating_sub(1) / 54) as u32
            });
        let win = Window::open(&WindowDesc::new(
            "Mail · Invitation response",
            SizeSpec::Fixed(600, height),
        ))
        .map_err(|e| e.to_string())?;
        let (actions, rx) = mpsc::channel();
        let dirty = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let changed = pergola::Mutable::with_dirty(None, dirty.clone());
        let app = App::new_with_flag(
            Confirmation {
                changed: changed.clone(),
                reply: reply.clone(),
                title,
                scope,
                proposed,
                verified,
                actions,
                height,
            },
            dirty,
        );
        Ok(Self {
            win,
            app,
            rx,
            changed,
            reply,
            msg,
            blob,
            until: Instant::now() + Duration::from_secs(calendar::REVIEW_SECONDS),
        })
    }
    pub fn tick(&mut self, engine: &Engine) -> Option<Finished> {
        if Instant::now() >= self.until {
            return Some(Finished::Note("Invitation review expired".into()));
        }
        if !matches!(self.win.step(&mut self.app,Some(Duration::ZERO)),Ok(r) if !matches!(r,StepResult::Closed))
        {
            return Some(Finished::Note("Response cancelled".into()));
        }
        match self.rx.try_recv().ok()? {
            Action::Cancel => Some(Finished::Note("Response cancelled".into())),
            Action::ReviewLatest(msg, press) => Some(Finished::Review(msg, press)),
            Action::Send if self.changed.get_cloned().is_some() => None,
            Action::Send => match engine.send_invitation_reply(self.msg, self.blob, &self.reply) {
                Ok(_) => Some(Finished::Note("Response queued in Mail".into())),
                Err(mail_engine::Error::InvitationChanged(change)) => {
                    self.changed.set(Some(change));
                    None
                }
                Err(e) => Some(Finished::Note(e.to_string())),
            },
        }
    }
}
