use super::{validate_records, Store, MAX_ENTRIES};
use crate::{
    import::{Prepared, ROW_LIMIT},
    Entry, Kind, UndoReceipt,
};
use chrono::NaiveDate;
use std::time::{Duration, Instant};
use uuid::Uuid;

pub(super) struct Pending {
    token: Uuid,
    revision: u64,
    created: Instant,
    rows: Vec<Entry>,
    date: NaiveDate,
}

// A copied series may already have an exception on its first date.
fn first_agenda_date(row: &Entry, zone: chrono_tz::Tz) -> Result<NaiveDate, String> {
    let mut date = row.date;
    if row.kind == Kind::Weekly {
        for skipped in &row.excluded {
            if *skipped != date {
                break;
            }
            date = date
                .checked_add_days(chrono::Days::new(7))
                .ok_or("Calendar date is out of range")?;
        }
    }
    row.fields_on(date).agenda_date(zone).map_err(str::to_owned)
}

impl Store {
    pub fn prepare_import(
        &mut self,
        revision: u64,
        hash: [u8; 32],
        mut rows: Vec<Entry>,
    ) -> Result<Prepared, String> {
        self.preview = None;
        if self.state.revision != revision {
            return Err("Calendar changed. Open the file again.".into());
        }
        if let Some(previous) = self
            .state
            .entries
            .iter()
            .find(|e| e.import_hash == Some(hash))
        {
            return Ok(Prepared::Existing(first_agenda_date(
                previous,
                self.viewing_zone(),
            )?));
        }
        if rows.is_empty()
            || rows.len() > ROW_LIMIT
            || self.state.entries.len() + rows.len() > MAX_ENTRIES
        {
            return Err("Calendar import entry limit reached".into());
        }
        // Validate this batch on its own before combining it with local data.
        // A format worker cannot point an exception at an existing meeting.
        validate_records(&rows)?;
        for row in &mut rows {
            if row.kind == Kind::Hold
                || row.import_hash.is_some()
                || !row.span.valid()
                || self.state.entries.iter().any(|e| e.id == row.id)
            {
                return Err("Invalid imported event".into());
            }
            row.import_hash = Some(hash);
        }
        let date = first_agenda_date(&rows[0], self.viewing_zone())?;
        let token = Uuid::new_v4();
        self.preview = Some(Pending {
            token,
            revision,
            created: Instant::now(),
            rows,
            date,
        });
        Ok(Prepared::New(token))
    }

    pub fn discard_import(&mut self, token: Uuid) {
        if self.preview.as_ref().is_some_and(|p| p.token == token) {
            self.preview = None;
        }
    }

    pub fn save_import(
        &mut self,
        revision: u64,
        token: Uuid,
    ) -> Result<(NaiveDate, UndoReceipt), String> {
        let p = self
            .preview
            .as_ref()
            .filter(|p| {
                p.token == token
                    && p.revision == revision
                    && revision == self.state.revision
                    && p.created.elapsed() < Duration::from_secs(300)
            })
            .ok_or("Import preview expired. Open the file again.")?;
        let date = p.date;
        let mut entries = self.state.entries.clone();
        entries.extend_from_slice(&p.rows);
        self.commit_event(revision, entries, date)
            .map(|receipt| (date, receipt))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Span;

    #[test]
    fn previews_are_private_expiring_and_cannot_claim_existing_meetings() {
        let work_dir = tempfile::tempdir().unwrap();
        let personal_dir = tempfile::tempdir().unwrap();
        let mut work = Store::open(work_dir.path()).unwrap();
        let mut personal = Store::open(personal_dir.path()).unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 11, 2).unwrap();
        let weekly = Entry {
            id: Uuid::new_v4(),
            date,
            span: Span {
                start: 540,
                end: 585,
            },
            kind: Kind::Weekly,
            title: "Studio review".into(),
            zone: None,
            excluded: vec![date],
            source: None,
            import_hash: None,
        };
        let Prepared::New(token) = work
            .prepare_import(0, [1; 32], vec![weekly.clone()])
            .unwrap()
        else {
            panic!("expected new preview");
        };
        assert!(personal.save_import(0, token).is_err());
        work.preview.as_mut().unwrap().created -= Duration::from_secs(300);
        assert!(work.save_import(0, token).is_err());
        assert!(!work_dir.path().join("calendar.json").exists());
        let Prepared::New(fresh) = work
            .prepare_import(0, [1; 32], vec![weekly.clone()])
            .unwrap()
        else {
            panic!("expected fresh preview");
        };
        assert!(work.save_import(0, token).is_err());
        work.save_import(0, fresh).unwrap();
        let moved = Entry {
            id: Uuid::new_v4(),
            kind: Kind::Occurrence {
                series: weekly.id,
                original: date,
            },
            excluded: vec![],
            ..weekly
        };
        assert!(work.prepare_import(1, [2; 32], vec![moved]).is_err());
        assert_eq!(work.state.revision, 1);
        assert_eq!(work.state.entries.len(), 1);
        assert!(!personal_dir.path().join("calendar.json").exists());
    }
}
