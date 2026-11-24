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
    page: usize,
    proposed_last: Option<NaiveDate>,
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
        let proposed_last = self
            .proposal
            .limit
            .zip(self.proposal.row.as_ref())
            .map(|(limit, row)| limit.last(row))
            .transpose()?;
        Ok(Sheet {
            proposed_last,
            source: self.source,
            proposal: self.proposal,
            stream: Arc::new(self.stream),
            page: 0,
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
    fn detail_count(&self) -> usize {
        self.proposal.exceptions.len() + self.proposal.cancelled_dates.len()
    }
    pub(super) fn set_page(&mut self, page: usize) {
        if page <= self.detail_count() && page <= self.page + 1 {
            self.page = page;
        }
    }

    fn reviewed_token(&self) -> Result<uuid::Uuid, String> {
        if self.page != self.detail_count() {
            return Err("Review each affected date first.".into());
        }
        self.review
            .token
            .ok_or_else(|| "Review this proposal again".into())
    }
    pub fn save(&self) -> Result<Request, String> {
        Ok(Request::ApplyInvitation {
            revision: self.revision,
            token: self.reviewed_token()?,
        })
    }
    pub fn reply(&self, path: &Path, answer: Answer) -> Result<(), String> {
        let token = self.reviewed_token()?;
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
        let grouped = !p.cancelled_dates.is_empty();
        let scope = if grouped {
            format!(
                "{} occurrences in this weekly series",
                p.cancelled_dates.len()
            )
        } else if let Some(o) = p.original {
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
        let scope = if self.review.weekly && p.original.is_none() && !grouped {
            let prior_bound = self
                .review
                .previous
                .as_ref()
                .and_then(|e| e.invitation.as_ref())
                .and_then(|o| o.last);
            let visible_bound = if p.row.is_some() && p.limit.is_none() && prior_bound.is_some() {
                " · proposed with no end date".to_string()
            } else if p.row.is_some() {
                match p.limit.zip(self.proposed_last) {
                    Some((atrium_calendar::WeeklyLimit::Count(n), date)) => {
                        format!(" · {n} meetings · last {date}")
                    }
                    Some((_, date)) => format!(" · last regular meeting {date}"),
                    None => String::new(),
                }
            } else {
                self.review
                    .previous
                    .as_ref()
                    .and_then(|e| e.invitation.as_ref())
                    .and_then(|o| o.last)
                    .map_or_else(String::new, |d| format!(" · last regular meeting {d}"))
            };
            format!("{scope}{visible_bound}")
        } else {
            scope
        };
        line(ctx, 121., 16., scope);
        if self.page == 0 {
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
            if let Some(old) = self.review.previous.as_ref().filter(|_| !grouped) {
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
                    if old.kind == Kind::Weekly
                        && p.original.is_none()
                        && p.row.is_some()
                        && (o.limit.is_some() || p.limit.is_some())
                    {
                        line(
                            ctx,
                            282.,
                            14.,
                            o.last.map_or_else(
                                || "Current series · no end date".into(),
                                |d| format!("Current series · last {d}"),
                            ),
                        );
                    }
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
                    if grouped {
                        "Review each date before removing these meetings."
                    } else if p.original.is_some() {
                        "Remove only this occurrence on this device."
                    } else if self.review.weekly {
                        "Remove the weekly series and its exceptions on this device."
                    } else {
                        "Remove this meeting on this device."
                    }
                    .into(),
                );
            }
            if let Some(base) = self.review.previous.as_ref().filter(|e| {
                !grouped && e.kind == Kind::Weekly && p.original.is_none() && !e.excluded.is_empty()
            }) {
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
                        base.excluded
                            .iter()
                            .filter(|d| !p.exceptions.iter().any(|m| m.original.date == **d))
                            .count()
                    ),
                );
            }
        } else {
            let index = self.page - 1;
            let saved = self
                .review
                .previous_dates
                .get(index)
                .and_then(Option::as_ref);
            let (original, heading, entry, place) = if grouped {
                (
                    p.cancelled_dates[index].original,
                    "Cancellation",
                    saved,
                    saved
                        .and_then(|e| e.invitation.as_ref())
                        .map(|o| o.location.as_str()),
                )
            } else {
                let m = &p.exceptions[index];
                (
                    m.original,
                    "Included move",
                    Some(&m.row),
                    Some(m.location.as_str()),
                )
            };
            line(
                ctx,
                160.,
                19.,
                format!("{heading} {} of {}", self.page, self.detail_count()),
            );
            if let Some(e) = entry {
                line(ctx, 194., 16., e.title.clone());
            }
            line(
                ctx,
                222.,
                15.,
                format!(
                    "Originally · {} {} · {}",
                    original.date,
                    clock(original.minute),
                    original.zone.map_or("Floating", |z| z.name())
                ),
            );
            if let Some(e) = saved {
                line(
                    ctx,
                    248.,
                    15.,
                    format!("Current · {}", viewing_schedule(e, planner.viewing_zone())),
                );
            }
            if grouped {
                if self.review.can_save {
                    line(
                        ctx,
                        277.,
                        16.,
                        "Remove this saved occurrence with the other reviewed dates.".into(),
                    );
                }
            } else {
                line(
                    ctx,
                    277.,
                    16.,
                    format!(
                        "Proposed · {}",
                        viewing_schedule(&p.exceptions[index].row, planner.viewing_zone())
                    ),
                );
            }
            if let Some(place) = place {
                line(
                    ctx,
                    307.,
                    15.,
                    format!(
                        "Place · {}",
                        if place.is_empty() {
                            "Unspecified"
                        } else {
                            place
                        }
                    ),
                );
            }
            planner.button(
                ctx,
                18.,
                337.,
                145.,
                "Previous",
                Action::InvitationPage(self.page - 1),
            );
        }
        if self.page < self.detail_count() {
            let label = if self.page == 0 {
                if grouped {
                    format!("Review {} dates", self.detail_count())
                } else {
                    format!("Review {} included moves", p.exceptions.len())
                }
            } else {
                if grouped { "Next date" } else { "Next move" }.into()
            };
            planner.button(
                ctx,
                250.,
                399.,
                220.,
                &label,
                Action::InvitationPage(self.page + 1),
            );
        }
        line(ctx, 377., 14., self.review.notice.clone());
        if self.page == self.detail_count() && self.review.token.is_some() && p.row.is_some() {
            line(
                ctx,
                421.,
                14.,
                if p.exceptions.is_empty() {
                    "Choose a response to review in Mail before sending."
                } else {
                    "Your response applies to the whole series. Review it in Mail before sending."
                }
                .into(),
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
        if self.page == self.detail_count() && self.review.token.is_some() && self.review.can_save {
            planner.button(
                ctx,
                456.,
                535.,
                224.,
                if p.row.is_none() {
                    if grouped {
                        "Remove reviewed dates"
                    } else if p.original.is_some() {
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
