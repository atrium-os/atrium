//! Remember the latest verified Mail proposal for an accepted one-off meeting.
use super::*;
use crate::invitation::{MeetingKey, Proposal, Review};
use chrono::{DateTime, Timelike, Utc};

const CONFLICT: &str =
    "Conflicting organizer changes use the same revision. A newer invitation is needed.";

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Ledger(BTreeMap<Uuid, (MeetingKey, MeetingChange)>);

impl Ledger {
    pub(super) fn load(dir: &Path) -> io::Result<Self> {
        let file = match File::open(dir.join("meeting-changes.json")) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e),
        };
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(io::Error::other("Meeting review storage limit exceeded"));
        }
        let ledger: Self = serde_json::from_slice(&bytes)?;
        if ledger.0.len() > MAX_ENTRIES {
            return Err(io::Error::other("Too many saved meeting reviews"));
        }
        Ok(ledger)
    }

    fn for_entry(&self, row: &Entry) -> Option<&MeetingChange> {
        let saved = row.invitation.as_ref()?;
        let (key, latest) = self.0.get(&row.id)?;
        (*key == saved.key && latest.sequence > saved.sequence).then_some(latest)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MeetingChange {
    pub sequence: u32,
    pub conflicted: bool,
    pub proposed: Option<EventEdit>,
    pub location: String,
}

impl MeetingChange {
    fn material(&self, entry: &Entry) -> bool {
        let Some(saved) = &entry.invitation else {
            return false;
        };
        self.conflicted
            || self.proposed.as_ref().is_none_or(|p| {
                p.date != saved.accepted.date
                    || p.span != saved.accepted.span
                    || p.zone != saved.accepted.zone
                    || self.location != saved.location
            })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChangeDetail {
    pub total: usize,
    pub index: usize,
    pub accepted: EventEdit,
    pub old_location: String,
    pub all_day: bool,
    pub latest: MeetingChange,
}

impl Store {
    pub(super) fn remember_change(
        &mut self,
        p: &Proposal,
        review: Review,
    ) -> Result<Review, String> {
        let Some(base) = review.previous.as_ref().filter(|e| {
            matches!(e.kind, Kind::Appointment | Kind::AllDay) && review.token.is_some()
        }) else {
            return Ok(review);
        };
        let mut next = MeetingChange {
            conflicted: false,
            sequence: p.sequence,
            proposed: p.row.as_ref().map(|row| row.fields_on(row.date)),
            location: p.location.clone(),
        };
        if let Some(last) = self.meeting_changes.for_entry(base) {
            if last.sequence > next.sequence {
                return Ok(review
                    .blocked("A newer organizer change has already been reviewed in Calendar."));
            }
            if last.sequence == next.sequence {
                if last.conflicted {
                    return Ok(review.blocked(CONFLICT));
                }
                if *last == next {
                    return Ok(review);
                }
                next.conflicted = true;
                next.proposed = None;
                next.location.clear();
            }
        }
        if !review.can_save {
            return Ok(review);
        }
        // Compact references to meetings since accepted or removed. The schedule
        // and its Undo receipt are untouched by previewing a Mail delivery.
        let mut ledger = Ledger::default();
        for row in &self.state.entries {
            if let Some(latest) = self.meeting_changes.for_entry(row) {
                ledger.0.insert(
                    row.id,
                    (row.invitation.as_ref().unwrap().key.clone(), latest.clone()),
                );
            }
        }
        let conflicted = next.conflicted;
        ledger.0.insert(
            base.id,
            (base.invitation.as_ref().unwrap().key.clone(), next),
        );
        let bytes = serde_json::to_vec(&ledger).map_err(|_| "Cannot encode meeting reviews")?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err("Meeting review storage limit reached".into());
        }
        let mut tmp =
            NamedTempFile::new_in(&self.dir).map_err(|_| "Cannot stage meeting reviews")?;
        tmp.write_all(&bytes)
            .and_then(|_| tmp.as_file().sync_all())
            .map_err(|_| "Cannot write meeting reviews")?;
        tmp.persist(self.dir.join("meeting-changes.json"))
            .map_err(|_| "Cannot publish meeting reviews")?;
        self.meeting_changes = ledger;
        File::open(&self.dir)
            .and_then(|d| d.sync_all())
            .map_err(|_| "Review durability is uncertain. Reopen the invitation.")?;
        Ok(if conflicted {
            review.blocked(CONFLICT)
        } else {
            review
        })
    }

    pub(super) fn outstanding(&self) -> impl Iterator<Item = (&Entry, &MeetingChange)> {
        self.state.entries.iter().filter_map(|row| {
            let latest = self.meeting_changes.for_entry(row)?;
            latest.material(row).then_some((row, latest))
        })
    }

    pub fn meeting_change(&self, index: usize, now: DateTime<Utc>) -> Result<ChangeDetail, String> {
        let mut changes: Vec<_> = self.outstanding().collect();
        let local = now.with_timezone(&self.viewing_zone());
        changes.sort_by_cached_key(|(row, latest)| {
            let date = latest
                .proposed
                .as_ref()
                .unwrap_or(&row.invitation.as_ref().unwrap().accepted);
            if let Some((start, end)) = date
                .zone
                .and_then(|z| z.interval(date.date, date.span, false).ok())
            {
                let shown = start.with_timezone(&self.viewing_zone());
                (
                    end <= now,
                    shown.date_naive(),
                    shown.hour() * 60 + shown.minute(),
                    row.id,
                )
            } else {
                let ended = (date.date, u32::from(date.span.end))
                    <= (local.date_naive(), local.hour() * 60 + local.minute());
                (ended, date.date, u32::from(date.span.start), row.id)
            }
        });
        let (row, latest) = changes
            .get(index)
            .ok_or("This meeting change is no longer pending.")?;
        let saved = row.invitation.as_ref().unwrap();
        Ok(ChangeDetail {
            total: changes.len(),
            index,
            accepted: saved.accepted.clone(),
            old_location: saved.location.clone(),
            all_day: row.kind == Kind::AllDay,
            latest: (*latest).clone(),
        })
    }
}
