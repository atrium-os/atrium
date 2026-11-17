//! Mail's window (docs/spec/mail.md §15.2), as far as M2 goes: the
//! places (each account's server folders), the message list of the chosen
//! folder, newest first, and a reader that shows the plain text. Read-only.
//!
//! The list reads the index a page at a time as rows come into view, so a
//! folder of any size opens at once (§6.4); the reader parses the message
//! in its own parse jail through the engine (§7.1) and never in this
//! process.

pub mod compose;
pub mod links;
pub mod plain;
pub mod sender;
pub mod view;

pub use view::{MailView, Ui};

/// A date for the list: the time today, the day and month this year,
/// else the full date. UTC until Atrium's time zones reach apps.
pub fn date_text(t: i64, now: i64) -> String {
    let (y, m, d, hh, mm) = civil(t);
    let (ny, nm, nd, _, _) = civil(now);
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    if (y, m, d) == (ny, nm, nd) {
        format!("{hh:02}:{mm:02}")
    } else if y == ny {
        format!("{d} {}", MONTHS[m as usize - 1])
    } else {
        format!("{d} {} {y}", MONTHS[m as usize - 1])
    }
}

/// The reader's full date: "17 Nov 2026, 09:41".
pub fn full_date(t: i64) -> String {
    let (y, m, d, hh, mm) = civil(t);
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!("{d} {} {y}, {hh:02}:{mm:02}", MONTHS[m as usize - 1])
}

/// (year, month, day, hour, minute) of seconds since the epoch, UTC
/// (Howard Hinnant's civil_from_days).
fn civil(t: i64) -> (i64, u32, u32, u32, u32) {
    let days = t.div_euclid(86_400);
    let rem = t.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d, (rem / 3600) as u32, (rem % 3600 / 60) as u32)
}

/// A folder's name as the places show it: the role's word for the
/// special ones, else the last part of its server path.
pub fn folder_label(name: &str, role: mail_store::Role) -> String {
    use mail_store::Role::*;
    match role {
        Inbox => "Inbox".into(),
        Sent => "Sent".into(),
        Drafts => "Drafts".into(),
        Archive => "Archive".into(),
        Junk => "Junk".into(),
        Trash => "Trash".into(),
        All => "All mail".into(),
        Flagged => "Flagged".into(),
        Other => name.rsplit(['/', '.']).next().unwrap_or(name).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        let now = 1_795_425_660; // 2026-11-23 09:21 UTC
        assert_eq!(date_text(now - 60, now), "09:20");
        assert_eq!(date_text(now - 3 * 86_400, now), "20 Nov");
        assert_eq!(date_text(now - 400 * 86_400, now), "19 Oct 2025");
        assert_eq!(full_date(now), "23 Nov 2026, 09:21");
    }

    #[test]
    fn labels() {
        assert_eq!(folder_label("INBOX", mail_store::Role::Inbox), "Inbox");
        assert_eq!(folder_label("Work/Projects/Q4", mail_store::Role::Other), "Q4");
        assert_eq!(folder_label("[Gmail]/All Mail", mail_store::Role::All), "All mail");
    }
}
