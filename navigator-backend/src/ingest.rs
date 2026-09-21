//! Turning bytes into a `Recording`, or refusing to.

use crate::*;
use serde_json::Value;

/// Ingest a recording.
///
/// ★ THE SIZE CHECK COMES BEFORE THE PARSE, which is the whole reason it is a
/// separate step: handing sixteen megabytes of adversarial JSON to a parser
/// and then deciding it was too big has already done the work the limit
/// exists to prevent.
pub fn ingest(bytes: &[u8], limits: &Limits) -> Result<Recording, Reject> {
    if bytes.len() > limits.max_total_bytes {
        return Err(Reject::TooLarge {
            what: "recording", measured: bytes.len(), allowed: limits.max_total_bytes,
        });
    }
    let v: Value = serde_json::from_slice(bytes)
        .map_err(|e| Reject::NotJson(e.to_string()))?;
    let obj = v.as_object().ok_or(Reject::NotAnObject)?;

    match obj.get("format").and_then(Value::as_str) {
        Some(FORMAT) | Some(crate::FORMAT_V1) => {}
        Some(other) => return Err(Reject::WrongFormat { found: other.to_string() }),
        None => return Err(Reject::MissingField("format")),
    }

    let mut notes = vec![];

    let url = bounded_str(obj.get("url"), "url", limits)?.unwrap_or_default();
    let tier = match obj.get("tier").and_then(Value::as_u64) {
        Some(1) => Tier::One,
        Some(2) => Tier::Two,
        Some(_) => return Err(Reject::BadField { field: "tier", why: "not 1 or 2" }),
        None => return Err(Reject::MissingField("tier")),
    };
    let tier_reason = bounded_str(obj.get("tier_reason"), "tier_reason", limits)?
        .unwrap_or_default();

    let measurements = obj.get("measurements").map(|m| {
        let g = |k: &str| m.get(k).and_then(Value::as_u64).unwrap_or(0);
        Measurements {
            elements: g("elements"),
            text_before: g("text_before"),
            text_after: g("text_after"),
            scripts_total: g("scripts_total"),
            scripts_failed: g("scripts_failed"),
            interactive_found: g("interactive_found"),
            transitions_dropped: g("transitions_dropped"),
        }
    }).unwrap_or_default();

    // ★ The document is REQUIRED. A recording without one is not a document
    // that failed to convert; it is not a recording.
    let document = match obj.get("document").and_then(Value::as_str) {
        Some(d) => {
            if d.len() > limits.max_document_bytes {
                return Err(Reject::TooLarge {
                    what: "document", measured: d.len(), allowed: limits.max_document_bytes,
                });
            }
            d.to_string()
        }
        None => return Err(Reject::MissingField("document")),
    };

    let mut transitions = vec![];
    if let Some(list) = obj.get("transitions") {
        let arr = list.as_array().ok_or(Reject::BadField {
            field: "transitions", why: "not an array",
        })?;
        // Truncate rather than refuse: a recording with too many transitions
        // is still a usable document, and the count is reported so a caller
        // is never silently handed a subset.
        let keep = arr.len().min(limits.max_transitions);
        if arr.len() > keep {
            notes.push(Note::TransitionsTruncated { kept: keep, discarded: arr.len() - keep });
        }
        for t in &arr[..keep] {
            if let Some(tr) = transition(t, limits, &mut notes)? { transitions.push(tr) }
        }
    }

    Ok(Recording { url, tier, tier_reason, measurements, transitions, document, notes })
}

fn bounded_str(v: Option<&Value>, field: &'static str, limits: &Limits)
    -> Result<Option<String>, Reject>
{
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => {
            if s.len() > limits.max_string_bytes {
                return Err(Reject::TooLarge {
                    what: "string", measured: s.len(), allowed: limits.max_string_bytes,
                });
            }
            Ok(Some(s.clone()))
        }
        Some(_) => Err(Reject::BadField { field, why: "not a string" }),
    }
}

fn transition(v: &Value, limits: &Limits, notes: &mut Vec<Note>)
    -> Result<Option<Transition>, Reject>
{
    let Some(o) = v.as_object() else {
        return Err(Reject::BadField { field: "transitions", why: "entry is not an object" });
    };
    let trigger = bounded_str(o.get("trigger"), "trigger", limits)?
        .ok_or(Reject::MissingField("trigger"))?;
    let event = bounded_str(o.get("event"), "event", limits)?
        .ok_or(Reject::MissingField("event"))?;
    let anchored = o.get("anchored").and_then(Value::as_bool).unwrap_or(false);

    let mut effects = vec![];
    if let Some(list) = o.get("effects") {
        let arr = list.as_array().ok_or(Reject::BadField {
            field: "effects", why: "not an array",
        })?;
        if arr.len() > limits.max_effects_per_transition {
            return Err(Reject::TooLarge {
                what: "effects", measured: arr.len(),
                allowed: limits.max_effects_per_transition,
            });
        }
        for e in arr {
            if let Some(eff) = effect(e, limits, notes)? { effects.push(eff) }
        }
    }

    let t = Transition { trigger, event, anchored, effects };

    // ★ Recomputed, never read. See Transition::is_attribute_only.
    if let Some(claimed) = o.get("attribute_only").and_then(Value::as_bool) {
        let actual = t.is_attribute_only();
        if claimed != actual {
            notes.push(Note::DerivedFieldDisagreed {
                trigger: t.trigger.clone(), claimed, actual,
            });
        }
    }
    Ok(Some(t))
}

fn effect(v: &Value, limits: &Limits, notes: &mut Vec<Note>)
    -> Result<Option<Effect>, Reject>
{
    let Some(o) = v.as_object() else {
        return Err(Reject::BadField { field: "effects", why: "entry is not an object" });
    };
    let kind = o.get("kind").and_then(Value::as_str).unwrap_or("");
    Ok(match kind {
        "attribute" => Some(Effect::Attribute {
            target: bounded_str(o.get("target"), "target", limits)?
                .ok_or(Reject::MissingField("target"))?,
            name: bounded_str(o.get("name"), "name", limits)?
                .ok_or(Reject::MissingField("name"))?,
            from: bounded_str(o.get("from"), "from", limits)?,
            to: bounded_str(o.get("to"), "to", limits)?,
        }),
        "insert" => {
            let html = bounded_str(o.get("html"), "html", limits)?
                .ok_or(Reject::MissingField("html"))?;
            if html.len() > limits.max_insert_bytes {
                return Err(Reject::TooLarge {
                    what: "insert html", measured: html.len(),
                    allowed: limits.max_insert_bytes,
                });
            }
            // ★ Absent is NOT zero. A version 1 recording has no position,
            // and defaulting it to the front would place content somewhere
            // the page never put it — the exact failure appending at least
            // makes visible.
            let index = match o.get("index") {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.as_u64().ok_or(Reject::BadField {
                    field: "index", why: "not a non-negative integer",
                })? as usize),
            };
            Some(Effect::Insert {
                parent: bounded_str(o.get("parent"), "parent", limits)?
                    .ok_or(Reject::MissingField("parent"))?,
                index,
                html,
            })
        }
        "remove" => Some(Effect::Remove {
            target: bounded_str(o.get("target"), "target", limits)?
                .ok_or(Reject::MissingField("target"))?,
        }),
        // ★ Parsed, not merely skipped, so the format is genuinely closed
        // under inversion: an undo is an ordinary transition that can be
        // written down, sent, stored and read back like any other.
        "remove-range" => {
            let num = |k: &'static str| -> Result<usize, Reject> {
                o.get(k).and_then(Value::as_u64)
                    .ok_or(Reject::BadField { field: k, why: "not a non-negative integer" })
                    .map(|v| v as usize)
            };
            let count = num("count")?;
            // A range is bounded by the same number that bounds a
            // transition's effects: an unbounded count is an unbounded
            // deletion described in a few bytes.
            if count > limits.max_effects_per_transition {
                return Err(Reject::TooLarge {
                    what: "remove-range count", measured: count,
                    allowed: limits.max_effects_per_transition,
                });
            }
            Some(Effect::RemoveRange {
                parent: bounded_str(o.get("parent"), "parent", limits)?
                    .ok_or(Reject::MissingField("parent"))?,
                index: num("index")?,
                count,
            })
        }
        "truncated" => Some(Effect::Truncated {
            dropped: o.get("dropped").and_then(Value::as_u64).unwrap_or(0),
        }),
        // ★ Skipped, not fatal: a recording from a LATER version may carry an
        // effect this one does not know. Refusing the whole document over one
        // unknown entry would make the format unable to grow.
        other => {
            notes.push(Note::UnknownEffectKind { kind: other.to_string() });
            None
        }
    })
}
