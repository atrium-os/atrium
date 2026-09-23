//! Shorthand → longhands. The profile admits longhands only (§3.2), and the
//! corpus named which shorthands it actually uses: `margin`, `padding`,
//! `background`, `border`, `border-radius`, `text-decoration`, `overflow`,
//! `list-style`, `grid-column`, `font`, `border-top/right/bottom/left`,
//! `gap`, `flex-flow` — plus the ones that come free from the same shapes.

use crate::css::write_tokens;
use navigator_style::token::{Tok, Token};

/// Split a value on top-level whitespace into its components, as text.
pub fn parts(v: &[Token]) -> Vec<String> {
    let (mut out, mut cur, mut d) = (vec![], Vec::<Token>::new(), 0usize);
    for t in v {
        match &t.tok {
            Tok::LParen | Tok::Function(_) => { d += 1; cur.push(t.clone()) }
            Tok::RParen => { d = d.saturating_sub(1); cur.push(t.clone()) }
            Tok::Whitespace if d == 0 => {
                if !cur.is_empty() { out.push(write_tokens(&cur).trim().to_string()); cur.clear() }
            }
            _ => cur.push(t.clone()),
        }
    }
    if !cur.is_empty() { out.push(write_tokens(&cur).trim().to_string()) }
    out.into_iter().filter(|s| !s.is_empty()).collect()
}

/// The 1-to-4 value box pattern: top, right, bottom, left.
fn box_sides(p: &[String]) -> Option<[String; 4]> {
    Some(match p.len() {
        1 => [p[0].clone(), p[0].clone(), p[0].clone(), p[0].clone()],
        2 => [p[0].clone(), p[1].clone(), p[0].clone(), p[1].clone()],
        3 => [p[0].clone(), p[1].clone(), p[2].clone(), p[1].clone()],
        4 => [p[0].clone(), p[1].clone(), p[2].clone(), p[3].clone()],
        _ => return None,
    })
}

fn is_len(s: &str) -> bool {
    let s = s.trim();
    s == "0" || s == "auto" || s.ends_with("px") || s.ends_with("em") || s.ends_with("rem") || s.ends_with('%')
        || s.ends_with("ex") || s.ends_with("ch") || s.ends_with("vw") || s.ends_with("vh")
        || s.starts_with("calc(") || s.starts_with("var(")
}

fn is_color(s: &str) -> bool {
    let s = s.trim();
    s.starts_with('#') || s.starts_with("rgb") || s.starts_with("hsl") || s == "transparent"
        || s == "currentcolor" || s == "currentColor" || navigator_style::values::named_color(s).is_some()
}

const BORDER_STYLES: &[&str] = &["none", "hidden", "solid", "dashed", "dotted", "double", "groove", "ridge", "inset", "outset"];

/// ★ LOGICAL properties → physical ones. The profile's rows are physical
/// (§3.3), and modern CSS is written logically: `padding-block` alone
/// appears 5024 times in a 29-document corpus, and mdBook's entire page
/// layout hangs on one `margin-inline-start`. Dropping them as "unknown"
/// silently removes the layout.
///
/// The mapping assumes a horizontal, left-to-right writing mode — which is
/// what the profile's `direction: ltr` default gives. A document that sets
/// `direction: rtl` would need inline-start and inline-end swapped, and that
/// is a real limitation rather than an oversight.
fn logical(name: &str) -> Option<Vec<&'static str>> {
    Some(match name {
        "margin-inline-start" => vec!["margin-left"],
        "margin-inline-end" => vec!["margin-right"],
        "margin-block-start" => vec!["margin-top"],
        "margin-block-end" => vec!["margin-bottom"],
        "padding-inline-start" => vec!["padding-left"],
        "padding-inline-end" => vec!["padding-right"],
        "padding-block-start" => vec!["padding-top"],
        "padding-block-end" => vec!["padding-bottom"],
        "inset-inline-start" => vec!["left"],
        "inset-inline-end" => vec!["right"],
        "inset-block-start" => vec!["top"],
        "inset-block-end" => vec!["bottom"],
        "inline-size" => vec!["width"],
        "block-size" => vec!["height"],
        "min-inline-size" => vec!["min-width"],
        "max-inline-size" => vec!["max-width"],
        "min-block-size" => vec!["min-height"],
        "max-block-size" => vec!["max-height"],
        "border-inline-start-width" => vec!["border-left-width"],
        "border-inline-end-width" => vec!["border-right-width"],
        "border-block-start-width" => vec!["border-top-width"],
        "border-block-end-width" => vec!["border-bottom-width"],
        "border-inline-start-style" => vec!["border-left-style"],
        "border-inline-end-style" => vec!["border-right-style"],
        "border-block-start-style" => vec!["border-top-style"],
        "border-block-end-style" => vec!["border-bottom-style"],
        "border-inline-start-color" => vec!["border-left-color"],
        "border-inline-end-color" => vec!["border-right-color"],
        "border-block-start-color" => vec!["border-top-color"],
        "border-block-end-color" => vec!["border-bottom-color"],
        // The two-value forms: one value for both sides, two for each.
        "margin-inline" => vec!["margin-left", "margin-right"],
        "margin-block" => vec!["margin-top", "margin-bottom"],
        "padding-inline" => vec!["padding-left", "padding-right"],
        "padding-block" => vec!["padding-top", "padding-bottom"],
        "inset-inline" => vec!["left", "right"],
        "inset-block" => vec!["top", "bottom"],
        _ => return None,
    })
}

/// Expand `name: value` into longhand pairs. An unknown shorthand returns
/// `None`, and the caller drops it — reported, never guessed.
pub fn expand(name: &str, value: &[Token]) -> Option<Vec<(String, String)>> {
    let p = parts(value);
    let whole = write_tokens(value).trim().to_string();
    if let Some(targets) = logical(name) {
        return Some(match (targets.len(), p.len()) {
            (2, 2) => vec![(targets[0].into(), p[0].clone()), (targets[1].into(), p[1].clone())],
            (2, _) => targets.iter().map(|t| ((*t).to_string(), whole.clone())).collect(),
            _ => vec![(targets[0].into(), whole.clone())],
        });
    }
    let one = |n: &str, v: &str| Some(vec![(n.to_string(), v.to_string())]);
    match name {
        "margin" | "padding" => {
            let s = box_sides(&p)?;
            Some(["top", "right", "bottom", "left"].iter().zip(s).map(|(side, v)| (format!("{name}-{side}"), v)).collect())
        }
        "border-width" | "border-style" | "border-color" => {
            let what = name.rsplit('-').next()?;
            let s = box_sides(&p)?;
            Some(["top", "right", "bottom", "left"].iter().zip(s).map(|(side, v)| (format!("border-{side}-{what}"), v)).collect())
        }
        "border-radius" => {
            // The `/` form (elliptical) has no profile representation: the
            // profile's radius is one length per corner.
            if p.iter().any(|x| x == "/") { return None }
            let s = box_sides(&p)?;
            Some(vec![("border-top-left-radius".into(), s[0].clone()), ("border-top-right-radius".into(), s[1].clone()),
                      ("border-bottom-right-radius".into(), s[2].clone()), ("border-bottom-left-radius".into(), s[3].clone())])
        }
        "border" | "border-top" | "border-right" | "border-bottom" | "border-left" => {
            let sides: Vec<&str> = if name == "border" { vec!["top", "right", "bottom", "left"] } else { vec![name.rsplit('-').next()?] };
            let (mut w, mut st, mut c) = (None, None, None);
            for x in &p {
                let lx = x.to_ascii_lowercase();
                if BORDER_STYLES.contains(&lx.as_str()) { st = Some(lx) }
                else if is_color(x) { c = Some(x.clone()) }
                else if is_len(x) || ["thin", "medium", "thick"].contains(&lx.as_str()) {
                    w = Some(match lx.as_str() { "thin" => "1px".into(), "medium" => "3px".into(), "thick" => "5px".into(), _ => x.clone() })
                }
            }
            let mut out = vec![];
            for side in sides {
                // ★ The initial values matter: `border: 1px solid` has no
                // colour, and CSS says currentColor — leaving it out would
                // let an earlier declaration show through.
                out.push((format!("border-{side}-width"), w.clone().unwrap_or_else(|| "3px".into())));
                out.push((format!("border-{side}-style"), st.clone().unwrap_or_else(|| "none".into())));
                out.push((format!("border-{side}-color"), c.clone().unwrap_or_else(|| "currentColor".into())));
            }
            Some(out)
        }
        "background" => {
            // Only the parts the profile has rows for; a layered background
            // (commas at the top level) is not representable.
            if value.iter().any(|t| t.tok == Tok::Comma) && !whole.contains('(') { return None }
            let (mut color, mut image, mut repeat, mut size, mut px, mut py) = (None, None, None, None, None, None);
            let mut i = 0;
            while i < p.len() {
                let x = &p[i];
                let lx = x.to_ascii_lowercase();
                if lx.starts_with("url(") || lx.starts_with("linear-gradient(") { image = Some(x.clone()) }
                else if ["repeat", "no-repeat", "repeat-x", "repeat-y"].contains(&lx.as_str()) { repeat = Some(lx) }
                else if lx == "cover" || lx == "contain" { size = Some(lx) }
                else if ["left", "right", "center"].contains(&lx.as_str()) && px.is_none() { px = Some(lx) }
                else if ["top", "bottom"].contains(&lx.as_str()) { py = Some(lx) }
                else if lx == "/" { if let Some(n) = p.get(i + 1) { size = Some(n.clone()); i += 1 } }
                else if is_color(x) { color = Some(x.clone()) }
                else if is_len(x) { if px.is_none() { px = Some(x.clone()) } else if py.is_none() { py = Some(x.clone()) } }
                i += 1;
            }
            let mut out = vec![];
            // A shorthand RESETS every longhand it covers, which is the whole
            // reason `background: #fff` must also clear an inherited image.
            out.push(("background-color".into(), color.unwrap_or_else(|| "transparent".into())));
            out.push(("background-image".into(), image.unwrap_or_else(|| "none".into())));
            out.push(("background-repeat".into(), repeat.unwrap_or_else(|| "repeat".into())));
            out.push(("background-size".into(), size.unwrap_or_else(|| "auto".into())));
            out.push(("background-position-x".into(), px.unwrap_or_else(|| "0%".into())));
            out.push(("background-position-y".into(), py.unwrap_or_else(|| "0%".into())));
            Some(out)
        }
        "overflow" => {
            let s = if p.len() == 1 { [p[0].clone(), p[0].clone()] } else if p.len() == 2 { [p[0].clone(), p[1].clone()] } else { return None };
            Some(vec![("overflow-x".into(), s[0].clone()), ("overflow-y".into(), s[1].clone())])
        }
        "gap" => {
            let s = if p.len() == 1 { [p[0].clone(), p[0].clone()] } else if p.len() == 2 { [p[0].clone(), p[1].clone()] } else { return None };
            Some(vec![("row-gap".into(), s[0].clone()), ("column-gap".into(), s[1].clone())])
        }
        "text-decoration" => {
            let (mut line, mut color) = (None, None);
            for x in &p {
                let lx = x.to_ascii_lowercase();
                if ["none", "underline", "line-through", "overline"].contains(&lx.as_str()) { line = Some(lx) }
                else if is_color(x) { color = Some(x.clone()) }
            }
            let mut out = vec![("text-decoration-line".into(), line.unwrap_or_else(|| "none".into()))];
            if let Some(c) = color { out.push(("text-decoration-color".into(), c)) }
            Some(out)
        }
        "list-style" => {
            let (mut ty, mut pos) = (None, None);
            for x in &p {
                let lx = x.to_ascii_lowercase();
                if ["inside", "outside"].contains(&lx.as_str()) { pos = Some(lx) }
                else if lx != "none" || ty.is_none() { ty = Some(lx) }
            }
            let mut out = vec![];
            if let Some(t) = ty { out.push(("list-style-type".into(), t)) }
            if let Some(pp) = pos { out.push(("list-style-position".into(), pp)) }
            Some(out)
        }
        "flex-flow" => {
            let (mut dir, mut wrap) = (None, None);
            for x in &p {
                let lx = x.to_ascii_lowercase();
                if lx.starts_with("row") || lx.starts_with("column") { dir = Some(lx) } else { wrap = Some(lx) }
            }
            let mut out = vec![];
            if let Some(d) = dir { out.push(("flex-direction".into(), d)) }
            if let Some(w) = wrap { out.push(("flex-wrap".into(), w)) }
            Some(out)
        }
        "flex" => {
            match p.len() {
                1 if p[0] == "none" => Some(vec![("flex-grow".into(), "0".into()), ("flex-shrink".into(), "0".into()), ("flex-basis".into(), "auto".into())]),
                1 if !is_len(&p[0]) => Some(vec![("flex-grow".into(), p[0].clone()), ("flex-shrink".into(), "1".into()), ("flex-basis".into(), "0".into())]),
                1 => Some(vec![("flex-grow".into(), "1".into()), ("flex-shrink".into(), "1".into()), ("flex-basis".into(), p[0].clone())]),
                2 if is_len(&p[1]) => Some(vec![("flex-grow".into(), p[0].clone()), ("flex-shrink".into(), "1".into()), ("flex-basis".into(), p[1].clone())]),
                2 => Some(vec![("flex-grow".into(), p[0].clone()), ("flex-shrink".into(), p[1].clone()), ("flex-basis".into(), "0".into())]),
                3 => Some(vec![("flex-grow".into(), p[0].clone()), ("flex-shrink".into(), p[1].clone()), ("flex-basis".into(), p[2].clone())]),
                _ => None,
            }
        }
        "grid-column" | "grid-row" => {
            let axis = name.rsplit('-').next()?;
            let joined = p.join(" ");
            let (a, b) = match joined.split_once('/') { Some((a, b)) => (a.trim().to_string(), b.trim().to_string()), None => (joined.trim().to_string(), "auto".to_string()) };
            Some(vec![(format!("grid-{axis}-start"), a), (format!("grid-{axis}-end"), b)])
        }
        "grid-area" => {
            let joined = p.join(" ");
            let f: Vec<String> = joined.split('/').map(|x| x.trim().to_string()).collect();
            if f.len() != 4 { return None }
            Some(vec![("grid-row-start".into(), f[0].clone()), ("grid-column-start".into(), f[1].clone()),
                      ("grid-row-end".into(), f[2].clone()), ("grid-column-end".into(), f[3].clone())])
        }
        "font" => {
            // Only the `[style] [weight] size[/line-height] family` form.
            let joined = p.join(" ");
            let (size_part, family) = {
                let idx = p.iter().position(|x| is_len(x) || x.contains('/'))?;
                (p[idx].clone(), p[idx + 1..].join(" "))
            };
            let (size, lh) = match size_part.split_once('/') { Some((s, l)) => (s.to_string(), Some(l.to_string())), None => (size_part, None) };
            let mut out = vec![("font-size".into(), size)];
            if let Some(l) = lh { out.push(("line-height".into(), l)) }
            if !family.is_empty() { out.push(("font-family".into(), family)) }
            for x in &p {
                let lx = x.to_ascii_lowercase();
                if lx == "italic" || lx == "oblique" { out.push(("font-style".into(), "italic".into())) }
                if lx == "bold" || lx == "700" { out.push(("font-weight".into(), "700".into())) }
            }
            let _ = joined;
            Some(out)
        }
        "place-items" | "place-content" | "place-self" => {
            let what = name.strip_prefix("place-")?;
            let s = if p.len() == 1 { [p[0].clone(), p[0].clone()] } else if p.len() == 2 { [p[0].clone(), p[1].clone()] } else { return None };
            Some(vec![(format!("align-{what}"), s[0].clone()), (format!("justify-{what}"), s[1].clone())])
        }
        "inset" => {
            let s = box_sides(&p)?;
            Some(vec![("top".into(), s[0].clone()), ("right".into(), s[1].clone()), ("bottom".into(), s[2].clone()), ("left".into(), s[3].clone())])
        }
        "outline" => {
            let (mut w, mut st, mut c) = (None, None, None);
            for x in &p {
                let lx = x.to_ascii_lowercase();
                if BORDER_STYLES.contains(&lx.as_str()) { st = Some(lx) }
                else if is_color(x) { c = Some(x.clone()) }
                else if is_len(x) { w = Some(x.clone()) }
            }
            let mut out = vec![];
            if let Some(v) = w { out.push(("outline-width".into(), v)) }
            if let Some(v) = st { out.push(("outline-style".into(), v)) }
            if let Some(v) = c { out.push(("outline-color".into(), v)) }
            Some(out)
        }
        _ => one(name, &whole).filter(|_| false),
    }
}
