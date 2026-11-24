//! Reconcile a weekly master's EXDATE set with exceptions in this persona.
use super::*;
use crate::invitation::{IncludedMove, SkipDecision, SkipEffect, SkipReview};

pub(super) struct LocalMove {
    original: NaiveDate,
    series: Uuid,
    child: Uuid,
}

pub(super) fn prepare(
    row: &mut Entry,
    previous: Option<&Entry>,
    rows: &mut Vec<Entry>,
    moves: &[IncludedMove],
    replay: bool,
    review: &mut Vec<SkipReview>,
) -> Result<Vec<LocalMove>, &'static str> {
    let mut conflicts = Vec::new();
    let origin = row.invitation.as_mut().ok_or("Missing series identity")?;
    let prior = previous.and_then(|r| r.invitation.as_ref());
    let old_owned = prior.map_or(&[][..], |o| o.owned_skips.as_slice());

    // Only release exclusions that the prior EXDATE actually created.
    for old in prior.into_iter().flat_map(|o| &o.skips) {
        if origin.skips.iter().any(|s| s.date == old.date) || !old_owned.contains(&old.date) {
            continue;
        }
        row.excluded.retain(|d| *d != old.date);
        if !moves.iter().any(|m| m.original.date == old.date) {
            review.push(SkipReview {
                original: *old,
                current: None,
                effect: SkipEffect::Restore,
            });
        }
    }
    for &original in &origin.skips {
        let date = original.date;
        let kind = Kind::Occurrence {
            series: row.id,
            original: date,
        };
        let child = rows.iter().find(|e| e.kind == kind).cloned();
        let current = child
            .clone()
            .or_else(|| previous.and_then(|r| r.inherited(date)))
            .or_else(|| {
                previous.filter(|r| r.appears(date)).map(|r| {
                    let mut entry = r.clone();
                    entry.date = date;
                    entry
                })
            });
        let effect = if let Some(child) = child {
            let saved = child.invitation.as_ref();
            if saved.is_some_and(|o| {
                o.original.is_some_and(|id| id != original)
                    || o.original.is_some() && o.sequence >= origin.sequence
            }) {
                return Err("A newer or conflicting occurrence needs separate review.");
            }
            let local = saved
                .is_none_or(|o| o.original.is_none() || o.accepted != child.fields_on(child.date));
            if local {
                if replay {
                    SkipEffect::KeepLocalMove
                } else {
                    conflicts.push(LocalMove {
                        original: date,
                        series: row.id,
                        child: child.id,
                    });
                    SkipEffect::ChooseLocalMove
                }
            } else {
                rows.retain(|e| e.id != child.id);
                origin.owned_skips.push(date);
                SkipEffect::Skip
            }
        } else if let Err(at) = row.excluded.binary_search(&date) {
            row.excluded.insert(at, date);
            origin.owned_skips.push(date);
            SkipEffect::Skip
        } else {
            if old_owned.contains(&date) {
                origin.owned_skips.push(date);
            }
            SkipEffect::AlreadyAbsent
        };
        review.push(SkipReview {
            original,
            current,
            effect,
        });
    }
    Ok(conflicts)
}

pub(super) fn resolve(
    rows: &mut Vec<Entry>,
    conflicts: &[LocalMove],
    choices: &[SkipDecision],
) -> Result<(), String> {
    if choices.len() != conflicts.len() {
        return Err("Choose what to do with each local move.".into());
    }
    for conflict in conflicts {
        let decision = choices
            .iter()
            .find(|c| c.original == conflict.original)
            .ok_or("Missing decision for a local move")?;
        if !decision.keep {
            rows.retain(|r| r.id != conflict.child);
            let origin = rows
                .iter_mut()
                .find(|r| r.id == conflict.series)
                .and_then(|r| r.invitation.as_mut())
                .ok_or("Missing reviewed series")?;
            origin.owned_skips.push(conflict.original);
        }
    }
    Ok(())
}
