//! The Boa side of the seam. Nothing outside this file knows which engine runs.

use crate::dom::{Dom, Handle, Kind};
use crate::engine::{RunReport, ScriptEngine, ScriptSource};
use boa_engine::{
    js_string, object::ObjectInitializer, property::Attribute, Context, JsArgs, JsObject,
    JsResult, JsValue, NativeFunction, Source,
};
use std::{cell::RefCell, collections::BTreeMap};

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
        MISSES.with(|m| *m.borrow_mut().entry(format!("{kind}.{name}")).or_default() += 1);
    }
    Ok(JsValue::undefined())
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
        // Element listeners are accepted and dropped: nothing in a headless
        // conversion will ever deliver a click. Accepting them keeps a page
        // running; pretending to deliver them would be a lie.
        .function(NativeFunction::from_fn_ptr(ignore), js_string!("addEventListener"), 2)
        .function(NativeFunction::from_fn_ptr(ignore), js_string!("removeEventListener"), 2)
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
    probed(o, "element", ctx)
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

fn ignore(_t: &JsValue, _a: &[JsValue], _c: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
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
    Ok(match run_query(this, args, ctx)?.first() {
        Some(&h) => node_obj(h, ctx),
        None => JsValue::null(),
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
#[derive(Default)]
pub struct BoaEngine {
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
  var __tick = 0;
  globalThis.performance = {
    now: function () { return ++__tick; },
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

  function mkurl(parts) {
    if (!parts) return null;
    var u = {};
    for (var k in parts) u[k] = parts[k];
    u.toString = function () { return this.href; };
    u.searchParams = { get: function () { return null; }, has: function () { return false; } };
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
      try { Object.defineProperty(document, 'URL', { get: function(){ return loc.href; }, configurable: true }); } catch (e) {}
    }
  }

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
  globalThis.navigator = {
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
  globalThis.__fire = function (type) {
    var ev = { type: type, target: document, currentTarget: document,
               preventDefault: function () {}, stopPropagation: function () {} };
    for (var i = 0; i < L.length; i++) {
      if (L[i][0] !== type) continue;
      try { L[i][1].call(document, ev); globalThis.__fired++; } catch (e) {}
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
        DOM.with(|d| *d.borrow_mut() = std::mem::take(dom));
        let mut ctx = match self.module_root.as_ref()
            .and_then(|r| boa_engine::module::SimpleModuleLoader::new(r).ok())
        {
            Some(loader) => Context::builder()
                .module_loader(std::rc::Rc::new(loader))
                .build()
                .unwrap_or_default(),
            None => Context::default(),
        };

        let body = with(|d| d.by_tag("body").first().copied()).unwrap_or(0);
        let body_v = node_obj(body, &mut ctx);
        let doc_el = with(|d| d.by_tag("html").first().copied()).unwrap_or(0);
        let doc_el_v = node_obj(doc_el, &mut ctx);
        let doc = ObjectInitializer::new(&mut ctx)
            .function(NativeFunction::from_fn_ptr(get_element_by_id), js_string!("getElementById"), 1)
            .function(NativeFunction::from_fn_ptr(create_element), js_string!("createElement"), 1)
            .function(NativeFunction::from_fn_ptr(create_text_node), js_string!("createTextNode"), 1)
            .function(NativeFunction::from_fn_ptr(query_all), js_string!("querySelectorAll"), 1)
            .function(NativeFunction::from_fn_ptr(query_first), js_string!("querySelector"), 1)
            .function(NativeFunction::from_fn_ptr(by_class), js_string!("getElementsByClassName"), 1)
            .function(NativeFunction::from_fn_ptr(by_tag_name), js_string!("getElementsByTagName"), 1)
            .property(js_string!("body"), body_v, Attribute::all())
            .property(js_string!("documentElement"), doc_el_v, Attribute::all())
            .property(js_string!("__h"), 0.0, Attribute::all())
            .build();
        {
            let getter = NativeFunction::from_fn_ptr(current_script).to_js_function(ctx.realm());
            let desc = boa_engine::property::PropertyDescriptor::builder()
                .get(getter).enumerable(true).configurable(true).build();
            let _ = doc.define_property_or_throw(js_string!("currentScript"), desc, &mut ctx);
        }
        let doc_v = probed(doc.clone(), "document", &mut ctx);
        let _ = ctx.register_global_property(js_string!("document"), doc_v, Attribute::all());

        // `window` was the single most common missing binding in the first
        // corpus run, so the instrument's own report earned it a place. It is
        // the global object with `document` hung off it — enough for the
        // `window.document` / `window.onload` shapes real pages use, without
        // pretending to be a browser.
        let doc_v2 = probed(doc, "document", &mut ctx);
        let win = ObjectInitializer::new(&mut ctx)
            .property(js_string!("document"), doc_v2, Attribute::all())
            .build();
        let win_v = probed(win, "window", &mut ctx);
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
        BASE.with(|b| *b.borrow_mut() = self.base_url.clone());
        CURRENT.with(|c| *c.borrow_mut() = None);
        let mut rep = RunReport { scripts_run: 0, scripts_failed: 0, errors: vec![],
            missing: vec![], listeners_fired: 0, module_retries: 0, observers_registered: 0 };
        RECORDING.with(|r| *r.borrow_mut() = false);
        let _ = ctx.register_global_callable(js_string!("__parse_url"), 2,
            NativeFunction::from_fn_ptr(parse_url));
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
                    rep.errors.push(msg.chars().take(200).collect());
                }
            }
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
        // Promise jobs queued by handlers (a bundle that awaits on ready).
        let _ = ctx.run_jobs();

        DOM.with(|d| *dom = std::mem::take(&mut d.borrow_mut()));
        rep.observers_registered = ctx
            .eval(Source::from_bytes(b"__observed"))
            .ok()
            .and_then(|v| v.as_number())
            .unwrap_or(0.0) as u32;
        rep.missing = MISSES.with(|m| m.borrow().iter().map(|(k, v)| (k.clone(), *v)).collect());
        rep
    }
}
