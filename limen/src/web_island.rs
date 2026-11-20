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
    BeforeUnload { query: u32 },
}

impl Report<'_> {
    fn valid(&self) -> bool {
        match self {
            Self::Started { origin } => origin.len() <= 255 && origin.strip_prefix("https://").is_some_and(|host|
                !host.is_empty() && host.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':'))),
            Self::Title(text) => text.len() <= 256 && !text.chars().any(char::is_control),
            Self::Resize { width, height } => (1..=16384).contains(width) && (1..=16384).contains(height),
            Self::Ended(_) => true,
            Self::BeforeUnload { query } => *query != 0,
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
    Running { last_query: u32 },
    AwaitingUnload(u32),
    Ended,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct UnloadAnswer { pub query: u32, pub leave: bool }

impl UnloadAnswer {
    pub fn encode(self) -> Vec<u8> {
        let mut bytes = vec![b'U'];
        bytes.extend(self.query.to_le_bytes());
        bytes.push(u8::from(self.leave));
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let &[b'U', a, b, c, d, flag @ 0..=1] = bytes else { return None };
        Some(Self { query: u32::from_le_bytes([a, b, c, d]), leave: flag == 1 })
    }
}

impl Lifecycle {
    pub fn answer(&mut self, answer: UnloadAnswer) -> bool {
        if !matches!(*self, Self::AwaitingUnload(query) if query == answer.query) { return false; }
        *self = Self::Running { last_query: answer.query };
        true
    }

    /// Reject malformed or out-of-order reports without changing the session.
    pub fn receive<'a>(&mut self, bytes: &'a [u8]) -> Option<Report<'a>> {
        if bytes.len() > MAX_FRAME { return None; }
        let (report, rest) = postcard::take_from_bytes::<Report>(bytes).ok()?;
        if !rest.is_empty() || !report.valid() { return None; }
        *self = match (*self, report) {
            (Self::Waiting, Report::Started { .. }) => Self::Running { last_query: 0 },
            (running @ Self::Running { .. }, Report::Title(_) | Report::Resize { .. }) => running,
            (Self::Running { last_query }, Report::BeforeUnload { query }) if query > last_query => Self::AwaitingUnload(query),
            (Self::Running { .. }, Report::Ended(_)) => Self::Ended,
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
            let mut state = if matches!(report, Report::Started { .. }) { Lifecycle::Waiting } else { Lifecycle::Running { last_query: 0 } };
            assert!(state.receive(&postcard::to_stdvec(&report).unwrap()).is_none());
        }
        assert!(Report::Title(&"x".repeat(256)).encode().is_some());
        assert!(Report::Resize { width: 16384, height: 1 }.encode().is_some());
    }

    #[test]
    fn generated_report_sequences_preserve_session_order() {
        use Report::*;
        let reports = [Started { origin: "https://widget.example" }, Title(""), Title("文"),
                       Resize { width: 1, height: 16384 }, Ended(EndReason::Closed), Ended(EndReason::Failed)];
        // Every five-report sequence, including every prefix and repeated terminal reports.
        for mut sequence in 0..reports.len().pow(5) {
            let mut state = Lifecycle::default();
            let (mut started, mut ended) = (false, false);
            for _ in 0..5 {
                let report = reports[sequence % reports.len()];
                sequence /= reports.len();
                let allowed = !ended && (matches!(report, Started { .. }) != started);
                let wire = report.encode().unwrap();
                assert_eq!(state.receive(&wire), allowed.then_some(report));
                if allowed {
                    started = true;
                    ended = matches!(report, Ended(_));
                }
            }
        }
    }

    #[test]
    fn generated_field_boundaries_are_checked_by_the_decoder() {
        for n in 0..=MAX_FRAME + 1 {
            let text = "x".repeat(n);
            let report = Report::Title(&text);
            let bytes = postcard::to_stdvec(&report).unwrap();
            assert_eq!((Lifecycle::Running { last_query: 0 }).receive(&bytes), (n <= 256).then_some(report));
            assert!((Lifecycle::Running { last_query: 0 }).receive(&bytes[..bytes.len()-1]).is_none());
        }
    }

}
