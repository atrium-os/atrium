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
