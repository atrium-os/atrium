//! The original message under a reply or forward, with its formatting
//! (mail.md §9.1): the parse jail's normalized HTML of the original, kept
//! to a small set of elements, under the person's own text in the HTML
//! part; the plain part quotes it with "> " as before.
//!
//! The normalized HTML comes from a jail and is still the other sender's
//! content, so only these elements survive, with no attributes but a
//! link's `href` (web and mail only): no pictures, styles, forms or
//! anything that loads.

/// The original under the person's text.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Quote {
    /// For the plain part: "On …, Arun wrote:" and the "> " lines (or a
    /// forward's header block and text).
    pub text: String,
    /// For the HTML part: the intro and the original, already kept to the
    /// safe set ([`html`]).
    pub html: String,
}

/// The longest quoted HTML kept; a longer original is cut (at a tag).
pub const QUOTE_MAX: usize = 1 << 20;

const KEEP: &[&str] = &[
    "p", "br", "div", "span", "b", "strong", "i", "em", "u", "s", "strike", "del", "sub", "sup", "a", "ul", "ol", "li", "blockquote", "h1", "h2", "h3",
    "h4", "h5", "h6", "pre", "code", "table", "thead", "tbody", "tr", "td", "th", "hr",
];
/// Elements dropped with everything inside them.
const DROP: &[&str] = &["script", "style", "head", "title", "template", "noscript", "svg", "math", "object", "iframe", "select", "textarea"];
const VOID: &[&str] = &["br", "hr"];

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn safe_link(u: &str) -> bool {
    let l = u.trim().to_ascii_lowercase();
    ["https://", "http://", "mailto:"].iter().any(|p| l.starts_with(p))
}

/// An attribute's value from a tag's text (`name="v"`, `name='v'`, or bare).
fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(name) {
        let at = from + i;
        let before = lower[..at].chars().last();
        let rest = lower[at + name.len()..].trim_start();
        if before.is_some_and(|c| c.is_whitespace()) && rest.starts_with('=') {
            let vstart = tag.len() - rest.len() + 1;
            let v = tag[vstart..].trim_start();
            return Some(match v.chars().next() {
                Some(q @ ('"' | '\'')) => v[1..].split(q).next().unwrap_or("").to_string(),
                _ => v.split(|c: char| c.is_whitespace() || c == '>').next().unwrap_or("").to_string(),
            });
        }
        from = at + name.len();
    }
    None
}

fn unesc_attr(s: &str) -> String {
    s.replace("&quot;", "\"").replace("&#39;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

/// `html` kept to the safe set: allowed elements without attributes (a
/// link keeps a web or mail `href`), text as it was (re-escaped), comments
/// and everything else dropped. Tags left open are closed.
pub fn sanitize(html: &str) -> String {
    let body = match html.find("<body") {
        Some(i) => html[i..].split_once('>').map_or("", |x| x.1),
        None => html,
    };
    let mut out = String::new();
    let mut open: Vec<&'static str> = Vec::new();
    let mut skip: Option<&'static str> = None;
    let mut rest = body;
    while !rest.is_empty() && out.len() < QUOTE_MAX {
        if let Some(r) = rest.strip_prefix("<!--") {
            rest = r.split_once("-->").map_or("", |x| x.1);
            continue;
        }
        if let Some(r) = rest.strip_prefix('<') {
            let Some(end) = r.find('>') else { break };
            let tag = &r[..end];
            rest = &r[end + 1..];
            let (close, inner) = match tag.strip_prefix('/') {
                Some(t) => (true, t),
                None => (false, tag),
            };
            let name = inner.split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or("").to_ascii_lowercase();
            if let Some(s) = skip {
                if close && name == s {
                    skip = None;
                }
                continue;
            }
            if let Some(&d) = DROP.iter().find(|d| **d == name) {
                if !close && !tag.ends_with('/') {
                    skip = Some(d);
                }
                continue;
            }
            if name == "body" || name == "html" {
                if close {
                    break;
                }
                continue;
            }
            let Some(&k) = KEEP.iter().find(|k| **k == name) else { continue };
            if close {
                if let Some(i) = open.iter().rposition(|o| *o == k) {
                    for o in open.drain(i..).rev() {
                        out.push_str(&format!("</{o}>"));
                    }
                }
                continue;
            }
            if k == "a" {
                match attr(inner, "href").map(|h| unesc_attr(&h)).filter(|h| safe_link(h)) {
                    Some(h) => out.push_str(&format!("<a href=\"{}\">", esc(&h))),
                    None => out.push_str("<a>"),
                }
            } else {
                out.push_str(&format!("<{k}>"));
            }
            if !VOID.contains(&k) {
                open.push(k);
            }
        } else {
            let end = rest.find('<').unwrap_or(rest.len());
            if skip.is_none() {
                // The text is already escaped HTML; keep it, escaping any
                // stray '>' or quote.
                out.push_str(&rest[..end].replace('>', "&gt;").replace('"', "&quot;"));
            }
            rest = &rest[end..];
        }
    }
    for o in open.into_iter().rev() {
        out.push_str(&format!("</{o}>"));
    }
    out
}

/// The text of (normalized) HTML, for the plain part's quote of an
/// HTML-only original: blocks on lines of their own, list items as "- ".
pub fn text_of(html: &str) -> String {
    let safe = sanitize(html);
    let mut out = String::new();
    let mut rest = safe.as_str();
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('<') {
            let end = r.find('>').unwrap_or(r.len());
            let tag = r[..end].trim_start_matches('/');
            let name = tag.split_whitespace().next().unwrap_or("");
            let closing = r.starts_with('/');
            match name {
                "br" | "p" | "div" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "tr" | "blockquote" | "ul" | "ol" | "pre" | "hr" | "table" => {
                    if !out.ends_with('\n') && !out.is_empty() {
                        out.push('\n');
                    }
                }
                "li" if !closing => {
                    if !out.ends_with('\n') && !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str("- ");
                }
                "td" | "th" if !closing => out.push(' '),
                _ => {}
            }
            rest = r.get(end + 1..).unwrap_or("");
        } else {
            let end = rest.find('<').unwrap_or(rest.len());
            let t = unesc_attr(&rest[..end].replace("&nbsp;", " ").replace('\n', " "));
            out.push_str(&t);
            rest = &rest[end..];
        }
    }
    out.lines().map(str::trim_end).collect::<Vec<_>>().join("\n").trim().to_string()
}

/// The HTML part's quote: `intro` lines (escaped; "On …, Arun wrote:" or
/// a forward's header block) above the original, which a reply puts in a
/// cited blockquote and a forward shows as it was.
pub fn html(intro: &str, original_html: &str, cite: bool) -> String {
    let lines: Vec<String> = intro.trim().lines().map(esc).collect();
    let body = sanitize(original_html);
    if cite {
        format!("<p>{}</p>\n<blockquote type=\"cite\" style=\"margin:0 0 0 .8ex;border-left:1px solid #ccc;padding-left:1ex\">{body}</blockquote>\n", lines.join("<br>"))
    } else {
        format!("<p>{}</p>\n<div>{body}</div>\n", lines.join("<br>"))
    }
}

/// `quote` put under the person's HTML (before its `</body>`).
pub fn under(person_html: &str, quote: &str) -> String {
    match person_html.rfind("</body>") {
        Some(i) => format!("{}{}{}", &person_html[..i], quote, &person_html[i..]),
        None => format!("{person_html}{quote}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kept_to_the_safe_set() {
        let h = r#"<html><head><style>p{color:red}</style></head><body class="x"><p style="color:red" onclick="x()">Hi <b>there</b> &amp; you</p>
<img src="res-1" width="10"><script>alert(1)</script><a href="https://dash.acme.example/q4?a=1&amp;b=2" target=_blank>dash</a>
<a href="javascript:alert(1)">bad</a><form><input value="x"></form><table><tr><td>1<td>2</table><!-- note --><div><ul><li>open</ul></body></html>"#;
        let s = sanitize(h);
        assert_eq!(
            s,
            "<p>Hi <b>there</b> &amp; you</p>\n<a href=\"https://dash.acme.example/q4?a=1&amp;b=2\">dash</a>\n<a>bad</a><table><tr><td>1<td>2</td></td></tr></table><div><ul><li>open</li></ul></div>"
        );
        let q = html("On Mon, Arun <arun@acme.example> wrote:", "<p>x</p>", true);
        assert!(q.starts_with("<p>On Mon, Arun &lt;arun@acme.example&gt; wrote:</p>\n<blockquote type=\"cite\""));
        assert_eq!(text_of("<p>The <b>numbers</b> &amp; more</p><ul><li>North</li><li>South</li></ul>"), "The numbers & more\n- North\n- South");
        assert_eq!(under("<html><body><p>Me</p></body></html>", "Q"), "<html><body><p>Me</p>Q</body></html>");
    }
}
