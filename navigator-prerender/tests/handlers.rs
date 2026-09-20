use navigator_prerender::{boa_impl::BoaEngine, convert};

fn run(html: &str) -> navigator_prerender::Conversion { convert(html, &mut BoaEngine::default()) }

fn r(c: &navigator_prerender::Conversion) -> String {
    let m = "data-r=\"";
    let i = c.html.find(m).unwrap_or_else(|| panic!("no data-r in {}", c.html)) + m.len();
    c.html[i..].split('"').next().unwrap().to_string()
}

/// ★ An unset handler is NULL, not undefined. Pages compare against null,
/// and `typeof el.onclick` is the older form of the same check.
#[test]
fn unset_handlers_are_null() {
    let c = run(r#"<html><body><div id=t></div><script>
      var e = document.getElementById('t');
      document.body.setAttribute('data-r',
        (e.oninput === null) + '|' + (e.onclick === null) + '|' + typeof e.onload);
    </script></body></html>"#);
    assert_eq!(r(&c), "true|true|object");
}

/// A handler slot is a real slot: it round-trips, and it RECEIVES the page's
/// own dispatch — which is the only kind of event this converter ever
/// delivers.
#[test]
fn a_handler_slot_receives_the_pages_own_dispatch() {
    let c = run(r#"<html><body><input id=t><script>
      var e = document.getElementById('t'), got = 'none';
      e.oninput = function (ev) { got = ev.type + ':' + (ev.target === e); };
      var same = (typeof e.oninput === 'function');
      e.dispatchEvent(new Event('input'));
      document.body.setAttribute('data-r', same + '|' + got);
    </script></body></html>"#);
    assert_eq!(r(&c), "true|input:true");
}

/// Assigning REPLACES where addEventListener appends, and assigning null
/// clears the slot.
#[test]
fn assignment_replaces_and_null_clears() {
    let c = run(r#"<html><body><div id=t></div><script>
      var e = document.getElementById('t'), log = [];
      e.onclick = function () { log.push('first'); };
      e.onclick = function () { log.push('second'); };
      e.dispatchEvent(new Event('click'));
      e.onclick = null;
      e.dispatchEvent(new Event('click'));
      document.body.setAttribute('data-r', log.join(',') + '|' + (e.onclick === null));
    </script></body></html>"#);
    assert_eq!(r(&c), "second|true");
}

/// The slot runs before added listeners, the order a browser uses when both
/// are present.
#[test]
fn slot_runs_before_added_listeners() {
    let c = run(r#"<html><body><div id=t></div><script>
      var e = document.getElementById('t'), log = [];
      e.addEventListener('click', function () { log.push('listener'); });
      e.onclick = function () { log.push('slot'); };
      e.dispatchEvent(new Event('click'));
      document.body.setAttribute('data-r', log.join(','));
    </script></body></html>"#);
    assert_eq!(r(&c), "slot,listener");
}

/// ★ Still no invented events. A handler for something only a USER can cause
/// is stored faithfully and never fired.
#[test]
fn user_events_still_never_fire_on_their_own() {
    let c = run(r#"<html><body><input id=t><script>
      var fired = 0;
      var e = document.getElementById('t');
      e.oninput = function () { fired++; };
      e.onclick = function () { fired++; };
      e.onscroll = function () { fired++; };
      document.body.setAttribute('data-r', String(fired));
    </script></body></html>"#);
    assert_eq!(r(&c), "0");
    assert_eq!(c.events_dispatched, 0);
}

/// rel is REFLECTED: setting the property writes the attribute, so
/// document.styleSheets — which reads the attribute — then finds the sheet.
#[test]
fn rel_is_reflected_and_stylesheets_sees_it() {
    let c = run(r#"<html><head></head><body><div id=t></div><script>
      var l = document.createElement('link');
      l.rel = 'stylesheet';
      l.setAttribute('href', '/x.css');
      document.head.appendChild(l);
      document.body.setAttribute('data-r',
        l.getAttribute('rel') + '|' + l.rel + '|' + document.styleSheets.length);
    </script></body></html>"#);
    assert_eq!(r(&c), "stylesheet|stylesheet|1");
    assert!(c.html.contains(r#"rel="stylesheet""#), "{}", c.html);
}

/// rel belongs to the elements that have one.
#[test]
fn rel_only_on_elements_that_have_it() {
    let c = run(r#"<html><body><a id=a rel=next></a><div id=d></div><script>
      document.body.setAttribute('data-r',
        document.getElementById('a').rel + '|' +
        typeof document.getElementById('d').rel);
    </script></body></html>"#);
    assert_eq!(r(&c), "next|undefined");
}

/// ★ contentWindow is NULL, and null is not the same as absent. Every corpus
/// use reaches for a PRISTINE REALM to borrow clean prototypes from; this
/// converter creates no child browsing contexts, so there is none. Handing
/// back our own window would answer the opposite of the question asked.
#[test]
fn content_window_is_null_not_our_own_window() {
    let c = run(r#"<html><body><iframe id=f></iframe><div id=d></div><script>
      var f = document.getElementById('f');
      document.body.setAttribute('data-r', [
        (f.contentWindow === null),
        (f.contentWindow === window),
        (f.contentDocument === null),
        typeof document.getElementById('d').contentWindow
      ].join('|'));
    </script></body></html>"#);
    assert_eq!(r(&c), "true|false|true|undefined");
}

/// The guarded fallback shape those call sites use must take its fallback
/// rather than dying.
#[test]
fn the_pristine_realm_fallback_chain_is_taken() {
    let c = run(r#"<html><body><div id=t></div><script>
      var frame = document.createElement('iframe');
      document.body.appendChild(frame);
      var doc = frame.contentWindow ? frame.contentWindow.document
              : (document.implementation
                  ? document.implementation.createHTMLDocument('') : null);
      document.body.setAttribute('data-r',
        (doc !== null) + '|' + (doc === document) + '|' + doc.nodeType);
    </script></body></html>"#);
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert_eq!(r(&c), "true|false|9");
}
