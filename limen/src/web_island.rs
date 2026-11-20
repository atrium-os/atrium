//! Reports from a broker-fetched web island (navigator-islands.md §5).
use serde::{Deserialize, Serialize};

pub const MAX_FRAME: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum EndReason { Closed, Failed }

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Report<'a> {
    Started { origin: &'a str },
    Title(&'a str),
    Resize { width: u16, height: u16 },
    Ended(EndReason),
}

impl Report<'_> {
    fn valid(&self) -> bool {
        match self {
            Self::Started { origin } => origin.len() <= 255 && origin.strip_prefix("https://").is_some_and(|host|
                !host.is_empty() && host.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':'))),
            Self::Title(text) => text.len() <= 256 && !text.chars().any(char::is_control),
            Self::Resize { width, height } => (1..=16384).contains(width) && (1..=16384).contains(height),
            Self::Ended(_) => true,
        }
    }

    pub fn encode(self) -> Option<Vec<u8>> {
        if !self.valid() { return None; }
        postcard::to_stdvec(&self).ok()
    }
}

#[derive(Clone, Copy, Default)]
pub enum Lifecycle {
    #[default]
    Waiting,
    Running,
    Ended,
}

impl Lifecycle {
    /// Reject malformed or out-of-order reports without changing the session.
    pub fn receive<'a>(&mut self, bytes: &'a [u8]) -> Option<Report<'a>> {
        if bytes.len() > MAX_FRAME { return None; }
        let (report, rest) = postcard::take_from_bytes::<Report>(bytes).ok()?;
        if !rest.is_empty() || !report.valid() { return None; }
        *self = match (*self, report) {
            (Self::Waiting, Report::Started { .. }) => Self::Running,
            (Self::Running, Report::Title(_) | Report::Resize { .. }) => Self::Running,
            (Self::Running, Report::Ended(_)) => Self::Ended,
            _ => return None,
        };
        Some(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_limits_apply_on_receipt_as_well_as_emission() {
        let long = "x".repeat(257);
        for report in [Report::Title(&long), Report::Title("a\nb"), Report::Resize { width: 0, height: 1 },
                       Report::Resize { width: 1, height: 16385 }, Report::Started { origin: "https://a/path" }] {
            assert!(report.encode().is_none());
            let mut state = if matches!(report, Report::Started { .. }) { Lifecycle::Waiting } else { Lifecycle::Running };
            assert!(state.receive(&postcard::to_stdvec(&report).unwrap()).is_none());
        }
        assert!(Report::Title(&"x".repeat(256)).encode().is_some());
        assert!(Report::Resize { width: 16384, height: 1 }.encode().is_some());
    }
}
