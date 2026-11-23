//! Mail receives one response choice on the channel for its selected part.
use super::*;
use atrium_calendar::{
    import::{Outcome, Parse, Parsed, FILE_LIMIT, FRAME_LIMIT},
    invitation::{Proposal, Review},
    wire,
};
use portcullis_ipc::calendar::{self, Answer, Source};
use std::{
    fs::File,
    os::{fd::OwnedFd, unix::net::UnixStream},
    path::Path,
};

pub(super) struct Incoming {
    pub stream: UnixStream,
    pub source: Source,
    pub proposal: Proposal,
}
#[derive(Clone)]
pub(super) struct Sheet {
    source: Source,
    proposal: Proposal,
    review: Review,
    revision: u64,
    stream: Arc<UnixStream>,
}
fn receive(file: File) -> Result<Incoming, String> {
    let mut stream = UnixStream::from(OwnedFd::from(file));
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|_| "Mail connection unavailable")?;
    let source: Source = calendar::read(&mut stream).map_err(|_| "Invalid Mail handoff")?;
    if !source.valid() {
        return Err("Invalid Mail account details".into());
    }
    let mut fds = portcullis_ipc::recv_fds(&stream, 1).map_err(|_| "Missing invitation part")?;
    let fd = fds.pop().ok_or("Missing invitation part")?;
    if !limen::fdpass::sealed(&fd) {
        return Err("Invitation part is not sealed".into());
    }
    let file = File::from(fd);
    if !file
        .metadata()
        .is_ok_and(|m| m.is_file() && m.len() <= FILE_LIMIT as u64)
    {
        return Err("Invitation exceeds the file limit".into());
    }
    let workers = atrium_worker::Workers::from_root(Path::new("/"), false)
        .map_err(|_| "Invitation worker unavailable")?;
    let parsed: Parsed = workers
        .max_answer(FRAME_LIMIT + 16)
        .call("ical", &Parse { invitation: true }, Some(file))
        .map_err(|_| "Invitation worker failed")?;
    let proposal = match serde_json::from_slice(&parsed.schedule)
        .map_err(|_| "Invalid invitation worker response")?
    {
        Outcome::Invitation(p) => p,
        Outcome::Blocked(atrium_calendar::import::Issue::Recurrence) => {
            return Err("Recurring invitations need a later Calendar build.".into())
        }
        Outcome::Blocked(i) => return Err(i.message().into()),
        _ => return Err("Expected a scheduling request".into()),
    };
    Ok(Incoming {
        source,
        proposal,
        stream,
    })
}
pub(super) fn listen() -> mpsc::Receiver<Result<Incoming, String>> {
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let Ok(Some(mut ch)) = atrium_open::Channel::connect() else {
            return;
        };
        while let Ok(Some(f)) = ch.recv(None) {
            if !f.write && f.mime == calendar::MIME && tx.send(receive(f.file)).is_err() {
                break;
            }
        }
    });
    rx
}
impl Incoming {
    pub fn review(self, path: &Path, revision: u64) -> Result<Sheet, String> {
        let request = Request::ReviewInvitation {
            revision,
            source: self.source.clone(),
            proposal: self.proposal.clone(),
        };
        let review = wire::exchange(path, &request)?;
        Ok(Sheet {
            source: self.source,
            proposal: self.proposal,
            stream: Arc::new(self.stream),
            review,
            revision,
        })
    }
}
fn schedule(row: &Entry) -> String {
    if row.kind == Kind::AllDay {
        return format!("{} · All day", row.date);
    }
    format!(
        "{} · {}–{} · {}",
        row.date,
        clock(row.span.start),
        clock(row.span.end),
        row.zone.map_or("Floating", |z| z.name.name())
    )
}
fn viewing_schedule(row: &Entry, view: chrono_tz::Tz) -> String {
    let Some(zone) = row.zone.filter(|z| z.name != view) else {
        return schedule(row);
    };
    let Ok((start, end)) = zone.interval(row.date, row.span, false) else {
        return schedule(row);
    };
    let start = start.with_timezone(&view);
    let end = end.with_timezone(&view);
    let end_format = if start.date_naive() == end.date_naive() {
        "%H:%M %:z"
    } else {
        "%Y-%m-%d %H:%M %:z"
    };
    format!(
        "{}–{} · {}",
        start.format("%Y-%m-%d · %H:%M %:z"),
        end.format(end_format),
        view
    )
}
impl Sheet {
    pub fn save(&self) -> Result<Request, String> {
        Ok(Request::ApplyInvitation {
            revision: self.revision,
            token: self.review.token.ok_or("Review this proposal again")?,
        })
    }
    pub fn reply(&self, path: &Path, answer: Answer) -> Result<(), String> {
        let token = self
            .review
            .token
            .ok_or("This proposal cannot be answered")?;
        let _: () = wire::exchange(
            path,
            &Request::CheckInvitation {
                revision: self.revision,
                token,
            },
        )?;
        let origin = self.proposal.origin(&self.source)?;
        let reply = calendar::Reply {
            answer,
            uid: origin.uid,
            organizer: origin.organizer,
            sequence: origin.sequence,
            attendee: origin.attendee,
        };
        calendar::write(
            &mut self.stream.try_clone().map_err(|_| "Mail disconnected")?,
            &reply,
        )
        .map_err(|_| "Reopen this invitation in Mail to respond".into())
    }
    pub fn render(&self, planner: &Planner, ctx: &mut Ctx) {
        let p = &self.proposal;
        let line =
            |ctx: &mut Ctx, y, size, s: String| Label::new(s).at(18., y).size(size).render(ctx);
        line(
            ctx,
            66.,
            22.,
            if self.review.previous.is_some() {
                "Review invitation change"
            } else {
                "Review invitation"
            }
            .into(),
        );
        line(ctx, 105., 18., p.row.title.clone());
        line(
            ctx,
            143.,
            14.,
            format!("Mail sender · {}", self.source.from),
        );
        line(ctx, 170., 14., format!("Organizer · {}", p.organizer));
        if let Ok(origin) = p.origin(&self.source) {
            line(ctx, 197., 14., format!("Invited as · {}", origin.attendee));
        }
        if let Some(old) = &self.review.previous {
            line(
                ctx,
                238.,
                15.,
                format!(
                    "Current · {}",
                    viewing_schedule(old, planner.viewing_zone())
                ),
            );
            if let Some(o) = &old.invitation {
                line(
                    ctx,
                    263.,
                    14.,
                    format!(
                        "Current place · {}",
                        if o.location.is_empty() {
                            "Unspecified"
                        } else {
                            &o.location
                        }
                    ),
                );
            }
        }
        line(
            ctx,
            302.,
            16.,
            format!(
                "Proposed · {}",
                viewing_schedule(&p.row, planner.viewing_zone())
            ),
        );
        line(
            ctx,
            333.,
            14.,
            format!(
                "Proposed place · {}",
                if p.location.is_empty() {
                    "Unspecified"
                } else {
                    &p.location
                }
            ),
        );
        line(ctx, 377., 14., self.review.notice.clone());
        if self.review.token.is_some() {
            line(
                ctx,
                421.,
                14.,
                "Choose a response to review in Mail before sending.".into(),
            );
            for (i, (label, answer)) in [
                ("Accept…", Answer::Accept),
                ("Maybe…", Answer::Maybe),
                ("Decline…", Answer::Decline),
            ]
            .into_iter()
            .enumerate()
            {
                planner.button(
                    ctx,
                    18. + i as f32 * 126.,
                    449.,
                    116.,
                    label,
                    Action::ReplyInvitation(answer),
                );
            }
            if self.review.can_save {
                planner.button(
                    ctx,
                    456.,
                    535.,
                    224.,
                    if self.review.previous.is_some() {
                        "Apply change on device"
                    } else {
                        "Add to this calendar"
                    },
                    Action::ApplyInvitation,
                );
            }
        }
        planner.button(
            ctx,
            18.,
            535.,
            130.,
            "Back to calendar",
            Action::CloseImport,
        );
    }
}
