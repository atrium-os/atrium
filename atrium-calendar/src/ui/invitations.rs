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
    let proposal =
        match serde_json::from_slice(&parsed.schedule)
            .map_err(|_| "Invalid invitation worker response")?
        {
            Outcome::Invitation(p) => p,
            Outcome::Blocked(atrium_calendar::import::Issue::Recurrence) => return Err(
                "Calendar supports simple weekly requests and changes to one original occurrence."
                    .into(),
            ),
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
        if self.proposal.row.is_none() {
            return Err("A cancellation has no RSVP".into());
        }
        let reply = calendar::Reply {
            answer,
            scope: self.proposal.scope(),
            uid: self.proposal.uid.clone(),
            organizer: self.proposal.organizer.clone(),
            sequence: self.proposal.sequence,
            attendee: self.review.attendee.clone(),
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
            if p.row.is_none() {
                "Review cancellation"
            } else if self.review.previous.is_some() {
                "Review invitation change"
            } else {
                "Review invitation"
            }
            .into(),
        );
        let title = p
            .row
            .as_ref()
            .or(self.review.previous.as_ref())
            .map_or("Meeting", |e| &e.title);
        line(ctx, 98., 17., title.into());
        let scope = if let Some(o) = p.original {
            format!(
                "One occurrence · originally {} {:02}:{:02} · {}",
                o.date,
                o.minute / 60,
                o.minute % 60,
                o.zone.map_or("Floating", |z| z.name())
            )
        } else if self.review.weekly || p.weekly_cancellation {
            "Whole weekly series".into()
        } else if p.row.is_none() && self.review.previous.is_none() {
            "No matching saved meeting".into()
        } else {
            "One meeting".into()
        };
        line(ctx, 121., 16., scope);
        line(
            ctx,
            143.,
            14.,
            format!("Mail sender · {}", self.source.from),
        );
        line(ctx, 170., 14., format!("Organizer · {}", p.organizer));
        line(
            ctx,
            197.,
            14.,
            format!("Invited as · {}", self.review.attendee),
        );
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
        if let Some(row) = &p.row {
            line(
                ctx,
                302.,
                16.,
                format!(
                    "Proposed · {}",
                    viewing_schedule(row, planner.viewing_zone())
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
        } else {
            line(
                ctx,
                302.,
                16.,
                if p.original.is_some() {
                    "Remove only this occurrence on this device."
                } else if self.review.weekly {
                    "Remove the weekly series and its exceptions on this device."
                } else {
                    "Remove this meeting on this device."
                }
                .into(),
            );
        }
        if let Some(base) =
            self.review.previous.as_ref().filter(|e| {
                e.kind == Kind::Weekly && p.original.is_none() && !e.excluded.is_empty()
            })
        {
            let verb = if p.row.is_none() {
                "removed"
            } else {
                "preserved"
            };
            line(
                ctx,
                356.,
                14.,
                format!(
                    "Dates with saved exceptions {} · {}",
                    verb,
                    base.excluded.len()
                ),
            );
        }
        line(ctx, 377., 14., self.review.notice.clone());
        if self.review.token.is_some() && p.row.is_some() {
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
        }
        if self.review.token.is_some() && self.review.can_save {
            planner.button(
                ctx,
                456.,
                535.,
                224.,
                if p.row.is_none() {
                    if p.original.is_some() {
                        "Remove this occurrence"
                    } else if self.review.weekly {
                        "Remove whole series"
                    } else {
                        "Remove meeting"
                    }
                } else if self.review.weekly && p.original.is_none() {
                    "Save whole series"
                } else if p.original.is_some() {
                    "Apply to this occurrence"
                } else if self.review.previous.is_some() {
                    "Apply change on device"
                } else {
                    "Add to this calendar"
                },
                Action::ApplyInvitation {
                    cancellation: p.row.is_none(),
                },
            );
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
