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
/// ★ The INLINE sides depend on the element's own `direction`, which the
/// cascade has not decided yet when a declaration is expanded. So an inline
/// side is written `@start` / `@end` here (`margin-@start`), carried through
/// the cascade beside the physical properties it competes with, and
/// resolved per element once directions are known (`physical`, and the
/// direction pass in lib.rs). Mapping inline-start to `left` outright laid
/// every right-to-left page out as if it were left-to-right. The block
/// sides are top and bottom: the profile is horizontal only.
fn logical(name: &str) -> Option<Vec<&'static str>> {
    Some(match name {
        "margin-inline-start" => vec!["margin-@start"],
        "margin-inline-end" => vec!["margin-@end"],
        "margin-block-start" => vec!["margin-top"],
        "margin-block-end" => vec!["margin-bottom"],
        "padding-inline-start" => vec!["padding-@start"],
        "padding-inline-end" => vec!["padding-@end"],
        "padding-block-start" => vec!["padding-top"],
        "padding-block-end" => vec!["padding-bottom"],
        "inset-inline-start" => vec!["@start"],
        "inset-inline-end" => vec!["@end"],
        "inset-block-start" => vec!["top"],
        "inset-block-end" => vec!["bottom"],
        "inline-size" => vec!["width"],
        "block-size" => vec!["height"],
        "min-inline-size" => vec!["min-width"],
        "max-inline-size" => vec!["max-width"],
        "min-block-size" => vec!["min-height"],
        "max-block-size" => vec!["max-height"],
        "border-inline-start-width" => vec!["border-@start-width"],
        "border-inline-end-width" => vec!["border-@end-width"],
        "border-block-start-width" => vec!["border-top-width"],
        "border-block-end-width" => vec!["border-bottom-width"],
        "border-inline-start-style" => vec!["border-@start-style"],
        "border-inline-end-style" => vec!["border-@end-style"],
        "border-block-start-style" => vec!["border-top-style"],
        "border-block-end-style" => vec!["border-bottom-style"],
        "border-inline-start-color" => vec!["border-@start-color"],
        "border-inline-end-color" => vec!["border-@end-color"],
        "border-block-start-color" => vec!["border-top-color"],
        "border-block-end-color" => vec!["border-bottom-color"],
        // The two-value forms: one value for both sides, two for each.
        "margin-inline" => vec!["margin-@start", "margin-@end"],
        "margin-block" => vec!["margin-top", "margin-bottom"],
        "padding-inline" => vec!["padding-@start", "padding-@end"],
        "padding-block" => vec!["padding-top", "padding-bottom"],
        "inset-inline" => vec!["@start", "@end"],
        "inset-block" => vec!["top", "bottom"],
        _ => return None,
    })
}

/// A property as written for an element whose direction is `rtl` (or not):
/// the inline `@start` / `@end` sides become physical ones.
pub fn physical(p: &str, rtl: bool) -> String {
    if !p.contains('@') { return p.to_string() }
    let (s, e) = if rtl { ("right", "left") } else { ("left", "right") };
    p.replace("@start", s).replace("@end", e)
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
    // The inline-side border shorthands are the physical ones on `@start` /
    // `@end` sides, and the logical corners are physical corners on them.
    let inline_side = |n: &str| match n { "border-inline-start" => Some(vec![("border-left", "@start")]),
        "border-inline-end" => Some(vec![("border-right", "@end")]),
        "border-inline" => Some(vec![("border-left", "@start"), ("border-right", "@end")]), _ => None };
    if let Some(sides) = inline_side(name) {
        let mut out = vec![];
        for (phys, side) in sides {
            let side_name = phys.trim_start_matches("border-");
            out.extend(expand(phys, value)?.into_iter().map(|(p, v)| (p.replace(&format!("-{side_name}-"), &format!("-{side}-")), v)));
        }
        return Some(out);
    }
    let corner = match name {
        "border-start-start-radius" => Some("border-top-@start-radius"), "border-start-end-radius" => Some("border-top-@end-radius"),
        "border-end-start-radius" => Some("border-bottom-@start-radius"), "border-end-end-radius" => Some("border-bottom-@end-radius"),
        _ => None,
    };
    if let Some(c) = corner { return one(c, &whole) }
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
        // ★ Two shorthands the corpus uses and the profile has longhands for
        // (765 and 623 declarations dropped before).
        "background-position" => {
            if value.iter().any(|t| t.tok == Tok::Comma) || p.is_empty() || p.len() > 2 { return None }
            let kw = |x: &str| x.to_ascii_lowercase();
            let vertical = |x: &str| matches!(kw(x).as_str(), "top" | "bottom");
            let horizontal = |x: &str| matches!(kw(x).as_str(), "left" | "right");
            let (x, y) = match p.len() {
                // One value: a vertical keyword sets Y; anything else sets X.
                1 if vertical(&p[0]) => ("center".to_string(), kw(&p[0])),
                1 => (if is_len(&p[0]) { p[0].clone() } else { kw(&p[0]) }, "center".to_string()),
                // Two: X then Y, unless the keywords say otherwise (`top left`).
                _ if vertical(&p[0]) || horizontal(&p[1]) => (kw(&p[1]), kw(&p[0])),
                _ => (p[0].clone(), p[1].clone()),
            };
            let pct = |v: String| match v.as_str() { "left" | "top" => "0%".to_string(), "center" => "50%".to_string(), "right" | "bottom" => "100%".to_string(), _ => v };
            Some(vec![("background-position-x".into(), pct(x)), ("background-position-y".into(), pct(y))])
        }
        "font" => {
            // `font: inherit` (a CSS-wide keyword) sets every longhand to it;
            // a lone SYSTEM font (`caption`, `menu`, …) describes the host: refused.
            if p.len() == 1 {
                let k = p[0].to_ascii_lowercase();
                if !matches!(k.as_str(), "inherit" | "initial" | "unset") { return None }
                return Some(["font-style", "font-weight", "font-size", "line-height", "font-family"].iter().map(|l| (l.to_string(), k.clone())).collect());
            }
            let (mut style, mut weight) = ("normal".to_string(), "400".to_string());
            let mut i = 0;
            // Optional style / variant / weight / stretch, in any order.
            while i < p.len() {
                let x = p[i].to_ascii_lowercase();
                match x.as_str() {
                    "normal" => {}
                    "italic" | "oblique" => style = x.clone(),
                    "bold" => weight = "700".into(), "bolder" => weight = "700".into(), "lighter" => weight = "400".into(),
                    "small-caps" | "ultra-condensed" | "extra-condensed" | "condensed" | "semi-condensed"
                    | "semi-expanded" | "expanded" | "extra-expanded" | "ultra-expanded" => {}
                    _ if x.parse::<f64>().is_ok() && !x.contains('.') && x.len() == 3 => weight = x.clone(),
                    _ => break,
                }
                i += 1;
            }
            // The size, required; `/line-height` attached or as separate tokens.
            let sz = p.get(i)?.clone();
            let (size, mut lh) = match sz.split_once('/') { Some((a, b)) => (a.to_string(), Some(b.to_string()).filter(|b| !b.is_empty())), None => (sz, None) };
            i += 1;
            if lh.is_none() && p.get(i).map(|x| x.as_str()) == Some("/") { lh = p.get(i + 1).cloned(); i += 2 }
            else if lh.is_none() && p.get(i).is_some_and(|x| x.starts_with('/')) { lh = Some(p[i][1..].to_string()); i += 1 }
            let family = p[i..].join(" ");
            if family.trim().is_empty() { return None }
            // A shorthand RESETS what it covers: `font: 16px serif` makes bold
            // text normal again.
            Some(vec![
                ("font-style".into(), style), ("font-weight".into(), weight),
                ("font-size".into(), size), ("line-height".into(), lh.unwrap_or_else(|| "normal".into())),
                ("font-family".into(), family),
            ])
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
            let (a, b) = match joined.split_once('/') { Some((a, b)) => (a.trim().to_string(), b.trim().to_string()), None => {
                // A lone NAME is that name at both ends (Grid §8.4): `grid-column:
                // full` spans full-start to full-end. A lone number or span
                // ends at `auto`.
                let a = joined.trim().to_string();
                let named = a.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && !a.contains(' ')
                    && !matches!(a.as_str(), "auto" | "inherit" | "initial" | "unset" | "revert");
                let b = if named { a.clone() } else { "auto".to_string() };
                (a, b)
            } };
            Some(vec![(format!("grid-{axis}-start"), a), (format!("grid-{axis}-end"), b)])
        }
        "grid-area" => {
            let joined = p.join(" ");
            let mut f: Vec<String> = joined.split('/').map(|x| x.trim().to_string()).collect();
            if f.is_empty() || f.len() > 4 { return None }
            // An omitted slot copies its START when that is a name, else is
            // `auto` (Grid §8.4): `grid-area: 2 / 1` is rows 2-auto, columns 1-auto.
            let named = |s: &str| s.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && !s.contains(' ') && s != "auto";
            while f.len() < 4 {
                let from = if f.len() == 1 { 0 } else { f.len() - 2 };
                let v = if named(&f[from]) { f[from].clone() } else { "auto".to_string() };
                f.push(v);
            }
            Some(vec![("grid-row-start".into(), f[0].clone()), ("grid-column-start".into(), f[1].clone()),
                      ("grid-row-end".into(), f[2].clone()), ("grid-column-end".into(), f[3].clone())])
        }
        "grid-template" => grid_template(value),
        "grid" => grid(value),
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

/// Split a value at its top-level `/`s.
fn slash_split(v: &[Token]) -> Vec<Vec<Token>> {
    let (mut out, mut cur, mut d) = (vec![], vec![], 0usize);
    for t in v {
        match &t.tok {
            Tok::LParen | Tok::Function(_) => { d += 1; cur.push(t.clone()) }
            Tok::RParen => { d = d.saturating_sub(1); cur.push(t.clone()) }
            Tok::Delim('/') if d == 0 => out.push(std::mem::take(&mut cur)),
            _ => cur.push(t.clone()),
        }
    }
    out.push(cur);
    out
}

/// `grid-template` (Grid §7.4): `none`, `<rows> / <columns>`, or the ASCII
/// art form — each string a row of `grid-template-areas`, followed by that
/// row's size (`auto` when absent) and its line names.
fn grid_template(value: &[Token]) -> Option<Vec<(String, String)>> {
    let halves = slash_split(value);
    let text = |v: &[Token]| write_tokens(v).trim().to_string();
    let out = |r: String, c: String, a: String| Some(vec![
        ("grid-template-rows".to_string(), r), ("grid-template-columns".to_string(), c),
        ("grid-template-areas".to_string(), a)]);
    if halves.len() == 1 && text(value).eq_ignore_ascii_case("none") {
        return out("none".into(), "none".into(), "none".into());
    }
    if halves.len() > 2 { return None }
    let has_str = halves[0].iter().any(|t| matches!(t.tok, Tok::Str(_)));
    let cols = halves.get(1).map(|h| text(h)).unwrap_or_else(|| "none".into());
    if !has_str {
        if halves.len() != 2 || text(&halves[0]).is_empty() || cols.is_empty() { return None }
        return out(text(&halves[0]), cols, "none".into());
    }
    // ASCII art: [names]? "row" size? [names]? … A row whose string has no
    // size is `auto`; names between two rows name ONE line, so they merge.
    let (mut rows, mut areas, mut open): (Vec<String>, Vec<String>, bool) = (vec![], vec![], false);
    for piece in parts(&halves[0]) {
        if piece.starts_with('"') || piece.starts_with('\'') {
            if open { rows.push("auto".into()) }
            areas.push(format!("\"{}\"", &piece[1..piece.len().saturating_sub(1)]));
            open = true;
        } else if piece.starts_with('[') {
            if open { rows.push("auto".into()); open = false }
            match rows.last_mut() {
                Some(last) if last.starts_with('[') =>
                    *last = format!("[{} {}]", last.trim_matches(['[', ']']).trim(), piece.trim_matches(['[', ']']).trim()),
                _ => rows.push(piece),
            }
        } else {
            if !open { return None }
            rows.push(piece); open = false;
        }
    }
    if open { rows.push("auto".into()) }
    if areas.is_empty() { return None }
    out(rows.join(" "), cols, areas.join(" "))
}

/// `grid` (Grid §7.8): a `grid-template`, or one axis's template with the
/// other axis `auto-flow` (+ its implicit track size). Every sub-property
/// the shorthand does not set is reset to its initial value.
fn grid(value: &[Token]) -> Option<Vec<(String, String)>> {
    let halves = slash_split(value);
    let flow_side = halves.iter().position(|h| parts(h).iter().any(|x| x.eq_ignore_ascii_case("auto-flow")));
    let Some(side) = flow_side else {
        let mut v = grid_template(value)?;
        v.extend([("grid-auto-flow".to_string(), "row".to_string()), ("grid-auto-rows".to_string(), "auto".to_string()),
                  ("grid-auto-columns".to_string(), "auto".to_string())]);
        return Some(v);
    };
    if halves.len() != 2 { return None }
    let mut dense = false;
    let mut size = vec![];
    for x in parts(&halves[side]) {
        if x.eq_ignore_ascii_case("auto-flow") { continue }
        if x.eq_ignore_ascii_case("dense") { dense = true; continue }
        size.push(x);
    }
    let size = if size.is_empty() { "auto".to_string() } else { size.join(" ") };
    let other = write_tokens(&halves[1 - side]).trim().to_string();
    if other.is_empty() { return None }
    let flow = |a: &str| if dense { format!("{a} dense") } else { a.to_string() };
    // auto-flow on the ROW side (before the slash): rows are implicit.
    let (rows, cols, f, ar, ac) = if side == 0 { ("none".to_string(), other, flow("row"), size, "auto".to_string()) }
        else { (other, "none".to_string(), flow("column"), "auto".to_string(), size) };
    Some(vec![("grid-template-rows".into(), rows), ("grid-template-columns".into(), cols),
              ("grid-template-areas".into(), "none".into()), ("grid-auto-flow".into(), f),
              ("grid-auto-rows".into(), ar), ("grid-auto-columns".into(), ac)])
}
