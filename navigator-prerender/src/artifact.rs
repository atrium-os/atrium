//! The recording as a publishable artifact: the chosen document, plus the
//! table of what a reader can do to it.
//!
//! JSON, written by hand rather than through a serializer, for two reasons
//! that matter here. Field ORDER is fixed, because this feeds a
//! content-addressed store and a map's iteration order would churn the hash
//! for a document that had not changed. And the schema is small and closed,
//! so the only real risk is string escaping — which is therefore the part
//! with its own tests.

use crate::{Conversion, Effect, Tier, TierPolicy};

/// ★ VERSION 2 adds `index` to an insert: the position the inserted markup
/// occupies among its parent's children. Version 1 recorded only the parent,
/// so a replay could do nothing but append — a row inserted into the middle
/// of a list came back at the end, silently.
///
/// Bumped rather than added compatibly because this is a content-addressed
/// store: a consumer that read a `/1` recording and assumed the field was
/// merely absent would produce a document that differs from the recorded one
/// with nothing to signal it. The version is how a consumer knows which
/// guarantee it is getting.
/// v3 adds `stylesheets` and `images`: what the normalizer needs and cannot
/// fetch for itself (Profile v1 §6 — it has no capabilities — and §3.13 —
/// there is no measurement path later in the lane). A v2 reader that ignores
/// unknown fields still reads a v3 recording; the bump is so a reader can
/// TELL, rather than infer it from a field's absence.
pub const FORMAT: &str = "atrium-navigator-recording/3";

/// ★ WHICH TRANSITIONS SURVIVE DEPENDS ON WHICH DOCUMENT IS PUBLISHED.
///
/// Transitions are recorded against the post-script DOM. When the policy
/// publishes THAT document, every one of them still addresses it. When the
/// policy demotes to tier 1, two things stop being true: a trigger the
/// scripts themselves created is not in that document at all, and a
/// POSITIONAL path may point at a different node because the script-modified
/// tree had a different shape.
///
/// So publishing tier 1 keeps only transitions that are anchored AND
/// addressed by id, which survives re-parsing. The rest are dropped and
/// COUNTED — a recording that silently lost half its entries would be
/// indistinguishable from a page with little to do.
fn usable(t: &crate::Transition, tier: Tier) -> bool {
    match tier {
        Tier::Two => true,
        Tier::One => t.anchored && t.trigger.starts_with('#'),
    }
}

pub fn emit(url: Option<&str>, c: &Conversion, policy: &TierPolicy) -> String {
    let (document, decision) = policy.artifact(c);
    let kept: Vec<&crate::Transition> =
        c.transitions.iter().filter(|t| usable(t, decision.tier)).collect();
    let dropped = c.transitions.len() - kept.len();

    let mut s = String::new();
    s.push_str("{\n");
    field(&mut s, "format", FORMAT, false);
    field(&mut s, "url", url.unwrap_or(""), false);
    s.push_str(&format!("  \"tier\": {},\n",
        match decision.tier { Tier::One => 1, Tier::Two => 2 }));
    field(&mut s, "tier_reason", decision.reason, false);

    s.push_str("  \"measurements\": {\n");
    s.push_str(&format!("    \"elements\": {},\n", c.elements_after));
    s.push_str(&format!("    \"text_before\": {},\n", c.text_before));
    s.push_str(&format!("    \"text_after\": {},\n", c.text_after));
    s.push_str(&format!("    \"scripts_total\": {},\n", c.scripts_total));
    s.push_str(&format!("    \"scripts_failed\": {},\n", c.scripts_failed));
    s.push_str(&format!("    \"interactive_found\": {},\n", c.interactive_found));
    s.push_str(&format!("    \"transitions_dropped\": {dropped}\n"));
    s.push_str("  },\n");

    s.push_str("  \"transitions\": [");
    for (i, t) in kept.iter().enumerate() {
        s.push_str(if i == 0 { "\n" } else { ",\n" });
        s.push_str("    {\n");
        s.push_str(&format!("      \"trigger\": {},\n", json_str(&t.trigger)));
        s.push_str(&format!("      \"event\": {},\n", json_str(&t.event)));
        s.push_str(&format!("      \"anchored\": {},\n", t.anchored));
        // Stated explicitly rather than left for a consumer to infer: it
        // decides whether the transition needs any content at all.
        s.push_str(&format!("      \"attribute_only\": {},\n", t.is_attribute_only()));
        s.push_str("      \"effects\": [");
        for (j, e) in t.effects.iter().enumerate() {
            s.push_str(if j == 0 { "\n" } else { ",\n" });
            s.push_str("        ");
            s.push_str(&effect_json(e));
        }
        s.push_str(if t.effects.is_empty() { "]\n" } else { "\n      ]\n" });
        s.push_str("    }");
    }
    s.push_str(if kept.is_empty() { "],\n" } else { "\n  ],\n" });

    // The subresources, in the order they were found.
    s.push_str("  \"stylesheets\": [");
    for (i, sh) in c.subresources.sheets.iter().enumerate() {
        s.push_str(if i == 0 { "\n" } else { ",\n" });
        s.push_str(&format!("    {{ \"href\": {}, \"media\": {}, \"text\": {} }}",
            json_str(&sh.href), json_str(&sh.media), json_str(&sh.text)));
    }
    s.push_str(if c.subresources.sheets.is_empty() { "],\n" } else { "\n  ],\n" });
    s.push_str("  \"images\": [");
    for (i, im) in c.subresources.images.iter().enumerate() {
        s.push_str(if i == 0 { "\n" } else { ",\n" });
        s.push_str(&format!("    {{ \"src\": {}, \"width\": {}, \"height\": {}, \"address\": {} }}",
            json_str(&im.src), im.width, im.height, json_str(&im.address)));
    }
    s.push_str(if c.subresources.images.is_empty() { "],\n" } else { "\n  ],\n" });
    s.push_str(&format!("  \"document\": {}\n", json_str(document)));
    s.push_str("}\n");
    s
}

fn effect_json(e: &Effect) -> String {
    match e {
        Effect::Attribute { target, name, from, to } => format!(
            "{{ \"kind\": \"attribute\", \"target\": {}, \"name\": {}, \"from\": {}, \"to\": {} }}",
            json_str(target), json_str(name), json_opt(from.as_deref()), json_opt(to.as_deref())),
        Effect::Insert { parent, index, html } => format!(
            "{{ \"kind\": \"insert\", \"parent\": {}, \"index\": {}, \"html\": {} }}",
            json_str(parent), index, json_str(html)),
        Effect::Remove { target } =>
            format!("{{ \"kind\": \"remove\", \"target\": {} }}", json_str(target)),
        Effect::Truncated { dropped } =>
            format!("{{ \"kind\": \"truncated\", \"dropped\": {dropped} }}"),
    }
}

fn field(s: &mut String, k: &str, v: &str, last: bool) {
    s.push_str(&format!("  \"{k}\": {}{}\n", json_str(v), if last { "" } else { "," }));
}

fn json_opt(v: Option<&str>) -> String {
    match v { Some(v) => json_str(v), None => "null".into() }
}

/// ★ The one genuinely risky part, so it is the part with tests. A document
/// is embedded whole, and it contains quotes, backslashes and control
/// characters; getting this wrong produces a file that parses as something
/// else, which is worse than one that does not parse at all.
pub fn json_str(v: &str) -> String {
    let mut out = String::with_capacity(v.len() + 2);
    out.push('"');
    for c in v.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            // Everything below 0x20 must be escaped; JSON has no literal
            // control characters inside a string.
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
