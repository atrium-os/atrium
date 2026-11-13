//! The daemon's [`Registry`]: the kernel's peer uid, resolved through the
//! launch registry portcullisd writes (`portcullis-peer`), and the fixed
//! uids of arcad and ostiarius. Never anything the peer says.
//!
//! Fail-closed choices, as in arcad's resolver (arca.md §4.1):
//!
//! - The registry is read fresh for every connection; a read failure is
//!   [`Label::Unknown`].
//! - Unsigned apps are unknown. A private RP ID (§13) is keyed by the
//!   publisher, so the all-zero "unsigned" publisher would make every
//!   unsigned app with the same app id the same relying party.
//! - A system uid that the launcher also gave to an app is unknown.
//! - Apps get no web RP IDs yet: `identity.webauthn` is verified by Opifex
//!   at install (§13), which is not built. Until then an app may use only
//!   its private RP ID; the web broker may use any.

use std::path::PathBuf;

use crate::label::{AppId, Label, Registry};

#[derive(Debug, Clone)]
pub struct Resolver {
    pub registry: PathBuf,
    pub arca_uid: Option<u32>,
    pub ostiarius_uid: Option<u32>,
}

/// `SIGILLUM_WEB_BROKER`: `app_id publisher-hex` (64 hex), as
/// `ARCA_WEB_BROKER`. An all-zero publisher is refused.
pub fn parse_web_broker(s: &str) -> Option<AppId> {
    let (app, hex) = s.trim().split_once(' ')?;
    let hex = hex.trim();
    if hex.len() != 64 || app.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut publisher = [0u8; 32];
    for (i, b) in publisher.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    (publisher != [0; 32]).then(|| AppId { publisher, app_id: app.to_string() })
}

impl Resolver {
    /// The label for `uid` given the registry's text.
    pub fn label_in(&self, reg: &portcullis_peer::AppRegistry, uid: u32) -> Label {
        let system = Some(uid) == self.arca_uid || Some(uid) == self.ostiarius_uid;
        let Some((user, app_id)) = reg.resolve(uid) else {
            return match () {
                _ if Some(uid) == self.arca_uid => Label::Arca,
                _ if Some(uid) == self.ostiarius_uid => Label::Ostiarius,
                _ => Label::Unknown,
            };
        };
        if system || reg.is_session(uid) {
            return Label::Unknown;
        }
        match reg.publisher(uid) {
            Some(Some(publisher)) if publisher != [0; 32] => Label::App {
                user: user.to_string(),
                app: AppId { publisher, app_id: app_id.to_string() },
                rp_ids: Vec::new(),
            },
            _ => Label::Unknown,
        }
    }
}

impl Registry for Resolver {
    fn label(&self, uid: u32) -> Label {
        match portcullis_peer::AppRegistry::load(self.registry.to_str().unwrap_or("")) {
            Ok(reg) => self.label_in(&reg, uid),
            Err(_) => Label::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &str = "abababababababababababababababababababababababababababababababab";

    fn r() -> Resolver {
        Resolver { registry: "/nonexistent".into(), arca_uid: Some(65003), ostiarius_uid: Some(65010) }
    }

    #[test]
    fn labels() {
        let reg = portcullis_peer::AppRegistry::parse(&format!(
            "70001 alice org.atrium.navigator {P}\n70002 alice com.example.unsigned\n65003 bob evil {P}\n70003 alice forum session\n"
        ));
        let r = r();
        match r.label_in(&reg, 70001) {
            Label::App { user, app, rp_ids } => {
                assert_eq!(user, "alice");
                assert_eq!(app.app_id, "org.atrium.navigator");
                assert_eq!(app.publisher, [0xab; 32]);
                assert!(rp_ids.is_empty());
            }
            l => panic!("{l:?}"),
        }
        assert_eq!(r.label_in(&reg, 70002), Label::Unknown, "unsigned");
        assert_eq!(r.label_in(&reg, 65003), Label::Unknown, "system uid also an app");
        assert_eq!(r.label_in(&reg, 70003), Label::Unknown, "session component");
        assert_eq!(r.label_in(&reg, 65010), Label::Ostiarius);
        assert_eq!(r.label_in(&reg, 99999), Label::Unknown);
        let empty = portcullis_peer::AppRegistry::parse("");
        assert_eq!(r.label_in(&empty, 65003), Label::Arca);
        assert_eq!(r.label(65003), Label::Unknown, "unreadable registry fails closed");
    }

    #[test]
    fn web_broker() {
        let a = parse_web_broker(&format!("org.atrium.navigator {P}")).unwrap();
        assert_eq!(a.app_id, "org.atrium.navigator");
        assert!(parse_web_broker("org.atrium.navigator 00").is_none());
        assert!(parse_web_broker(&format!("x {}", "0".repeat(64))).is_none());
        assert!(parse_web_broker(&format!("x {}", "g".repeat(64))).is_none());
    }
}
