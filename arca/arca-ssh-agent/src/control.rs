//! The control socket: one request line, an answer (`ok` or `error ...`
//! first, then data lines), then the connection closes.
//!
//! - root (portcullisd): `open OWNER WORKSPACE PERSONA UID`, `close OWNER
//!   WORKSPACE`, `forget OWNER WORKSPACE PERSONA` (deleted: its grants go),
//!   `list OWNER PERSONA` and `grant|revoke OWNER PERSONA ID WORKSPACE`;
//! - a person (a uid with a passwd entry that is not a launched app's):
//!   `new PERSONA COMMENT...`, `list PERSONA`, `grant|revoke PERSONA ID
//!   WORKSPACE`, `delete PERSONA ID`, `token PERSONA HOST USERNAME` (the
//!   token on the next line), acting on their own keys only.

use crate::{unhex16, valid_owner, valid_persona};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admin {
    /// Portcullisd brokers these two operations for Settings.
    List { owner: String, persona: String },
    Grant { owner: String, persona: String, id: [u8; 16], ws: String, on: bool },
    Open { owner: String, ws: String, persona: String, uid: u32 },
    Close { owner: String, ws: String },
    /// The workspace was deleted: close it and end its grants.
    Forget { owner: String, ws: String, persona: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Person {
    New { persona: String, comment: String },
    List { persona: String },
    Grant { persona: String, id: [u8; 16], ws: String, on: bool },
    Delete { persona: String, id: [u8; 16] },
    /// An HTTPS token for `host`; the token itself follows on the next line.
    Token { persona: String, host: String, username: String },
}

fn ws_ok(w: &str) -> bool {
    arcad::ssh::valid_workspace(w)
}

pub fn parse_admin(line: &str) -> Option<Admin> {
    let f: Vec<&str> = line.split_ascii_whitespace().collect();
    match f.as_slice() {
        ["list", owner, persona] if valid_owner(owner) && valid_persona(persona) => {
            Some(Admin::List { owner: owner.to_string(), persona: persona.to_string() })
        }
        [verb @ ("grant" | "revoke"), owner, persona, id, ws]
            if valid_owner(owner) && valid_persona(persona) && ws_ok(ws) =>
        {
            Some(Admin::Grant {
                owner: owner.to_string(), persona: persona.to_string(), id: unhex16(id)?,
                ws: ws.to_string(), on: *verb == "grant",
            })
        }
        ["open", o, w, p, u] if valid_owner(o) && ws_ok(w) && valid_persona(p) => {
            let uid: u32 = u.parse().ok().filter(|u| *u != 0)?;
            Some(Admin::Open { owner: o.to_string(), ws: w.to_string(), persona: p.to_string(), uid })
        }
        ["close", o, w] if valid_owner(o) && ws_ok(w) => Some(Admin::Close { owner: o.to_string(), ws: w.to_string() }),
        ["forget", o, w, p] if valid_owner(o) && ws_ok(w) && valid_persona(p) => {
            Some(Admin::Forget { owner: o.to_string(), ws: w.to_string(), persona: p.to_string() })
        }
        _ => None,
    }
}

pub fn parse_person(line: &str) -> Option<Person> {
    let line = line.trim_end_matches(['\r', '\n']);
    let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
    let f: Vec<&str> = rest.split_ascii_whitespace().collect();
    let p = |s: &&str| valid_persona(s).then(|| s.to_string());
    match (verb, f.as_slice()) {
        ("new", [persona, ..]) => {
            let comment = rest.trim_start().strip_prefix(*persona)?.trim().to_string();
            (!comment.is_empty() && comment.len() <= arcad::proto::ssh_op::COMMENT_MAX && !comment.chars().any(char::is_control))
                .then_some(())?;
            Some(Person::New { persona: p(persona)?, comment })
        }
        ("list", [persona]) => Some(Person::List { persona: p(persona)? }),
        ("grant" | "revoke", [persona, id, ws]) if ws_ok(ws) => {
            Some(Person::Grant { persona: p(persona)?, id: unhex16(id)?, ws: ws.to_string(), on: verb == "grant" })
        }
        ("delete", [persona, id]) => Some(Person::Delete { persona: p(persona)?, id: unhex16(id)? }),
        ("token", [persona, host, user]) if arcad::ssh::valid_host(host) && user.len() <= 128 && !user.chars().any(char::is_control) => {
            Some(Person::Token { persona: p(persona)?, host: host.to_string(), username: user.to_string() })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines() {
        assert_eq!(parse_admin("open alice web personal 50012"),
                   Some(Admin::Open { owner: "alice".into(), ws: "web".into(), persona: "personal".into(), uid: 50012 }));
        assert_eq!(parse_admin("open alice web personal 0"), None);
        assert_eq!(parse_admin("open a.b web personal 5"), None);
        assert_eq!(parse_admin("open alice ../x personal 5"), None);
        assert_eq!(parse_admin("close alice web"), Some(Admin::Close { owner: "alice".into(), ws: "web".into() }));
        assert!(matches!(parse_admin("forget alice web work"), Some(Admin::Forget { .. })));
        assert_eq!(parse_person("new personal alice@laptop key"),
                   Some(Person::New { persona: "personal".into(), comment: "alice@laptop key".into() }));
        assert_eq!(parse_person("new personal"), None);
        assert_eq!(parse_person("new Bad x"), None);
        let id = "00112233445566778899aabbccddeeff";
        assert!(matches!(parse_person(&format!("grant work {id} web")), Some(Person::Grant { on: true, .. })));
        assert!(matches!(parse_person(&format!("revoke work {id} web")), Some(Person::Grant { on: false, .. })));
        assert_eq!(parse_person(&format!("grant work {id} We")), None);
        assert!(matches!(parse_person(&format!("delete personal {id}")), Some(Person::Delete { .. })));
        assert_eq!(parse_person("open alice web personal 5"), None);
        assert!(matches!(parse_admin("list alice personal"), Some(Admin::List { .. })));
        assert!(matches!(parse_admin(&format!("revoke alice work {id} web")), Some(Admin::Grant { on: false, .. })));
        assert_eq!(parse_admin(&format!("grant alice work {id} ../x")), None);
        assert_eq!(parse_admin(&format!("delete alice work {id}")), None);
        assert_eq!(parse_person("token personal github.com alice"),
                   Some(Person::Token { persona: "personal".into(), host: "github.com".into(), username: "alice".into() }));
        assert_eq!(parse_person("token personal GitHub.com alice"), None);
        assert_eq!(parse_person("token personal github.com:443 alice"), None);
        assert_eq!(parse_person("token personal github.com"), None);
    }
}
