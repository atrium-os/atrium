use navigator_prerender::{boa_impl::BoaEngine, convert_with, fetch::NoNetwork};

fn explore(html: &str) -> navigator_prerender::Conversion {
    let mut eng = BoaEngine { explore: true, ..Default::default() };
    convert_with(html, Some("https://example.test/p.html"), &mut eng, &mut NoNetwork)
}
fn plain(html: &str) -> navigator_prerender::Conversion {
    convert_with(html, Some("https://example.test/p.html"),
                 &mut BoaEngine::default(), &mut NoNetwork)
}

/// ★ THE PAGE'S OWN CODE IS THE ORACLE. Nothing is guessed about which
/// elements do something — these are the handlers the page actually
/// registered, and the recording says what each one DOES.
#[test]
fn records_what_an_interactive_element_does() {
    let c = explore(r#"<html><body>
      <button id=more>Show more</button>
      <div id=panel></div>
      <script>
        document.getElementById('more').addEventListener('click', function () {
          document.getElementById('panel').innerHTML =
            '<p>revealed content that was hidden behind the button</p>';
        });
      </script></body></html>"#);
    assert_eq!(c.interactive_found, 1);
    assert_eq!(c.transitions.len(), 1, "{:?}", c.transitions);
    let t = &c.transitions[0];
    assert_eq!(t.trigger, "#more");
    assert_eq!(t.event, "click");
    assert!(t.anchored, "the button is in the tier 1 document");
    assert!(t.text_delta > 0 && t.elements_added > 0, "{t:?}");
}

/// ★★ EVERY PROBE IS REVERTED. Exploring must not leave the document in a
/// state no reader reached — the artifact has to be identical to one produced
/// without exploring at all.
#[test]
fn exploring_does_not_change_the_artifact() {
    let html = r#"<html><body>
      <button id=b>go</button><div id=p>original</div>
      <script>
        document.getElementById('b').addEventListener('click', function () {
          document.getElementById('p').textContent = 'mutated by the probe';
        });
      </script></body></html>"#;
    let explored = explore(html);
    let untouched = plain(html);
    assert_eq!(explored.html, untouched.html,
        "exploration leaked into the artifact");
    // ★ Look only AFTER the script element. The artifact carries the script
    // SOURCE, which contains this very string as a literal — searching the
    // whole document finds the code under test and reports a leak that is
    // not there. Fifth time this session; the equality above is the real
    // assertion, this one guards the specific value.
    let body = &explored.html[explored.html.find("</script>").unwrap()..];
    assert!(!body.contains("mutated by the probe"), "{}", explored.html);
    assert_eq!(explored.transitions.len(), 1, "but it still learned what the button does");
}

/// An element the SCRIPTS created cannot be replayed against tier 1. Saying
/// so is more useful than dropping the transition.
#[test]
fn transitions_on_script_made_elements_are_marked_unanchored() {
    let c = explore(r#"<html><body><div id=host></div>
      <script>
        var b = document.createElement('button');
        document.getElementById('host').appendChild(b);
        b.addEventListener('click', function () {
          var d = document.createElement('p');
          d.textContent = 'from a button the script itself built';
          document.getElementById('host').appendChild(d);
        });
      </script></body></html>"#);
    assert_eq!(c.transitions.len(), 1, "{:?}", c.transitions);
    assert!(!c.transitions[0].anchored,
        "the trigger did not come from the parser: {:?}", c.transitions[0]);
}

/// A handler that changes nothing is not a state transition. The denominator
/// is still reported, so "found many, learned little" is visible.
#[test]
fn handlers_that_do_nothing_are_found_but_not_recorded() {
    let c = explore(r#"<html><body>
      <button id=a>a</button><button id=b>b</button>
      <script>
        document.getElementById('a').addEventListener('click', function () {});
        document.getElementById('b').addEventListener('click', function () {
          document.body.setAttribute('data-x', '1');
        });
      </script></body></html>"#);
    assert_eq!(c.interactive_found, 2, "both were found");
    assert_eq!(c.transitions.len(), 1, "only one does anything: {:?}", c.transitions);
    assert_eq!(c.transitions[0].trigger, "#b");
}

/// ★★ THE NETWORK IS CLOSED WHILE EXPLORING. A simulated click must not be
/// able to reach anything — destructive GETs exist. Prerendering may fetch;
/// exploration may not.
#[test]
fn a_probe_cannot_reach_the_network() {
    use navigator_prerender::fetch::MapFetcher;
    let mut api = MapFetcher::default();
    api.0.insert("https://example.test/delete".into(), "deleted!".into());
    let mut eng = BoaEngine {
        explore: true,
        page_fetcher: Some(Box::new(api)),
        ..Default::default()
    };
    let c = convert_with(r#"<html><body><button id=b>x</button><div id=o></div>
      <script>
        document.getElementById('b').addEventListener('click', function () {
          fetch('/delete').then(function (r) { return r.text(); }).then(function (t) {
            document.getElementById('o').textContent = t;
          });
        });
        // The page's own load-time fetch is allowed; only the PROBE is not.
        fetch('/delete').then(function () {});
      </script></body></html>"#,
      Some("https://example.test/p.html"), &mut eng, &mut NoNetwork);

    assert_eq!(c.page_fetches, 1,
        "the page's own fetch happens; the probe's must not add to it");
    assert!(!c.html.contains("deleted!"));
}

/// Probing is bounded: a page with many handlers must not explore forever.
#[test]
fn exploration_is_bounded() {
    let mut html = String::from("<html><body>");
    for i in 0..300 { html.push_str(&format!("<button id=b{i}>b</button>")); }
    html.push_str(r#"<div id=o></div><script>
      var bs = document.getElementsByTagName('button');
      for (var i = 0; i < bs.length; i++) {
        bs[i].addEventListener('click', function () {
          document.getElementById('o').appendChild(document.createElement('p'));
        });
      }
    </script></body></html>"#);
    let c = explore(&html);
    assert_eq!(c.interactive_found, 300, "all are found");
    assert!(c.transitions.len() <= 64,
        "but probing is capped: {}", c.transitions.len());
}

/// Recording must be DETERMINISTIC, or a content-addressed store sees churn
/// for a document that has not changed.
#[test]
fn the_recording_is_reproducible() {
    let html = r#"<html><body>
      <button id=x>x</button><button id=y>y</button><div id=o></div>
      <script>
        ['x','y'].forEach(function (id) {
          document.getElementById(id).addEventListener('click', function () {
            document.getElementById('o').appendChild(document.createElement('p'));
          });
        });
      </script></body></html>"#;
    let a = explore(html);
    let b = explore(html);
    assert_eq!(a.transitions, b.transitions);
    assert!(a.transitions.len() == 2, "{:?}", a.transitions);
}

use navigator_prerender::Effect;

/// ★ THE COMMONEST MENU ON THE WEB IS A CLASS TOGGLE, and it needs NO content
/// captured: whatever it reveals is already in the tier 1 document, merely
/// hidden. The transition is one attribute write, wholly replayable.
#[test]
fn a_class_toggle_is_recorded_as_an_attribute_write_only() {
    let c = explore(r#"<html><body>
      <button id=toggle>Menu</button>
      <nav id=menu class="nav hidden"><a href=/a>One</a><a href=/b>Two</a></nav>
      <script>
        document.getElementById('toggle').addEventListener('click', function () {
          document.getElementById('menu').classList.remove('hidden');
        });
      </script></body></html>"#);
    assert_eq!(c.transitions.len(), 1, "{:?}", c.transitions);
    let t = &c.transitions[0];
    assert!(t.is_attribute_only(), "{:?}", t.effects);
    match &t.effects[0] {
        Effect::Attribute { target, name, from, to } => {
            assert_eq!(target, "#menu");
            assert_eq!(name, "class");
            assert_eq!(from.as_deref(), Some("nav hidden"));
            assert_eq!(to.as_deref(), Some("nav"));
        }
        other => panic!("expected an attribute write, got {other:?}"),
    }
}

/// ★ And it is recorded even though element and text counts are IDENTICAL —
/// counting alone would have discarded the commonest case entirely.
#[test]
fn a_toggle_is_recorded_despite_unchanged_counts() {
    let c = explore(r#"<html><body>
      <button id=b>x</button><div id=p class=a>content</div>
      <script>
        document.getElementById('b').addEventListener('click', function () {
          document.getElementById('p').setAttribute('class', 'a open');
        });
      </script></body></html>"#);
    assert_eq!(c.transitions.len(), 1);
    assert_eq!(c.transitions[0].elements_added, 0);
    assert_eq!(c.transitions[0].text_delta, 0);
    assert!(c.transitions[0].is_attribute_only());
}

/// Content that does NOT exist until the click has to be carried, anchored to
/// where it belongs in the tier 1 document.
#[test]
fn inserted_content_is_captured_with_its_anchor() {
    let c = explore(r#"<html><body>
      <button id=more>More</button><div id=panel></div>
      <script>
        document.getElementById('more').addEventListener('click', function () {
          document.getElementById('panel').innerHTML = '<p class=x>revealed</p>';
        });
      </script></body></html>"#);
    let t = &c.transitions[0];
    assert!(!t.is_attribute_only());
    let ins: Vec<_> = t.effects.iter().filter_map(|e| match e {
        Effect::Insert { parent, html, .. } => Some((parent.as_str(), html.as_str())),
        _ => None,
    }).collect();
    assert_eq!(ins.len(), 1, "{:?}", t.effects);
    assert_eq!(ins[0].0, "#panel", "anchored where it belongs");
    assert_eq!(ins[0].1, r#"<p class="x">revealed</p>"#);
}

/// A removal names only the TOP of the removed subtree: listing every
/// descendant would bury the one fact a replay needs.
#[test]
fn a_removal_names_only_the_subtree_root() {
    let c = explore(r#"<html><body>
      <button id=b>hide</button>
      <div id=box><p>one</p><p>two</p></div>
      <script>
        document.getElementById('b').addEventListener('click', function () {
          document.getElementById('box').remove();
        });
      </script></body></html>"#);
    let t = &c.transitions[0];
    let rm: Vec<_> = t.effects.iter().filter_map(|e| match e {
        Effect::Remove { target } => Some(target.as_str()), _ => None,
    }).collect();
    assert_eq!(rm, vec!["#box"], "{:?}", t.effects);
}

/// ★ A recording is an ANNOTATION on the tier 1 document. A transition that
/// shipped a whole page would quietly turn it back into a second artifact, so
/// oversized effects are dropped and the drop is COUNTED — a truncated
/// recording must never be mistaken for a small one.
#[test]
fn oversized_effects_are_truncated_and_say_so() {
    let c = explore(r#"<html><body>
      <button id=b>x</button><div id=p></div>
      <script>
        document.getElementById('b').addEventListener('click', function () {
          var big = new Array(4000).join('some长 repeated filler text ');
          document.getElementById('p').innerHTML = '<p>' + big + '</p>';
        });
      </script></body></html>"#);
    let t = &c.transitions[0];
    assert!(t.effects.iter().any(|e| matches!(e, Effect::Truncated { .. })),
        "oversized insert must be reported as truncated: {:?}", t.effects);
    assert!(!t.effects.iter().any(|e| matches!(e, Effect::Insert { .. })),
        "and must not be carried");
}

/// ★ VERSION 2 OF THE RECORDING FORMAT: an insert records WHERE it went.
///
/// Version 1 recorded only the parent, so a replay could do nothing but
/// append — a row inserted into the middle of a list came back at the end,
/// with nothing to report the difference. The index is measured against the
/// AFTER tree, which is why removals are applied before insertions on replay:
/// by then the indices mean what they meant when they were recorded.
#[test]
fn an_insert_records_the_position_it_landed_at() {
    let c = explore(r#"<html><body><ul id="l"><li>a</li><li>b</li><li>c</li></ul>
      <button id="go">go</button><script>
        document.getElementById('go').addEventListener('click', function () {
          var ul = document.getElementById('l');
          var li = document.createElement('li');
          li.textContent = 'inserted';
          ul.insertBefore(li, ul.children[1]);
        });
      </script></body></html>"#);

    let inserts: Vec<(&usize, &str)> = c.transitions.iter()
        .flat_map(|t| &t.effects)
        .filter_map(|e| match e {
            Effect::Insert { index, html, .. } => Some((index, html.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(inserts.len(), 1, "expected one insert, got {inserts:?}");
    assert!(inserts[0].1.contains("inserted"), "{:?}", inserts[0]);
    assert_eq!(*inserts[0].0, 1,
        "the insert went between the first and second item, not at the end: {inserts:?}");
}
