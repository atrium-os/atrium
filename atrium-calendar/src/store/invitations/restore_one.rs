//! Reopen one original weekly slot while preserving the cancelled weeks on either side.
use super::super::invitation_skips::LocalMove;
use super::*;
use crate::invitation::{CancelledInterval, RangeDate};

pub(super) struct SingleDate<'a> {
    gap: &'a CancelledInterval,
    sequence: u32,
}

impl<'a> SingleDate<'a> {
    pub(super) fn review(
        gap: &'a CancelledInterval,
        saved: &Origin,
        p: &Proposal,
        source: &Source,
    ) -> Result<Self, &'static str> {
        // prepare() also handles children synthesized from a master or a range.
        // Only Mail's separately selected single-occurrence delivery can reopen a slot.
        if p.row.is_none()
            || p.this_and_future
            || !source.verified
            || source
                .delivery
                .as_ref()
                .is_none_or(|d| d.scope != p.scope())
        {
            return Err("This cancelled interval needs a verified request for one original date.");
        }
        // Version 12 stored the restart revision on the rule, outside the interval.
        let sequence = gap
            .sequence
            .or_else(|| {
                saved
                    .ranges
                    .iter()
                    .find(|r| r.anchor == gap.restart)
                    .map(|r| r.sequence)
            })
            .ok_or("The earlier calendar has no restart revision for this date.")?;
        if p.sequence
            <= sequence
                .max(saved.sequence)
                .max(saved.range_sequence.unwrap_or(0))
        {
            return Err("A newer organizer revision is required to restore this date.");
        }
        Ok(Self { gap, sequence })
    }

    pub(super) fn prepare(
        self,
        parent: &mut Origin,
        original: NaiveDate,
        rows: &[Entry],
        p: &Proposal,
        review: &mut Review,
    ) -> Result<Option<LocalMove>, &'static str> {
        let at = parent
            .cancelled_intervals
            .binary_search_by_key(&self.gap.first, |g| g.first)
            .map_err(|_| "Missing reviewed interval")?;
        parent
            .cancelled_intervals
            .splice(at..=at, self.gap.except(original, self.sequence));
        let private = self.gap.copies.get(&original).and_then(|id| {
            rows.iter()
                .find(|r| r.id == *id && r.kind == Kind::Appointment && r.invitation.is_none())
        });
        review.previous = None;
        review.reinstating = true;
        review.range_summary = Some(format!(
            "Restore only {original}. Other cancelled dates stay absent."
        ));
        if self.gap.sequence.is_none() {
            review.notice =
                "Older calendar data · existing private appointments stay in place.".into();
        }
        review.range_dates.push(RangeDate {
            original,
            before: None,
            after: p.row.as_ref().map(|r| r.fields_on(r.date)),
            private_copy: private.map(|r| r.fields_on(r.date)),
        });
        Ok(private.map(|r| LocalMove::private_copy(original, r.id)))
    }
}

impl CancelledInterval {
    /// Two half-open weekly intervals retain the slots on either side of this date.
    fn except(&self, date: NaiveDate, sequence: u32) -> impl Iterator<Item = Self> + '_ {
        let next = date
            .checked_add_days(chrono::Days::new(7))
            .unwrap_or(self.restart);
        [(self.first, date), (next, self.restart)]
            .into_iter()
            .filter(|(first, end)| first < end)
            .map(move |(first, restart)| Self {
                first,
                restart,
                sequence: Some(sequence),
                copies: self
                    .copies
                    .range(first..restart)
                    .map(|(&d, &id)| (d, id))
                    .collect(),
            })
    }
}
