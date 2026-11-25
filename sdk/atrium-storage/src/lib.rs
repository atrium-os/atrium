//! Whether this app's data is encrypted on the device (mail.md §6.4a A2).
//!
//! portcullisd asks the kernel about the app's data folder before the app
//! starts, refuses the launch when it is plain and plain is not allowed,
//! and passes the answer as `ATRIUM_APP_DATA`. This is for what the app
//! shows and what it agrees to keep: the enforcement is portcullisd's, so an
//! app reading a wrong value only misleads itself.

/// What the kernel said about this app's data folder at launch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppData {
    /// In the person's (or persona's) key group, with the group open.
    Sealed,
    /// Plain, which portcullisd allowed: a machine without Tessera key
    /// groups that opts out, or Personal's one login while its folders move
    /// into their group.
    Plain,
    /// No answer: not launched by portcullisd (a host build, a test), or a
    /// jail with no persistent app data. Treat as plain for anything that
    /// must be sealed.
    Unknown,
}

/// The variable portcullisd sets.
pub const VAR: &str = "ATRIUM_APP_DATA";

impl AppData {
    /// Parse the variable's value; anything else is `Unknown`.
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            Some("sealed") => AppData::Sealed,
            Some("plain") => AppData::Plain,
            _ => AppData::Unknown,
        }
    }

    pub fn sealed(self) -> bool {
        self == AppData::Sealed
    }
}

/// This process's answer, from its environment.
pub fn app_data() -> AppData {
    AppData::parse(std::env::var(VAR).ok().as_deref())
}

#[cfg(test)]
mod tests {
    use super::AppData;

    #[test]
    fn only_the_two_known_values_count() {
        assert_eq!(AppData::parse(Some("sealed")), AppData::Sealed);
        assert_eq!(AppData::parse(Some("plain")), AppData::Plain);
        for other in [None, Some(""), Some("Sealed"), Some("sealed ")] {
            assert_eq!(AppData::parse(other), AppData::Unknown);
        }
        assert!(AppData::Sealed.sealed() && !AppData::Unknown.sealed());
    }
}
