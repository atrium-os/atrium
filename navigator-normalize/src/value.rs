//! Value repair, then VALIDATION.
//!
//! ★ The normalizer checks its own output against the profile's grammar
//! before emitting it (§6: "its output is checkable"). A declaration that
//! does not parse is repaired if there is an honest mapping, and DROPPED and
//! reported otherwise. So `value.invalid` from the renderer is impossible by
//! construction: the renderer can only see values this function accepted.

use navigator_style::token::tokenize;
use navigator_style::values::{grammar, parse_value, Specified};

/// Does the profile accept this value for this property?
///
/// ★ `Unresolved` — a value still holding `var()` — is NOT accepted. The
/// normalizer's job is to resolve custom properties; one that survives means
/// the substitution gave up (a cycle, or a chain past the depth limit), and
/// emitting it hands the renderer something it will refuse. Wikipedia
/// defines `--font-size-medium: var(--font-size-small)` and the reverse in
/// different scopes, which merges into a cycle here.
pub fn admits(prop: &str, value: &str) -> bool {
    grammar(prop).is_some() && matches!(parse_value(prop, &tokenize(value), &[]), Ok(Specified::Value(_) | Specified::Inherit | Specified::Initial))
}

/// Map a real-world value onto one the profile admits, or `None`.
pub fn repair(prop: &str, value: &str) -> Option<String> {
    let v = value.trim();
    let lv = v.to_ascii_lowercase();
    // ★ `unset` is `inherit` for an inherited property and `initial` for the
    // rest (CSS Cascade 4 §7.3) — both of which the profile admits, so it
    // compiles exactly. 747 declarations had been dropped for the keyword.
    if lv == "unset" {
        let (_, _, inherited) = navigator_style::values::grammar(prop)?;
        return Some((if inherited { "inherit" } else { "initial" }).to_string()).filter(|x| admits(prop, x));
    }
    // ★ `#rgba`, the 4-digit hex colour (CSS Color 4): the profile spells
    // colours `#rgb`, `#rrggbb` or `#rrggbbaa`, so it is expanded digit by
    // digit — `#0000` is transparent black. 3,820 declarations in the corpus
    // were dropped for their spelling alone.
    if prop.ends_with("color") {
        if let Some(h) = lv.strip_prefix('#').filter(|h| h.len() == 4 && h.chars().all(|c| c.is_ascii_hexdigit())) {
            let e: String = h.chars().flat_map(|c| [c, c]).collect();
            return Some(format!("#{e}")).filter(|x| admits(prop, x));
        }
    }
    let out = match prop {
        // `left`/`right` are physical; the profile is direction-relative.
        // ★ This assumes a left-to-right base direction, which is what the
        // corpus is; an rtl document would need the inverse, and that is a
        // real limitation rather than a subtlety to hide.
        "text-align" => match lv.as_str() { "left" => "start", "right" => "end", "justify-all" => "justify", _ => return None }.to_string(),
        "font-weight" => match lv.as_str() {
            "normal" => "400".into(), "bold" => "700".into(),
            "lighter" => "400".into(), "bolder" => "700".into(),
            _ => { let n: f64 = lv.parse().ok()?; format!("{}", ((n / 100.0).round() * 100.0).clamp(100.0, 900.0) as i64) }
        },
        "font-style" => match lv.as_str() { "oblique" => "italic", _ => return None }.to_string(),
        "display" => match lv.as_str() {
            "inline-flex" => "flex", "inline-grid" => "grid", "flow-root" => "block",
            "list-item" => "block", "inline-table" => "table",
            "table-row-group" | "table-header-group" | "table-footer-group" => "block",
            "table-column" | "table-column-group" | "table-caption" => "block",
            "-webkit-box" | "-webkit-flex" => "flex",
            _ => return None,
        }.to_string(),
        "list-style-type" => match lv.as_str() {
            "lower-alpha" | "upper-alpha" | "lower-roman" | "upper-roman" | "lower-latin" | "upper-latin" => "decimal",
            _ => return None,
        }.to_string(),
        "vertical-align" => match lv.as_str() {
            "text-top" | "super" => "top", "text-bottom" | "sub" => "bottom", "baseline" => "baseline",
            _ => return None,
        }.to_string(),
        "line-height" => match lv.as_str() { "normal" => "1.5".to_string(), _ => convert_units(&lv)? },
        "font-family" => {
            // ★ The profile's families are the SHIPPED stacks plus web fonts
            // declared by `@font-face` (§3.7). A page's own family names are
            // neither, so they are dropped and the stack is chosen by what
            // they were asking for — the shipped fonts then always cover the
            // text, which is the point of the rule.
            let mono = lv.contains("mono") || lv.contains("courier") || lv.contains("consol") || lv.contains("code");
            (if mono { "mono" } else { "sans" }).to_string()
        }
        "border-top-style" | "border-right-style" | "border-bottom-style" | "border-left-style" | "outline-style" =>
            match lv.as_str() { "double" | "groove" | "ridge" | "inset" | "outset" => "solid", "hidden" => "none", _ => return None }.to_string(),
        "border-spacing" | "transform-origin" => {
            let p: Vec<&str> = v.split_ascii_whitespace().collect();
            if p.len() == 1 { format!("{} {}", p[0], p[0]) } else { return None }
        }
        "background-position-x" | "background-position-y" => v.split_ascii_whitespace().next()?.to_string(),
        "background-image" => {
            // One image only: a layered background is not representable.
            if v.contains(',') && !v.starts_with("linear-gradient") { return None }
            return None;
        }
        "transform" => {
            let t = lv.replace("scalex(", "scale(").replace("scaley(", "scale(")
                      .replace("translatex(", "translate(").replace("translatey(", "translate(");
            if t == lv { return None } else { t }
        }
        // ★ NAMED GRID LINES. `[full-start] minmax(…,1fr) [content-start] …`
        // is how a real track list is written, and the profile places by
        // NUMBER. The names are only ever referenced by name, and those
        // references are resolved separately, so dropping the brackets keeps
        // every track and loses nothing the profile can use. Dropping the
        // whole declaration instead left MDN's page with one implicit column
        // and its sidebar on top of its content.
        "grid-template-columns" | "grid-template-rows" => {
            let stripped = strip_line_names(v);
            if stripped.trim() == v.trim() { return None }
            stripped
        }
        _ => convert_units(&lv)?,
    };
    admits(prop, &out).then_some(out)
}

/// Remove `[name other-name]` groups from a track list.
fn strip_line_names(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut depth = 0usize;
    for c in v.chars() {
        match c {
            '[' => depth += 1,
            ']' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Units the profile does not admit, converted to ones it does: `pt`, `pc`,
/// `in`, `cm`, `mm` are absolute and convert exactly at 96 dpi.
fn convert_units(v: &str) -> Option<String> {
    let mut out = String::new();
    let mut changed = false;
    for part in v.split_inclusive(|c: char| c == ' ' || c == ',') {
        let (num, rest) = part.trim_end_matches([' ', ',']).split_at(part.trim_end_matches([' ', ',']).len());
        let _ = (num, rest);
        let t = part.trim_end_matches([' ', ',']);
        let tail = &part[t.len()..];
        let conv = |suffix: &str, per_px: f64| -> Option<String> {
            let n: f64 = t.strip_suffix(suffix)?.parse().ok()?;
            Some(format!("{}px", (n * per_px * 1000.0).round() / 1000.0))
        };
        let repl = conv("pt", 96.0 / 72.0).or_else(|| conv("pc", 16.0))
            .or_else(|| conv("in", 96.0)).or_else(|| conv("cm", 96.0 / 2.54)).or_else(|| conv("mm", 96.0 / 25.4));
        match repl {
            Some(r) => { changed = true; out.push_str(&r); out.push_str(tail) }
            None => out.push_str(part),
        }
    }
    changed.then_some(out)
}
