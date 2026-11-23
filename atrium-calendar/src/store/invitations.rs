use super::*;
use crate::invitation::{Proposal, Review};
use portcullis_ipc::calendar::{Source, REVIEW_SECONDS};
use std::time::Instant;

pub(super) struct Pending {
    token: Uuid,
    revision: u64,
    created: Instant,
    row: Entry,
    previous: Option<usize>,
    can_save: bool,
}
impl Store {
    pub fn review_invitation(
        &mut self,
        revision: u64,
        source: &Source,
        p: Proposal,
    ) -> Result<Review, String> {
        self.invite = None;
        if revision != self.state.revision {
            return Err("Calendar changed. Review this invitation again.".into());
        }
        let origin = p.origin(source)?;
        let previous = self.state.entries.iter().position(|e| {
            e.invitation.as_ref().is_some_and(|o| {
                o.account == origin.account
                    && o.attendee == origin.attendee
                    && o.organizer == origin.organizer
                    && o.uid == origin.uid
            })
        });
        let old = previous.map(|i| self.state.entries[i].clone());
        let mut review = Review {
            previous: old.clone(),
            can_save: true,
            token: None,
            notice: String::new(),
        };
        let blocked = if !source.from.eq_ignore_ascii_case(&origin.organizer)
            || !source.reply_destination_matches
        {
            Some("The Mail sender or reply address differs from the organizer.")
        } else if let Some(ref e) = old {
            let o = e.invitation.as_ref().unwrap();
            if origin.sequence < o.sequence {
                Some("An older revision. Your current meeting is shown below.")
            } else if origin.sequence == o.sequence {
                if o == &origin && e.kind == p.row.kind {
                    review.can_save = false;
                    review.notice = "Already saved in this calendar.".into();
                    None
                } else {
                    Some("Conflicting proposals use the same revision.")
                }
            } else if !source.verified {
                Some("Mail could not verify the sender of this change.")
            } else if o.accepted != e.fields_on(e.date) {
                Some("You edited this meeting locally. Review your calendar before replacing it.")
            } else {
                None
            }
        } else {
            None
        };
        if let Some(reason) = blocked {
            review.notice = reason.into();
            return Ok(review);
        }
        if !source.verified {
            review.notice = "Mail has not verified the sender.".into();
        }
        let mut row = p.row;
        row.id = old.map(|e| e.id).unwrap_or_else(Uuid::new_v4);
        row.invitation = Some(origin);
        let token = Uuid::new_v4();
        self.invite = Some(Pending {
            token,
            revision,
            created: Instant::now(),
            row,
            previous,
            can_save: review.can_save,
        });
        review.token = Some(token);
        Ok(review)
    }

    pub fn check_invitation(&self, revision: u64, token: Uuid) -> Result<(), String> {
        self.invite
            .as_ref()
            .filter(|p| {
                p.token == token
                    && p.revision == revision
                    && self.state.revision == revision
                    && p.created.elapsed().as_secs() < REVIEW_SECONDS
            })
            .map(|_| ())
            .ok_or_else(|| "This review expired. Reopen the invitation in Mail.".into())
    }

    pub fn apply_invitation(
        &mut self,
        revision: u64,
        token: Uuid,
    ) -> Result<(NaiveDate, UndoReceipt), String> {
        self.check_invitation(revision, token)?;
        let p = self.invite.as_ref().unwrap();
        if !p.can_save {
            return Err("This revision is already saved".into());
        }
        let date = p
            .row
            .fields_on(p.row.date)
            .agenda_date(self.viewing_zone())?;
        let mut rows = self.state.entries.clone();
        if let Some(i) = p.previous {
            rows[i] = p.row.clone();
        } else {
            rows.push(p.row.clone());
        }
        self.commit_event(revision, rows, date).map(|u| (date, u))
    }
}
