use navigator_prerender::{boa_impl::BoaEngine, convert_with, fetch::NoNetwork};

fn run(html: &str) -> navigator_prerender::Conversion {
    convert_with(html, Some("https://example.test/docs/a.html"),
                 &mut BoaEngine::default(), &mut NoNetwork)
}

fn r(c: &navigator_prerender::Conversion) -> String {
    let m = "data-r=\"";
    let i = c.html.find(m).unwrap_or_else(|| panic!("no data-r in {}", c.html)) + m.len();
    c.html[i..].split('"').next().unwrap().to_string()
}

/// ★ A history write really does move `location`. Accepting pushState and
/// then reporting the old URL is worse than refusing: a script that pushes
/// and then builds paths from location would compute them against a URL its
/// own code believes it has left.
#[test]
fn push_state_moves_location_and_document_url() {
    let c = run(r#"<html><body><div id=t></div><script>
      history.pushState({ n: 1 }, '', 'b.html?q=2');
      document.body.setAttribute('data-r', [
        location.pathname, location.search, history.state.n,
        history.length, document.URL
      ].join('|'));
    </script></body></html>"#);
    assert_eq!(r(&c), "/docs/b.html|?q=2|1|2|https://example.test/docs/b.html?q=2");
    assert_eq!(c.history_writes, 1);
}

/// replaceState does not grow the stack — it is the commonest call in the
/// corpus (66 references) and is normally URL normalisation during load.
#[test]
fn replace_state_rewrites_without_growing_the_stack() {
    let c = run(r#"<html><body><div id=t></div><script>
      history.replaceState({ a: 1 }, '', 'c.html');
      history.replaceState({ a: 2 }, '', 'd.html');
      document.body.setAttribute('data-r',
        history.length + '|' + location.pathname + '|' + history.state.a);
    </script></body></html>"#);
    assert_eq!(r(&c), "1|/docs/d.html|2");
    assert_eq!(c.history_writes, 2);
}

/// Going back to an entry WE pushed is genuinely same-document: no request,
/// no new document. So popstate here is real rather than invented.
#[test]
fn back_to_a_pushed_entry_is_real_and_fires_popstate() {
    let c = run(r#"<html><body><div id=t></div><script>
      var seen = [];
      window.addEventListener('popstate', function (e) {
        seen.push(e.state ? e.state.page : 'null');
      });
      history.pushState({ page: 'two' }, '', 'two.html');
      history.pushState({ page: 'three' }, '', 'three.html');
      history.back();
      document.body.setAttribute('data-r',
        location.pathname + '|' + history.state.page + '|' + seen.join(','));
    </script></body></html>"#);
    assert_eq!(r(&c), "/docs/two.html|two|two");
    assert_eq!(c.history_refused, 0);
}

#[test]
fn forward_returns_to_the_later_entry() {
    let c = run(r#"<html><body><div id=t></div><script>
      history.pushState({}, '', 'two.html');
      history.back();
      history.forward();
      document.body.setAttribute('data-r', location.pathname);
    </script></body></html>"#);
    assert_eq!(r(&c), "/docs/two.html");
}

/// ★ What it must NOT do is leave the document. Going back past our own
/// first entry is a real navigation, go(0) is a reload. Both are refused and
/// COUNTED — the cost is visible rather than silently swallowed.
#[test]
fn navigations_that_would_leave_the_document_are_refused_and_counted() {
    let c = run(r#"<html><body><div id=t></div><script>
      var fired = 0;
      window.addEventListener('popstate', function () { fired++; });
      history.back();        // nothing pushed: would leave the document
      history.forward();     // nothing ahead
      history.go(0);         // a reload
      history.go(-5);
      document.body.setAttribute('data-r',
        location.pathname + '|' + history.length + '|' + fired);
    </script></body></html>"#);
    assert_eq!(r(&c), "/docs/a.html|1|0");
    assert_eq!(c.history_refused, 4, "every refusal must be counted");
    assert_eq!(c.history_writes, 0);
}

/// Cross-origin history writing is a SecurityError in a browser, and a page
/// that relies on that to detect a sandbox must get the real answer.
#[test]
fn cross_origin_write_is_a_security_error() {
    let c = run(r#"<html><body><div id=t></div><script>
      var m = 'none';
      try { history.pushState({}, '', 'https://evil.test/x'); m = 'allowed'; }
      catch (e) { m = 'threw'; }
      document.body.setAttribute('data-r', m + '|' + location.host);
    </script></body></html>"#);
    assert_eq!(r(&c), "threw|example.test");
    assert_eq!(c.history_writes, 0);
}

/// A push after going back truncates the forward entries, as the DOM says.
#[test]
fn pushing_after_back_truncates_the_forward_entries() {
    let c = run(r#"<html><body><div id=t></div><script>
      history.pushState({}, '', 'two.html');
      history.pushState({}, '', 'three.html');
      history.back();                       // at two
      history.pushState({}, '', 'four.html');
      var afterPush = history.length;
      history.forward();                    // nothing ahead now
      document.body.setAttribute('data-r', afterPush + '|' + location.pathname);
    </script></body></html>"#);
    assert_eq!(r(&c), "3|/docs/four.html");
}

/// A bare pushState with no URL keeps the current one and only sets state —
/// a common idiom for storing scroll or filter state.
#[test]
fn state_only_write_keeps_the_url() {
    let c = run(r#"<html><body><div id=t></div><script>
      history.replaceState({ scroll: 42 }, '');
      document.body.setAttribute('data-r',
        location.pathname + '|' + history.state.scroll + '|' + history.scrollRestoration);
    </script></body></html>"#);
    assert_eq!(r(&c), "/docs/a.html|42|auto");
}

/// scrollRestoration is settable and readable; pages set it during load.
#[test]
fn scroll_restoration_round_trips() {
    let c = run(r#"<html><body><div id=t></div><script>
      history.scrollRestoration = 'manual';
      document.body.setAttribute('data-r', history.scrollRestoration);
    </script></body></html>"#);
    assert_eq!(r(&c), "manual");
}
