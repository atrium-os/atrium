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
