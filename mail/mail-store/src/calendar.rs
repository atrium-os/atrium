//! Mail keeps only message IDs here. The verified source and calendar part
//! are re-read from the ordinary inbox before delivery.
use super::*;
use mail_schema::{InviteMethod, SenderCheck};

pub(super) const PENDING: TableDefinition<u64, u64> = TableDefinition::new("calendar_updates");

pub(super) const REVISIONS: TableDefinition<(u32, &str, u64), u32> =
    TableDefinition::new("calendar_revisions");

fn candidate(m: &Message) -> bool {
    m.full.is_some()
        && matches!(m.summary.sender_check, SenderCheck::Verified { .. })
        && m.summary.invite.as_ref().is_some_and(|i| {
            (i.sequence > 0 || i.method == InviteMethod::Cancel)
                && (!i.repeats || i.method == InviteMethod::Request)
                && matches!(i.method, InviteMethod::Request | InviteMethod::Cancel)
        })
}

/// Compare parser-normalized single-occurrence properties. Floating slots keep
/// their civil clock. Without a saved fold choice, either instant of a repeated
/// local hour can refer to that occurrence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OccurrenceSlot {
    Floating(chrono::NaiveDateTime),
    Zoned { first: i64, last: i64 },
}

impl OccurrenceSlot {
    pub fn parse(scope: &str) -> Option<Self> {
        Self::from_params(scope.strip_prefix("RECURRENCE-ID")?)
    }

    pub fn exdate(scope: &str) -> Option<Self> {
        Self::from_params(scope.strip_prefix("EXDATE")?)
    }

    pub fn range_cutoff(scope: &str) -> Option<Self> {
        Self::from_params(scope.strip_prefix("RECURRENCE-ID;RANGE=THISANDFUTURE")?)
    }

    fn from_params(params: &str) -> Option<Self> {
        use chrono::{NaiveDateTime, TimeZone};
        let (property, value) = params.split_once(':')?;
        let stamp = value.strip_suffix('Z').unwrap_or(value);
        let wall = NaiveDateTime::parse_from_str(stamp, "%Y%m%dT%H%M%S").ok()?;
        let zone = match property {
            "" if stamp == value => return Some(Self::Floating(wall)),
            "" => chrono_tz::UTC,
            name if stamp == value => name.strip_prefix(";TZID=")?.parse().ok()?,
            _ => return None,
        };
        let times = zone.from_local_datetime(&wall);
        Some(Self::Zoned {
            first: times.earliest()?.timestamp(),
            last: times.latest()?.timestamp(),
        })
    }

    /// A repeated local hour can cover the reply if either possible instant does.
    pub fn on_or_after(self, cutoff: Self) -> bool {
        match (self, cutoff) {
            (Self::Floating(slot), Self::Floating(start)) => slot >= start,
            (Self::Zoned { last, .. }, Self::Zoned { first, .. }) => last >= first,
            _ => false,
        }
    }

    pub fn matches(self, other: Self) -> bool {
        match (self, other) {
            (Self::Floating(a), Self::Floating(b)) => a == b,
            (Self::Zoned { first: a, last: b }, Self::Zoned { first: c, last: d }) => {
                a == c || a == d || b == c || b == d
            }
            _ => false,
        }
    }
}

/// Eligible immutable parts at the moment Mail checks an individual response.
#[derive(Debug, PartialEq, Eq)]
pub struct CalendarRevisions {
    account: AccountId,
    uid: String,
    sequence: u32,
    organizer: String,
    pub messages: Vec<(u32, MsgId, Hash)>,
}

impl CalendarRevisions {
    pub fn recheck(&self, writer: &Writer<'_>) -> Result<()> {
        if self
            != &writer.calendar_revisions(
                self.account,
                &self.uid,
                self.sequence,
                &self.organizer,
            )?
        {
            return Err(Error::CalendarChanged);
        }
        Ok(())
    }
}

impl Writer<'_> {
    pub fn calendar_revisions(
        &self,
        account: AccountId,
        uid: &str,
        sequence: u32,
        organizer: &str,
    ) -> Result<CalendarRevisions> {
        let index = self.tx.open_table(REVISIONS)?;
        let folders = self.tx.open_table(FOLDERS)?;
        let mut messages = Vec::new();
        for row in index.range((account, uid, 0)..=(account, uid, u64::MAX))? {
            let (key, seq) = row?;
            if seq.value() < sequence {
                continue;
            }
            let id = key.value().2;
            let Some(m) = self.get_msg(id)? else { continue };
            if !candidate(&m) || !m.held.is_empty() {
                continue;
            }
            let i = m.summary.invite.as_ref().unwrap();
            if i.uid != uid || !i.organizer.eq_ignore_ascii_case(organizer) {
                continue;
            }
            for (folder, _) in &m.locations {
                let Some(row) = folders.get(*folder)? else {
                    continue;
                };
                if dec::<Folder>(row.value())?.role == Role::Inbox {
                    messages.push((i.sequence, id, m.full.unwrap()));
                    break;
                }
            }
        }
        messages.sort_unstable_by(|a, b| b.cmp(a));
        Ok(CalendarRevisions {
            account,
            uid: uid.into(),
            organizer: organizer.into(),
            sequence,
            messages,
        })
    }

    pub(super) fn queue_calendar(&mut self, id: MsgId, m: &Message) -> Result<()> {
        if self
            .index_calendar_revision(id, m)?
            .is_some_and(|seq| seq > 0)
        {
            self.mark_calendar_pending(id)?;
        }
        Ok(())
    }

    fn index_calendar_revision(&mut self, id: MsgId, m: &Message) -> Result<Option<u32>> {
        if candidate(m) {
            let i = m.summary.invite.as_ref().unwrap();
            self.tx
                .open_table(REVISIONS)?
                .insert((m.account, i.uid.as_str(), id), i.sequence)?;
            return Ok((!i.repeats).then_some(i.sequence));
        }
        Ok(None)
    }

    fn mark_calendar_pending(&mut self, id: MsgId) -> Result<()> {
        let generation = Store::next_id(self.tx, "calendar-generation")?;
        self.tx.open_table(PENDING)?.insert(id, generation)?;
        Ok(())
    }

    // Updates can reach Mail before the recipient saves the original. A
    // completed handoff replays later revisions without asking what they saved.
    pub fn replay_calendar_revisions(&mut self, msg: MsgId) -> Result<()> {
        let Some(m) = self.get_msg(msg)? else {
            return Ok(());
        };
        let Some(i) = &m.summary.invite else {
            return Ok(());
        };
        let revisions = self.tx.open_table(REVISIONS)?;
        let key = i.uid.as_str();
        for item in revisions.range((m.account, key, 0)..=(m.account, key, u64::MAX))? {
            let (k, v) = item?;
            if v.value() > i.sequence
                && self
                    .get_msg(k.value().2)?
                    .is_some_and(|m| m.summary.invite.is_some_and(|i| !i.repeats))
            {
                self.mark_calendar_pending(k.value().2)?;
            }
        }
        Ok(())
    }

    pub fn finish_calendar_delivery(&mut self, id: MsgId, generation: u64) -> Result<()> {
        let mut pending = self.tx.open_table(PENDING)?;
        if pending.get(id)?.is_some_and(|g| g.value() == generation) {
            pending.remove(id)?;
        }
        Ok(())
    }
}

impl Store {
    /// A bounded upgrade pass also handles inboxes created before delivery existed.
    pub fn index_calendar_page(&self) -> Result<bool> {
        self.index_calendar_through(u64::MAX)
    }

    /// A final Send includes the backlog present when the check started.
    /// Messages arriving later already enter the revision index on write.
    pub fn index_calendar_before_send(&self) -> Result<()> {
        let through = self.last_message()?;
        while self.index_calendar_through(through)? {}
        Ok(())
    }

    fn index_calendar_through(&self, through: MsgId) -> Result<bool> {
        const CURSOR: &str = "calendar-weekly-backfill";
        let tx = self.db.begin_read()?;
        let meta = tx.open_table(META)?;
        let cursor = meta.get(CURSOR)?.map_or(0, |n| n.value());
        if cursor >= through {
            return Ok(false);
        }
        let page = self.messages_after(cursor, 64)?;
        let last = if page.len() == 64 {
            page.last().unwrap().0
        } else {
            u64::MAX
        };
        self.write(|w| {
            // The background worker can finish this page while Send is reading it.
            if w.tx
                .open_table(META)?
                .get(CURSOR)?
                .is_some_and(|v| v.value() > cursor)
            {
                return Ok(());
            }
            // Previously scanned messages only need the wider index. Preserve
            // completed background deliveries and outstanding receipt generations.
            let meta = w.tx.open_table(META)?;
            let mut previous = 0;
            for key in ["calendar-backfill", "calendar-group-backfill"] {
                previous = previous.max(meta.get(key)?.map_or(0, |n| n.value()));
            }
            drop(meta);
            for (id, _) in &page {
                if let Some(m) = w.get_msg(*id)? {
                    if *id <= previous {
                        w.index_calendar_revision(*id, &m)?;
                    } else {
                        w.queue_calendar(*id, &m)?;
                    }
                }
            }
            w.tx.open_table(META)?.insert(CURSOR, last)?;
            Ok(())
        })?;
        Ok(last < through)
    }

    pub fn pending_calendar(&self, after: MsgId) -> Result<Vec<(MsgId, u64)>> {
        let tx = self.db.begin_read()?;
        let pending = tx.open_table(PENDING)?;
        pending
            .range((std::ops::Bound::Excluded(after), std::ops::Bound::Unbounded))?
            .take(16)
            .map(|row| {
                let (id, generation) = row?;
                Ok((id.value(), generation.value()))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::OccurrenceSlot;

    #[test]
    fn upgrade_indexes_old_cancellations_and_weekly_requests_without_replaying_deliveries() {
        use super::*;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let account = store.add_account("dev@fixture.invalid").unwrap();
        let inbox = store.folder_id(account, "INBOX", Role::Inbox).unwrap();
        let add = |uid, sequence, weekly| {
            let blob = store.put_blob(&[uid as u8]).unwrap();
            let summary = mail_schema::Summary {
                invite: Some(mail_schema::Invite {
                    uid: "group".into(),
                    sequence,
                    method: if weekly {
                        InviteMethod::Request
                    } else {
                        InviteMethod::Cancel
                    },
                    organizer: "mina@fixture.invalid".into(),
                    cancelled: !weekly,
                    title: String::new(),
                    start: None,
                    end: None,
                    location: String::new(),
                    attendees: 0,
                    my_status: None,
                    repeats: weekly,
                }),
                sender_check: SenderCheck::Verified {
                    domain: "fixture.invalid".into(),
                    by_provider: false,
                },
                ..Default::default()
            };
            store
                .write(|w| {
                    w.put(
                        inbox,
                        account,
                        uid,
                        Incoming {
                            server_id: None,
                            size: 1,
                            internal_date: None,
                            flags: 0,
                            blob,
                            full: true,
                            summary,
                        },
                    )
                })
                .unwrap()
        };
        let zero = add(1, 0, false);
        let other = add(2, 3, false);
        let weekly = add(3, 5, true);
        let pending = store.pending_calendar(0).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0, other);
        store
            .write(|w| {
                w.tx.open_table(REVISIONS)?
                    .remove((account, "group", zero))?;
                w.tx.open_table(REVISIONS)?
                    .remove((account, "group", weekly))?;
                w.tx.open_table(META)?
                    .insert("calendar-group-backfill", u64::MAX)?;
                Ok(())
            })
            .unwrap();
        let revisions = || {
            store
                .write(|w| w.calendar_revisions(account, "group", 0, "mina@fixture.invalid"))
                .unwrap()
        };
        assert_eq!(revisions().messages.len(), 1);
        store.index_calendar_before_send().unwrap();
        assert!(revisions()
            .messages
            .iter()
            .any(|&(sequence, id, _)| sequence == 0 && id == zero));
        assert!(revisions()
            .messages
            .iter()
            .any(|&(sequence, id, _)| sequence == 5 && id == weekly));
        store.write(|w| w.replay_calendar_revisions(other)).unwrap();
        assert_eq!(store.pending_calendar(0).unwrap(), pending);
    }

    #[test]
    fn cutoff_is_inclusive_and_compares_original_clocks() {
        let covered = |slot: &str, start: &str| {
            OccurrenceSlot::parse(slot).unwrap().on_or_after(
                OccurrenceSlot::range_cutoff(&format!("RECURRENCE-ID;RANGE=THISANDFUTURE{start}"))
                    .unwrap(),
            )
        };
        let start = ";TZID=Asia/Kolkata:20261216T003000";
        assert!(!covered("RECURRENCE-ID:20261215T185959Z", start));
        assert!(covered("RECURRENCE-ID:20261215T190000Z", start));
        assert!(covered("RECURRENCE-ID:20261215T190001Z", start));
        assert!(covered("RECURRENCE-ID:20261215T140000", ":20261215T140000"));
        assert!(!covered("RECURRENCE-ID:20261215T140000", start));
        assert!(covered(
            "RECURRENCE-ID:20261101T060000Z",
            ";TZID=America/New_York:20261101T013000"
        ));
    }

    #[test]
    fn original_slots_cross_midnight_and_distinguish_floating_times_and_folds() {
        let slot = |s| OccurrenceSlot::parse(s).unwrap();
        let utc = slot("RECURRENCE-ID:20261208T220000Z");
        assert!(utc.matches(slot("RECURRENCE-ID;TZID=Asia/Kolkata:20261209T033000")));
        assert!(!utc.matches(slot("RECURRENCE-ID:20261208T220000")));
        let fold = slot("RECURRENCE-ID;TZID=America/New_York:20261101T013000");
        let early = slot("RECURRENCE-ID:20261101T053000Z");
        let late = slot("RECURRENCE-ID:20261101T063000Z");
        assert!(fold.matches(early) && fold.matches(late) && !early.matches(late));
        assert!(
            OccurrenceSlot::parse("RECURRENCE-ID;TZID=America/New_York:20260308T023000").is_none()
        );
        assert!(
            OccurrenceSlot::parse("RECURRENCE-ID;RANGE=THISANDFUTURE:20261208T220000Z").is_none()
        );
    }
}
