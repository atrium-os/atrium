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
/// ★ v4 adds `templates` (legacy-web §5.4.5.6c): one handler's transition
/// recorded ONCE over the elements that share it, each instance binding its
/// trigger and the addresses (`$t`, `$N`) its effects name. It is written
/// ONLY when a recording has a template, and a recording without one stays
/// `/3`, byte for byte: a `/3` reader loses nothing from it. A recording
/// that has templates must not be read by a `/3` reader, which would ignore
/// the field and replay a subset with nothing to say so; the version is how
/// it can tell.
pub const FORMAT_V4: &str = "atrium-navigator-recording/4";

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
    usable_trigger(&t.trigger, t.anchored, tier)
}

/// The same rule for a template's instances, which are transitions.
fn usable_trigger(trigger: &str, anchored: bool, tier: Tier) -> bool {
    match tier {
        Tier::Two => true,
        Tier::One => anchored && trigger.starts_with('#'),
    }
}

pub fn emit(url: Option<&str>, c: &Conversion, policy: &TierPolicy) -> String {
    emit_part(url, c, policy, true)
}

/// ★ THE READABLE ARTIFACT (legacy-web §5.4.5.7, step 3): the recording
/// with no transition table — the document, its stylesheets and image sizes,
/// and the measurements a control (non-exploring) conversion would report.
/// It is what a reader needs to READ, and it exists before exploration runs;
/// `emit_transitions` carries the rest. For an exploring conversion it is
/// byte-identical to the recording a control conversion of the same page
/// writes (exploration is reverted, so the document is the same), and
/// `merge` of the two parts is byte-identical to `emit`.
pub fn emit_readable(url: Option<&str>, c: &Conversion, policy: &TierPolicy) -> String {
    emit_part(url, c, policy, false)
}

/// The transitions artifact's format.
pub const TRANSITIONS_FORMAT: &str = "atrium-navigator-transitions/1";

/// ★ THE TRANSITIONS ARTIFACT: exploration's output, delivered after the
/// readable artifact and bound to it by `readable`, the BLAKE3 of the
/// readable recording's bytes — a table recorded against one document must
/// not be attached to another. The `transitions` (and `templates`) blocks
/// are written exactly as `emit` writes them, which is what lets `merge`
/// splice them back verbatim.
pub fn emit_transitions(url: Option<&str>, c: &Conversion, policy: &TierPolicy, readable: &str) -> String {
    let (_, decision) = policy.artifact(c);
    let (block, _, dropped) = transitions_block(c, decision.tier);
    let mut s = String::new();
    s.push_str("{\n");
    field(&mut s, "format", TRANSITIONS_FORMAT, false);
    field(&mut s, "url", url.unwrap_or(""), false);
    field(&mut s, "readable", &format!("blake3:{}", blake3::hash(readable.as_bytes()).to_hex()), false);
    s.push_str(&format!("  \"interactive_found\": {},\n", c.interactive_found));
    s.push_str(&format!("  \"transitions_dropped\": {dropped},\n"));
    s.push_str(&block);
    s.push_str("  \"end\": true\n}\n");
    s
}

/// The whole recording from its two parts, byte for byte what `emit` writes
/// for the same conversion. Refuses a transitions artifact recorded against
/// a different readable recording, and either part not in the shape this
/// module writes.
pub fn merge(readable: &str, transitions: &str) -> Result<String, String> {
    let want = format!("\"readable\": \"blake3:{}\",", blake3::hash(readable.as_bytes()).to_hex());
    if !transitions.contains(&want) { return Err("the transitions were recorded against another document".into()) }
    let line = |k: &str| -> Result<u64, String> {
        let at = transitions.find(&format!("\n  \"{k}\": ")).ok_or(format!("no {k}"))? + k.len() + 6;
        transitions[at..].split(',').next().and_then(|v| v.trim().parse().ok()).ok_or(format!("bad {k}"))
    };
    let (found, dropped) = (line("interactive_found")?, line("transitions_dropped")?);
    let start = transitions.find("\n  \"transitions\": [").ok_or("no transitions block")? + 1;
    let end = transitions.rfind("  \"end\": true\n}").ok_or("no end")?;
    let block = &transitions[start..end];
    let mut out = readable.to_string();
    let swap = |out: &mut String, a: &str, b: &str| -> Result<(), String> {
        let at = out.find(a).ok_or(format!("readable recording without {a:?}"))?;
        out.replace_range(at..at + a.len(), b);
        Ok(())
    };
    swap(&mut out, "\n  \"transitions\": [],\n", &format!("\n{block}"))?;
    swap(&mut out, "\n    \"interactive_found\": 0,\n", &format!("\n    \"interactive_found\": {found},\n"))?;
    swap(&mut out, "\n    \"transitions_dropped\": 0\n", &format!("\n    \"transitions_dropped\": {dropped}\n"))?;
    if block.contains("\n  \"templates\": [") {
        swap(&mut out, &format!("\"format\": \"{FORMAT}\""), &format!("\"format\": \"{FORMAT_V4}\""))?;
    }
    Ok(out)
}

/// The `transitions` (and `templates`) blocks of a recording, as `emit`
/// writes them, with whether there were templates and how many were dropped.
fn transitions_block(c: &Conversion, tier: Tier) -> (String, bool, usize) {
    let kept: Vec<&crate::Transition> =
        c.transitions.iter().filter(|t| usable(t, tier)).collect();
    let mut dropped = c.transitions.len() - kept.len();
    let templates: Vec<(&crate::Template, Vec<&crate::Instance>)> = c.templates.iter().filter_map(|t| {
        let inst: Vec<&crate::Instance> = t.instances.iter()
            .filter(|i| usable_trigger(&i.trigger, i.anchored, tier)).collect();
        dropped += t.instances.len() - inst.len();
        (!inst.is_empty()).then_some((t, inst))
    }).collect();
    let mut s = String::new();
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
    if !templates.is_empty() {
        s.push_str("  \"templates\": [");
        for (i, (t, inst)) in templates.iter().enumerate() {
            s.push_str(if i == 0 { "\n" } else { ",\n" });
            s.push_str("    {\n");
            s.push_str(&format!("      \"event\": {},\n", json_str(&t.event)));
            s.push_str(&format!("      \"attribute_only\": {},\n", t.is_attribute_only()));
            s.push_str("      \"effects\": [");
            for (j, e) in t.effects.iter().enumerate() {
                s.push_str(if j == 0 { "\n" } else { ",\n" });
                s.push_str("        ");
                s.push_str(&effect_json(e));
            }
            s.push_str(if t.effects.is_empty() { "],\n" } else { "\n      ],\n" });
            s.push_str("      \"instances\": [");
            for (j, n) in inst.iter().enumerate() {
                s.push_str(if j == 0 { "\n" } else { ",\n" });
                let slots: Vec<String> = n.slots.iter().map(|v| json_str(v)).collect();
                s.push_str(&format!("        {{ \"trigger\": {}, \"anchored\": {}, \"slots\": [{}] }}",
                    json_str(&n.trigger), n.anchored, slots.join(", ")));
            }
            s.push_str("\n      ]\n");
            s.push_str("    }");
        }
        s.push_str("\n  ],\n");
    }

    (s, !templates.is_empty(), dropped)
}

fn emit_part(url: Option<&str>, c: &Conversion, policy: &TierPolicy, whole: bool) -> String {
    let (document, decision) = policy.artifact(c);
    let (block, has_templates, dropped) = transitions_block(c, decision.tier);
    // The readable part reports what a control conversion would: no table.
    let (has_templates, dropped, found) = if whole { (has_templates, dropped, c.interactive_found) } else { (false, 0, 0) };

    let mut s = String::new();
    s.push_str("{\n");
    field(&mut s, "format", if has_templates { FORMAT_V4 } else { FORMAT }, false);
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
    s.push_str(&format!("    \"interactive_found\": {found},\n"));
    s.push_str(&format!("    \"transitions_dropped\": {dropped}\n"));
    s.push_str("  },\n");

    if whole { s.push_str(&block) } else { s.push_str("  \"transitions\": [],\n") }
    // The subresources, in the order they were found.
    s.push_str("  \"stylesheets\": [");
    for (i, sh) in c.subresources.sheets.iter().enumerate() {
        s.push_str(if i == 0 { "\n" } else { ",\n" });
        s.push_str(&format!("    {{ \"href\": {}, \"media\": {}, \"text\": {} }}",
            json_str(&sh.href), json_str(&sh.media), json_str(&sh.text)));
    }
    s.push_str(if c.subresources.sheets.is_empty() { "],\n" } else { "\n  ],\n" });
    s.push_str(&images_block(c));
    s.push_str(&format!("  \"document\": {}\n", json_str(document)));
    s.push_str("}\n");
    s
}

/// The `images` block of a recording, as `emit` writes it: one object per
/// line, in the order found. A missing natural dimension is `null`, and the
/// ratio is explicit: `W/H`, or `none` when the image has no ratio. An image
/// still `pending` (the image-free first display) says so, and how its box
/// is sized; no other recording has the field.
fn images_block(c: &Conversion) -> String {
    let mut s = String::new();
    s.push_str("  \"images\": [");
    for (i, im) in c.subresources.images.iter().enumerate() {
        s.push_str(if i == 0 { "\n" } else { ",\n" });
        let num = |v: Option<u32>| v.map(|n| n.to_string()).unwrap_or_else(|| "null".into());
        let ratio = im.natural.ratio.map(|(a, b)| format!("{a}/{b}")).unwrap_or_else(|| "none".into());
        let pending = im.pending.map(|p| format!(", \"pending\": {}", json_str(p.as_str()))).unwrap_or_default();
        s.push_str(&format!("    {{ \"src\": {}, \"width\": {}, \"height\": {}, \"ratio\": {}, \"address\": {}{pending} }}",
            json_str(&im.src), num(im.natural.width), num(im.natural.height), json_str(&ratio), json_str(&im.address)));
    }
    s.push_str(if c.subresources.images.is_empty() { "],\n" } else { "\n  ],\n" });
    s
}

/// The images artifact's format.
pub const IMAGES_FORMAT: &str = "atrium-navigator-images/1";

/// ★ THE IMAGES ARTIFACT (legacy-web §5.4.5.7, the image-free first
/// display): the first artifact's `images` block once (some of) its images
/// have arrived, bound to the first artifact by `first`, the BLAKE3 of its
/// bytes. `complete` says every image has been handled; until then the ones
/// not yet fetched are still written `pending`, exactly as the first
/// artifact wrote them. Each artifact carries the WHOLE block, so a reader
/// that missed one loses nothing by taking the next.
pub fn emit_images(url: Option<&str>, c: &Conversion, first: &str, complete: bool) -> String {
    let mut s = String::new();
    s.push_str("{\n");
    field(&mut s, "format", IMAGES_FORMAT, false);
    field(&mut s, "url", url.unwrap_or(""), false);
    field(&mut s, "first", &format!("blake3:{}", blake3::hash(first.as_bytes()).to_hex()), false);
    s.push_str(&format!("  \"complete\": {complete},\n"));
    s.push_str(&images_block(c));
    s.push_str("  \"end\": true\n}\n");
    s
}

/// The first artifact with an images artifact's block in place of its own:
/// for a complete one, byte for byte the recording a tier-1 conversion that
/// waited for its images writes. Refuses an images artifact recorded
/// against another first artifact, and either part not in the shape this
/// module writes.
///
/// ★ The blocks are found by their line starts. A JSON string cannot hold a
/// raw newline, so `\n  "images": [` cannot occur inside one — not in a
/// stylesheet's text, not in the document.
pub fn merge_images(first: &str, images: &str) -> Result<String, String> {
    let want = format!("\n  \"first\": \"blake3:{}\",\n", blake3::hash(first.as_bytes()).to_hex());
    if !images.contains(&want) { return Err("the images were recorded against another document".into()) }
    if !images.starts_with(&format!("{{\n  \"format\": \"{IMAGES_FORMAT}\",\n")) { return Err("not an images artifact".into()) }
    let (open, close) = ("\n  \"images\": [", "\n  \"end\": true\n}");
    let start = images.find(open).ok_or("no images block")? + 1;
    let end = images.rfind(close).ok_or("no end")? + 1;
    let block = &images[start..end];
    let at = first.find(open).ok_or("first artifact without an images block")? + 1;
    let to = first.find("\n  \"document\": ").ok_or("first artifact without a document")? + 1;
    if to < at { return Err("first artifact out of shape".into()) }
    let mut out = String::with_capacity(first.len() + block.len());
    out.push_str(&first[..at]);
    out.push_str(block);
    out.push_str(&first[to..]);
    Ok(out)
}

/// Whether an images artifact says every image has been handled.
pub fn images_complete(images: &str) -> bool { images.contains("\n  \"complete\": true,\n") }

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
