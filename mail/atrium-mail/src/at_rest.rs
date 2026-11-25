//! Mail's store at rest (mail.md §6.4a A2): whether this device keeps
//! Mail's data encrypted, and what Mail agrees to keep when it does not.
//!
//! portcullisd refuses to start Mail on plain storage unless the machine
//! opts out or Personal is moving in, and says which it was in
//! `ATRIUM_APP_DATA`. A named persona's account (Work) is never added or
//! synced into a plain store; Personal works, with a standing line.

pub use atrium_storage::{app_data, AppData};

/// The standing line in Settings and under the account list.
pub const PLAIN_LINE: &str = "Mail on this device is not encrypted";

/// Why a named persona's mail is not kept here.
pub const NAMED_REFUSED: &str = "This device doesn't encrypt Mail's storage, so this persona's accounts are not added or synced here.";

/// Why nothing syncs when Mail could not learn its persona on plain storage.
pub const PERSONA_UNKNOWN: &str = "Mail couldn't tell which persona it is, and this device doesn't encrypt its storage, so nothing syncs.";

/// Whether accounts may be added and synced here: always on sealed
/// storage; on anything else only once the persona is known to be
/// Personal. An unknown persona or an unknown answer refuses.
pub fn may_keep(data: AppData, persona: &Result<Option<String>, String>) -> Result<(), &'static str> {
    if data.sealed() {
        return Ok(());
    }
    match persona {
        Ok(None) => Ok(()),
        Ok(Some(_)) => Err(NAMED_REFUSED),
        Err(_) => Err(PERSONA_UNKNOWN),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_storage_keeps_every_persona() {
        for p in [Ok(None), Ok(Some("work".to_string())), Err("no answer".to_string())] {
            assert_eq!(may_keep(AppData::Sealed, &p), Ok(()));
        }
    }

    #[test]
    fn plain_storage_keeps_only_personal() {
        for d in [AppData::Plain, AppData::Unknown] {
            assert_eq!(may_keep(d, &Ok(None)), Ok(()));
            assert_eq!(may_keep(d, &Ok(Some("work".into()))), Err(NAMED_REFUSED));
            assert_eq!(may_keep(d, &Err("launcher unreachable".into())), Err(PERSONA_UNKNOWN));
        }
    }
}
