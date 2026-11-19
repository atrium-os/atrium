//! The sign-in exchange (`config-auth`, vpn.md §9.3): what the gateway
//! asks, as typed prompts, and our answers. Values the person types are
//! never kept here; replies that carry them are built in zeroizing memory.

use crate::xml::{self, Element};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// The most fields one form may ask for, and the most choices in one.
pub const MAX_FIELDS: usize = 8;
pub const MAX_CHOICES: usize = 64;
/// Gateway text shown to the person (title, message, banner) is cut here.
pub const MAX_TEXT: usize = 4096;
pub const MAX_VALUE: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Text,
    /// A password: typed into Arca's field (vpn.md §9.3).
    Secret,
    /// A one-time code: also Arca's field, never saved.
    Otp,
    /// `(value, label)` pairs.
    Choice(Vec<(String, String)>),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Field {
    pub name: String,
    pub label: String,
    pub kind: Kind,
}

/// One page of the gateway's questions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Form {
    /// The gateway's `auth id` (`main`, `challenge`, ...).
    pub id: String,
    pub title: String,
    pub message: String,
    /// What went wrong with the last answer, in the gateway's words.
    pub error: String,
    pub banner: String,
    pub fields: Vec<Field>,
}

/// What the gateway said to a config-auth request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// Questions; answer with [`reply`] (and `opaque` echoed).
    Form { form: Form, action: String, opaque: Option<Element> },
    /// Sign in at an identity provider (SAML, vpn.md §9.3): open `login`;
    /// the gateway's final page at `final_url` sets `token_cookie`.
    Sso { login: String, final_url: String, token_cookie: String, opaque: Option<Element>, form: Form },
    /// Signed in. The session cookie usually comes as `Set-Cookie:
    /// webvpn=`; some gateways put it here too.
    Complete { session_token: Option<Zeroizing<String>>, banner: String },
    /// The gateway wants its posture program run (HostScan, §10.3). Atrium
    /// never runs it.
    Posture,
    /// The gateway refused, in its words.
    Refused(String),
}

/// Text from the gateway for a person to read: control and bidi
/// characters out, whitespace runs kept, at most [`MAX_TEXT`] bytes
/// (vpn.md §9.4).
pub fn clean(t: &str) -> String {
    let mut o = String::new();
    for c in t.trim().chars() {
        let bidi = matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}' | '\u{061c}');
        if bidi || (c.is_control() && c != '\n' && c != '\t') {
            continue;
        }
        if o.len() + c.len_utf8() > MAX_TEXT {
            break;
        }
        o.push(c);
    }
    o
}

fn short(t: &str, n: usize) -> String {
    let c = clean(t);
    match c.char_indices().nth(n) {
        Some((i, _)) => c[..i].to_string(),
        None => c,
    }
}

fn valid_field_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 64 && n.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-') && n.as_bytes()[0].is_ascii_alphabetic()
}

/// Whether a password field is a one-time code, by the gateway's words.
fn looks_otp(name: &str, label: &str, message: &str) -> bool {
    let t = format!("{name} {label} {message}").to_ascii_lowercase();
    ["otp", "one-time", "one time", "passcode", "token code", "verification code", "secondary_password", "authenticator"].iter().any(|w| t.contains(w))
}

/// Our first request (`init`), to `url` (`https://host/` or a group URL).
/// We advertise SSO (single-sign-on-v2) so gateways that use it say so.
pub fn init(url: &str) -> String {
    Element::new("config-auth")
        .with_attr("client", "vpn")
        .with_attr("type", "init")
        .with_attr("aggregate-auth-version", "2")
        .with_child(Element::new("version").with_attr("who", "vpn").with_text(crate::VERSION))
        .with_child(Element::new("device-id").with_text(crate::DEVICE_ID))
        .with_child(Element::new("group-access").with_text(url))
        .with_child(Element::new("capabilities").with_child(Element::new("auth-method").with_text("single-sign-on-v2")))
        .to_doc()
}

fn base_reply(opaque: Option<&Element>) -> Element {
    let mut e = Element::new("config-auth")
        .with_attr("client", "vpn")
        .with_attr("type", "auth-reply")
        .with_attr("aggregate-auth-version", "2")
        .with_child(Element::new("version").with_attr("who", "vpn").with_text(crate::VERSION))
        .with_child(Element::new("device-id").with_text(crate::DEVICE_ID));
    if let Some(o) = opaque {
        e.children.push(o.clone());
    }
    e
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReplyError {
    /// The answer names a field the form did not ask for, or misses one.
    Fields,
    /// A choice that was not offered.
    Choice,
    TooLong,
}

/// The answer to `form`: one value per field, by name. A choice must be
/// one of the values offered; a value is at most [`MAX_VALUE`] bytes.
pub fn reply(form: &Form, opaque: Option<&Element>, values: &[(String, Zeroizing<String>)]) -> Result<Zeroizing<String>, ReplyError> {
    if values.len() != form.fields.len() {
        return Err(ReplyError::Fields);
    }
    let mut auth = Element::new("auth");
    let mut group = None;
    for f in &form.fields {
        let v = values.iter().find(|(n, _)| *n == f.name).map(|(_, v)| v).ok_or(ReplyError::Fields)?;
        if v.len() > MAX_VALUE || v.contains('\0') {
            return Err(ReplyError::TooLong);
        }
        if let Kind::Choice(opts) = &f.kind {
            if !opts.iter().any(|(o, _)| o == v.as_str()) {
                return Err(ReplyError::Choice);
            }
            if f.name == "group_list" {
                group = Some(v.to_string());
            }
        }
        auth.children.push(Element::new(&f.name).with_text(v));
    }
    let mut e = base_reply(opaque);
    e.children.push(auth);
    if let Some(g) = group {
        e.children.push(Element::new("group-select").with_text(&g));
    }
    let doc = Zeroizing::new(e.to_doc());
    zeroize_tree(&mut e);
    Ok(doc)
}

/// The answer that hands over an SSO token (the `acSamlv2Token` cookie's
/// value, vpn.md §9.3).
pub fn sso_reply(opaque: Option<&Element>, token: &str) -> Zeroizing<String> {
    let mut e = base_reply(opaque);
    e.children.push(Element::new("auth").with_child(Element::new("sso-token").with_text(token)));
    let doc = Zeroizing::new(e.to_doc());
    zeroize_tree(&mut e);
    doc
}

fn zeroize_tree(e: &mut Element) {
    use zeroize::Zeroize;
    e.text.zeroize();
    for c in &mut e.children {
        zeroize_tree(c);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Xml(xml::Error),
    NotConfigAuth,
    /// A form with nothing we can show, or more than we take.
    BadForm(&'static str),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Xml(e) => write!(f, "XML: {e}"),
            Error::NotConfigAuth => f.write_str("not a config-auth document"),
            Error::BadForm(s) => write!(f, "form: {s}"),
        }
    }
}

/// Read a config-auth answer.
pub fn parse(body: &[u8]) -> Result<Answer, Error> {
    let e = xml::parse(body).map_err(Error::Xml)?;
    if e.name != "config-auth" {
        return Err(Error::NotConfigAuth);
    }
    let opaque = e.child("opaque").cloned();
    let auth = e.child("auth");
    match e.attr("type") {
        Some("complete") => {
            let banner = auth.and_then(|a| a.child_text("banner")).map(clean).unwrap_or_default();
            let session_token = e.child_text("session-token").filter(|t| !t.is_empty()).map(|t| Zeroizing::new(t.to_string()));
            return Ok(Answer::Complete { session_token, banner });
        }
        Some("auth-request") => {}
        _ => return Err(Error::NotConfigAuth),
    }
    if e.child("csd").is_some() || e.child("host-scan").is_some() {
        return Ok(Answer::Posture);
    }
    let auth = auth.ok_or(Error::BadForm("no auth"))?;
    let mut form = Form {
        id: short(auth.attr("id").unwrap_or("main"), 32),
        title: short(auth.child_text("title").unwrap_or(""), 200),
        message: clean(auth.child_text("message").unwrap_or("")),
        error: clean(auth.child_text("error").unwrap_or("")),
        banner: clean(auth.child_text("banner").unwrap_or("")),
        fields: Vec::new(),
    };
    if let Some(login) = auth.child_text("sso-v2-login").filter(|u| !u.is_empty()) {
        let final_url = auth.child_text("sso-v2-login-final").unwrap_or("").to_string();
        let token_cookie = auth.child_text("sso-v2-token-cookie-name").unwrap_or("acSamlv2Token").to_string();
        if !https_url(login) || !https_url(&final_url) || !valid_field_name(&token_cookie) {
            return Err(Error::BadForm("SSO addresses"));
        }
        return Ok(Answer::Sso { login: login.to_string(), final_url, token_cookie, opaque, form });
    }
    let Some(f) = auth.child("form") else {
        // No questions: a refusal, in the gateway's words.
        let why = [form.error.as_str(), form.message.as_str()].into_iter().find(|t| !t.is_empty()).unwrap_or("the gateway refused").to_string();
        return Ok(Answer::Refused(why));
    };
    let action = f.attr("action").unwrap_or("/").to_string();
    if !action.starts_with('/') || action.starts_with("//") || action.len() > 256 || action.bytes().any(|c| c <= b' ') {
        return Err(Error::BadForm("action"));
    }
    for c in &f.children {
        let name = c.attr("name").unwrap_or("");
        let label = short(c.attr("label").unwrap_or(name), 120);
        let kind = match (c.name.as_str(), c.attr("type")) {
            ("input", Some("text")) => Kind::Text,
            ("input", Some("password")) => {
                if looks_otp(name, &label, &form.message) {
                    Kind::Otp
                } else {
                    Kind::Secret
                }
            }
            // Hidden and submit inputs carry nothing a person answers.
            ("input", Some("hidden" | "submit" | "reset")) => continue,
            ("select", _) => {
                let mut opts = Vec::new();
                for o in c.children_named("option") {
                    if opts.len() >= MAX_CHOICES {
                        return Err(Error::BadForm("too many choices"));
                    }
                    let label = short(&o.text, 120);
                    let value = o.attr("value").map(str::to_string).unwrap_or_else(|| label.clone());
                    if value.len() > MAX_VALUE {
                        return Err(Error::BadForm("choice too long"));
                    }
                    opts.push((value, label));
                }
                if opts.is_empty() {
                    return Err(Error::BadForm("a choice with no options"));
                }
                Kind::Choice(opts)
            }
            _ => return Err(Error::BadForm("an unknown kind of field")),
        };
        if !valid_field_name(name) || form.fields.iter().any(|x| x.name == name) {
            return Err(Error::BadForm("field name"));
        }
        if form.fields.len() >= MAX_FIELDS {
            return Err(Error::BadForm("too many fields"));
        }
        form.fields.push(Field { name: name.to_string(), label, kind });
    }
    if form.fields.is_empty() {
        return Err(Error::BadForm("no fields"));
    }
    Ok(Answer::Form { form, action, opaque })
}

fn https_url(u: &str) -> bool {
    u.len() <= 2048 && u.starts_with("https://") && u.len() > 8 && !u.bytes().any(|c| c <= b' ' || c == 0x7f)
}

#[cfg(test)]
mod tests {
    use super::*;

    // As ocserv 1.2 sends them.
    const USER: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<config-auth client="vpn" type="auth-request">
<version who="sg">0.1(1)</version>
<auth id="main">
<message>Please enter your username.</message>
<form method="post" action="/auth">
<input type="text" name="username" label="Username:" />
</form></auth>
</config-auth>"#;
    const OTP: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<config-auth client="vpn" type="auth-request"><auth id="main"><message>Please enter your OTP password.</message>
<form method="post" action="/auth"><input type="password" name="password" label="Password:" /></form></auth></config-auth>"#;
    const GROUPS: &[u8] = br#"<?xml version="1.0"?><config-auth client="vpn" type="auth-request" aggregate-auth-version="2">
<opaque is-for="sg"><tunnel-group>x</tunnel-group></opaque>
<auth id="main"><title>Login</title><message>Please enter your username and password.</message><banner>Welcome&#x202e;!</banner>
<form method="post" action="/"><input type="text" name="username" label="Username:" /><input type="password" name="password" label="Password:" />
<select name="group_list" label="GROUP:"><option>Staff</option><option value="eng">Engineers</option></select>
<input type="submit" name="Login" value="Login" /></form></auth></config-auth>"#;

    #[test]
    fn ocserv_forms() {
        let Answer::Form { form, action, .. } = parse(USER).unwrap() else { panic!() };
        assert_eq!(action, "/auth");
        assert_eq!(form.fields, vec![Field { name: "username".into(), label: "Username:".into(), kind: Kind::Text }]);
        let Answer::Form { form, .. } = parse(OTP).unwrap() else { panic!() };
        assert_eq!(form.fields[0].kind, Kind::Otp);
    }

    #[test]
    fn groups_opaque_and_reply() {
        let Answer::Form { form, opaque, .. } = parse(GROUPS).unwrap() else { panic!() };
        assert_eq!(form.banner, "Welcome!");
        assert_eq!(form.fields.len(), 3);
        assert_eq!(form.fields[1].kind, Kind::Secret);
        assert_eq!(form.fields[2].kind, Kind::Choice(vec![("Staff".into(), "Staff".into()), ("eng".into(), "Engineers".into())]));
        let v = |a: &str, b: &str| (a.to_string(), Zeroizing::new(b.to_string()));
        let r = reply(&form, opaque.as_ref(), &[v("username", "ann"), v("password", "p<w"), v("group_list", "eng")]).unwrap();
        let e = xml::parse(r.as_bytes()).unwrap();
        assert_eq!(e.attr("type"), Some("auth-reply"));
        assert_eq!(e.child("opaque").unwrap().child_text("tunnel-group"), Some("x"));
        assert_eq!(e.child("auth").unwrap().child_text("password"), Some("p<w"));
        assert_eq!(e.child_text("group-select"), Some("eng"));
        assert_eq!(reply(&form, None, &[v("username", "a"), v("password", "b"), v("group_list", "nope")]), Err(ReplyError::Choice));
        assert_eq!(reply(&form, None, &[v("username", "a")]), Err(ReplyError::Fields));
        assert_eq!(reply(&form, None, &[v("username", "a"), v("pw", "b"), v("group_list", "eng")]), Err(ReplyError::Fields));
    }

    #[test]
    fn sso_complete_posture_refusal() {
        let s = br#"<config-auth client="vpn" type="auth-request" aggregate-auth-version="2"><auth id="main"><title>SSO</title>
<sso-v2-login>https://gw.example/+CSCOE+/saml/sp/login?tgname=x</sso-v2-login><sso-v2-login-final>https://gw.example/+CSCOE+/saml_ac_login.html</sso-v2-login-final>
<sso-v2-token-cookie-name>acSamlv2Token</sso-v2-token-cookie-name></auth></config-auth>"#;
        let Answer::Sso { login, token_cookie, .. } = parse(s).unwrap() else { panic!() };
        assert!(login.starts_with("https://gw.example/"));
        assert_eq!(token_cookie, "acSamlv2Token");
        let bad = std::str::from_utf8(s).unwrap().replace("https://gw.example/+CSCOE+/saml/sp", "javascript:alert(1)//");
        assert!(parse(bad.as_bytes()).is_err());
        let r = sso_reply(None, "tok");
        assert_eq!(xml::parse(r.as_bytes()).unwrap().child("auth").unwrap().child_text("sso-token"), Some("tok"));
        let c = br#"<config-auth client="vpn" type="complete"><session-token>abc</session-token><auth id="success"><banner>Hi</banner></auth></config-auth>"#;
        let Answer::Complete { session_token, banner } = parse(c).unwrap() else { panic!() };
        assert_eq!((session_token.as_deref().map(|s| s.as_str()), banner.as_str()), (Some("abc"), "Hi"));
        assert_eq!(parse(br#"<config-auth type="auth-request"><csd token="x" stuburl="https://a/b"/><auth id="main"/></config-auth>"#), Ok(Answer::Posture));
        assert_eq!(parse(br#"<config-auth type="auth-request"><auth id="main"><error>Login failed.</error></auth></config-auth>"#), Ok(Answer::Refused("Login failed.".into())));
    }

    #[test]
    fn hostile_forms() {
        let f = |inner: &str| format!(r#"<config-auth type="auth-request"><auth id="main"><form action="/auth">{inner}</form></auth></config-auth>"#);
        assert!(parse(f(r#"<input type="file" name="x"/>"#).as_bytes()).is_err());
        assert!(parse(f(r#"<input type="text" name="a b"/>"#).as_bytes()).is_err());
        assert!(parse(f(r#"<input type="text" name="a"/><input type="text" name="a"/>"#).as_bytes()).is_err());
        assert!(parse(f(&r#"<input type="text" name="a"/>"#.repeat(1)).replace("/auth", "https://evil/").as_bytes()).is_err());
        let many: String = (0..=MAX_FIELDS).map(|i| format!(r#"<input type="text" name="f{i}"/>"#)).collect();
        assert!(parse(f(&many).as_bytes()).is_err());
        assert!(parse(f(r#"<select name="g"></select>"#).as_bytes()).is_err());
        assert!(parse(f("").as_bytes()).is_err());
        assert_eq!(clean("a\u{1b}[31mb\u{202e}c"), "a[31mbc");
        assert_eq!(clean(&"x".repeat(MAX_TEXT + 10)).len(), MAX_TEXT);
    }
}
