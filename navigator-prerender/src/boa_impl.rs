//! The Boa side of the seam. Nothing outside this file knows which engine runs.

use crate::dom::{Dom, Handle, Kind};
use crate::engine::{RunReport, ScriptEngine};
use boa_engine::{
    js_string, object::ObjectInitializer, property::Attribute, Context, JsArgs, JsObject,
    JsResult, JsValue, NativeFunction, Source,
};
use std::cell::RefCell;

// The arena lives here for the duration of a run. A thread-local keeps every
// host function a plain fn pointer — no captured state to trace through the
// engine's GC, which is exactly the coupling §11.8 says is the hard part of
// swapping engines. Single-threaded and short-lived by construction.
thread_local! {
    static DOM: RefCell<Dom> = RefCell::new(Dom::new());
}

fn with<R>(f: impl FnOnce(&mut Dom) -> R) -> R { DOM.with(|d| f(&mut d.borrow_mut())) }

/// Read the `__h` handle off a node wrapper.
fn handle_of(v: &JsValue, ctx: &mut Context) -> Option<Handle> {
    let o = v.as_object()?;
    let h = o.get(js_string!("__h"), ctx).ok()?;
    h.as_number().map(|n| n as Handle)
}

fn node_obj(h: Handle, ctx: &mut Context) -> JsValue {
    let tag = with(|d| d.tag(h).map(|s| s.to_string())).unwrap_or_default();
    let o = ObjectInitializer::new(ctx)
        .property(js_string!("__h"), h as f64, Attribute::all())
        .property(js_string!("tagName"), js_string!(tag.to_uppercase()), Attribute::all())
        .function(NativeFunction::from_fn_ptr(append_child), js_string!("appendChild"), 1)
        .function(NativeFunction::from_fn_ptr(set_attribute), js_string!("setAttribute"), 2)
        .function(NativeFunction::from_fn_ptr(get_attribute), js_string!("getAttribute"), 1)
        .function(NativeFunction::from_fn_ptr(get_text), js_string!("getText"), 0)
        .function(NativeFunction::from_fn_ptr(set_text), js_string!("setText"), 1)
        .build();
    install_text_accessor(&o, ctx);
    JsValue::from(o)
}

/// `textContent` as a real accessor: scripts use the property, not a method.
fn install_text_accessor(o: &JsObject, ctx: &mut Context) {
    let getter = NativeFunction::from_fn_ptr(get_text).to_js_function(ctx.realm());
    let setter = NativeFunction::from_fn_ptr(set_text).to_js_function(ctx.realm());
    let desc = boa_engine::property::PropertyDescriptor::builder()
        .get(getter).set(setter).enumerable(true).configurable(true).build();
    let _ = o.define_property_or_throw(js_string!("textContent"), desc, ctx);
}

fn append_child(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let (p, c) = (handle_of(this, ctx), handle_of(args.get_or_undefined(0), ctx));
    if let (Some(p), Some(c)) = (p, c) {
        with(|d| { if d.append(p, c) { d.script_mutations += 1; } });
    }
    Ok(args.get_or_undefined(0).clone())
}

fn set_attribute(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(h) = handle_of(this, ctx) {
        let k = args.get_or_undefined(0).to_string(ctx)?.to_std_string_escaped();
        let v = args.get_or_undefined(1).to_string(ctx)?.to_std_string_escaped();
        with(|d| { d.set_attr(h, &k, &v); d.script_mutations += 1; });
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
    }
    Ok(JsValue::undefined())
}

fn get_element_by_id(_t: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = args.get_or_undefined(0).to_string(ctx)?.to_std_string_escaped();
    Ok(match with(|d| d.by_id(&id)) { Some(h) => node_obj(h, ctx), None => JsValue::null() })
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

fn query_all(_t: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let sel = args.get_or_undefined(0).to_string(ctx)?.to_std_string_escaped();
    // v0: type selectors only. Anything else returns empty, and the corpus
    // report counts how often that mattered.
    let hs = if sel.starts_with('#') {
        with(|d| d.by_id(&sel[1..])).into_iter().collect::<Vec<_>>()
    } else {
        with(|d| d.by_tag(&sel))
    };
    let arr = boa_engine::object::builtins::JsArray::new(ctx)?;
    for h in hs { let n = node_obj(h, ctx); arr.push(n, ctx)?; }
    Ok(JsValue::from(arr))
}

pub struct BoaEngine;

impl ScriptEngine for BoaEngine {
    fn name(&self) -> &'static str { "boa" }

    fn run(&mut self, dom: &mut Dom, scripts: &[String]) -> RunReport {
        DOM.with(|d| *d.borrow_mut() = std::mem::take(dom));
        let mut ctx = Context::default();

        let body = with(|d| d.by_tag("body").first().copied()).unwrap_or(0);
        let body_v = node_obj(body, &mut ctx);
        let doc = ObjectInitializer::new(&mut ctx)
            .function(NativeFunction::from_fn_ptr(get_element_by_id), js_string!("getElementById"), 1)
            .function(NativeFunction::from_fn_ptr(create_element), js_string!("createElement"), 1)
            .function(NativeFunction::from_fn_ptr(create_text_node), js_string!("createTextNode"), 1)
            .function(NativeFunction::from_fn_ptr(query_all), js_string!("querySelectorAll"), 1)
            .property(js_string!("body"), body_v, Attribute::all())
            .build();
        let _ = ctx.register_global_property(js_string!("document"), doc.clone(), Attribute::all());

        // `window` was the single most common missing binding in the first
        // corpus run, so the instrument's own report earned it a place. It is
        // the global object with `document` hung off it — enough for the
        // `window.document` / `window.onload` shapes real pages use, without
        // pretending to be a browser.
        let win = ObjectInitializer::new(&mut ctx)
            .property(js_string!("document"), doc, Attribute::all())
            .build();
        let _ = ctx.register_global_property(js_string!("window"), win, Attribute::all());

        let mut rep = RunReport { scripts_run: 0, scripts_failed: 0, errors: vec![] };
        for s in scripts {
            match ctx.eval(Source::from_bytes(s.as_bytes())) {
                Ok(_) => rep.scripts_run += 1,
                Err(e) => {
                    rep.scripts_failed += 1;
                    let msg = e.to_string();
                    // The error list IS the missing-API report: a script that
                    // reaches for an unimplemented binding fails here by name.
                    rep.errors.push(msg.chars().take(200).collect());
                }
            }
        }
        DOM.with(|d| *dom = std::mem::take(&mut d.borrow_mut()));
        rep
    }
}
