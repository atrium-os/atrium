//! Calendar returns a choice. Only the final Send button enqueues it.
use mail_engine::{invitations::Selected, Engine};
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

pub fn offer(selected: Selected, activation: [u8; 16]) -> Result<Option<Reply>, String> {
    fn handoff(selected: Selected, activation: [u8; 16]) -> io::Result<Option<Reply>> {
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
        let mut session = UnixStream::from(fd);
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
    handoff(selected, activation).map_err(|e| e.to_string())
}

struct Confirmation {
    reply: Reply,
    title: String,
    proposed: String,
    scope: Option<String>,
    verified: bool,
    actions: mpsc::Sender<bool>,
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
        Label::new("Review your invitation response")
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
                let _ = back.send(false);
            })
            .render(ctx);
        let tx = self.actions.clone();
        Button::primary("Send response")
            .at(398., self.height as f32 - 58.)
            .width(174.)
            .on_click(move || {
                let _ = tx.send(true);
            })
            .render(ctx);
    }
}
pub struct ResponseWindow {
    win: Window,
    app: App<Confirmation>,
    rx: mpsc::Receiver<bool>,
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
        let app = App::new(Confirmation {
            reply: reply.clone(),
            title,
            scope,
            proposed,
            verified,
            actions,
            height,
        });
        Ok(Self {
            win,
            app,
            rx,
            reply,
            msg,
            blob,
            until: Instant::now() + Duration::from_secs(calendar::REVIEW_SECONDS),
        })
    }
    pub fn tick(&mut self, engine: &Engine) -> Option<String> {
        if Instant::now() >= self.until {
            return Some("Invitation review expired".into());
        }
        if !matches!(self.win.step(&mut self.app,Some(Duration::ZERO)),Ok(r) if !matches!(r,StepResult::Closed))
        {
            return Some("Response cancelled".into());
        }
        let send = self.rx.try_recv().ok()?;
        Some(if send {
            match engine.send_invitation_reply(self.msg, self.blob, &self.reply) {
                Ok(_) => "Response queued in Mail".into(),
                Err(e) => e.to_string(),
            }
        } else {
            "Response cancelled".into()
        })
    }
}
