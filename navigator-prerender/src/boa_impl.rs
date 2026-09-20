//! The Boa side of the seam. Nothing outside this file knows which engine runs.

use crate::dom::{Dom, Handle, Kind};
use crate::engine::{RunReport, ScriptEngine, ScriptSource};
use boa_engine::{
    js_string, object::ObjectInitializer, property::Attribute, Context, JsArgs, JsObject,
    JsResult, JsValue, NativeFunction, Source,
};
use std::{cell::RefCell, collections::{BTreeMap, HashMap}};

// The arena lives here for the duration of a run. A thread-local keeps every
// host function a plain fn pointer — no captured state to trace through the
// engine's GC, which is exactly the coupling §11.8 says is the hard part of
// swapping engines. Single-threaded and short-lived by construction.
thread_local! {
    static DOM: RefCell<Dom> = RefCell::new(Dom::new());
    /// ★ ATTRIBUTION. Boa reports "not a callable function" without naming the
    /// callee, so the largest failure bucket in a corpus run could not direct
    /// any work. Rather than parse error text, record what the script ASKED
    /// FOR: every host object is a Proxy whose `get` trap notes any property
    /// the target does not have. The result names the missing API instead of
    /// describing the symptom.
    static MISSES: RefCell<BTreeMap<String, u32>> = RefCell::new(BTreeMap::new());
    /// ★ Only record misses caused by PAGE script. The instrument's own
    /// bootstrap and lifecycle passes probe for `window.onload` and
    /// `document.onreadystatechange` with `typeof`, which are property gets —
    /// and those promptly appeared as the top two "most-wanted APIs" in 18 of
    /// 18 documents. A measurement that reports its own probes is measuring
    /// itself; this flag keeps the instrument out of its own numbers.
    static RECORDING: RefCell<bool> = const { RefCell::new(false) };
    /// The `<script>` element currently executing, for `document.currentScript`.
    static CURRENT: RefCell<Option<Handle>> = const { RefCell::new(None) };
    /// ★ A SECOND attribution channel. The missing-API report cannot see this
    /// class at all: `cannot convert 'null' or 'undefined' to object` means an
    /// API we DO implement returned nothing where a browser would have found
    /// something, so the property was never missing. Recording which lookup
    /// came back empty, and with what argument, turns 14 identical symptoms
    /// into named queries.
    static NULLS: RefCell<BTreeMap<String, u32>> = RefCell::new(BTreeMap::new());
    /// ★ The same two signals, IN ORDER. Document-wide counts cannot say which
    /// event caused a given throw — a miss recorded by an unrelated script
    /// that ran fine will outvote the real proximate cause. The last event
    /// before the first failure is the causal one, and that needs a sequence,
    /// not a tally.
    static EVENTS: RefCell<Vec<(&'static str, String)>> = const { RefCell::new(Vec::new()) };
    /// How often the page read a layout metric we cannot truthfully answer.
    static LAYOUT_READS: RefCell<u32> = const { RefCell::new(0) };
    /// ★ Mutations, recorded as they happen. Unlike the geometry observers,
    /// this one can be served HONESTLY: a converter has no layout to report,
    /// but it does have real mutations. Every mutation already passes through
    /// a host function, so the log is exact rather than inferred.
    /// (kind, target handle, attribute name)
    static MUTATIONS: RefCell<Vec<(&'static str, Handle, String)>> =
        const { RefCell::new(Vec::new()) };
    /// ★ ONE WRAPPER PER HANDLE, FOR THE LIFE OF A RUN.
    ///
    /// node_obj() used to mint a fresh object every call, so two lookups of
    /// the same element compared UNEQUAL. `document.body === document.body`
    /// was false. Libraries lean on node identity constantly — jQuery's
    /// setDocument opens with `doc == document`, event delegation walks
    /// parents comparing against a root, and caches key on the node itself.
    /// Identity is part of the DOM contract, not an optimisation.
    static NODE_CACHE: RefCell<HashMap<Handle, JsValue>> =
        RefCell::new(HashMap::new());
    /// ★ ELEMENT LISTENERS ARE NO LONGER DROPPED.
    ///
    /// They were, on the grounds that nothing headless will ever deliver a
    /// click — which is true of USER events and false of the page's own
    /// `dispatchEvent`. A page dispatching an event to itself is not an
    /// interaction we would have to invent; it is code the page runs, and
    /// the corpus does it 383 times. Storing them is what makes dispatch
    /// real instead of a stub.
    ///
    /// Holds JsValues, so like NODE_CACHE it must be cleared on the way in
    /// AND on the way out: engine objects must not outlive the engine.
    static ELISTENERS: RefCell<HashMap<Handle, Vec<(String, JsValue)>>> =
        RefCell::new(HashMap::new());
    /// (dispatches, listener invocations) on ELEMENTS.
    static DISPATCH: RefCell<(u32, u32)> = const { RefCell::new((0, 0)) };
    /// (writes applied, writes refused because they would erase the document)
    static DOC_WRITE: RefCell<(u32, u32)> = const { RefCell::new((0, 0)) };
    /// ★ The advancing insertion point, per script element.
    ///
    /// A browser's parser position moves forward as each write lands, so the
    /// next write goes AFTER the previous one's output. Recomputing it from
    /// the script element every call inserted each write before the last,
    /// and three writes came out in reverse order.
    static WRITE_POS: RefCell<HashMap<Handle, Handle>> = RefCell::new(HashMap::new());
    /// The page's fetcher, parked here for the duration of a run so native
    /// functions can reach it without capturing state the engine's GC traces.
    static PAGE_NET: RefCell<Option<Box<dyn crate::fetch::Fetcher>>> =
        const { RefCell::new(None) };
    static PAGE_FETCHES: RefCell<(u32, u32)> = const { RefCell::new((0, 0)) };
    /// Requests refused by policy, and the hosts they were aimed at.
    static PAGE_BLOCKED: RefCell<(u32, BTreeMap<String, u32>)> =
        RefCell::new((0, BTreeMap::new()));
    /// The document's URL, so `script.src` can be reported ABSOLUTE as a
    /// browser does — webpack derives publicPath from it, and a relative
    /// value there yields the wrong base.
    static BASE: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut Dom) -> R) -> R { DOM.with(|d| f(&mut d.borrow_mut())) }

/// Read the `__h` handle off a node wrapper.
fn handle_of(v: &JsValue, ctx: &mut Context) -> Option<Handle> {
    let o = v.as_object()?;
    let h = o.get(js_string!("__h"), ctx).ok()?;
    h.as_number().map(|n| n as Handle)
}

/// The `get` trap: pass through what exists, record what does not.
///
/// Note a miss is not automatically a gap — `if (el.foo)` feature-detection
/// deliberately probes for absent properties. Frequency still ranks the work
/// correctly, and the report says so rather than overclaiming.
fn probe_get(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let target = args.get_or_undefined(0).as_object().ok_or_else(|| {
        boa_engine::JsNativeError::typ().with_message("proxy target")
    })?;
    let key_v = args.get_or_undefined(1).clone();
    let key = key_v.to_property_key(ctx)?;
    if target.has_property(key.clone(), ctx)? {
        return target.get(key, ctx);
    }
    let kind = target.get(js_string!("__kind"), ctx)
        .ok()
        .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
        .unwrap_or_else(|| "object".into());
    let name = key.to_string();
    // Ignore engine-internal lookups; they are not APIs a page asked for.
    let recording = RECORDING.with(|r| *r.borrow());
    let own_probe = matches!(name.as_str(), "onreadystatechange" | "onload");
    if recording && !own_probe && !name.starts_with("__")
        && !name.starts_with("Symbol(") && name != "then" {
        let full = format!("{kind}.{name}");
        // ★ A DELIBERATE PROBE IS NOT A PROXIMATE CAUSE.
        //
        // Cause attribution takes the LAST event before a throw, so a
        // feature detection that is SUPPOSED to find nothing, running at
        // bundle init just before an unrelated failure, gets blamed for it.
        // `document.all` is the canonical case: core-js reads it to detect
        // the IsHTMLDDA quirk and expects the absence. It ranked as the cause
        // for two documents and was the cause of neither.
        //
        // Still counted as a MISS — the most-wanted list should show what
        // pages ask for — but kept out of the causal log.
        if !is_detection_probe(&full) {
            EVENTS.with(|e| e.borrow_mut().push(("missing", full.clone())));
        }
        MISSES.with(|m| *m.borrow_mut().entry(full).or_default() += 1);
    }
    Ok(JsValue::undefined())
}

/// Lookups a page makes EXPECTING to find nothing. Absence is the answer
/// they want, so their absence explains no failure.
fn is_detection_probe(full: &str) -> bool {
    matches!(full,
        // core-js and friends read this to detect the IsHTMLDDA quirk, a
        // behaviour no JS implementation can reproduce: an object that is
        // falsy and whose typeof is "undefined". Every non-browser host
        // fails this probe, and that is the intended path.
        "document.all"
        // Vendor-prefixed fallbacks, always tried after the standard name.
        | "window.msCrypto" | "window.webkitURL" | "window.mozRequestAnimationFrame"
        | "window.webkitRequestAnimationFrame" | "window.msRequestAnimationFrame"
    )
}

/// ★ THE CONVERTER'S TIMEZONE IS UTC, NOT THE HOST MACHINE'S.
///
/// Boa's default hook reports the local offset of whatever machine is
/// running, so `new Date().getTimezoneOffset()` returned -330 here (IST) and
/// would return something else on another box — baking the converter's
/// location into the artifact. A conversion is amortised across many
/// readers, none of whom are in that timezone, and the same document would
/// convert differently on two machines.
///
/// This is the same decision already made for the user agent and the
/// viewport: fixed identity, the converter's and not the reader's. UTC is
/// the only offset that is nobody's local guess.
#[derive(Debug)]
struct FixedHooks;

impl boa_engine::context::HostHooks for FixedHooks {
    fn local_timezone_offset_seconds(&self, _unix_time_seconds: i64) -> i32 { 0 }
}

/// Wrap a host object so its misses are attributed.
fn probed(obj: JsObject, kind: &str, ctx: &mut Context) -> JsValue {
    let _ = obj.set(js_string!("__kind"), js_string!(kind.to_string()), false, ctx);
    match boa_engine::object::builtins::JsProxy::builder(obj.clone())
        .get(probe_get)
        .build(ctx)
    {
        Ok(p) => JsValue::from(JsObject::from(p)),
        Err(_) => JsValue::from(obj),
    }
}

/// A live accessor over the arena: tree shape changes under script, so these
/// must be read at ACCESS time. A snapshot taken when the wrapper was built
/// would be stale the moment anything moved.
fn live_get(o: &JsObject, name: &str,
            f: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>,
            ctx: &mut Context) {
    let desc = boa_engine::property::PropertyDescriptor::builder()
        .get(NativeFunction::from_fn_ptr(f).to_js_function(ctx.realm()))
        .enumerable(false).configurable(true).build();
    let _ = o.define_property_or_throw(js_string!(name.to_string()), desc, ctx);
}

fn live_get_set(o: &JsObject, name: &str,
                g: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>,
                st: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>,
                ctx: &mut Context) {
    let desc = boa_engine::property::PropertyDescriptor::builder()
        .get(NativeFunction::from_fn_ptr(g).to_js_function(ctx.realm()))
        .set(NativeFunction::from_fn_ptr(st).to_js_function(ctx.realm()))
        .enumerable(false).configurable(true).build();
    let _ = o.define_property_or_throw(js_string!(name.to_string()), desc, ctx);
}

fn handles_to_array(hs: Vec<Handle>, ctx: &mut Context) -> JsResult<JsValue> {
    let arr = boa_engine::object::builtins::JsArray::new(ctx)?;
    for h in hs { let v = node_obj(h, ctx); arr.push(v, ctx)?; }
    Ok(JsValue::from(arr))
}

fn opt_node(h: Option<Handle>, ctx: &mut Context) -> JsResult<JsValue> {
    Ok(match h { Some(h) => node_obj(h, ctx), None => JsValue::null() })
}

fn this_h(t: &JsValue, ctx: &mut Context) -> Option<Handle> { handle_of(t, ctx) }

fn n_parent(t: &JsValue, _a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let h = this_h(t, ctx); opt_node(h.and_then(|h| with(|d| d.get(h).and_then(|n| n.parent))), ctx)
}
fn n_parent_element(t: &JsValue, _a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let h = this_h(t, ctx)
        .and_then(|h| with(|d| d.get(h).and_then(|n| n.parent)))
        .filter(|&p| with(|d| d.node_type(p)) == 1);
    opt_node(h, ctx)
}
fn n_child_nodes(t: &JsValue, _a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let h = this_h(t, ctx).unwrap_or(0);
    handles_to_array(with(|d| d.children_of(h)), ctx)
}
fn n_children(t: &JsValue, _a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let h = this_h(t, ctx).unwrap_or(0);
    handles_to_array(with(|d| d.element_children(h)), ctx)
}
fn n_first_child(t: &JsValue, _a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let h = this_h(t, ctx); opt_node(h.and_then(|h| with(|d| d.first_child(h))), ctx)
}
fn n_last_child(t: &JsValue, _a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let h = this_h(t, ctx); opt_node(h.and_then(|h| with(|d| d.last_child(h))), ctx)
}
fn n_first_el_child(t: &JsValue, _a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let h = this_h(t, ctx); opt_node(h.and_then(|h| with(|d| d.element_children(h).first().copied())), ctx)
}
fn n_last_el_child(t: &JsValue, _a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let h = this_h(t, ctx); opt_node(h.and_then(|h| with(|d| d.element_children(h).last().copied())), ctx)
}
fn n_next(t: &JsValue, _a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let h = this_h(t, ctx); opt_node(h.and_then(|h| with(|d| d.next_sibling(h))), ctx)
}
fn n_prev(t: &JsValue, _a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let h = this_h(t, ctx); opt_node(h.and_then(|h| with(|d| d.previous_sibling(h))), ctx)
}
fn n_node_type(t: &JsValue, _a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let h = this_h(t, ctx).unwrap_or(0);
    Ok(JsValue::from(with(|d| d.node_type(h))))
}
fn n_node_name(t: &JsValue, _a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let h = this_h(t, ctx).unwrap_or(0);
    Ok(JsValue::from(js_string!(with(|d| d.node_name(h)))))
}
fn n_node_value(t: &JsValue, _a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let h = this_h(t, ctx).unwrap_or(0);
    Ok(match with(|d| d.get(h).map(|n| n.kind.clone())) {
        Some(Kind::Text(s)) | Some(Kind::Comment(s)) => JsValue::from(js_string!(s)),
        _ => JsValue::null(),
    })
}
fn n_owner_document(t: &JsValue, _a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let h = this_h(t, ctx).unwrap_or(0);
    // The document owns everything except itself, which owns nothing.
    if with(|d| d.node_type(h)) == 9 { return Ok(JsValue::null()) }
    Ok(ctx.global_object().get(js_string!("document"), ctx).unwrap_or(JsValue::null()))
}
fn n_inner_html(t: &JsValue, _a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let h = this_h(t, ctx).unwrap_or(0);
    Ok(JsValue::from(js_string!(with(|d| d.inner_html(h)))))
}
fn n_outer_html(t: &JsValue, _a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let h = this_h(t, ctx).unwrap_or(0);
    Ok(JsValue::from(js_string!(with(|d| d.outer_html(h)))))
}
/// `innerHTML =` goes through the REAL parser and grafts the result in. A
/// second, hand-rolled parser here would disagree with the one that built the
/// document, which is exactly the divergence a converter cannot afford.
fn n_set_inner_html(t: &JsValue, a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let h = match this_h(t, ctx) { Some(h) => h, None => return Ok(JsValue::undefined()) };
    let html = a.get_or_undefined(0).to_string(ctx)?.to_std_string_escaped();
    let frag = crate::parse::parse_fragment(&html);
    with(|d| {
        for c in d.children_of(h) { d.detach(c); }
        let root = frag.root();
        for c in frag.children_of(root) {
            let g = d.graft(&frag, c);
            d.append(h, g);
        }
        d.script_mutations += 1;
    });
    record_mutation("childList", h, "");
    Ok(JsValue::undefined())
}
fn n_clone(t: &JsValue, a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let h = match this_h(t, ctx) { Some(h) => h, None => return Ok(JsValue::null()) };
    let deep = a.get_or_undefined(0).to_boolean();
    let c = with(|d| d.clone_node(h, deep));
    opt_node(c, ctx)
}
fn n_insert_before(t: &JsValue, a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let p = match this_h(t, ctx) { Some(h) => h, None => return Ok(JsValue::null()) };
    let node = handle_of(a.get_or_undefined(0), ctx);
    let before = handle_of(a.get_or_undefined(1), ctx);
    if let Some(n) = node {
        let ok = with(|d| { let r = d.insert_before(p, n, before); if r { d.script_mutations += 1 } r });
        if ok { record_mutation("childList", p, ""); }
    }
    Ok(a.get_or_undefined(0).clone())
}
fn n_remove_child(t: &JsValue, a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let p = match this_h(t, ctx) { Some(h) => h, None => return Ok(JsValue::null()) };
    if let Some(c) = handle_of(a.get_or_undefined(0), ctx) {
        let ok = with(|d| { let r = d.remove_child(p, c); if r { d.script_mutations += 1 } r });
        if ok { record_mutation("childList", p, ""); }
    }
    Ok(a.get_or_undefined(0).clone())
}
fn n_replace_child(t: &JsValue, a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let p = match this_h(t, ctx) { Some(h) => h, None => return Ok(JsValue::null()) };
    let new = handle_of(a.get_or_undefined(0), ctx);
    let old = handle_of(a.get_or_undefined(1), ctx);
    if let (Some(n), Some(o)) = (new, old) {
        let ok = with(|d| { let r = d.replace_child(p, n, o); if r { d.script_mutations += 1 } r });
        if ok { record_mutation("childList", p, ""); }
    }
    Ok(a.get_or_undefined(1).clone())
}
fn n_remove(t: &JsValue, _a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(h) = this_h(t, ctx) {
        let p = with(|d| d.get(h).and_then(|n| n.parent));
        let ok = with(|d| { let r = d.detach(h); if r { d.script_mutations += 1 } r });
        if ok { if let Some(p) = p { record_mutation("childList", p, ""); } }
    }
    Ok(JsValue::undefined())
}
fn n_contains(t: &JsValue, a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let h = match this_h(t, ctx) { Some(h) => h, None => return Ok(JsValue::from(false)) };
    let other = handle_of(a.get_or_undefined(0), ctx);
    Ok(JsValue::from(other.map(|o| with(|d| d.contains(h, o))).unwrap_or(false)))
}
fn n_has_child_nodes(t: &JsValue, _a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let h = this_h(t, ctx).unwrap_or(0);
    Ok(JsValue::from(!with(|d| d.children_of(h)).is_empty()))
}
fn n_has_attribute(t: &JsValue, a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let h = match this_h(t, ctx) { Some(h) => h, None => return Ok(JsValue::from(false)) };
    let k = a.get_or_undefined(0).to_string(ctx)?.to_std_string_escaped();
    Ok(JsValue::from(with(|d| d.attr(h, &k).is_some())))
}
fn n_remove_attribute(t: &JsValue, a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let h = match this_h(t, ctx) { Some(h) => h, None => return Ok(JsValue::undefined()) };
    let k = a.get_or_undefined(0).to_string(ctx)?.to_std_string_escaped();
    with(|d| { if let Some(n) = d.get_mut(h) { n.attrs.retain(|(a, _)| a != &k); } d.script_mutations += 1; });
    record_mutation("attributes", h, &k);
    Ok(JsValue::undefined())
}

/// Install the node tree surface shared by elements, text nodes, fragments
/// and the document itself.
fn install_tree(o: &JsObject, ctx: &mut Context) {
    live_get(o, "parentNode", n_parent, ctx);
    live_get(o, "parentElement", n_parent_element, ctx);
    live_get(o, "childNodes", n_child_nodes, ctx);
    live_get(o, "children", n_children, ctx);
    live_get(o, "firstChild", n_first_child, ctx);
    live_get(o, "lastChild", n_last_child, ctx);
    live_get(o, "firstElementChild", n_first_el_child, ctx);
    live_get(o, "lastElementChild", n_last_el_child, ctx);
    live_get(o, "nextSibling", n_next, ctx);
    live_get(o, "previousSibling", n_prev, ctx);
    live_get(o, "nodeType", n_node_type, ctx);
    live_get(o, "nodeName", n_node_name, ctx);
    live_get(o, "nodeValue", n_node_value, ctx);
    live_get(o, "ownerDocument", n_owner_document, ctx);
    live_get(o, "outerHTML", n_outer_html, ctx);
    live_get_set(o, "innerHTML", n_inner_html, n_set_inner_html, ctx);
    for (name, f) in [
        ("cloneNode", n_clone as fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>),
        ("insertBefore", n_insert_before),
        ("removeChild", n_remove_child),
        ("replaceChild", n_replace_child),
        ("remove", n_remove),
        ("contains", n_contains),
        ("hasChildNodes", n_has_child_nodes),
        ("hasAttribute", n_has_attribute),
        ("removeAttribute", n_remove_attribute),
    ] {
        let f = NativeFunction::from_fn_ptr(f).to_js_function(ctx.realm());
        let _ = o.set(js_string!(name.to_string()), f, false, ctx);
    }
}

/// `document.implementation.createHTMLDocument(title)`.
///
/// A real, detached document in the same arena — html/head/body actually
/// built, not an object pretending. jQuery uses one to parse untrusted markup
/// away from the live tree, which is a habit worth supporting rather than
/// faking: the nodes it creates here genuinely cannot reach the page unless
/// something grafts them in.
fn create_html_document(_t: &JsValue, _a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let (doc, body) = with(|d| {
        let doc = d.create(Kind::Document);
        let html = d.create(Kind::Element("html".into()));
        let head = d.create(Kind::Element("head".into()));
        let body = d.create(Kind::Element("body".into()));
        d.append(doc, html);
        d.append(html, head);
        d.append(html, body);
        (doc, body)
    });
    let o = node_obj(doc, ctx);
    if let Some(obj) = o.as_object() {
        let b = node_obj(body, ctx);
        let _ = obj.set(js_string!("body"), b, false, ctx);
        let html_h = with(|d| d.children_of(doc).first().copied()).unwrap_or(doc);
        let de = node_obj(html_h, ctx);
        let _ = obj.set(js_string!("documentElement"), de, false, ctx);
    }
    Ok(o)
}

/// `document.write(markup)`.
///
/// ★ WHERE IT WRITES IS THE WHOLE QUESTION. During parsing a browser inserts
/// at the parser's position, which is immediately after the running script —
/// and `currentScript` tells us exactly which element that is, so that case
/// is served faithfully.
///
/// Called with no script running (from a timer, a callback, after load) a
/// browser does something a converter must not: an implicit `document.open()`
/// that ERASES the document and starts a new one. Reproducing that would
/// destroy the artifact on behalf of a script that, in a real browser, would
/// equally have destroyed the page — almost always an ad or a legacy loader.
/// It is refused and COUNTED, so the choice is visible rather than silent.
fn document_write(_t: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let mut markup = String::new();
    for a in args { markup.push_str(&a.to_string(ctx)?.to_std_string_escaped()); }
    let Some(anchor) = CURRENT.with(|c| *c.borrow()) else {
        DOC_WRITE.with(|c| c.borrow_mut().1 += 1);
        return Ok(JsValue::undefined());
    };
    let frag = crate::parse::parse_fragment(&markup);
    with(|d| {
        let Some(parent) = d.get(anchor).and_then(|n| n.parent) else { return };
        // After the script element, in order, which is where the parser was.
        let from = WRITE_POS.with(|m| m.borrow().get(&anchor).copied()).unwrap_or(anchor);
        let mut before = d.next_sibling(from);
        let mut last = from;
        let root = frag.root();
        for c in frag.children_of(root) {
            let g = d.graft(&frag, c);
            d.insert_before(parent, g, before);
            before = d.next_sibling(g);
            last = g;
            d.script_mutations += 1;
        }
        WRITE_POS.with(|m| m.borrow_mut().insert(anchor, last));
        DOC_WRITE.with(|c| c.borrow_mut().0 += 1);
    });
    let parent = with(|d| d.get(anchor).and_then(|n| n.parent));
    if let Some(p) = parent { record_mutation("childList", p, ""); }
    Ok(JsValue::undefined())
}

/// `document.createDocumentFragment()`.
fn create_fragment(_t: &JsValue, _a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let h = with(|d| d.create(Kind::Fragment));
    Ok(node_obj(h, ctx))
}

/// `document.getElementsByName(name)`.
fn by_name(_t: &JsValue, a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let want = a.get_or_undefined(0).to_string(ctx)?.to_std_string_escaped();
    let hs = with(|d| (0..d.nodes.len() as Handle)
        .filter(|&h| d.attr(h, "name") == Some(want.as_str()))
        .collect::<Vec<_>>());
    handles_to_array(hs, ctx)
}

/// The URL decomposition a hyperlink element carries: `href`, and the parts
/// of it. Only `<a>`, `<area>`, `<link>` and `<base>` have these in a browser
/// — on anything else they are undefined, and reporting them everywhere
/// would answer a feature detection wrongly.
fn is_hyperlink(h: Handle) -> bool {
    with(|d| d.tag(h).map(|t| matches!(t.to_ascii_lowercase().as_str(),
        "a" | "area" | "link" | "base")).unwrap_or(false))
}

/// Resolve the element's `href` against the document URL, the way a browser
/// reports it: the PROPERTY is absolute even when the attribute is relative.
fn hyperlink_url(h: Handle) -> Option<url::Url> {
    let raw = with(|d| d.attr(h, "href").map(str::to_string))?;
    let base = BASE.with(|b| b.borrow().clone());
    match base.and_then(|b| url::Url::parse(&b).ok()) {
        Some(b) => b.join(&raw).ok(),
        None => url::Url::parse(&raw).ok(),
    }
}

/// Every part is "" when there is no resolvable href, which is what a browser
/// reports for `<a>` without one — not undefined, and not a throw.
fn hyperlink_part(this: &JsValue, part: &str, ctx: &mut Context) -> JsResult<JsValue> {
    let Some(h) = handle_of(this, ctx) else { return Ok(JsValue::from(js_string!(""))) };
    let Some(u) = hyperlink_url(h) else { return Ok(JsValue::from(js_string!(""))) };
    let v = match part {
        "href" => u.to_string(),
        // Includes the colon, as the DOM says: "https:" not "https".
        "protocol" => format!("{}:", u.scheme()),
        "host" => match u.port() {
            Some(p) => format!("{}:{}", u.host_str().unwrap_or(""), p),
            None => u.host_str().unwrap_or("").to_string(),
        },
        "hostname" => u.host_str().unwrap_or("").to_string(),
        "port" => u.port().map(|p| p.to_string()).unwrap_or_default(),
        "pathname" => u.path().to_string(),
        // Leading "?" and "#" are part of the value, and EMPTY when absent.
        "search" => u.query().map(|q| format!("?{q}")).unwrap_or_default(),
        "hash" => u.fragment().map(|f| format!("#{f}")).unwrap_or_default(),
        "origin" => u.origin().ascii_serialization(),
        _ => String::new(),
    };
    Ok(JsValue::from(js_string!(v)))
}

macro_rules! hyperlink_getter {
    ($name:ident, $part:literal) => {
        fn $name(t: &JsValue, _a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
            hyperlink_part(t, $part, ctx)
        }
    };
}
hyperlink_getter!(hl_href, "href");
hyperlink_getter!(hl_protocol, "protocol");
hyperlink_getter!(hl_host, "host");
hyperlink_getter!(hl_hostname, "hostname");
hyperlink_getter!(hl_port, "port");
hyperlink_getter!(hl_pathname, "pathname");
hyperlink_getter!(hl_search, "search");
hyperlink_getter!(hl_hash, "hash");
hyperlink_getter!(hl_origin, "origin");

/// Assigning `href` writes the ATTRIBUTE, so the artifact carries it and the
/// parts recompute from it. A plain property would diverge from the markup.
fn hl_set_href(t: &JsValue, a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(h) = handle_of(t, ctx) else { return Ok(JsValue::undefined()) };
    let v = a.get_or_undefined(0).to_string(ctx)?.to_std_string_escaped();
    with(|d| { d.set_attr(h, "href", &v); d.script_mutations += 1 });
    record_mutation("attributes", h, "href");
    Ok(JsValue::undefined())
}

fn install_hyperlink(o: &JsObject, ctx: &mut Context) {
    live_get_set(o, "href", hl_href, hl_set_href, ctx);
    for (n, f) in [
        ("protocol", hl_protocol as fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>),
        ("host", hl_host), ("hostname", hl_hostname), ("port", hl_port),
        ("pathname", hl_pathname), ("search", hl_search), ("hash", hl_hash),
        ("origin", hl_origin),
    ] {
        live_get(o, n, f, ctx);
    }
}

fn node_obj(h: Handle, ctx: &mut Context) -> JsValue {
    if let Some(v) = NODE_CACHE.with(|c| c.borrow().get(&h).cloned()) { return v }
    let tag = with(|d| d.tag(h).map(|s| s.to_string())).unwrap_or_default();
    let o = ObjectInitializer::new(ctx)
        .property(js_string!("__h"), h as f64, Attribute::all())
        .property(js_string!("tagName"), js_string!(tag.to_uppercase()), Attribute::all())
        .function(NativeFunction::from_fn_ptr(append_child), js_string!("appendChild"), 1)
        .function(NativeFunction::from_fn_ptr(set_attribute), js_string!("setAttribute"), 2)
        .function(NativeFunction::from_fn_ptr(get_attribute), js_string!("getAttribute"), 1)
        .function(NativeFunction::from_fn_ptr(get_text), js_string!("getText"), 0)
        .function(NativeFunction::from_fn_ptr(set_text), js_string!("setText"), 1)
        // Element listeners are accepted and dropped: nothing in a headless
        // conversion will ever deliver a click. Accepting them keeps a page
        // running; pretending to deliver them would be a lie.
        .function(NativeFunction::from_fn_ptr(el_add_listener), js_string!("addEventListener"), 2)
        .function(NativeFunction::from_fn_ptr(el_dispatch), js_string!("dispatchEvent"), 1)
        .function(NativeFunction::from_fn_ptr(el_remove_listener), js_string!("removeEventListener"), 2)
        .function(NativeFunction::from_fn_ptr(query_first), js_string!("querySelector"), 1)
        .function(NativeFunction::from_fn_ptr(query_all), js_string!("querySelectorAll"), 1)
        .function(NativeFunction::from_fn_ptr(by_class), js_string!("getElementsByClassName"), 1)
        .function(NativeFunction::from_fn_ptr(by_tag_name), js_string!("getElementsByTagName"), 1)
        .build();
    // `src` as a browser reports it: absolute against the document URL.
    if let Some(raw) = with(|d| d.attr(h, "src").map(str::to_string)) {
        let abs = BASE.with(|b| b.borrow().clone())
            .and_then(|b| url::Url::parse(&b).ok())
            .and_then(|b| b.join(&raw).ok())
            .map(|u| u.to_string())
            .unwrap_or(raw);
        let _ = o.set(js_string!("src"), js_string!(abs), false, ctx);
    }
    install_text_accessor(&o, ctx);
    for n in ["clientWidth", "offsetWidth", "scrollWidth"] { layout_prop(&o, n, layout_w, ctx); }
    for n in ["clientHeight", "offsetHeight", "scrollHeight"] { layout_prop(&o, n, layout_h, ctx); }
    for n in ["offsetTop", "offsetLeft", "scrollTop", "scrollLeft"] { layout_prop(&o, n, layout_zero, ctx); }
    {
        let cl = class_list_obj(h, ctx);
        let _ = o.set(js_string!("classList"), cl, false, ctx);
        let st = style_obj(h, ctx);
        let _ = o.set(js_string!("style"), st, false, ctx);
        let ds = dataset_obj(h, ctx);
        let _ = o.set(js_string!("dataset"), ds, false, ctx);
        let cn = with(|d| d.class_list(h).join(" "));
        let _ = o.set(js_string!("className"), js_string!(cn), false, ctx);
    }
    {
        let f = NativeFunction::from_fn_ptr(bounding_rect);
        let _ = o.set(js_string!("getBoundingClientRect"),
                      f.to_js_function(ctx.realm()), false, ctx);
    }
    install_tree(&o, ctx);
    if is_hyperlink(h) { install_hyperlink(&o, ctx); }
    let v = probed(o, "element", ctx);
    NODE_CACHE.with(|c| c.borrow_mut().insert(h, v.clone()));
    v
}

/// `textContent` as a real accessor: scripts use the property, not a method.
fn install_text_accessor(o: &JsObject, ctx: &mut Context) {
    let getter = NativeFunction::from_fn_ptr(get_text).to_js_function(ctx.realm());
    let setter = NativeFunction::from_fn_ptr(set_text).to_js_function(ctx.realm());
    let desc = boa_engine::property::PropertyDescriptor::builder()
        .get(getter).set(setter).enumerable(true).configurable(true).build();
    let _ = o.define_property_or_throw(js_string!("textContent"), desc, ctx);
}

/// `__parse_url(href, base?)` -> parts object, or null.
/// Parsing in Rust keeps one URL implementation rather than a JS reimplementation
/// that would disagree with the one used to resolve scripts.
fn parse_url(_t: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let href = args.get_or_undefined(0).to_string(ctx)?.to_std_string_escaped();
    let base = args.get_or_undefined(1);
    let parsed = if base.is_undefined() || base.is_null() {
        url::Url::parse(&href).ok()
    } else {
        let b = base.to_string(ctx)?.to_std_string_escaped();
        url::Url::parse(&b).ok().and_then(|b| b.join(&href).ok())
    };
    let Some(u) = parsed else { return Ok(JsValue::null()) };
    let o = ObjectInitializer::new(ctx)
        .property(js_string!("href"), js_string!(u.as_str().to_string()), Attribute::all())
        .property(js_string!("protocol"), js_string!(format!("{}:", u.scheme())), Attribute::all())
        .property(js_string!("hostname"), js_string!(u.host_str().unwrap_or("").to_string()), Attribute::all())
        .property(js_string!("host"), js_string!(match u.port() {
            Some(p) => format!("{}:{p}", u.host_str().unwrap_or("")),
            None => u.host_str().unwrap_or("").to_string() }), Attribute::all())
        .property(js_string!("port"), js_string!(u.port().map(|p| p.to_string()).unwrap_or_default()), Attribute::all())
        .property(js_string!("pathname"), js_string!(u.path().to_string()), Attribute::all())
        .property(js_string!("search"), js_string!(u.query().map(|q| format!("?{q}")).unwrap_or_default()), Attribute::all())
        .property(js_string!("hash"), js_string!(u.fragment().map(|f| format!("#{f}")).unwrap_or_default()), Attribute::all())
        .property(js_string!("origin"), js_string!(u.origin().ascii_serialization()), Attribute::all())
        .build();
    Ok(JsValue::from(o))
}

/// ★ LAYOUT METRICS ARE FICTION, AND THE QUESTION IS WHICH FICTION HURTS LEAST.
///
/// This converter performs no layout, so clientHeight and its relatives have
/// no true answer. Three options, none of them honest:
///
///   - absent (what we did): the script throws on `undefined.foo` and the
///     WHOLE script's content is lost, including everything it would have
///     built before touching layout;
///   - zero: a script told an element has no height routinely collapses or
///     hides it, so the artifact is silently missing content;
///   - a generous, self-consistent box: the script proceeds and content
///     survives, at the cost of any geometry-dependent decision being made on
///     numbers that are not real.
///
/// The third is chosen because this converter's output is laid out LATER by
/// the real renderer: erring toward "everything is visible and nothing
/// overflows" leaves that decision to the thing that can actually make it.
/// scroll* equals client* deliberately, so no script concludes it must
/// truncate or paginate.
///
/// Every read is counted (`layout_reads`) so the reach of the fiction is
/// measured rather than assumed.
const NOMINAL_W: f64 = 1280.0;
const NOMINAL_H: f64 = 600.0;

fn layout_w(_t: &JsValue, _a: &[JsValue], _c: &mut Context) -> JsResult<JsValue> {
    LAYOUT_READS.with(|n| *n.borrow_mut() += 1);
    Ok(JsValue::from(NOMINAL_W))
}
fn layout_h(_t: &JsValue, _a: &[JsValue], _c: &mut Context) -> JsResult<JsValue> {
    LAYOUT_READS.with(|n| *n.borrow_mut() += 1);
    Ok(JsValue::from(NOMINAL_H))
}
fn layout_zero(_t: &JsValue, _a: &[JsValue], _c: &mut Context) -> JsResult<JsValue> {
    LAYOUT_READS.with(|n| *n.borrow_mut() += 1);
    Ok(JsValue::from(0.0))
}
fn bounding_rect(_t: &JsValue, _a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    LAYOUT_READS.with(|n| *n.borrow_mut() += 1);
    let o = ObjectInitializer::new(ctx)
        .property(js_string!("x"), 0.0, Attribute::all())
        .property(js_string!("y"), 0.0, Attribute::all())
        .property(js_string!("top"), 0.0, Attribute::all())
        .property(js_string!("left"), 0.0, Attribute::all())
        .property(js_string!("width"), NOMINAL_W, Attribute::all())
        .property(js_string!("height"), NOMINAL_H, Attribute::all())
        .property(js_string!("right"), NOMINAL_W, Attribute::all())
        .property(js_string!("bottom"), NOMINAL_H, Attribute::all())
        .build();
    Ok(JsValue::from(o))
}

/// Install a read-only accessor backed by one of the layout getters.
fn layout_prop(o: &JsObject, name: &str, f: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>, ctx: &mut Context) {
    let getter = NativeFunction::from_fn_ptr(f).to_js_function(ctx.realm());
    let desc = boa_engine::property::PropertyDescriptor::builder()
        .get(getter).enumerable(true).configurable(true).build();
    let _ = o.define_property_or_throw(js_string!(name.to_string()), desc, ctx);
}

// --- classList and style ---------------------------------------------------
//
// Both are live views onto an attribute, so both read and write through the
// arena rather than caching: a script that sets a class and then reads
// className must see its own write, and the serialized output must carry it.

fn cl_args(args: &[JsValue], ctx: &mut Context) -> Vec<String> {
    args.iter().filter_map(|a| a.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .flat_map(|s| s.split_whitespace().map(str::to_string).collect::<Vec<_>>())
        .collect()
}

fn cl_add(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(h) = handle_of(this, ctx) {
        let names = cl_args(args, ctx);
        with(|d| { d.class_add(h, &names); d.script_mutations += 1 });
        record_mutation("attributes", h, "class");
    }
    Ok(JsValue::undefined())
}
fn cl_remove(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(h) = handle_of(this, ctx) {
        let names = cl_args(args, ctx);
        with(|d| { d.class_remove(h, &names); d.script_mutations += 1 });
        record_mutation("attributes", h, "class");
    }
    Ok(JsValue::undefined())
}
fn cl_contains(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let n = args.get_or_undefined(0).to_string(ctx)?.to_std_string_escaped();
    Ok(JsValue::from(match handle_of(this, ctx) {
        Some(h) => with(|d| d.class_list(h).iter().any(|c| *c == n)),
        None => false,
    }))
}
fn cl_toggle(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let n = args.get_or_undefined(0).to_string(ctx)?.to_std_string_escaped();
    let force = match args.get(1) {
        Some(v) if !v.is_undefined() => Some(v.to_boolean()),
        _ => None,
    };
    Ok(JsValue::from(match handle_of(this, ctx) {
        Some(h) => with(|d| { d.script_mutations += 1; d.class_toggle(h, &n, force) }),
        None => false,
    }))
}
fn cl_value(this: &JsValue, _a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(js_string!(match handle_of(this, ctx) {
        Some(h) => with(|d| d.class_list(h).join(" ")),
        None => String::new(),
    })))
}
fn cl_length(this: &JsValue, _a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(match handle_of(this, ctx) {
        Some(h) => with(|d| d.class_list(h).len()) as f64,
        None => 0.0,
    }))
}

fn class_list_obj(h: Handle, ctx: &mut Context) -> JsValue {
    let o = ObjectInitializer::new(ctx)
        .property(js_string!("__h"), h as f64, Attribute::all())
        .function(NativeFunction::from_fn_ptr(cl_add), js_string!("add"), 1)
        .function(NativeFunction::from_fn_ptr(cl_remove), js_string!("remove"), 1)
        .function(NativeFunction::from_fn_ptr(cl_contains), js_string!("contains"), 1)
        .function(NativeFunction::from_fn_ptr(cl_toggle), js_string!("toggle"), 2)
        .build();
    layout_prop_named(&o, "value", cl_value, ctx);
    layout_prop_named(&o, "length", cl_length, ctx);
    JsValue::from(o)
}

/// Install a read-only accessor from any getter fn.
fn layout_prop_named(o: &JsObject, name: &str,
    f: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>, ctx: &mut Context) {
    let getter = NativeFunction::from_fn_ptr(f).to_js_function(ctx.realm());
    let desc = boa_engine::property::PropertyDescriptor::builder()
        .get(getter).enumerable(true).configurable(true).build();
    let _ = o.define_property_or_throw(js_string!(name.to_string()), desc, ctx);
}

/// `el.style` — a Proxy, because `el.style.display = 'none'` is a set of an
/// ARBITRARY property name and there is no other way to intercept it.
fn style_get_trap(_t: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let target = args.get_or_undefined(0).as_object()
        .ok_or_else(|| boa_engine::JsNativeError::typ().with_message("proxy target"))?;
    let key = args.get_or_undefined(1).clone().to_property_key(ctx)?;
    let name = key.to_string();
    if name.starts_with("__") || target.has_property(key.clone(), ctx)? {
        return target.get(key, ctx);
    }
    let h = target.get(js_string!("__h"), ctx)?.as_number().unwrap_or(0.0) as Handle;
    if name == "cssText" {
        return Ok(JsValue::from(js_string!(with(|d| d.attr(h, "style").unwrap_or("").to_string()))));
    }
    Ok(JsValue::from(js_string!(with(|d| d.style_get(h, &name)))))
}

fn style_set_trap(_t: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let target = args.get_or_undefined(0).as_object()
        .ok_or_else(|| boa_engine::JsNativeError::typ().with_message("proxy target"))?;
    let name = args.get_or_undefined(1).clone().to_property_key(ctx)?.to_string();
    let val = args.get_or_undefined(2).to_string(ctx)?.to_std_string_escaped();
    let h = target.get(js_string!("__h"), ctx)?.as_number().unwrap_or(0.0) as Handle;
    with(|d| {
        if name == "cssText" { d.set_attr(h, "style", &val) } else { d.style_set(h, &name, &val) }
        d.script_mutations += 1;
    });
    Ok(JsValue::from(true))
}

fn style_set_property(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let p = args.get_or_undefined(0).to_string(ctx)?.to_std_string_escaped();
    let v = args.get_or_undefined(1).to_string(ctx)?.to_std_string_escaped();
    if let Some(h) = handle_of(this, ctx) {
        with(|d| { d.style_set(h, &p, &v); d.script_mutations += 1 });
        record_mutation("attributes", h, "style");
    }
    Ok(JsValue::undefined())
}
fn style_get_property(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let p = args.get_or_undefined(0).to_string(ctx)?.to_std_string_escaped();
    Ok(JsValue::from(js_string!(match handle_of(this, ctx) {
        Some(h) => with(|d| d.style_get(h, &p)),
        None => String::new(),
    })))
}
fn style_remove_property(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let p = args.get_or_undefined(0).to_string(ctx)?.to_std_string_escaped();
    if let Some(h) = handle_of(this, ctx) {
        with(|d| { d.style_set(h, &p, ""); d.script_mutations += 1 });
    }
    Ok(JsValue::undefined())
}

/// `element.dataset` — live over the arena in both directions, so a write
/// really sets the attribute and a read really sees one set elsewhere. A
/// snapshot object would silently diverge the moment anything called
/// setAttribute.
fn dataset_obj(h: Handle, ctx: &mut Context) -> JsValue {
    let target = ObjectInitializer::new(ctx)
        .property(js_string!("__h"), h as f64, Attribute::all())
        .build();
    match boa_engine::object::builtins::JsProxy::builder(target.clone())
        .get(dataset_get_trap)
        .set(dataset_set_trap)
        .has(dataset_has_trap)
        .delete_property(dataset_delete_trap)
        .own_keys(dataset_keys_trap)
        .get_own_property_descriptor(dataset_desc_trap)
        .build(ctx)
    {
        Ok(p) => JsValue::from(JsObject::from(p)),
        Err(_) => JsValue::from(target),
    }
}

fn dataset_handle(args: &[JsValue], ctx: &mut Context) -> JsResult<Handle> {
    let target = args.get_or_undefined(0).as_object()
        .ok_or_else(|| boa_engine::JsNativeError::typ().with_message("proxy target"))?;
    Ok(target.get(js_string!("__h"), ctx)?.as_number().unwrap_or(0.0) as Handle)
}

fn dataset_get_trap(_t: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let key = args.get_or_undefined(1).clone().to_property_key(ctx)?;
    let name = key.to_string();
    if name.starts_with("__") {
        let target = args.get_or_undefined(0).as_object()
            .ok_or_else(|| boa_engine::JsNativeError::typ().with_message("proxy target"))?;
        return target.get(key, ctx);
    }
    let h = dataset_handle(args, ctx)?;
    // An ABSENT data attribute is undefined, not "". Pages branch on it.
    Ok(match with(|d| d.data_get(h, &name)) {
        Some(v) => JsValue::from(js_string!(v)),
        None => JsValue::undefined(),
    })
}

fn dataset_set_trap(_t: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = args.get_or_undefined(1).clone().to_property_key(ctx)?.to_string();
    let val = args.get_or_undefined(2).to_string(ctx)?.to_std_string_escaped();
    let h = dataset_handle(args, ctx)?;
    with(|d| { d.data_set(h, &name, &val); d.script_mutations += 1 });
    record_mutation("attributes", h, &Dom::data_attr_name(&name));
    Ok(JsValue::from(true))
}

fn dataset_has_trap(_t: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = args.get_or_undefined(1).clone().to_property_key(ctx)?.to_string();
    let h = dataset_handle(args, ctx)?;
    Ok(JsValue::from(with(|d| d.data_get(h, &name)).is_some()))
}

fn dataset_delete_trap(_t: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = args.get_or_undefined(1).clone().to_property_key(ctx)?.to_string();
    let h = dataset_handle(args, ctx)?;
    with(|d| { d.data_remove(h, &name); d.script_mutations += 1 });
    record_mutation("attributes", h, &Dom::data_attr_name(&name));
    Ok(JsValue::from(true))
}

/// So `Object.keys(el.dataset)` and `for...in` enumerate the real set.
fn dataset_keys_trap(_t: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let h = dataset_handle(args, ctx)?;
    let arr = boa_engine::object::builtins::JsArray::new(ctx)?;
    for k in with(|d| d.data_keys(h)) { arr.push(JsValue::from(js_string!(k)), ctx)?; }
    Ok(JsValue::from(arr))
}

/// ownKeys alone is not enough for `Object.keys`: the spec filters by the
/// descriptor's enumerable flag, so without this every key is dropped and
/// enumeration silently returns nothing.
fn dataset_desc_trap(_t: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = args.get_or_undefined(1).clone().to_property_key(ctx)?.to_string();
    let h = dataset_handle(args, ctx)?;
    Ok(match with(|d| d.data_get(h, &name)) {
        Some(v) => JsValue::from(ObjectInitializer::new(ctx)
            .property(js_string!("value"), js_string!(v), Attribute::all())
            .property(js_string!("writable"), true, Attribute::all())
            .property(js_string!("enumerable"), true, Attribute::all())
            .property(js_string!("configurable"), true, Attribute::all())
            .build()),
        None => JsValue::undefined(),
    })
}

fn style_obj(h: Handle, ctx: &mut Context) -> JsValue {
    let target = ObjectInitializer::new(ctx)
        .property(js_string!("__h"), h as f64, Attribute::all())
        .function(NativeFunction::from_fn_ptr(style_set_property), js_string!("setProperty"), 2)
        .function(NativeFunction::from_fn_ptr(style_get_property), js_string!("getPropertyValue"), 1)
        .function(NativeFunction::from_fn_ptr(style_remove_property), js_string!("removeProperty"), 1)
        .build();
    match boa_engine::object::builtins::JsProxy::builder(target.clone())
        .get(style_get_trap).set(style_set_trap).build(ctx)
    {
        Ok(p) => JsValue::from(JsObject::from(p)),
        Err(_) => JsValue::from(target),
    }
}

/// `__fetch_sync(url)` -> { ok, status, url, body } | null.
///
/// Synchronous by design: the conversion is a single pass with no event loop
/// to await on, so the request is made now and handed to JS as an
/// already-settled promise. Determinism comes from the fetch cache — a second
/// conversion of the same document reads the same bytes.
/// ★ THE PAGE NETWORK IS SAME-ORIGIN GET ONLY. Everything else is refused.
///
/// A converter that runs a page's scripts will otherwise fire its analytics:
/// a measured 72 of 77 requests in the corpus went to third-party telemetry
/// endpoints, sent on behalf of NOBODY — no reader existed. Worse than a
/// browser doing it, because a conversion is amortised across many readers
/// who never made the request, so one run speaks for all of them.
///
/// Structural rather than a blocklist, deliberately. A filter list is an
/// arms race and is wrong the day it ships; origin and method are properties
/// of the request itself. Telemetry is overwhelmingly cross-origin or POST,
/// and content a document needs to render itself is overwhelmingly neither.
///
/// This will refuse some legitimate cross-origin content APIs. That is the
/// intended trade for a privacy-motivated architecture: refusals are COUNTED
/// and reported by host, so the cost is visible and arguable rather than
/// assumed. The complete answer is tier-3 substitution by content hash — the
/// analytics SDK never runs at all — and this is the structural floor beneath
/// it.
fn blocked(reason: &str, host: &str) {
    PAGE_BLOCKED.with(|b| {
        let mut b = b.borrow_mut();
        b.0 += 1;
        *b.1.entry(format!("{reason} {host}")).or_default() += 1;
    });
}

fn fetch_sync(_t: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let url = args.get_or_undefined(0).to_string(ctx)?.to_std_string_escaped();
    let base = BASE.with(|b| b.borrow().clone());
    let abs = match &base {
        Some(b) => url::Url::parse(b).ok().and_then(|b| b.join(&url).ok())
            .map(|u| u.to_string()).unwrap_or(url.clone()),
        None => url.clone(),
    };

    let method = args.get_or_undefined(1).to_string(ctx)
        .map(|s| s.to_std_string_escaped().to_ascii_uppercase())
        .unwrap_or_else(|_| "GET".into());
    let parsed = url::Url::parse(&abs).ok();
    let host = parsed.as_ref().and_then(|u| u.host_str()).unwrap_or("?").to_string();

    if method != "GET" && method != "HEAD" {
        blocked(&format!("{method} to"), &host);
        return Ok(JsValue::null());
    }
    let same_origin = match (&base, &parsed) {
        (Some(b), Some(u)) => url::Url::parse(b).ok()
            .map(|b| b.origin() == u.origin()).unwrap_or(false),
        _ => false,
    };
    if !same_origin {
        blocked("cross-origin", &host);
        return Ok(JsValue::null());
    }
    let got = PAGE_NET.with(|n| {
        n.borrow_mut().as_mut().map(|f| f.get(&abs))
    });
    match got {
        Some(Ok(body)) => {
            PAGE_FETCHES.with(|c| c.borrow_mut().0 += 1);
            let o = ObjectInitializer::new(ctx)
                .property(js_string!("ok"), true, Attribute::all())
                .property(js_string!("status"), 200.0, Attribute::all())
                .property(js_string!("url"), js_string!(abs), Attribute::all())
                .property(js_string!("body"), js_string!(body), Attribute::all())
                .build();
            Ok(JsValue::from(o))
        }
        // Both "no network configured" and "request failed" reach the page as
        // a rejected promise, which is what a browser does on a network error.
        _ => { PAGE_FETCHES.with(|c| c.borrow_mut().1 += 1); Ok(JsValue::null()) }
    }
}

fn record_mutation(kind: &'static str, h: Handle, name: &str) {
    MUTATIONS.with(|m| {
        let mut m = m.borrow_mut();
        // Bounded: a mutation-driven observer can otherwise feed itself.
        if m.len() < 10_000 { m.push((kind, h, name.to_string())) }
    });
}

/// Hand the pending records to JS and clear the log.
fn take_mutations(_t: &JsValue, _a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let recs = MUTATIONS.with(|m| std::mem::take(&mut *m.borrow_mut()));
    let arr = boa_engine::object::builtins::JsArray::new(ctx)?;
    for (kind, h, name) in recs {
        let target = node_obj(h, ctx);
        let o = ObjectInitializer::new(ctx)
            .property(js_string!("type"), js_string!(kind.to_string()), Attribute::all())
            .property(js_string!("target"), target, Attribute::all())
            .property(js_string!("__th"), h as f64, Attribute::all())
            .property(js_string!("attributeName"),
                if name.is_empty() { JsValue::null() } else { JsValue::from(js_string!(name)) },
                Attribute::all())
            .build();
        arr.push(JsValue::from(o), ctx)?;
    }
    Ok(JsValue::from(arr))
}

/// Subtree matching needs ancestry, and the arena is the only thing that
/// knows it.
fn is_ancestor(_t: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let a = args.get_or_undefined(0).to_number(ctx)? as Handle;
    let b = args.get_or_undefined(1).to_number(ctx)? as Handle;
    Ok(JsValue::from(with(|d| {
        let mut cur = d.get(b).and_then(|n| n.parent);
        while let Some(p) = cur {
            if p == a { return true }
            cur = d.get(p).and_then(|n| n.parent);
        }
        false
    })))
}

fn el_add_listener(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(h) = handle_of(this, ctx) else { return Ok(JsValue::undefined()) };
    let ty = args.get_or_undefined(0).to_string(ctx)?.to_std_string_escaped();
    let f = args.get_or_undefined(1).clone();
    if f.as_callable().is_some() {
        ELISTENERS.with(|m| m.borrow_mut().entry(h).or_default().push((ty, f)));
    }
    Ok(JsValue::undefined())
}

fn el_remove_listener(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(h) = handle_of(this, ctx) else { return Ok(JsValue::undefined()) };
    let ty = args.get_or_undefined(0).to_string(ctx)?.to_std_string_escaped();
    let f = args.get_or_undefined(1).clone();
    ELISTENERS.with(|m| {
        if let Some(v) = m.borrow_mut().get_mut(&h) {
            v.retain(|(t, g)| !(t == &ty && JsValue::strict_equals(g, &f)));
        }
    });
    Ok(JsValue::undefined())
}

/// `el.dispatchEvent(event)` — a real dispatch, because the page is talking
/// to itself. The event walks the target then its ANCESTORS when it bubbles,
/// which is only expressible because the tree is navigable; it then reaches
/// the document and window listeners. Returns `!defaultPrevented`, as the
/// DOM says, and pages branch on that.
fn el_dispatch(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(h) = handle_of(this, ctx) else { return Ok(JsValue::from(true)) };
    let ev = args.get_or_undefined(0).clone();
    let Some(evo) = ev.as_object() else { return Ok(JsValue::from(true)) };
    let evo = evo.clone();
    let ty = evo.get(js_string!("type"), ctx)?.to_string(ctx)?.to_std_string_escaped();
    let bubbles = evo.get(js_string!("bubbles"), ctx)?.to_boolean();

    DISPATCH.with(|c| c.borrow_mut().0 += 1);
    let target = node_obj(h, ctx);
    let _ = evo.set(js_string!("target"), target.clone(), false, ctx);
    let _ = evo.set(js_string!("srcElement"), target, false, ctx);
    let _ = evo.set(js_string!("eventPhase"), 2.0, false, ctx);

    // The propagation path: the target, then ancestors while it bubbles.
    let mut path = vec![h];
    if bubbles {
        let mut cur = with(|d| d.get(h).and_then(|n| n.parent));
        while let Some(p) = cur {
            path.push(p);
            cur = with(|d| d.get(p).and_then(|n| n.parent));
        }
    }
    for node in path {
        if evo.get(js_string!("__stop"), ctx)?.to_boolean() { break }
        let here = node_obj(node, ctx);
        let _ = evo.set(js_string!("currentTarget"), here, false, ctx);
        let fns: Vec<JsValue> = ELISTENERS.with(|m| m.borrow().get(&node)
            .map(|v| v.iter().filter(|(t, _)| t == &ty).map(|(_, f)| f.clone()).collect())
            .unwrap_or_default());
        for f in fns {
            if let Some(c) = f.as_callable() {
                // A listener that throws must not take the dispatch, or the
                // page, with it — a browser reports and continues.
                DISPATCH.with(|c| c.borrow_mut().1 += 1);
                let _ = c.call(&node_obj(node, ctx), &[ev.clone()], ctx);
            }
        }
    }
    // Document- and window-level listeners live on the JS side; hand them the
    // SAME event object rather than a second one built to look like it.
    if !evo.get(js_string!("__stop"), ctx)?.to_boolean() {
        let g = ctx.global_object();
        if let Ok(f) = g.get(js_string!("__fireExisting"), ctx) {
            if let Some(c) = f.as_callable() { let _ = c.call(&JsValue::undefined(), &[ev.clone()], ctx); }
        }
    }
    Ok(JsValue::from(!evo.get(js_string!("defaultPrevented"), ctx)?.to_boolean()))
}

fn ignore(_t: &JsValue, _a: &[JsValue], _c: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

fn append_child(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let (p, c) = (handle_of(this, ctx), handle_of(args.get_or_undefined(0), ctx));
    if let (Some(p), Some(c)) = (p, c) {
        // Through insert_before so a DocumentFragment inserts its CHILDREN
        // here too. appendChild used to call the raw arena append, which
        // grafted the fragment NODE into the tree — it then serialized as
        // nothing and its children never appeared.
        with(|d| { if d.insert_before(p, c, None) { d.script_mutations += 1; } });
        record_mutation("childList", p, "");
    }
    Ok(args.get_or_undefined(0).clone())
}

fn set_attribute(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(h) = handle_of(this, ctx) {
        let k = args.get_or_undefined(0).to_string(ctx)?.to_std_string_escaped();
        let v = args.get_or_undefined(1).to_string(ctx)?.to_std_string_escaped();
        with(|d| { d.set_attr(h, &k, &v); d.script_mutations += 1; });
        record_mutation("attributes", h, &k);
    }
    Ok(JsValue::undefined())
}

fn get_attribute(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(h) = handle_of(this, ctx) {
        let k = args.get_or_undefined(0).to_string(ctx)?.to_std_string_escaped();
        if let Some(v) = with(|d| d.attr(h, &k).map(|s| s.to_string())) {
            return Ok(JsValue::from(js_string!(v)));
        }
    }
    Ok(JsValue::null())
}

fn get_text(this: &JsValue, _a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(match handle_of(this, ctx) {
        Some(h) => JsValue::from(js_string!(with(|d| d.text_content(h)))),
        None => JsValue::from(js_string!("")),
    })
}

fn set_text(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(h) = handle_of(this, ctx) {
        let t = args.get_or_undefined(0).to_string(ctx)?.to_std_string_escaped();
        with(|d| { d.set_text(h, &t); d.script_mutations += 1; });
        record_mutation("characterData", h, "");
    }
    Ok(JsValue::undefined())
}

/// `document.currentScript` — the element being executed, or null.
///
/// Null for modules, as the specification requires, and null outside script
/// execution (a DOMContentLoaded handler sees null, not the last script).
fn current_script(_t: &JsValue, _a: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(match CURRENT.with(|c| *c.borrow()) {
        Some(h) => node_obj(h, ctx),
        None => JsValue::null(),
    })
}

/// Note a lookup that found nothing, with its argument.
fn note_null(api: &str, arg: &str) {
    if !RECORDING.with(|r| *r.borrow()) { return }
    let arg: String = arg.chars().take(60).collect();
    let full = format!("{api}({arg})");
    EVENTS.with(|e| e.borrow_mut().push((
        if api.starts_with("querySelector-UNPARSEABLE") { "ours" } else { "no-match" },
        full.clone())));
    NULLS.with(|n| *n.borrow_mut().entry(full).or_default() += 1);
}

fn get_element_by_id(_t: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = args.get_or_undefined(0).to_string(ctx)?.to_std_string_escaped();
    Ok(match with(|d| d.by_id(&id)) {
        Some(h) => node_obj(h, ctx),
        None => { note_null("getElementById", &id); JsValue::null() }
    })
}

fn create_element(_t: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let tag = args.get_or_undefined(0).to_string(ctx)?.to_std_string_escaped().to_lowercase();
    let h = with(|d| { d.script_mutations += 1; d.create(Kind::Element(tag)) });
    Ok(node_obj(h, ctx))
}

fn create_text_node(_t: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let t = args.get_or_undefined(0).to_string(ctx)?.to_std_string_escaped();
    let h = with(|d| { d.script_mutations += 1; d.create(Kind::Text(t)) });
    Ok(node_obj(h, ctx))
}

/// Scope for a query: the element it was called on, or the document root.
/// Giving the `document` object the root handle lets one implementation serve
/// both `document.querySelector` and `element.querySelector`.
fn scope_of(this: &JsValue, ctx: &mut Context) -> Handle {
    handle_of(this, ctx).unwrap_or(0)
}

fn run_query(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<Vec<Handle>> {
    let sel = args.get_or_undefined(0).to_string(ctx)?.to_std_string_escaped();
    let scope = scope_of(this, ctx);
    Ok(match crate::selector::parse(&sel) {
        Some(q) => with(|d| crate::selector::select(d, scope, &q)),
        // An unparseable selector yields nothing rather than failing the
        // script: a converter should lose one query, not the document.
        None => vec![],
    })
}

fn query_all(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let hs = run_query(this, args, ctx)?;
    let arr = boa_engine::object::builtins::JsArray::new(ctx)?;
    for h in hs { let n = node_obj(h, ctx); arr.push(n, ctx)?; }
    Ok(JsValue::from(arr))
}

fn query_first(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let hs = run_query(this, args, ctx)?;
    Ok(match hs.first() {
        Some(&h) => node_obj(h, ctx),
        None => {
            let sel = args.get_or_undefined(0).to_string(ctx)?.to_std_string_escaped();
            // Separate the two causes: a selector we could not parse is OUR
            // gap, one that parsed and matched nothing is the document's.
            let api = if crate::selector::parse(&sel).is_some() {
                "querySelector-no-match"
            } else {
                "querySelector-UNPARSEABLE"
            };
            note_null(api, &sel);
            JsValue::null()
        }
    })
}

fn by_class(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = args.get_or_undefined(0).to_string(ctx)?.to_std_string_escaped();
    let sel: String = name.split_whitespace().map(|c| format!(".{c}")).collect();
    query_all(this, &[JsValue::from(js_string!(sel))], ctx)
}

fn by_tag_name(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = args.get_or_undefined(0).to_string(ctx)?.to_std_string_escaped();
    let sel = if name == "*" { "*".to_string() } else { name };
    query_all(this, &[JsValue::from(js_string!(sel))], ctx)
}

/// ★ A converter must not be hangable by the content it converts, and Boa's
/// instruction budget is gated behind its `fuzz` feature — which drags
/// `arbitrary` into two crates to obtain a counter. The driver instead runs
/// each document in its OWN PROCESS with a wall-clock deadline (see main.rs),
/// which bounds hangs, stack overflows and runaway allocation alike, needs no
/// feature flags, and mirrors the shipped design: one jail per document.
/// Bounds on the timer drain. A callback cap stops a runaway rescheduler; the
/// horizon keeps the snapshot faithful to "shortly after load".
pub const TIMER_BUDGET: u32 = 1000;
pub const TIMER_HORIZON_MS: u32 = 5000;

#[derive(Default)]
pub struct BoaEngine {
    /// ★ The PAGE's network, deliberately separate from the one used to fetch
    /// scripts and modules. A page calling `fetch()` makes the converter issue
    /// requests on its behalf — the same thing a browser does, but a distinct
    /// concern from loading the page's own code, and one a deployment may want
    /// to refuse independently. `None` means a page cannot reach the network
    /// at all, which is also the default for tests.
    pub page_fetcher: Option<Box<dyn crate::fetch::Fetcher>>,
    /// Root of the mirrored module graph; Boa's loader resolves against it.
    pub module_root: Option<std::path::PathBuf>,
    /// The document's own URL.
    pub base_url: Option<String>,
}

/// Parse, link and evaluate a module, then settle its promise.
///
/// `load_link_evaluate` returns a promise: without draining the job queue the
/// module's top-level body may not have run at all, and a rejection would be
/// reported as success.
fn run_module(ctx: &mut Context, text: &str, path: Option<&std::path::Path>) -> Result<(), String> {
    // Giving the source its mirrored path is what lets relative specifiers
    // resolve: Boa's loader joins them against it.
    let src = match path {
        Some(p) => Source::from_bytes(text.as_bytes()).with_path(p),
        None => Source::from_bytes(text.as_bytes()),
    };
    let module = boa_engine::Module::parse(src, None, ctx).map_err(|e| e.to_string())?;
    let promise = module.load_link_evaluate(ctx);
    ctx.run_jobs().map_err(|e| e.to_string())?;
    match promise.state() {
        boa_engine::builtins::promise::PromiseState::Fulfilled(_) => Ok(()),
        boa_engine::builtins::promise::PromiseState::Rejected(v) => {
            Err(v.to_string(ctx).map(|s| s.to_std_string_escaped())
                .unwrap_or_else(|_| "module rejected".into()))
        }
        // Pending after the queue drained means an import never resolved —
        // this build has no module loader, so a bundle with real imports is
        // reported rather than silently half-run.
        boa_engine::builtins::promise::PromiseState::Pending =>
            Err("module pending: unresolved import (no module loader)".into()),
    }
}


/// Browser globals, defined in JS over one native URL helper.
///
/// Every one is DETERMINISTIC, because G1 requires the same bytes in to give
/// the same bytes out and these are the usual routes by which a clock, a
/// random source or host state leaks into a page.
const GLOBALS: &str = r#"
(function () {
  // ★ A MONOTONIC COUNTER, NOT A CLOCK. performance.now() is the single most
  // common way a real timestamp reaches page content; returning wall time
  // would make conversions differ run to run and break determinism. Counting
  // satisfies both feature-detection and elapsed-time arithmetic.
  // ★ TIMERS ON A VIRTUAL CLOCK, DRAINED UNDER A BUDGET.
  //
  // Two requirements pull against a real implementation. Determinism (G1)
  // forbids wall-clock delays: the same document must convert identically
  // every time. Termination forbids honouring a self-rescheduling timer
  // forever — `setTimeout(loop, 100)` is a perfectly ordinary idiom and would
  // never finish.
  //
  // So time is a NUMBER that only advances when a callback is dispatched,
  // ordering is (time, insertion sequence) so it is total and reproducible,
  // and the drain stops at whichever of two bounds comes first: a callback
  // count, or a virtual-time HORIZON. The horizon is what makes this
  // faithful rather than merely safe — a converter snapshots shortly after
  // load, so a banner scheduled for +60s legitimately never runs, exactly as
  // it would not appear in a screenshot taken at load.
  var __timers = [], __tseq = 0, __now = 0, __tfired = 0, __tdropped = 0;
  // ★ A timer cleared from INSIDE its own callback must stay cleared. The
  // dispatched timer is off the queue while it runs, so __clear scans a queue
  // it is not in and the rescheduled copy comes back — an interval that calls
  // clearInterval(self) then ran to the budget instead of stopping. Cleared
  // ids are remembered, not just removed.
  var __cleared = Object.create(null);
  function __schedule(fn, ms, repeat) {
    if (typeof fn !== 'function') return 0;
    var d = Number(ms); if (!isFinite(d) || d < 0) d = 0;
    var id = ++__tseq;
    __timers.push({ id: id, at: __now + d, seq: id, fn: fn, every: repeat ? d : null });
    return id;
  }
  globalThis.setTimeout = function (fn, ms) { return __schedule(fn, ms, false); };
  globalThis.setInterval = function (fn, ms) { return __schedule(fn, ms, true); };
  function __clear(id) {
    __cleared[id] = true;
    for (var i = __timers.length - 1; i >= 0; i--) if (__timers[i].id === id) __timers.splice(i, 1);
  }
  globalThis.clearTimeout = __clear;
  globalThis.clearInterval = __clear;
  globalThis.requestAnimationFrame = function (fn) { return __schedule(fn, 16, false); };
  globalThis.cancelAnimationFrame = __clear;
  globalThis.queueMicrotask = function (fn) { Promise.resolve().then(fn); };
  globalThis.requestIdleCallback = function (fn) {
    return __schedule(function () { fn({ didTimeout: false, timeRemaining: function () { return 50; } }); }, 1, false);
  };
  globalThis.cancelIdleCallback = __clear;

  globalThis.__drainTimers = function (maxCallbacks, horizonMs) {
    while (__timers.length) {
      if (__tfired >= maxCallbacks) break;
      __timers.sort(function (a, b) { return a.at - b.at || a.seq - b.seq; });
      var t = __timers[0];
      if (t.at > horizonMs) break;          // beyond the snapshot horizon
      __timers.shift();
      if (__cleared[t.id]) continue;
      __now = t.at;
      __tfired++;
      try { t.fn(); } catch (e) {}
      __deliverMutations(4); __upgradePending();
      if (t.every !== null && !__cleared[t.id]) {
        // A repeating timer is rescheduled, and the same bounds apply to it.
        __timers.push({ id: t.id, at: __now + Math.max(t.every, 1), seq: ++__tseq, fn: t.fn, every: t.every });
      }
    }
    __tdropped = __timers.length;
    return __tfired;
  };
  globalThis.__timerStats = function () { return [__tfired, __tdropped]; };

  // ★ THE ONE OBSERVER A CONVERTER CAN SERVE HONESTLY.
  //
  // ResizeObserver and IntersectionObserver report LAYOUT, which this
  // converter does not perform, so anything it delivered would be invented —
  // hence accept-and-never-deliver above. MutationObserver reports DOM
  // MUTATIONS, and those are real here: every mutation goes through a host
  // function, so the record log is exact rather than inferred. It is
  // therefore implemented for real, not stubbed.
  //
  // The one place fidelity is deliberately lower than a browser's: records
  // carry the mutated node and attribute name, but not addedNodes /
  // removedNodes / oldValue, which the arena does not retain. Scripts that
  // branch on those get empty lists rather than wrong ones.
  var __mos = [];
  globalThis.__moDelivered = 0;
  function MutationObserver(cb) {
    var self = this;
    self._cb = cb; self._targets = []; self._queue = [];
    __mos.push(self);
    self.observe = function (target, opts) {
      opts = opts || {};
      // `document` is a common target and is not itself a node object here;
      // its documentElement stands in, which observes the same subtree.
      var h = (target && target.__h !== undefined) ? target.__h
            : (target && target.documentElement ? target.documentElement.__h
                                                : undefined);
      self._targets.push({
        h: h,
        subtree: !!opts.subtree,
        childList: !!opts.childList,
        attributes: !!opts.attributes || !!opts.attributeFilter,
        characterData: !!opts.characterData,
        filter: opts.attributeFilter || null,
      });
    };
    self.unobserve = function () {};
    self.disconnect = function () { self._targets = []; self._queue = []; };
    self.takeRecords = function () { var q = self._queue; self._queue = []; return q; };
    self._match = function (r) {
      for (var i = 0; i < self._targets.length; i++) {
        var t = self._targets[i];
        if (t.h === undefined) continue;
        if (r.type === 'childList' && !t.childList) continue;
        if (r.type === 'attributes' && !t.attributes) continue;
        if (r.type === 'characterData' && !t.characterData) continue;
        if (r.type === 'attributes' && t.filter &&
            t.filter.indexOf(r.attributeName) < 0) continue;
        if (t.h === r.__th) return true;
        if (t.subtree && __is_ancestor(t.h, r.__th)) return true;
      }
      return false;
    };
  }
  globalThis.MutationObserver = MutationObserver;
  globalThis.WebKitMutationObserver = MutationObserver;

  // Deliver pending records to whoever asked for them. Bounded rounds,
  // because a callback that mutates feeds itself more records; a browser is
  // bounded by the microtask checkpoint rather than a count, but an
  // unbounded loop here would be a hang, not a fidelity win.
  globalThis.__deliverMutations = function (rounds) {
    for (var n = 0; n < (rounds || 8); n++) {
      var recs = __take_mutations();
      if (!recs.length) return;
      var any = false;
      for (var i = 0; i < __mos.length; i++) {
        var mo = __mos[i];
        if (!mo._targets.length) continue;
        for (var j = 0; j < recs.length; j++) {
          if (mo._match(recs[j])) { mo._queue.push(recs[j]); any = true; }
        }
      }
      if (!any) return;
      for (var i = 0; i < __mos.length; i++) {
        var mo = __mos[i];
        if (!mo._queue.length) continue;
        var q = mo._queue; mo._queue = [];
        globalThis.__moDelivered += q.length;
        try { mo._cb(q, mo); } catch (e) {}
      }
    }
  };

  var __tick = 0;
  globalThis.performance = {
    // Virtual time plus a monotonic tick: reflects the clock the timers use,
    // and still strictly increases within a single synchronous run.
    now: function () { return __now + (++__tick) / 1000; },
    timeOrigin: 0,
    mark: function () {}, measure: function () {},
    getEntriesByName: function () { return []; },
    getEntriesByType: function () { return []; }
  };

  // ★ Storage that is ALWAYS EMPTY and NEVER PERSISTS is not a capability:
  // no data enters from anywhere, none survives the conversion, and every
  // reader of the artifact gets the identical result. What a converter must
  // not do is manufacture state — so this starts empty every time, which is
  // also what makes it deterministic.
  function Store() {
    var m = Object.create(null);
    return {
      getItem: function (k) { k = String(k); return k in m ? m[k] : null; },
      setItem: function (k, v) { m[String(k)] = String(v); },
      removeItem: function (k) { delete m[String(k)]; },
      clear: function () { m = Object.create(null); },
      key: function (i) { var ks = Object.keys(m); return i < ks.length ? ks[i] : null; },
      get length() { return Object.keys(m).length; }
    };
  }
  globalThis.localStorage = Store();
  globalThis.sessionStorage = Store();

  // Same reasoning: "there are no cookies" rather than a cookie jar. Writes
  // are accepted and dropped so a script that sets one does not throw.
  try {
    Object.defineProperty(document, 'cookie', {
      get: function () { return ''; }, set: function () {}, configurable: true
    });
  } catch (e) {}

  // `self` is how bundles detect a global context; without it a worker-aware
  // build reaches for it before doing anything else. globalThis is the whole
  // definition.
  globalThis.self = globalThis;
  globalThis.globalThis = globalThis;

  // URLSearchParams, implemented over an ordered pair list because order and
  // duplicates are both observable: getAll and toString must preserve them.
  function __decode(s) {
    try { return decodeURIComponent(String(s).replace(/\+/g, ' ')); }
    catch (e) { return String(s); }
  }
  function __encode(s) {
    try { return encodeURIComponent(String(s)); } catch (e) { return String(s); }
  }
  globalThis.URLSearchParams = function (init) {
    var pairs = [];
    if (typeof init === 'string') {
      var q = init.charAt(0) === '?' ? init.slice(1) : init;
      if (q.length) {
        var parts = q.split('&');
        for (var i = 0; i < parts.length; i++) {
          if (!parts[i].length) continue;
          var eq = parts[i].indexOf('=');
          if (eq < 0) pairs.push([__decode(parts[i]), '']);
          else pairs.push([__decode(parts[i].slice(0, eq)), __decode(parts[i].slice(eq + 1))]);
        }
      }
    } else if (init && typeof init === 'object') {
      if (Array.isArray(init)) {
        for (var j = 0; j < init.length; j++) pairs.push([String(init[j][0]), String(init[j][1])]);
      } else {
        for (var k in init) if (Object.prototype.hasOwnProperty.call(init, k)) {
          pairs.push([String(k), String(init[k])]);
        }
      }
    }
    this._p = pairs;
    this.get = function (n) {
      n = String(n);
      for (var i = 0; i < this._p.length; i++) if (this._p[i][0] === n) return this._p[i][1];
      return null;
    };
    this.getAll = function (n) {
      n = String(n); var out = [];
      for (var i = 0; i < this._p.length; i++) if (this._p[i][0] === n) out.push(this._p[i][1]);
      return out;
    };
    this.has = function (n) { return this.get(n) !== null; };
    this.append = function (n, v) { this._p.push([String(n), String(v)]); };
    this.set = function (n, v) {
      n = String(n); var done = false, out = [];
      for (var i = 0; i < this._p.length; i++) {
        if (this._p[i][0] !== n) { out.push(this._p[i]); continue; }
        if (!done) { out.push([n, String(v)]); done = true; }
      }
      if (!done) out.push([n, String(v)]);
      this._p = out;
    };
    this['delete'] = function (n) {
      n = String(n); var out = [];
      for (var i = 0; i < this._p.length; i++) if (this._p[i][0] !== n) out.push(this._p[i]);
      this._p = out;
    };
    this.forEach = function (fn, thisArg) {
      for (var i = 0; i < this._p.length; i++) fn.call(thisArg, this._p[i][1], this._p[i][0], this);
    };
    this.keys = function () { return this._p.map(function (x) { return x[0]; }); };
    this.values = function () { return this._p.map(function (x) { return x[1]; }); };
    this.entries = function () { return this._p.map(function (x) { return [x[0], x[1]]; }); };
    this.toString = function () {
      var out = [];
      for (var i = 0; i < this._p.length; i++) {
        out.push(__encode(this._p[i][0]) + '=' + __encode(this._p[i][1]));
      }
      return out.join('&');
    };
    Object.defineProperty(this, 'size', { get: function () { return this._p.length; },
                                          configurable: true });
  };

  function mkurl(parts) {
    if (!parts) return null;
    var u = {};
    for (var k in parts) u[k] = parts[k];
    u.toString = function () { return this.href; };
    // A real one now, built from this URL's own query string.
    u.searchParams = new URLSearchParams(u.search || '');
    return u;
  }
  globalThis.URL = function (href, base) {
    var p = __parse_url(String(href), base === undefined ? undefined : String(base));
    if (!p) throw new TypeError('Invalid URL: ' + href);
    return mkurl(p);
  };

  if (typeof __doc_url === 'string' && __doc_url) {
    var loc = mkurl(__parse_url(__doc_url));
    if (loc) {
      // Read-only: assigning location.href is navigation, which a converter
      // must never perform.
      loc.assign = function () {}; loc.replace = function () {}; loc.reload = function () {};
      globalThis.location = loc;
      if (typeof window !== 'undefined') { window.location = loc; }
      // ★ Reads the CURRENT location, not the one captured here. history
      // replaces the location object, and a getter closed over `loc` would
      // keep reporting the URL the document was fetched from after the page
      // had rewritten it.
      // `document.location` IS `location` in a browser. A getter, not a
      // copy: history replaces the location object, and a copy taken here
      // would go stale exactly when a page rewrites its own URL.
      try { Object.defineProperty(document, 'location',
        { get: function(){ return globalThis.location; },
          set: function(){},            // assigning it is navigation
          configurable: true }); } catch (e) {}
      try { Object.defineProperty(document, 'URL',
        { get: function(){ return globalThis.location ? globalThis.location.href : ''; },
          configurable: true }); } catch (e) {}
    }
  }

  // ── history ─────────────────────────────────────────────────────────
  //
  // The corpus asks for replaceState (66 references), pushState (40), state
  // (35), scrollRestoration (21) and go (10). All of it is SAME-DOCUMENT
  // navigation, which is the one kind of navigation a converter can honestly
  // perform: no request, no new document, just a URL and a state object the
  // page manages itself.
  //
  // ★ IT REALLY DOES MOVE `location`. Accepting a pushState and then
  // reporting the old URL would be worse than refusing: a script that pushes
  // and then builds links or fetch paths from location would compute them
  // against a URL its own code believes it has left. The artifact records
  // where the page put itself.
  //
  // What it will NOT do is leave the document. back() past our first entry,
  // forward() past the last, and go(0) are all real navigations or reloads;
  // they are refused and COUNTED rather than faked.
  (function () {
    var start = globalThis.location ? globalThis.location.href
              : (typeof __doc_url === 'string' ? __doc_url : '');
    var entries = [ { url: start, state: null } ];
    var idx = 0;
    globalThis.__histWrites = 0;
    globalThis.__histRefused = 0;

    function apply() {
      var p = __parse_url(entries[idx].url);
      if (!p) return;
      var nu = mkurl(p);
      nu.assign = function () {}; nu.replace = function () {}; nu.reload = function () {};
      globalThis.location = nu;
    }
    function resolve(url) {
      if (url === undefined || url === null || url === '') return entries[idx].url;
      var p = __parse_url(String(url), entries[idx].url);
      return p ? p.href : entries[idx].url;
    }
    function sameOrigin(href) {
      var a = __parse_url(href), b = __parse_url(entries[idx].url);
      return !!(a && b) && a.origin === b.origin;
    }
    function write(state, url, replace) {
      var target = resolve(url);
      if (!sameOrigin(target)) {
        // A browser throws here; cross-origin history writing is not a thing.
        throw new Error("SecurityError: history " +
          (replace ? "replaceState" : "pushState") + " to a different origin");
      }
      if (replace) { entries[idx] = { url: target, state: state }; }
      else { entries = entries.slice(0, idx + 1);
             entries.push({ url: target, state: state }); idx = entries.length - 1; }
      globalThis.__histWrites++;
      apply();
    }
    globalThis.history = {
      get length() { return entries.length; },
      get state() { return entries[idx].state; },
      scrollRestoration: 'auto',
      pushState: function (state, title, url) { write(state, url, false); },
      replaceState: function (state, title, url) { write(state, url, true); },
      back: function () { this.go(-1); },
      forward: function () { this.go(1); },
      go: function (n) {
        n = (n === undefined) ? 0 : (n | 0);
        var t = idx + n;
        // go(0) is a reload; anything outside our own entries leaves the
        // document. Both are navigations this converter must not perform.
        if (n === 0 || t < 0 || t >= entries.length) { globalThis.__histRefused++; return; }
        idx = t;
        apply();
        // Going back to an entry WE pushed is genuinely same-document, so
        // popstate here is real rather than invented.
        var st = entries[idx].state;
        __fire('popstate', { state: st });
        if (typeof globalThis.onpopstate === 'function') {
          try { globalThis.onpopstate({ type: 'popstate', state: st }); globalThis.__fired++; }
          catch (e) {}
        }
      },
    };
  })();

  // ★ GEOMETRY OBSERVERS ACCEPT AND NEVER DELIVER.
  //
  // ResizeObserver and IntersectionObserver report layout, and this converter
  // performs no layout — so any box it handed a callback would be fiction.
  // That is not a harmless fiction either: a script told an element is 0x0
  // routinely collapses or hides it, which would make the conversion WORSE
  // than not firing at all. Real implementations deliver one initial
  // observation on observe(); we deliberately do not.
  //
  // Registrations are counted so the cost of that choice is visible rather
  // than assumed — see `observers_registered` in the report.
  globalThis.__observed = 0;
  function GeometryObserver(cb) {
    this._cb = cb;
    this.observe = function () { globalThis.__observed++; };
    this.unobserve = function () {};
    this.disconnect = function () {};
    this.takeRecords = function () { return []; };
  }
  globalThis.ResizeObserver = GeometryObserver;
  globalThis.IntersectionObserver = GeometryObserver;
  globalThis.PerformanceObserver = GeometryObserver;

  // Fixed identity: the converter's, not the reader's. A real user agent
  // string would be host state leaking into the artifact.
  // Same nominal viewport as the element metrics, for consistency.
  globalThis.innerWidth = 1280; globalThis.innerHeight = 600;
  globalThis.devicePixelRatio = 1;
  globalThis.scrollX = 0; globalThis.scrollY = 0;
  if (typeof window !== 'undefined') {
    window.innerWidth = 1280; window.innerHeight = 600;
    window.devicePixelRatio = 1; window.scrollX = 0; window.scrollY = 0;
    window.scrollTo = function () {}; window.matchMedia = function (q) {
      return { matches: false, media: String(q), addListener: function(){}, removeListener: function(){},
               addEventListener: function(){}, removeEventListener: function(){} };
    };
  }

  // fetch over one synchronous native call, handed back as a settled promise.
  // Response is the subset pages actually use; anything else is absent and
  // will be named by the missing-API report rather than faked.
  globalThis.fetch = function (input, init) {
    var url = (input && typeof input === 'object' && input.url) ? input.url : String(input);
    var r = __fetch_sync(url, init && init.method ? String(init.method) : 'GET');
    if (!r) return Promise.reject(new TypeError('Failed to fetch: ' + url));
    var resp = {
      ok: true, status: r.status, statusText: 'OK', url: r.url,
      redirected: false, type: 'basic', bodyUsed: false,
      headers: { get: function () { return null; }, has: function () { return false; },
                 forEach: function () {} },
      text: function () { return Promise.resolve(r.body); },
      json: function () {
        try { return Promise.resolve(JSON.parse(r.body)); }
        catch (e) { return Promise.reject(e); }
      },
      clone: function () { return resp; }
    };
    return Promise.resolve(resp);
  };
  // ── Event ───────────────────────────────────────────────────────────
  //
  // The corpus constructs events 296 times and DISPATCHES them 383, so a
  // constructor without a dispatch would be a stub of the smaller half.
  //
  // ★ A PAGE DISPATCHING TO ITSELF IS HONESTLY SERVABLE. Element listeners
  // used to be accepted and dropped, on the grounds that nothing headless
  // delivers a click — true of USER events, false of the page's own
  // dispatchEvent, which is just code the page runs. The line is between
  // events we would have to INVENT (a click, a scroll, a resize) and events
  // the page itself raises. The first are still never fired; the second now
  // are, for real.
  function __mkEvent(type, init, detail) {
    init = init || {};
    this.type = String(type);
    this.bubbles = !!init.bubbles;
    this.cancelable = !!init.cancelable;
    this.composed = !!init.composed;
    this.detail = (detail !== undefined) ? detail
                : (init.detail !== undefined ? init.detail : null);
    this.defaultPrevented = false;
    this.target = null; this.currentTarget = null; this.srcElement = null;
    this.eventPhase = 0;
    this.__stop = false;
    this.timeStamp = (globalThis.performance && performance.now) ? performance.now() : 0;
    this.isTrusted = false;   // a script-made event never is
    this.preventDefault = function () { if (this.cancelable) this.defaultPrevented = true; };
    this.stopPropagation = function () { this.__stop = true; };
    this.stopImmediatePropagation = function () { this.__stop = true; };
    this.composedPath = function () { return this.target ? [this.target] : []; };
    this.initEvent = function (t, b, c) {
      this.type = String(t); this.bubbles = !!b; this.cancelable = !!c;
    };
    this.initCustomEvent = function (t, b, c, d) {
      this.initEvent(t, b, c); this.detail = d;
    };
  }
  function Event(type, init) { __mkEvent.call(this, type, init); }
  function CustomEvent(type, init) { __mkEvent.call(this, type, init); }
  __defIfaceLike('Event', Event);
  __defIfaceLike('CustomEvent', CustomEvent);
  // Aliases pages construct by name; all carry the same shape here, and the
  // report will name any that turn out to need more.
  ['MouseEvent', 'KeyboardEvent', 'FocusEvent', 'InputEvent', 'PointerEvent',
   'TouchEvent', 'UIEvent', 'PopStateEvent', 'MessageEvent', 'HashChangeEvent',
   'ErrorEvent', 'ProgressEvent', 'SubmitEvent', 'WheelEvent', 'DragEvent',
   'AnimationEvent', 'TransitionEvent', 'StorageEvent', 'CloseEvent'
  ].forEach(function (n) {
    function E(type, init) { __mkEvent.call(this, type, init); }
    __defIfaceLike(n, E);
  });
  // The legacy path: createEvent + initEvent, 91 and 29 references.
  document.createEvent = function (kind) {
    var e = new Event('', {});
    e.__legacyKind = String(kind);
    return e;
  };
  // document and window dispatch to their own listeners. `window` is the
  // global here, so one implementation serves both.
  globalThis.__docDispatches = 0;
  document.dispatchEvent = function (ev) {
    globalThis.__docDispatches++;
    __fireExisting(ev);
    return !ev.defaultPrevented;
  };
  globalThis.dispatchEvent = document.dispatchEvent;

  // ── HTMLElement and custom elements ─────────────────────────────────
  //
  // Two different needs behind one name. Most corpus uses are
  // `x instanceof HTMLElement` — a type TEST. One is `class X extends
  // HTMLElement` — a base CONSTRUCTOR. A stub satisfies neither.
  //
  // instanceof is answered with Symbol.hasInstance rather than by plumbing a
  // prototype chain onto the node wrappers: the wrappers are host proxies, so
  // "is this an element" is a question the arena can answer directly and
  // exactly (nodeType), where a faked prototype chain would be a second,
  // divergent notion of what counts as an element.
  var __upgradeTarget = null;
  function HTMLElement() {
    // A browser's HTMLElement constructor returns the element being upgraded,
    // which is how `super()` inside a custom element's constructor binds
    // `this` to the real element. Same mechanism here.
    if (__upgradeTarget) { return __upgradeTarget; }
    throw new TypeError('Illegal constructor');
  }
  function __isNode(v, type) {
    if (!v || typeof v !== 'object') return false;
    try { return type ? v.nodeType === type : typeof v.nodeType === 'number'; }
    catch (e) { return false; }
  }
  function __defIfaceLike(name, fn) { globalThis[name] = fn; }
  function __defIface(name, fn, type) {
    try {
      Object.defineProperty(fn, Symbol.hasInstance,
        { value: function (v) { return __isNode(v, type); } });
    } catch (e) {}
    globalThis[name] = fn;
  }
  __defIface('HTMLElement', HTMLElement, 1);
  __defIface('Element', function Element() {}, 1);
  __defIface('Node', function Node() {}, 0);
  __defIface('HTMLDivElement', function HTMLDivElement() {}, 1);
  __defIface('HTMLAnchorElement', function HTMLAnchorElement() {}, 1);
  __defIface('HTMLInputElement', function HTMLInputElement() {}, 1);
  __defIface('HTMLImageElement', function HTMLImageElement() {}, 1);
  __defIface('DocumentFragment', function DocumentFragment() {}, 11);

  // An element is upgraded only once it is CONNECTED, as the spec says —
  // connectedCallback that fires on a detached node would be a lie about
  // where the element is. Reachability is a question the tree can now answer.
  function __connected(el) {
    var p = el, n = 0;
    while (p && n++ < 1000) { if (p.nodeType === 9) return true; p = p.parentNode; }
    return false;
  }

  var __ceRegistry = {};
  globalThis.__ceUpgrades = 0;
  function __upgradeOne(el, ctor) {
    if (el.__ce) return;
    el.__ce = true;
    __upgradeTarget = el;
    try { Reflect.construct(ctor, [], ctor); }
    catch (e) { __upgradeTarget = null; return; }
    finally { __upgradeTarget = null; }
    // Copy the class's own methods onto the element. A real custom element IS
    // an instance of the class; ours is a host node the constructor ran
    // against, so without this the page could not call the methods its own
    // class defines. `this` inside them is the element either way.
    try {
      var proto = ctor.prototype;
      Object.getOwnPropertyNames(proto).forEach(function (k) {
        if (k === 'constructor' || k in el) return;
        try { el[k] = proto[k]; } catch (e) {}
      });
    } catch (e) {}
    globalThis.__ceUpgrades++;
    // observedAttributes: report the attributes already present, which is the
    // initial state a browser reports on upgrade.
    try {
      var obs = ctor.observedAttributes;
      if (obs && proto.attributeChangedCallback) {
        for (var i = 0; i < obs.length; i++) {
          var v = el.getAttribute(obs[i]);
          if (v !== null) proto.attributeChangedCallback.call(el, obs[i], null, v);
        }
      }
    } catch (e) {}
    try { if (proto.connectedCallback) proto.connectedCallback.call(el); } catch (e) {}
  }
  /// Upgrade everything registered that is connected and not yet upgraded.
  /// Called wherever mutations are delivered, so elements added by a script
  /// or a timer are upgraded too, not only those present at define() time.
  globalThis.__upgradePending = function () {
    for (var name in __ceRegistry) {
      var els = document.getElementsByTagName(name);
      for (var i = 0; i < els.length; i++) {
        var el = els[i];
        if (!el.__ce && __connected(el)) __upgradeOne(el, __ceRegistry[name]);
      }
    }
  };
  globalThis.customElements = {
    define: function (name, ctor) {
      name = String(name).toLowerCase();
      if (__ceRegistry[name]) throw new Error('already defined: ' + name);
      __ceRegistry[name] = ctor;
      globalThis.__upgradePending();
    },
    get: function (name) { return __ceRegistry[String(name).toLowerCase()]; },
    getName: function (c) {
      for (var n in __ceRegistry) if (__ceRegistry[n] === c) return n;
      return null;
    },
    upgrade: function () { globalThis.__upgradePending(); },
    whenDefined: function (name) {
      return __ceRegistry[String(name).toLowerCase()]
        ? Promise.resolve(__ceRegistry[String(name).toLowerCase()])
        : new Promise(function () {});   // never settles: it never will here
    },
  };

  // ── XMLHttpRequest ──────────────────────────────────────────────────
  //
  // Over the same page-network seam as fetch, so the same policy applies
  // without restating it: same-origin GET only, everything else refused and
  // counted. XHR is the OLDER telemetry transport, so routing it anywhere
  // else would have quietly reopened the hole fetch closed.
  //
  // ★ THE HARD PART IS NOT THE NETWORK, IT IS THE ORDERING. The seam is
  // synchronous; XHR as pages use it is not. Delivering the callbacks inline
  // from send() would run onload BEFORE the statement after send(), which is
  // the opposite of what every async XHR caller is written against. So the
  // request is performed at send() time (the seam gives no choice) but the
  // callbacks are posted to the timer queue, preserving "send returns first".
  // Synchronous XHR — open(m, u, false) — delivers inline, correctly.
  //
  // A refusal is reported as status 0 with an `error` event, which is what a
  // browser reports for a blocked or failed request. That is an honest
  // mapping rather than a special one: pages already have a code path for a
  // network error, and inventing a fake 200 would be worse than the refusal.
  globalThis.__xhrSends = 0;
  function XMLHttpRequest() {
    var self = this;
    self.readyState = 0; self.status = 0; self.statusText = '';
    self.responseText = ''; self.response = ''; self.responseType = '';
    self.responseURL = ''; self.withCredentials = false; self.timeout = 0;
    self.upload = { addEventListener: function () {}, removeEventListener: function () {} };
    var _m = 'GET', _u = '', _async = true, _aborted = false, _handlers = {};

    self.open = function (method, url, async) {
      _m = String(method || 'GET').toUpperCase();
      _u = String(url);
      _async = (async === undefined) ? true : !!async;
      _aborted = false;
      self.readyState = 1; _emit('readystatechange');
    };
    self.setRequestHeader = function () {};
    self.overrideMimeType = function () {};
    // Response headers are not retained by the seam. Reporting none is
    // accurate; synthesising plausible ones would not be.
    self.getResponseHeader = function () { return null; };
    self.getAllResponseHeaders = function () { return ''; };
    self.abort = function () { _aborted = true; self.readyState = 0; };
    self.addEventListener = function (t, fn) {
      (_handlers[t] = _handlers[t] || []).push(fn);
    };
    self.removeEventListener = function (t, fn) {
      var h = _handlers[t]; if (!h) return;
      var i = h.indexOf(fn); if (i >= 0) h.splice(i, 1);
    };
    function _emit(type) {
      var ev = { type: type, target: self, currentTarget: self,
                 lengthComputable: false, loaded: 0, total: 0 };
      var on = self['on' + type];
      if (typeof on === 'function') { try { on.call(self, ev); } catch (e) {} }
      var h = _handlers[type];
      if (h) for (var i = 0; i < h.length; i++) {
        try { h[i].call(self, ev); } catch (e) {}
      }
    }
    function _settle() {
      if (_aborted) return;
      var r = __fetch_sync(_u, _m);
      if (r) {
        self.status = r.status; self.statusText = 'OK';
        self.responseText = r.body; self.responseURL = r.url;
        if (self.responseType === 'json') {
          try { self.response = JSON.parse(r.body); } catch (e) { self.response = null; }
        } else {
          self.response = r.body;
        }
      } else {
        // Refused or failed: a browser's network-error shape.
        self.status = 0; self.statusText = '';
        self.responseText = ''; self.response = null;
      }
      self.readyState = 4;
      _emit('readystatechange');
      _emit(self.status === 0 ? 'error' : 'load');
      _emit('loadend');
    }
    self.send = function () {
      globalThis.__xhrSends++;
      if (_async) { setTimeout(_settle, 0); } else { _settle(); }
    };
  }
  XMLHttpRequest.UNSENT = 0; XMLHttpRequest.OPENED = 1;
  XMLHttpRequest.HEADERS_RECEIVED = 2; XMLHttpRequest.LOADING = 3;
  XMLHttpRequest.DONE = 4;
  globalThis.XMLHttpRequest = XMLHttpRequest;

  // A beacon is telemetry by definition — there is no response to use. It
  // reports success so a page's teardown path does not break, and sends
  // nothing.
  globalThis.__beacons = 0;
  globalThis.Request = function (url, init) { this.url = String(url); this.init = init; };
  globalThis.Headers = function () {
    this.get = function () { return null; }; this.has = function () { return false; };
    this.set = function () {}; this.append = function () {};
  };

  globalThis.navigator = {
    sendBeacon: function () { globalThis.__beacons++; return true; },
    userAgent: 'atrium-navigator-prerender/0.1',
    language: 'en', languages: ['en'], onLine: true, cookieEnabled: false
  };
})();
"#;

/// Installed before any page script.
///
/// ★ The listener list lives in JS, not in Rust. Holding JsFunction values in
/// a thread-local would mean rooting them outside the Context and tracing them
/// through the engine's GC by hand — exactly the coupling §11.8 identifies as
/// the hard part of swapping engines. Keeping them in JS leaves the GC's job
/// with the GC.
const BOOTSTRAP: &str = r#"
(function () {
  var L = [];
  globalThis.__fired = 0;
  function add(t, f) { if (typeof f === 'function') L.push([String(t), f]); }
  function remove(t, f) {
    for (var i = L.length - 1; i >= 0; i--) if (L[i][0] === String(t) && L[i][1] === f) L.splice(i, 1);
  }
  globalThis.__dispatchRan = 0;
  globalThis.__fire = function (type, extra) {
    var ev = { type: type, target: document, currentTarget: document,
               preventDefault: function () {}, stopPropagation: function () {} };
    if (extra) for (var k in extra) ev[k] = extra[k];
    for (var i = 0; i < L.length; i++) {
      if (L[i][0] !== type) continue;
      try { L[i][1].call(document, ev); globalThis.__fired++; } catch (e) {}
    }
  };
  /// Deliver an event object the page already built, rather than
  /// manufacturing a second one that merely looks like it: listeners compare
  /// `e.target`, read `e.detail`, and call `e.preventDefault()`, and all of
  /// that has to land on the object the page is holding.
  globalThis.__fireExisting = function (ev) {
    var type = ev && ev.type;
    for (var i = 0; i < L.length; i++) {
      if (L[i][0] !== type) continue;
      if (ev.__stop) break;
      try { ev.currentTarget = document; } catch (e) {}
      // Counted as a DISPATCH listener as well as a lifecycle one: these
      // run because the page dispatched, and reporting 0 while they ran
      // would misdescribe the mechanism as inert.
      try { L[i][1].call(document, ev); globalThis.__fired++;
            globalThis.__dispatchRan++; } catch (e) {}
    }
    var on = globalThis['on' + type];
    if (typeof on === 'function') {
      try { on.call(document, ev); globalThis.__fired++;
            globalThis.__dispatchRan++; } catch (e) {}
    }
  };
  document.addEventListener = add;
  document.removeEventListener = remove;
  if (typeof window !== 'undefined') {
    window.addEventListener = add;
    window.removeEventListener = remove;
  }
  document.readyState = 'loading';
})();
"#;

/// Run after every script, because that is where the content usually is.
///
/// ★ `document.addEventListener` was the most-requested API in the corpus, and
/// merely stubbing it would have been the wrong fix: real pages put their
/// content-generating work inside a DOMContentLoaded callback, so a converter
/// that registers listeners and never fires them runs every bundle and still
/// emits an empty page. The event is the point, not the registration.
const FIRE: &str = r#"
(function () {
  document.readyState = 'interactive';
  __fire('DOMContentLoaded');
  if (typeof document.onreadystatechange === 'function') {
    try { document.onreadystatechange(); __fired++; } catch (e) {}
  }
  document.readyState = 'complete';
  __fire('load');
  if (typeof window !== 'undefined' && typeof window.onload === 'function') {
    try { window.onload({ type: 'load' }); __fired++; } catch (e) {}
  }
})();
"#;

impl ScriptEngine for BoaEngine {
    fn name(&self) -> &'static str { "boa" }

    fn set_module_root(&mut self, root: &std::path::Path) {
        self.module_root = Some(root.to_path_buf());
    }

    fn set_base_url(&mut self, url: Option<&str>) {
        self.base_url = url.map(str::to_string);
    }

    fn run(&mut self, dom: &mut Dom, scripts: &[ScriptSource]) -> RunReport {
        // ★ BEFORE any wrapper is built. Clearing it later threw away the
        // `document.body` wrapper that had just been cached, so the next
        // lookup minted a second object and `x.parentNode === document.body`
        // was false — the identity bug this cache exists to prevent,
        // reintroduced by the reset that was meant to keep it clean.
        NODE_CACHE.with(|c| c.borrow_mut().clear());
        ELISTENERS.with(|m| m.borrow_mut().clear());
        DISPATCH.with(|c| *c.borrow_mut() = (0, 0));
        DOC_WRITE.with(|c| *c.borrow_mut() = (0, 0));
        WRITE_POS.with(|m| m.borrow_mut().clear());
        DOM.with(|d| *d.borrow_mut() = std::mem::take(dom));
        let hooks = std::rc::Rc::new(FixedHooks);
        let mut ctx = match self.module_root.as_ref()
            .and_then(|r| boa_engine::module::SimpleModuleLoader::new(r).ok())
        {
            Some(loader) => Context::builder()
                .module_loader(std::rc::Rc::new(loader))
                .host_hooks(hooks.clone())
                .build()
                .unwrap_or_default(),
            None => Context::builder().host_hooks(hooks).build().unwrap_or_default(),
        };

        let body = with(|d| d.by_tag("body").first().copied()).unwrap_or(0);
        let body_v = node_obj(body, &mut ctx);
        // `document.head` — named by the corpus the moment tree semantics let
        // MediaWiki's ResourceLoader run far enough to call
        // `document.head.appendChild(script)`. Twelve documents, one property.
        let head = with(|d| d.by_tag("head").first().copied()).unwrap_or(0);
        let head_v = node_obj(head, &mut ctx);
        let doc_el = with(|d| d.by_tag("html").first().copied()).unwrap_or(0);
        let doc_el_v = node_obj(doc_el, &mut ctx);
        let doc = ObjectInitializer::new(&mut ctx)
            .function(NativeFunction::from_fn_ptr(get_element_by_id), js_string!("getElementById"), 1)
            .function(NativeFunction::from_fn_ptr(create_element), js_string!("createElement"), 1)
            .function(NativeFunction::from_fn_ptr(create_text_node), js_string!("createTextNode"), 1)
            .function(NativeFunction::from_fn_ptr(create_fragment), js_string!("createDocumentFragment"), 0)
            .function(NativeFunction::from_fn_ptr(by_name), js_string!("getElementsByName"), 1)
            .property(js_string!("namespaceURI"),
                js_string!("http://www.w3.org/1999/xhtml"), Attribute::all())
            .function(NativeFunction::from_fn_ptr(query_all), js_string!("querySelectorAll"), 1)
            .function(NativeFunction::from_fn_ptr(query_first), js_string!("querySelector"), 1)
            .function(NativeFunction::from_fn_ptr(by_class), js_string!("getElementsByClassName"), 1)
            .function(NativeFunction::from_fn_ptr(by_tag_name), js_string!("getElementsByTagName"), 1)
            .property(js_string!("body"), body_v, Attribute::all())
            .property(js_string!("documentElement"), doc_el_v, Attribute::all())
            .property(js_string!("head"), head_v, Attribute::all())
            // ★ THE HONEST REFERRER IS THE EMPTY STRING. No navigation
            // happened: the converter fetched this document directly, which
            // is exactly the case a browser also reports as "". Inventing a
            // plausible referring page would be fabricating provenance, and
            // the corpus wants it mostly to put in analytics payloads.
            .property(js_string!("referrer"), js_string!(""), Attribute::all())
            .function(NativeFunction::from_fn_ptr(document_write), js_string!("write"), 1)
            .function(NativeFunction::from_fn_ptr(document_write), js_string!("writeln"), 1)
            .property(js_string!("__h"), 0.0, Attribute::all())
            .build();
        {
            let getter = NativeFunction::from_fn_ptr(current_script).to_js_function(ctx.realm());
            let desc = boa_engine::property::PropertyDescriptor::builder()
                .get(getter).enumerable(true).configurable(true).build();
            let _ = doc.define_property_or_throw(js_string!("currentScript"), desc, &mut ctx);
        }
        // The document is a node too: nodeType 9, real children, and the
        // same navigation every other node has.
        {
            let imp = ObjectInitializer::new(&mut ctx)
                .function(NativeFunction::from_fn_ptr(create_html_document),
                          js_string!("createHTMLDocument"), 1)
                .build();
            let _ = doc.set(js_string!("implementation"), imp, false, &mut ctx);
        }
        install_tree(&doc, &mut ctx);
        let doc_v = probed(doc.clone(), "document", &mut ctx);
        let _ = ctx.register_global_property(js_string!("document"), doc_v, Attribute::all());

        // `window` was the single most common missing binding in the first
        // corpus run, so the instrument's own report earned it a place. It is
        // the global object with `document` hung off it — enough for the
        // `window.document` / `window.onload` shapes real pages use, without
        // pretending to be a browser.
        // ★ ONE document object, not two. This used to build a SECOND proxy
        // over the same node, so `window.document === document` was FALSE.
        // jQuery's setDocument opens with `doc == document` where doc came
        // from `window.document` — with two wrappers that comparison fails
        // and the branch taken is the wrong one. Identity is observable.
        //
        // ★ AND `window` IS THE GLOBAL OBJECT, not a side object with
        // `document` hung off it. It used to be its own object, so
        // `window.jQuery = jQuery` stored a property nobody could reach:
        // jQuery ran to completion and then `jQuery` was still not defined,
        // because assigning to our window created no global binding. Every
        // library that publishes itself does it exactly this way.
        //
        // It stays a PROXY over the global so the missing-API probe keeps
        // working; a proxy that only traps `get` forwards writes to the
        // target, so `window.x = 1` really does define a global.
        let win_v = probed(ctx.global_object().clone(), "window", &mut ctx);
        let _ = ctx.register_global_property(js_string!("window"), win_v, Attribute::all());

        // `console` is a no-op sink. Named by the corpus report, and a script
        // that logs should not be recorded as a conversion failure.
        fn noop(_t: &JsValue, _a: &[JsValue], _c: &mut Context) -> JsResult<JsValue> {
            Ok(JsValue::undefined())
        }
        let console = ObjectInitializer::new(&mut ctx)
            .function(NativeFunction::from_fn_ptr(noop), js_string!("log"), 1)
            .function(NativeFunction::from_fn_ptr(noop), js_string!("warn"), 1)
            .function(NativeFunction::from_fn_ptr(noop), js_string!("error"), 1)
            .function(NativeFunction::from_fn_ptr(noop), js_string!("info"), 1)
            .function(NativeFunction::from_fn_ptr(noop), js_string!("debug"), 1)
            .build();
        let _ = ctx.register_global_property(js_string!("console"), console, Attribute::all());

        MISSES.with(|m| m.borrow_mut().clear());
        NULLS.with(|n| n.borrow_mut().clear());
        EVENTS.with(|e| e.borrow_mut().clear());
        LAYOUT_READS.with(|n| *n.borrow_mut() = 0);
        BASE.with(|b| *b.borrow_mut() = self.base_url.clone());
        CURRENT.with(|c| *c.borrow_mut() = None);
        let mut rep = RunReport { scripts_run: 0, scripts_failed: 0, errors: vec![],
            missing: vec![], nulls: vec![], first_error: None, cause: None, listeners_fired: 0,
            module_retries: 0, observers_registered: 0, mutation_records: 0,
            ce_upgrades: 0, history_writes: 0, history_refused: 0,
            events_dispatched: 0, event_listeners_run: 0,
            doc_writes: 0, doc_writes_refused: 0, layout_reads: 0,
            timers_fired: 0, timers_dropped: 0, page_fetches: 0, page_fetch_failures: 0,
            page_blocked: 0, blocked_hosts: vec![], beacons_suppressed: 0 };
        RECORDING.with(|r| *r.borrow_mut() = false);
        let _ = ctx.register_global_callable(js_string!("__parse_url"), 2,
            NativeFunction::from_fn_ptr(parse_url));
        let _ = ctx.register_global_callable(js_string!("__fetch_sync"), 2,
            NativeFunction::from_fn_ptr(fetch_sync));
        let _ = ctx.register_global_callable(js_string!("__take_mutations"), 0,
            NativeFunction::from_fn_ptr(take_mutations));
        let _ = ctx.register_global_callable(js_string!("__is_ancestor"), 2,
            NativeFunction::from_fn_ptr(is_ancestor));
        PAGE_NET.with(|n| *n.borrow_mut() = self.page_fetcher.take());
        PAGE_FETCHES.with(|c| *c.borrow_mut() = (0, 0));
        PAGE_BLOCKED.with(|b| { let mut b = b.borrow_mut(); b.0 = 0; b.1.clear(); });
        MUTATIONS.with(|m| m.borrow_mut().clear());
        // Wrappers belong to one run's context; carrying them across would
        // hand the next document objects from a dead realm.

        let _ = ctx.register_global_property(js_string!("__doc_url"),
            js_string!(self.base_url.clone().unwrap_or_default()), Attribute::all());
        if let Err(e) = ctx.eval(Source::from_bytes(BOOTSTRAP.as_bytes())) {
            rep.errors.push(format!("bootstrap: {e}"));
        }
        if let Err(e) = ctx.eval(Source::from_bytes(GLOBALS.as_bytes())) {
            rep.errors.push(format!("globals: {e}"));
        }
        RECORDING.with(|r| *r.borrow_mut() = true);
        for s in scripts {
            // Per spec, currentScript is null while a MODULE evaluates.
            CURRENT.with(|c| *c.borrow_mut() = if s.module { None } else { s.element });
            let mut err = if s.module {
                run_module(&mut ctx, &s.text, s.path.as_deref()).err()
            } else {
                ctx.eval(Source::from_bytes(s.text.as_bytes())).err().map(|e| e.to_string())
            };
            // ★ A classic script that fails on module-only syntax is retried
            // as a module. The corpus showed bare `export` in files a page
            // loaded as ordinary scripts (10 of them) — a converter should
            // not lose a bundle because the page mislabelled its goal type.
            if !s.module {
                if let Some(msg) = &err {
                    if msg.contains("'export'") || msg.contains("'import'") {
                        // Count the ATTEMPT, not the success: a retry that
                        // parses but then fails on an unresolved import is
                        // still module syntax we would otherwise have lost,
                        // and hiding it would understate the work done.
                        rep.module_retries += 1;
                        match run_module(&mut ctx, &s.text, s.path.as_deref()) {
                            Ok(()) => err = None,
                            Err(e2) => err = Some(e2),
                        }
                    }
                }
            }
            match err {
                None => rep.scripts_run += 1,
                Some(msg) => {
                    rep.scripts_failed += 1;
                    let short: String = msg.chars().take(200).collect();
                    if rep.first_error.is_none() {
                        rep.first_error = Some(short.clone());
                        // The last thing that happened before this throw.
                        rep.cause = EVENTS.with(|e| e.borrow().last().cloned())
                            .map(|(k, n)| (k.to_string(), n));
                    }
                    rep.errors.push(short);
                }
            }
            // A browser reaches a microtask checkpoint between scripts, and
            // that is where mutation records are delivered. Doing it per
            // script also means mutations made BEFORE an observer existed are
            // already drained, so a late-registering observer is not handed
            // history it never asked for.
            let _ = ctx.eval(Source::from_bytes(b"__deliverMutations(8); __upgradePending()"));
        }
        // No script is executing during the lifecycle pass.
        CURRENT.with(|c| *c.borrow_mut() = None);
        // Lifecycle AFTER the scripts have registered their handlers. Page
        // handlers run inside this pass, so recording stays ON for them; the
        // FIRE script's own typeof probes are named below and filtered.
        match ctx.eval(Source::from_bytes(FIRE.as_bytes())) {
            Ok(_) => {
                rep.listeners_fired = ctx
                    .eval(Source::from_bytes(b"__fired"))
                    .ok()
                    .and_then(|v| v.as_number())
                    .unwrap_or(0.0) as u32;
            }
            Err(e) => rep.errors.push(format!("lifecycle: {e}")),
        }
        // Mutations made by the lifecycle handlers, delivered before the
        // timer queue opens.
        let _ = ctx.eval(Source::from_bytes(b"__deliverMutations(8); __upgradePending()"));
        // Promise jobs queued by handlers (a bundle that awaits on ready).
        let _ = ctx.run_jobs();

        // Then the timer queue, under its budget. Content built in a
        // `setTimeout(fn, 0)` is an ordinary deferred-init idiom, and the
        // DOMContentLoaded lesson applies again: registering is not the point,
        // dispatching is.
        let drain = format!("__drainTimers({}, {})", TIMER_BUDGET, TIMER_HORIZON_MS);
        if let Err(e) = ctx.eval(Source::from_bytes(drain.as_bytes())) {
            rep.errors.push(format!("timers: {e}"));
        }
        let _ = ctx.run_jobs();
        if let Ok(v) = ctx.eval(Source::from_bytes(b"__timerStats()")) {
            if let Some(o) = v.as_object() {
                rep.timers_fired = o.get(0, &mut ctx).ok()
                    .and_then(|x| x.as_number()).unwrap_or(0.0) as u32;
                rep.timers_dropped = o.get(1, &mut ctx).ok()
                    .and_then(|x| x.as_number()).unwrap_or(0.0) as u32;
            }
        }

        // ★ AND AGAIN ON THE WAY OUT. These wrappers are garbage-collected
        // objects belonging to THIS Context; left in a thread-local they
        // outlive the realm that owns them, and the next run on the same
        // thread trips Boa's GC — which showed up as SIGTRAP under the
        // parallel test harness and passed cleanly with --test-threads=1.
        // A cache of engine objects must not outlive the engine.
        NODE_CACHE.with(|c| c.borrow_mut().clear());
        ELISTENERS.with(|m| m.borrow_mut().clear());
        DOM.with(|d| *dom = std::mem::take(&mut d.borrow_mut()));
        rep.layout_reads = LAYOUT_READS.with(|n| *n.borrow());
        let (pf, pfail) = PAGE_FETCHES.with(|c| *c.borrow());
        rep.page_fetches = pf;
        rep.page_fetch_failures = pfail;
        let (nblocked, hosts) = PAGE_BLOCKED.with(|b| {
            let b = b.borrow();
            (b.0, b.1.iter().map(|(k, v)| (k.clone(), *v)).collect::<Vec<_>>())
        });
        rep.page_blocked = nblocked;
        rep.blocked_hosts = hosts;
        rep.beacons_suppressed = ctx.eval(Source::from_bytes(b"__beacons")).ok()
            .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
        // Hand the fetcher back so a caller may reuse the engine.
        self.page_fetcher = PAGE_NET.with(|n| n.borrow_mut().take());
        rep.mutation_records = ctx
            .eval(Source::from_bytes(b"__moDelivered"))
            .ok()
            .and_then(|v| v.as_number())
            .unwrap_or(0.0) as u32;
        let (dw, dwr) = DOC_WRITE.with(|c| *c.borrow());
        rep.doc_writes = dw;
        rep.doc_writes_refused = dwr;
        let (nd, nl) = DISPATCH.with(|c| *c.borrow());
        rep.events_dispatched = nd + ctx.eval(Source::from_bytes(b"__docDispatches")).ok()
            .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
        rep.event_listeners_run = nl + ctx.eval(Source::from_bytes(b"__dispatchRan")).ok()
            .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
        rep.history_writes = ctx.eval(Source::from_bytes(b"__histWrites")).ok()
            .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
        rep.history_refused = ctx.eval(Source::from_bytes(b"__histRefused")).ok()
            .and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
        rep.ce_upgrades = ctx
            .eval(Source::from_bytes(b"__ceUpgrades"))
            .ok().and_then(|v| v.as_number()).unwrap_or(0.0) as u32;
        rep.observers_registered = ctx
            .eval(Source::from_bytes(b"__observed"))
            .ok()
            .and_then(|v| v.as_number())
            .unwrap_or(0.0) as u32;
        rep.missing = MISSES.with(|m| m.borrow().iter().map(|(k, v)| (k.clone(), *v)).collect());
        rep.nulls = NULLS.with(|n| n.borrow().iter().map(|(k, v)| (k.clone(), *v)).collect());
        rep
    }
}
