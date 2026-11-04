//! Probe fidelity (legacy-web §5.4.5.6a, "what it says to build", item 1):
//! a probe performs the GESTURE a reader makes, with real event fields, and
//! reaches handlers that delegate from an ancestor. Every probe kind has a
//! test here, and every test also checks the safety invariants that do not
//! change with the kind: the artifact is the non-exploring one, byte for byte.

use navigator_prerender::{boa_impl::BoaEngine, convert_with, fetch::NoNetwork, Conversion};

fn explore(html: &str) -> Conversion {
    let mut eng = BoaEngine { explore: true, ..Default::default() };
    convert_with(html, Some("https://example.test/p.html"), &mut eng, &mut NoNetwork)
}
fn plain(html: &str) -> Conversion {
    convert_with(html, Some("https://example.test/p.html"), &mut BoaEngine::default(), &mut NoNetwork)
}

/// Explore, check the artifact is untouched, and return the conversion.
fn probe(html: &str) -> Conversion {
    let c = explore(html);
    assert_eq!(c.html, plain(html).html, "exploration leaked into the artifact");
    c
}

fn events(c: &Conversion) -> Vec<(String, String)> {
    c.transitions.iter().map(|t| (t.trigger.clone(), t.event.clone())).collect()
}

fn page(body: &str, script: &str) -> String {
    format!("<html><body>{body}<p id=p>x</p><script>var P = document.getElementById('p');\n{script}</script></body></html>")
}

#[test]
fn hover_handlers_are_probed_with_the_hover_sequence() {
    for ty in ["mouseenter", "mouseover", "pointerenter", "pointerover"] {
        let c = probe(&page("<div id=m>menu</div>", &format!(
            "document.getElementById('m').addEventListener('{ty}', function () {{ P.className = 'open'; }});")));
        assert_eq!(events(&c), vec![("#m".into(), "hover".into())], "{ty}");
    }
}

#[test]
fn focus_handlers_are_probed() {
    for ty in ["focus", "focusin"] {
        let c = probe(&page("<a id=a href=/x>link</a>", &format!(
            "document.getElementById('a').addEventListener('{ty}', function (e) {{ \
               if (e.type === '{ty}' && e.isTrusted) P.className = 'tip'; }});")));
        assert_eq!(events(&c), vec![("#a".into(), "focus".into())], "{ty}");
    }
}

/// A key-only handler that tests the key it was given: a bare Event has no
/// `key`, so this recorded nothing before.
#[test]
fn key_only_handlers_get_enter_with_real_fields() {
    let c = probe(&page("<div id=k tabindex=0 role=button>act</div>",
        "document.getElementById('k').addEventListener('keydown', function (e) { \
           if (e.key === 'Enter' && e.keyCode === 13) P.className = 'on'; });"));
    assert_eq!(events(&c), vec![("#k".into(), "key:Enter".into())]);
}

/// …but not on a text field: what a key handler there reacts to is what was
/// typed, which is free-form input (§5.4.5.2), not one key.
#[test]
fn key_handlers_on_text_fields_are_not_probed() {
    let c = probe(&page("<input id=q type=search>",
        "document.getElementById('q').addEventListener('keyup', function () { P.className = 'on'; });"));
    assert!(c.transitions.is_empty(), "{:?}", c.transitions);
    let q = c.coverage.iter().find(|k| k.input_type == "search").unwrap();
    assert!(!q.explorable && !q.probed, "{q:?}");
}

/// A click handler that checks it was a primary-button click with a
/// position. All three fields were undefined on the bare Event.
#[test]
fn clicks_carry_mouse_fields() {
    let c = probe(&page("<button id=b>b</button>",
        "document.getElementById('b').addEventListener('click', function (e) { \
           if (e.button === 0 && e.clientX > 0 && e.detail === 1 && e instanceof MouseEvent) P.className = 'on'; });"));
    assert_eq!(events(&c), vec![("#b".into(), "click".into())]);
}

/// pointerdown/mousedown-only handlers are reached by the click gesture,
/// which delivers the whole down/up/click sequence.
#[test]
fn pointer_down_handlers_are_reached_by_a_click() {
    for ty in ["pointerdown", "mousedown", "pointerup", "mouseup"] {
        let c = probe(&page("<span id=s>s</span>", &format!(
            "document.getElementById('s').addEventListener('{ty}', function (e) {{ \
               if (e.type === '{ty}') P.className = 'on'; }});")));
        assert_eq!(events(&c), vec![("#s".into(), "click".into())], "{ty}");
    }
}

#[test]
fn touch_handlers_get_a_tap_and_a_swipe() {
    // A tap handler.
    let c = probe(&page("<div id=t>t</div>",
        "document.getElementById('t').addEventListener('touchend', function (e) { \
           if (e.changedTouches.length === 1) P.className = 'tapped'; });"));
    let ev = events(&c);
    assert!(ev.contains(&("#t".into(), "tap".into())), "{ev:?}");
    // A carousel's swipe: start x on touchstart, compare on touchend.
    let c = probe(&page("<div id=c>slides</div>",
        "var x0 = 0, el = document.getElementById('c'); \
         el.addEventListener('touchstart', function (e) { x0 = e.touches[0].clientX; }); \
         el.addEventListener('touchend', function (e) { \
           if (x0 - e.changedTouches[0].clientX > 50) P.className = 'next'; });"));
    assert_eq!(events(&c), vec![("#c".into(), "swipe".into())], "only the swipe moves the carousel");
}

#[test]
fn pointer_drags_are_probed() {
    let c = probe(&page("<div id=d>slides</div>",
        "var x0 = null, el = document.getElementById('d'); \
         el.addEventListener('pointerdown', function (e) { x0 = e.clientX; }); \
         el.addEventListener('pointermove', function (e) { \
           if (x0 !== null && x0 - e.clientX > 100) P.className = 'next'; });"));
    assert_eq!(events(&c), vec![("#d".into(), "drag".into())]);
}

/// ★ The reader's own state change comes FIRST: the box is checked when
/// `change` fires. Before, the handler saw an unchecked box and did nothing.
/// The checkbox's own `checked` is the reader's action, not the page's
/// effect, so it is not in the transition — and the artifact still has the
/// box unchecked (probe() checks).
#[test]
fn a_checkbox_is_checked_before_its_change_fires() {
    let c = probe(&page("<input id=c type=checkbox>",
        "document.getElementById('c').addEventListener('change', function () { \
           P.className = this.checked ? 'on' : 'off'; });"));
    assert_eq!(events(&c), vec![("#c".into(), "change".into())]);
    let t = &c.transitions[0];
    assert!(t.effects.iter().all(|e| !format!("{e:?}").contains("checked")), "{:?}", t.effects);
    assert!(format!("{:?}", t.effects).contains("\"on\""), "{:?}", t.effects);
}

#[test]
fn a_select_changes_to_its_next_option() {
    let c = probe(&page("<select id=s><option value=a>A<option value=b>B</select>",
        "document.getElementById('s').addEventListener('change', function () { \
           P.textContent = 'chose ' + this.value; });"));
    assert_eq!(events(&c), vec![("#s".into(), "change".into())]);
    assert!(format!("{:?}", c.transitions[0].effects).contains("chose b"), "{:?}", c.transitions[0]);
}

#[test]
fn a_details_toggle_fires_after_it_opens() {
    let c = probe(&page("<details id=d><summary>s</summary>t</details>",
        "document.getElementById('d').addEventListener('toggle', function () { \
           if (this.open) P.className = 'opened'; });"));
    assert_eq!(events(&c), vec![("#d".into(), "toggle".into())]);
}

/// ★★ DELEGATION through the document. The handler tests
/// `e.target.closest(sel)`; the discovery probe at <body> sees the selector,
/// and the buttons it names are probed. Before, this button was a "native
/// control with no handler" and nothing ran.
#[test]
fn a_document_listener_is_probed_through_the_targets_it_tests_for() {
    let c = probe(&page("<button id=t data-toggle=menu>menu</button><button id=n>other</button>",
        "document.addEventListener('click', function (e) { \
           var b = e.target.closest('[data-toggle]'); if (!b) return; \
           P.className = 'open-' + b.getAttribute('data-toggle'); });"));
    assert_eq!(events(&c), vec![("#t".into(), "click".into())],
        "the tested-for button only; the other one does not match the selector");
    let t = c.coverage.iter().find(|k| k.path == "#t").unwrap();
    assert!(t.native && t.delegated && t.recorded, "{t:?}");
    let doc = c.coverage.iter().find(|k| k.tag == "#document").unwrap();
    assert!(doc.outside && doc.via, "{doc:?}");
    let n = c.coverage.iter().find(|k| k.path == "#n").unwrap();
    assert!(!n.probed, "#n does not match what the handler tests: {n:?}");
}

/// Delegation from an element: a list that dispatches on `matches`. The
/// items are not interactive elements by themselves, so they are reported
/// as outside the universe, while the list is credited.
#[test]
fn a_list_listener_is_probed_through_matching_items() {
    let c = probe(&page("<ul id=l><li class=item id=i1>one</li><li class=item id=i2>two</li><li id=i3>sep</li></ul>",
        "document.getElementById('l').addEventListener('click', function (e) { \
           if (e.target.matches('li.item')) e.target.setAttribute('aria-selected', 'true'); });"));
    let ev = events(&c);
    assert_eq!(ev, vec![("#i1".into(), "click".into()), ("#i2".into(), "click".into())], "{ev:?}");
    let l = c.coverage.iter().find(|k| k.path == "#l").unwrap();
    assert!(l.via && !l.outside, "{l:?}");
    let i1 = c.coverage.iter().find(|k| k.path == "#i1").unwrap();
    assert!(i1.outside && i1.delegated && i1.recorded, "{i1:?}");
}

/// `classList.contains` is a test too.
#[test]
fn class_tests_are_discovered() {
    let c = probe(&page("<div id=r><span class=tab id=s1>a</span><span id=s2>b</span></div>",
        "document.getElementById('r').addEventListener('click', function (e) { \
           if (e.target.classList.contains('tab')) P.textContent = 'tab ' + e.target.id; });"));
    assert_eq!(events(&c), vec![("#s1".into(), "click".into())]);
}

/// A delegating handler that tests nothing observable (a tagName compare)
/// is probed through the interactive descendants.
#[test]
fn an_opaque_delegating_handler_falls_back_to_interactive_descendants() {
    let c = probe(&page("<div><button id=b>b</button><span id=s>s</span></div>",
        "document.body.addEventListener('click', function (e) { \
           if (e.target.tagName === 'BUTTON') P.className = 'pressed'; });"));
    assert_eq!(events(&c), vec![("#b".into(), "click".into())]);
}

/// Delegated probes keep every safety invariant: the network stays closed,
/// navigation is refused, and every probe is reverted (probe() checks the
/// artifact).
#[test]
fn delegated_probes_cannot_reach_the_network_or_navigate() {
    use navigator_prerender::fetch::MapFetcher;
    let mut api = MapFetcher::default();
    api.0.insert("https://example.test/delete".into(), "deleted!".into());
    let mut eng = BoaEngine { explore: true, page_fetcher: Some(Box::new(api)), ..Default::default() };
    let html = page("<a id=a href=/next class=go>go</a>",
        "document.addEventListener('click', function (e) { \
           var a = e.target.closest('a.go'); if (!a) return; \
           fetch('/delete').then(function (r) { return r.text(); }).then(function (t) { P.textContent = t; }); \
           location.href = a.href; });");
    let c = convert_with(&html, Some("https://example.test/p.html"), &mut eng, &mut NoNetwork);
    assert_eq!(c.page_fetches, 0, "the delegated probe's fetch must not go out");
    assert_eq!(c.html, plain(&html).html);
    let a = c.coverage.iter().find(|k| k.path == "#a").unwrap();
    assert!(a.delegated && a.net_get > 0, "attempted, refused, and counted: {a:?}");
}

/// Load and error handlers are not user-interactive: never probed, and not
/// explorable (coverage.py reports them outside the interactive denominator).
#[test]
fn load_and_error_handlers_are_not_probed() {
    let c = probe(&page("<img id=i src=x.png>",
        "var i = document.getElementById('i'); \
         i.addEventListener('load', function () { P.className = 'loaded'; }); \
         i.onerror = function () { P.className = 'broken'; };"));
    assert!(c.transitions.is_empty(), "{:?}", c.transitions);
    let i = c.coverage.iter().find(|k| k.path == "#i").unwrap();
    assert!(!i.explorable && !i.probed, "{i:?}");
}

/// The recording stays deterministic with gestures and delegation.
#[test]
fn gesture_and_delegated_recordings_are_reproducible() {
    let html = page("<ul id=l><li class=x>a</li><li class=x>b</li></ul><div id=h>h</div><input id=c type=checkbox>",
        "document.getElementById('l').addEventListener('click', function (e) { \
           if (e.target.closest('.x')) e.target.className = 'x on'; }); \
         document.getElementById('h').addEventListener('mouseenter', function () { P.className = 'h'; }); \
         document.getElementById('c').addEventListener('change', function () { P.className = 'c' + this.checked; });");
    let (a, b) = (probe(&html), probe(&html));
    assert_eq!(format!("{:?}", a.transitions), format!("{:?}", b.transitions));
    assert_eq!(a.transitions.len(), 4, "{:?}", events(&a));
}
