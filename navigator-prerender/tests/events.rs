use navigator_prerender::{boa_impl::BoaEngine, convert};

fn run(html: &str) -> navigator_prerender::Conversion { convert(html, &mut BoaEngine::default()) }

fn r(c: &navigator_prerender::Conversion) -> String {
    let m = "data-r=\"";
    let i = c.html.find(m).unwrap_or_else(|| panic!("no data-r in {}", c.html)) + m.len();
    c.html[i..].split('"').next().unwrap().to_string()
}

/// ★ THE LINE THIS DRAWS. Events the converter would have to INVENT — a
/// click, a scroll, a resize — are still never fired. Events the page raises
/// ITSELF are just code the page runs, and are delivered for real. Element
/// listeners used to be accepted and dropped; that was right for the first
/// kind and wrong for the second.
#[test]
fn a_page_dispatching_to_itself_is_delivered() {
    let c = run(r#"<html><body><div id=t></div><script>
      var got = 'none';
      var t = document.getElementById('t');
      t.addEventListener('ready', function (e) { got = e.type + ':' + e.detail.v; });
      t.dispatchEvent(new CustomEvent('ready', { detail: { v: 7 } }));
      document.body.setAttribute('data-r', got);
    </script></body></html>"#);
    assert_eq!(r(&c), "ready:7");
    assert_eq!(c.events_dispatched, 1);
    assert_eq!(c.event_listeners_run, 1);
}

/// No user event is ever synthesised: registering a click listener and never
/// dispatching one must leave it unfired.
#[test]
fn user_events_are_still_never_invented() {
    let c = run(r#"<html><body><div id=t></div><script>
      var fired = 0;
      document.getElementById('t').addEventListener('click', function () { fired++; });
      window.addEventListener('resize', function () { fired++; });
      document.body.setAttribute('data-r', String(fired));
    </script></body></html>"#);
    assert_eq!(r(&c), "0");
    assert_eq!(c.events_dispatched, 0);
}

/// Bubbling walks real ancestors — only expressible because the tree is
/// navigable — and stops at the target when it does not bubble.
#[test]
fn bubbling_walks_real_ancestors() {
    let c = run(r#"<html><body><div id=outer><div id=mid><div id=inner></div></div></div><script>
      var seen = [];
      ['outer','mid','inner'].forEach(function (id) {
        document.getElementById(id).addEventListener('ping', function (e) {
          seen.push(id + '@' + e.currentTarget.getAttribute('id') + ':' + e.target.getAttribute('id'));
        });
      });
      document.getElementById('inner').dispatchEvent(new Event('ping', { bubbles: true }));
      var bubbled = seen.join(',');
      seen = [];
      document.getElementById('inner').dispatchEvent(new Event('ping'));
      document.body.setAttribute('data-r', bubbled + '|' + seen.join(','));
    </script></body></html>"#);
    assert_eq!(r(&c), "inner@inner:inner,mid@mid:inner,outer@outer:inner|inner@inner:inner");
}

/// stopPropagation really stops the walk.
#[test]
fn stop_propagation_halts_the_walk() {
    let c = run(r#"<html><body><div id=outer><div id=inner></div></div><script>
      var seen = [];
      document.getElementById('outer').addEventListener('ping', function () { seen.push('outer'); });
      document.getElementById('inner').addEventListener('ping', function (e) {
        seen.push('inner'); e.stopPropagation();
      });
      document.getElementById('inner').dispatchEvent(new Event('ping', { bubbles: true }));
      document.body.setAttribute('data-r', seen.join(','));
    </script></body></html>"#);
    assert_eq!(r(&c), "inner");
}

/// dispatchEvent returns !defaultPrevented, and preventDefault only works on
/// a cancelable event — pages branch on both.
#[test]
fn prevent_default_only_when_cancelable() {
    let c = run(r#"<html><body><div id=t></div><script>
      var t = document.getElementById('t');
      t.addEventListener('a', function (e) { e.preventDefault(); });
      var cancelable = t.dispatchEvent(new Event('a', { cancelable: true }));
      var plain = t.dispatchEvent(new Event('a'));
      document.body.setAttribute('data-r', cancelable + '|' + plain);
    </script></body></html>"#);
    assert_eq!(r(&c), "false|true");
}

/// A bubbling event reaches document and window listeners, and they get the
/// SAME object the page built — not a second one that merely looks like it.
#[test]
fn events_reach_document_listeners_as_the_same_object() {
    let c = run(r#"<html><body><div id=t></div><script>
      var seen = 'none';
      var made = new CustomEvent('up', { bubbles: true, detail: { k: 'v' } });
      document.addEventListener('up', function (e) {
        seen = (e === made) + ':' + e.detail.k + ':' + e.target.getAttribute('id');
      });
      document.getElementById('t').dispatchEvent(made);
      document.body.setAttribute('data-r', seen);
    </script></body></html>"#);
    assert_eq!(r(&c), "true:v:t");
}

#[test]
fn remove_event_listener_really_removes() {
    let c = run(r#"<html><body><div id=t></div><script>
      var n = 0;
      var t = document.getElementById('t');
      function h() { n++; }
      t.addEventListener('x', h);
      t.dispatchEvent(new Event('x'));
      t.removeEventListener('x', h);
      t.dispatchEvent(new Event('x'));
      document.body.setAttribute('data-r', String(n));
    </script></body></html>"#);
    assert_eq!(r(&c), "1");
    assert_eq!(c.events_dispatched, 2, "both dispatches count even if one lands nowhere");
}

/// A listener that throws must not take the dispatch, or the page, with it.
#[test]
fn a_throwing_listener_does_not_break_the_dispatch() {
    let c = run(r#"<html><body><div id=t></div><script>
      var after = 'no';
      var t = document.getElementById('t');
      t.addEventListener('x', function () { throw new Error('boom'); });
      t.addEventListener('x', function () { after = 'yes'; });
      t.dispatchEvent(new Event('x'));
      document.body.setAttribute('data-r', after);
    </script></body></html>"#);
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert_eq!(r(&c), "yes");
}

/// The legacy createEvent + initEvent path: 91 and 29 references in the
/// corpus, still very much alive.
#[test]
fn legacy_create_event_and_init_event() {
    let c = run(r#"<html><body><div id=t></div><script>
      var got = 'none';
      var t = document.getElementById('t');
      t.addEventListener('legacy', function (e) { got = e.type + ':' + e.bubbles; });
      var e = document.createEvent('CustomEvent');
      e.initEvent('legacy', true, false);
      t.dispatchEvent(e);
      document.body.setAttribute('data-r', got);
    </script></body></html>"#);
    assert_eq!(r(&c), "legacy:true");
}

/// A script-made event is never trusted, and pages check that.
#[test]
fn constructed_events_are_not_trusted() {
    let c = run(r#"<html><body><div id=t></div><script>
      var e = new Event('x');
      document.body.setAttribute('data-r',
        e.isTrusted + '|' + (e instanceof Event) + '|' + (new CustomEvent('y')).type);
    </script></body></html>"#);
    assert_eq!(r(&c), "false|true|y");
}
