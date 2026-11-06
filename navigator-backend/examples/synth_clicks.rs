//! Measurement tooling (backend §4.3, replay part 2): a copy of a recording
//! with SYNTHETIC clicks, for documents whose own recording has none that
//! replay (the big ones). At 10%, 50% and 90% of the body's block elements,
//! two transitions: one hides the element (`hidden`: everything below it
//! moves), one restyles it (`style`: a colour and a border, no reflow
//! beyond its box). Targets are the shared DOM's own exact paths, so they
//! resolve as a session resolves them. The copy is written to `<out>`; the
//! corpus is not touched.
//!
//!   synth_clicks <recording.json> <out.json>

use navigator_dom::Kind;

fn main() {
    let mut a = std::env::args().skip(1);
    let (src, out) = (a.next().expect("recording"), a.next().expect("out"));
    let bytes = std::fs::read(&src).expect("readable");
    let mut j: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    let doc = j["document"].as_str().expect("document").to_string();
    let mut dom = navigator_dom::parse(&doc);
    const BLOCK: [&str; 16] = ["div", "section", "h1", "h2", "h3", "h4", "p", "table", "tr", "li", "pre", "dl", "dd", "ul", "ol", "article"];
    let body = dom.by_tag_anywhere("body").into_iter().next().expect("body");
    let mut blocks = vec![];
    let mut st = vec![body];
    while let Some(h) = st.pop() {
        if let Some(Kind::Element(t)) = dom.get(h).map(|n| &n.kind) {
            if BLOCK.contains(&t.to_ascii_lowercase().as_str()) && dom.attr(h, "hidden").is_none() && h != body { blocks.push(h) }
        }
        st.extend(dom.children_of(h).into_iter().rev());
    }
    let mut ts = vec![];
    let picks: Vec<_> = [0.1, 0.5, 0.9].iter().map(|f| blocks[((blocks.len() as f64 * f) as usize).min(blocks.len() - 1)]).collect();
    // Each target is given an id, so a measurement can
    // put the view where the reader sees it (`VIEW=trigger`).
    for (n, &h) in picks.iter().enumerate() {
        // (Always its own: some pages repeat ids, and `#id` then names none.)
        dom.set_attr(h, "id", &format!("synth-{n}"));
    }
    j["document"] = serde_json::Value::String(dom.serialize());
    for (n, &h) in picks.iter().enumerate() {
        let frac = [0.1, 0.5, 0.9][n];
        let path = format!("#{}", dom.attr(h, "id").expect("set"));
        assert_eq!(dom.resolve_path(&path), Some(h), "{path} resolves");
        let style = dom.attr(h, "style").map(str::to_string);
        let restyled = format!("{}color: #c00; border: 1px solid #c00", style.as_deref().map(|s| format!("{s}; ")).unwrap_or_default());
        // The second trigger is the element's parent, so the two triggers differ.
        let parent = dom.exact_path(dom.get(h).and_then(|n| n.parent).expect("parent"));
        ts.push(serde_json::json!({"trigger": path, "event": "click", "anchored": true, "attribute_only": true,
            "effects": [{"kind": "attribute", "target": path, "name": "hidden", "from": null, "to": ""}]}));
        ts.push(serde_json::json!({"trigger": parent, "event": "click", "anchored": true, "attribute_only": true,
            "effects": [{"kind": "attribute", "target": path, "name": "style", "from": style, "to": restyled}]}));
        eprintln!("{frac}: {path} ({} of {} blocks)", (blocks.len() as f64 * frac) as usize, blocks.len());
    }
    j["transitions"] = serde_json::Value::Array(ts);
    std::fs::write(&out, serde_json::to_vec(&j).expect("json")).expect("written");
}
