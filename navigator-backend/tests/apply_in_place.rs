//! ★★ In-place replay (backend §4.3, replay part 1) against the clone path
//! it replaces, which stays as the reference: on every step and every
//! rewind, the same document byte for byte and the same undo — or the same
//! refusal. `NAVIGATOR_RECORDINGS=<dir>` runs every anchored transition of a
//! directory of emitted recordings, both one step and back at a time and as
//! one long chain (so the incremental profile bounds drift and every
//! composite state is reached); without it the built-in fixture runs. A
//! fuzzer then mutates transitions into every refusal there is.
use navigator_backend::document::Document;
use navigator_backend::{ingest, Effect, Limits, Transition};

/// The two documents serialize identically — and the in-place one's KEPT
/// length (`Document::serialized_bytes`, maintained by each step from what
/// it changed) is its real one. The first call measures; every later call
/// checks what the steps since then kept.
fn same(a: &Document, b: &Document) -> bool {
    let sb = b.dom.serialize();
    let eq = a.dom.serialize() == sb;
    // (A planted difference edits the tree directly, outside any step, so
    // its count is not kept by design; it is checked where the trees agree.)
    if eq { assert_eq!(b.serialized_bytes(), sb.len(), "the kept byte count drifted from the document") }
    eq
}

/// One step on both paths; panics on any difference. Returns the undo if it applied.
fn step(clone: &mut Document, inplace: &mut Document, t: &Transition, what: &str) -> Option<Transition> {
    let a = clone.applied_with_undo(t);
    let b = inplace.apply_in_place_with_undo(t);
    match (a, b) {
        (Ok((next, ua)), Ok(ub)) => {
            assert_eq!(ua, ub, "{what}: the undo differs");
            *clone = next;
            assert!(same(clone, inplace), "{what}: the document differs");
            Some(ua)
        }
        (Err(ea), Err(eb)) => {
            assert_eq!(ea, eb, "{what}: the refusal differs");
            assert!(same(clone, inplace), "{what}: a refused step changed the document");
            None
        }
        (a, b) => panic!("{what}: clone {:?} vs in place {:?}", a.map(|_| ()), b.map(|_| ())),
    }
}

fn back(clone: &mut Document, inplace: &mut Document, undo: &Transition, what: &str) {
    let a = clone.undone(undo);
    let b = inplace.undo_in_place(undo);
    match (a, b) {
        (Ok(prev), Ok(())) => { *clone = prev; assert!(same(clone, inplace), "{what}: the rewound document differs") }
        (Err(ea), Err(eb)) => assert_eq!(ea, eb, "{what}: the rewind refusal differs"),
        (a, b) => panic!("{what}: rewind clone {:?} vs in place {:?}", a.map(|_| ()), b),
    }
}

const FIXTURE: &str = r#"<html><body><nav><button id=t aria-expanded="false">Menu</button><ul id=m hidden><li>a</li></ul></nav><p id=p>x</p><div id=d><span>1</span><span>2</span></div></body></html>"#;

fn fixture_transitions() -> Vec<Transition> {
    let a = |target: &str, name: &str, from: Option<&str>, to: Option<&str>| Effect::Attribute {
        target: target.into(), name: name.into(), from: from.map(Into::into), to: to.map(Into::into), at: None };
    vec![
        Transition { trigger: "#t".into(), event: "click".into(), anchored: true,
            effects: vec![a("#t", "aria-expanded", Some("false"), Some("true")), a("#m", "hidden", Some(""), None)] },
        Transition { trigger: "#t".into(), event: "click".into(), anchored: true,
            effects: vec![Effect::Insert { parent: "#d".into(), index: Some(1), html: "<b>new</b><i>two</i>".into() },
                          Effect::Remove { target: "#p".into() }] },
        Transition { trigger: "#t".into(), event: "click".into(), anchored: true,
            effects: vec![Effect::RemoveRange { parent: "#d".into(), index: 0, count: 2 }, a("#d", "class", None, Some("open"))] },
    ]
}

fn run_doc(html: &str, ts: &[Transition], name: &str) -> (usize, usize) {
    let (Ok(mut c), Ok(mut i)) = (Document::accept(html), Document::accept(html)) else { return (0, 0) };
    let (mut steps, mut rewinds) = (0, 0);
    // One step and back, from the base each time.
    for (k, t) in ts.iter().enumerate() {
        if let Some(u) = step(&mut c, &mut i, t, &format!("{name} step {k}")) {
            steps += 1;
            back(&mut c, &mut i, &u, &format!("{name} rewind {k}"));
            rewinds += 1;
        }
    }
    // One long chain, then all the way back.
    let mut undos = vec![];
    for (k, t) in ts.iter().enumerate() {
        if let Some(u) = step(&mut c, &mut i, t, &format!("{name} chain {k}")) { undos.push(u); steps += 1 }
    }
    for (k, u) in undos.iter().enumerate().rev() { back(&mut c, &mut i, u, &format!("{name} chain rewind {k}")); rewinds += 1 }
    (steps, rewinds)
}

#[test]
fn every_step_and_rewind_is_the_clone_paths_byte_for_byte() {
    let (s, r) = run_doc(FIXTURE, &fixture_transitions(), "fixture");
    assert!(s >= 6 && r >= 6, "the fixture must exercise steps: {s} {r}");
    let Ok(dir) = std::env::var("NAVIGATOR_RECORDINGS") else { eprintln!("corpus: set NAVIGATOR_RECORDINGS"); return };
    let (mut docs, mut steps, mut rewinds) = (0, 0, 0);
    let mut files: Vec<_> = std::fs::read_dir(&dir).expect("dir").flatten().map(|e| e.path()).collect();
    files.sort();
    for p in files {
        let Ok(bytes) = std::fs::read(&p) else { continue };
        let Ok(r) = ingest(&bytes, &Limits::default()) else { continue };
        let ts: Vec<Transition> = r.transitions.iter().filter(|t| t.anchored).cloned().collect();
        if ts.is_empty() { continue }
        let (s, w) = run_doc(&r.document, &ts, &p.display().to_string());
        docs += 1; steps += s; rewinds += w;
    }
    eprintln!("in place == clone: {docs} documents, {steps} steps, {rewinds} rewinds");
    assert!(steps > 0, "the corpus ran no steps");
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 { self.0 ^= self.0 << 13; self.0 ^= self.0 >> 7; self.0 ^= self.0 << 17; self.0 }
    fn below(&mut self, n: usize) -> usize { (self.next() % n.max(1) as u64) as usize }
}

#[test]
fn mutated_transitions_refuse_identically() {
    let targets = ["#t", "#m", "#p", "#d", "#nope", "html>body:0>div:2", "#d>span:0", ""];
    let names = ["aria-expanded", "hidden", "class", "id"];
    let vals = [None, Some(""), Some("false"), Some("true"), Some("open"), Some("x\"<&")];
    let htmls = ["<b>x</b>", "", "<td>cell", "<script>evil()</script>", "<div><p>deep</p></div>", "text & more"];
    let mut r = Rng(0x5EED_1234_ABCD_0001);
    let mut outcomes = std::collections::BTreeMap::<String, usize>::new();
    for n in 0..4_000 {
        let mut effects = vec![];
        for _ in 0..1 + r.below(4) {
            effects.push(match r.below(5) {
                0 | 1 => Effect::Attribute { target: targets[r.below(targets.len())].into(), name: names[r.below(names.len())].into(),
                    from: vals[r.below(vals.len())].map(Into::into), to: vals[r.below(vals.len())].map(Into::into), at: None },
                2 => Effect::Remove { target: targets[r.below(targets.len())].into() },
                3 => Effect::RemoveRange { parent: targets[r.below(targets.len())].into(), index: r.below(3), count: r.below(3) },
                _ => Effect::Insert { parent: targets[r.below(targets.len())].into(), index: Some(r.below(4)), html: htmls[r.below(htmls.len())].into() },
            });
        }
        if r.below(20) == 0 { effects.push(Effect::Truncated { dropped: 1 }) }
        let t = Transition { trigger: if r.below(10) == 0 { "#gone".into() } else { "#t".into() }, event: "click".into(), anchored: true, effects };
        let (mut c, mut i) = (Document::accept(FIXTURE).unwrap(), Document::accept(FIXTURE).unwrap());
        let a = c.applied_with_undo(&t).map(|(d, u)| (d.dom.serialize(), u));
        let b = i.apply_in_place_with_undo(&t).map(|u| (i.dom.serialize(), u));
        assert_eq!(a, b, "mutant {n}: {t:?}");
        let k = match &a { Ok(_) => "applied".to_string(), Err(e) => format!("{e:?}").split(['{', '(']).next().unwrap().trim().to_string() };
        *outcomes.entry(k).or_default() += 1;
        if let Ok((_, u)) = a {
            let pa = c.applied_with_undo(&t).map(|(d, _)| d).unwrap();
            let pb = i.undo_in_place(&u).map(|_| i.dom.serialize());
            assert_eq!(pa.undone(&u).map(|d| d.dom.serialize()), pb, "mutant {n} rewind");
        }
    }
    eprintln!("mutant outcomes: {outcomes:?}");
    for want in ["applied", "TriggerUnresolved", "TargetUnresolved", "PreconditionFailed", "Incomplete"] {
        assert!(outcomes.get(want).copied().unwrap_or(0) > 0, "never reached {want}: {outcomes:?}");
    }
}

#[test]
fn the_comparison_catches_a_planted_difference() {
    // Positive control: the in-place document after a step, then one byte changed by hand.
    let t = &fixture_transitions()[0];
    let (mut c, mut i) = (Document::accept(FIXTURE).unwrap(), Document::accept(FIXTURE).unwrap());
    c = c.applied_with_undo(t).unwrap().0;
    i.apply_in_place_with_undo(t).unwrap();
    assert!(same(&c, &i));
    let h = i.dom.by_id("p").unwrap();
    i.dom.set_attr(h, "data-x", "1");
    assert!(!same(&c, &i), "a planted difference must be seen");
}

#[test]
fn a_step_near_a_ceiling_takes_the_exact_path_and_refuses_identically() {
    // 1.1 MB of text: its byte bound (8x) passes the 8 MiB ceiling, so the
    // in-place path cannot prove it inside and applies it exactly — and it is.
    let big = Transition { trigger: "#t".into(), event: "click".into(), anchored: true,
        effects: vec![Effect::Insert { parent: "#d".into(), index: Some(0), html: "y".repeat(1_100_000) }] };
    let (mut c, mut i) = (Document::accept(FIXTURE).unwrap(), Document::accept(FIXTURE).unwrap());
    let u = step(&mut c, &mut i, &big, "near the byte ceiling").expect("applies");
    back(&mut c, &mut i, &u, "near the byte ceiling, back");
    // Past the element ceiling: refused, with the exact measurement, by both.
    let many = Transition { trigger: "#t".into(), event: "click".into(), anchored: true,
        effects: vec![Effect::Insert { parent: "#d".into(), index: Some(0), html: "<i></i>".repeat(131_100) }] };
    let (mut c, mut i) = (Document::accept(FIXTURE).unwrap(), Document::accept(FIXTURE).unwrap());
    assert!(step(&mut c, &mut i, &many, "past the element ceiling").is_none());
    assert!(matches!(i.apply_in_place_with_undo(&many), Err(navigator_backend::apply::ApplyError::OutsideProfile(_))));
}
