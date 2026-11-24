use std::collections::BTreeMap;

/// The document's plain HTML forms (limen `ToParent::Forms`; atrium-net.md
/// §8.1 P5): those a form panel can offer, each with a button to send it.
/// A form with a field the panel can't show (a file, a textarea is shown as
/// a line, an image button is a button) is left out; so is one with no
/// button. A `<select>` is sent as its chosen option, as a page would send it
/// untouched.
pub fn forms(doc: &str) -> Vec<limen::doc_viewer::Form> {
    use limen::doc_viewer::{cut, Field, FieldKind, Form, FIELDS_MAX, FIELD_LABEL_MAX, FIELD_NAME_MAX, FIELD_VALUE_MAX, FIXED_VALUE_MAX, FORMS_MAX, URL_MAX};
    if !doc.get(..doc.len().min(8 << 20)).unwrap_or(doc).to_ascii_lowercase().contains("<form") {
        return Vec::new();
    }
    let dom = navigator_dom::parse(doc);
    let fold = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
    // `<label for=id>` texts, by id.
    let mut labels: BTreeMap<String, String> = BTreeMap::new();
    for l in dom.by_tag("label") {
        if let Some(id) = dom.attr(l, "for") {
            labels.entry(id.to_string()).or_insert_with(|| fold(&dom.visible_text(l)));
        }
    }
    // A control's label: its `<label for>`, an enclosing `<label>`, its
    // placeholder or title, else its name.
    let label_of = |h: navigator_dom::dom::Handle| -> String {
        if let Some(t) = dom.attr(h, "id").and_then(|id| labels.get(id)).filter(|t| !t.is_empty()) {
            return t.clone();
        }
        let mut up = dom.get(h).and_then(|n| n.parent);
        while let Some(p) = up {
            if dom.tag(p).is_some_and(|t| t.eq_ignore_ascii_case("label")) {
                let t = fold(&dom.visible_text(p));
                if !t.is_empty() {
                    return t;
                }
            }
            up = dom.get(p).and_then(|n| n.parent);
        }
        ["aria-label", "placeholder", "title", "name"].iter().find_map(|a| dom.attr(h, a).map(fold).filter(|t| !t.is_empty())).unwrap_or_default()
    };
    let mut out = Vec::new();
    'forms: for f in dom.by_tag("form") {
        if out.len() == FORMS_MAX {
            break;
        }
        let post = dom.attr(f, "method").is_some_and(|m| m.trim().eq_ignore_ascii_case("post"));
        let enctype = dom.attr(f, "enctype").map(|e| e.trim().to_ascii_lowercase());
        if enctype.as_deref().is_some_and(|e| e != "application/x-www-form-urlencoded") {
            continue;
        }
        let action = dom.attr(f, "action").unwrap_or("").trim().to_string();
        if action.len() > URL_MAX || action.to_ascii_lowercase().starts_with("javascript:") {
            continue;
        }
        let mut fields = Vec::new();
        let mut stack: Vec<navigator_dom::dom::Handle> = dom.element_children(f).into_iter().rev().collect();
        while let Some(h) = stack.pop() {
            let tag = dom.tag(h).unwrap_or("").to_ascii_lowercase();
            if tag == "form" {
                continue;
            }
            if dom.attr(h, "disabled").is_some() && matches!(tag.as_str(), "input" | "button" | "select" | "textarea") {
                continue;
            }
            let name = dom.attr(h, "name").unwrap_or("").to_string();
            let value = |d: &str| dom.attr(h, "value").unwrap_or(d).to_string();
            let field = |kind, label: String, value: String, checked| Field {
                kind, name: cut(&name, FIELD_NAME_MAX), label: cut(&label, FIELD_LABEL_MAX),
                value: cut(&value, if kind == FieldKind::Fixed { FIXED_VALUE_MAX } else { FIELD_VALUE_MAX }), checked,
            };
            let got = match tag.as_str() {
                "input" => match dom.attr(h, "type").map(|t| t.trim().to_ascii_lowercase()).as_deref().unwrap_or("text") {
                    "hidden" => Some(field(FieldKind::Fixed, String::new(), value(""), false)),
                    "text" | "email" | "tel" | "number" | "search" | "url" | "" => Some(field(FieldKind::Text, label_of(h), value(""), false)),
                    "password" => Some(field(FieldKind::Password, label_of(h), value(""), false)),
                    "checkbox" => Some(field(FieldKind::Checkbox, label_of(h), value("on"), dom.attr(h, "checked").is_some())),
                    "radio" => Some(field(FieldKind::Radio, label_of(h), value("on"), dom.attr(h, "checked").is_some())),
                    "submit" | "image" => Some(field(FieldKind::Submit, dom.attr(h, "value").or(dom.attr(h, "alt")).map(fold).filter(|t| !t.is_empty()).unwrap_or_else(|| "Submit".into()), value(""), false)),
                    "button" | "reset" => None,
                    _ => continue 'forms,
                },
                "button" => match dom.attr(h, "type").map(|t| t.trim().to_ascii_lowercase()).as_deref().unwrap_or("submit") {
                    "submit" => {
                        let t = fold(&dom.visible_text(h));
                        Some(field(FieldKind::Submit, if t.is_empty() { "Submit".into() } else { t }, value(""), false))
                    }
                    _ => None,
                },
                "textarea" => Some(field(FieldKind::Text, label_of(h), fold(&dom.text_content(h)), false)),
                // We parse with scripting on, so a `<noscript>` is text; a
                // browser without scripts shows it, and a sign-in's
                // self-posting form (a SAML answer) puts its button there.
                "noscript" => {
                    let inner = navigator_dom::parse(&dom.text_content(h));
                    for b in inner.by_tag_anywhere("input").into_iter().chain(inner.by_tag_anywhere("button")) {
                        let is_input = inner.tag(b).is_some_and(|t| t.eq_ignore_ascii_case("input"));
                        let ty = inner.attr(b, "type").map(|t| t.trim().to_ascii_lowercase()).unwrap_or_else(|| if is_input { "text".into() } else { "submit".into() });
                        if ty != "submit" || fields.len() == FIELDS_MAX {
                            continue;
                        }
                        let text = if is_input { inner.attr(b, "value").map(fold).unwrap_or_default() } else { fold(&inner.visible_text(b)) };
                        let n = inner.attr(b, "name").unwrap_or("");
                        fields.push(Field {
                            kind: FieldKind::Submit, name: cut(n, FIELD_NAME_MAX),
                            label: cut(if text.is_empty() { "Continue" } else { &text }, FIELD_LABEL_MAX),
                            value: cut(inner.attr(b, "value").unwrap_or(""), FIELD_VALUE_MAX), checked: false,
                        });
                    }
                    None
                }
                "select" => {
                    let opts = dom.by_tag_anywhere("option").into_iter().filter(|&o| dom.contains(h, o)).collect::<Vec<_>>();
                    let chosen = opts.iter().copied().find(|&o| dom.attr(o, "selected").is_some()).or(opts.first().copied());
                    let v = chosen.map(|o| dom.attr(o, "value").map(String::from).unwrap_or_else(|| fold(&dom.text_content(o)))).unwrap_or_default();
                    Some(field(FieldKind::Fixed, String::new(), v, false))
                }
                _ => {
                    stack.extend(dom.element_children(h).into_iter().rev());
                    None
                }
            };
            if let Some(g) = got {
                if fields.len() == FIELDS_MAX {
                    continue 'forms;
                }
                fields.push(g);
            }
        }
        let form = Form { action, post, fields };
        if form.ok() {
            out.push(form);
        }
    }
    out
}

/// `scheme://host[:port]` of an absolute address.
fn origin_of(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    (end > 0).then(|| &url[..8 + end])
}

/// `application/x-www-form-urlencoded`, decoded. `None` for anything
/// malformed or not UTF-8.
fn form_decode(body: &[u8]) -> Option<Vec<(String, String)>> {
    fn part(b: &[u8]) -> Option<String> {
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                b'+' => out.push(b' '),
                b'%' => {
                    let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
                    out.push(u8::from_str_radix(hex, 16).ok()?);
                    i += 2;
                }
                c => out.push(c),
            }
            i += 1;
        }
        String::from_utf8(out).ok()
    }
    if body.is_empty() { return Some(vec![]); }
    body.split(|&c| c == b'&').map(|pair| {
        let mut it = pair.splitn(2, |&c| c == b'=');
        Some((part(it.next()?)?, part(it.next().unwrap_or(b""))?))
    }).collect()
}

/// The form a VPN page's script submitted, as the window's panel shows
/// it: the script's values, never shown, and one button. `None` when it
/// is out of shape, not to the page's own HTTPS origin, or carries a
/// field the document has as a password.
pub fn scripted_form(location: &str, secret: &[String], url: &str, body: &[u8]) -> Option<limen::doc_viewer::Form> {
    use limen::doc_viewer::{Field, FieldKind, Form, FIELDS_MAX};
    let origin = origin_of(location)?;
    if origin_of(url)? != origin { return None; }
    let pairs = form_decode(body)?;
    if pairs.len() >= FIELDS_MAX || pairs.iter().any(|(n, _)| n.is_empty() || secret.contains(n)) { return None; }
    let mut fields: Vec<Field> = pairs.into_iter().map(|(name, value)| Field { kind: FieldKind::Scripted, name, label: String::new(), value, checked: false }).collect();
    fields.push(Field { kind: FieldKind::Submit, name: String::new(), label: "Continue".into(), value: String::new(), checked: false });
    let form = Form { action: url.into(), post: true, fields };
    form.ok().then_some(form)
}

#[cfg(test)]
mod scripted_tests {
    use super::*;
    use limen::doc_viewer::FieldKind;

    #[test]
    fn a_script_form_goes_only_to_its_pages_origin_without_a_password() {
        let page = "https://idp.example:448/realms/corp/login-actions/authenticate?session_code=1";
        let to = "https://idp.example:448/realms/corp/login-actions/authenticate?session_code=1&execution=2";
        let f = scripted_form(page, &[], to, b"clientDataJSON=eyJ0eXBl&signature=MEUC%2Bx&error=").expect("a form");
        let got: Vec<_> = f.fields.iter().map(|x| (x.kind, x.name.as_str(), x.value.as_str())).collect();
        assert_eq!(got, vec![(FieldKind::Scripted, "clientDataJSON", "eyJ0eXBl"), (FieldKind::Scripted, "signature", "MEUC+x"),
            (FieldKind::Scripted, "error", ""), (FieldKind::Submit, "", "")]);
        assert!(f.post && f.action == to);
        assert!(scripted_form(page, &[], "https://evil.example/x", b"a=1").is_none(), "another origin");
        assert!(scripted_form(page, &[], "https://idp.example/x", b"a=1").is_none(), "another port");
        assert!(scripted_form("http://idp.example/", &[], "http://idp.example/x", b"a=1").is_none(), "never http");
        assert!(scripted_form(page, &["password".into()], to, b"username=a&password=b").is_none(), "a password only through Arca");
        assert!(scripted_form(page, &[], to, b"a=%zz").is_none());
        assert!(scripted_form(page, &[], to, b"=1").is_none());
        let many: Vec<String> = (0..40).map(|i| format!("f{i}=1")).collect();
        assert!(scripted_form(page, &[], to, many.join("&").as_bytes()).is_none());
    }

    #[test]
    fn an_origin_ends_at_the_path() {
        assert_eq!(origin_of("https://a.example:8443/x?y"), Some("https://a.example:8443"));
        assert_eq!(origin_of("https://a.example?y"), Some("https://a.example"));
        assert_eq!(origin_of("https://a.example"), Some("https://a.example"));
        assert_eq!(origin_of("http://a.example/"), None);
    }
}
