use navigator_prerender::{boa_impl::BoaEngine, convert_with, fetch::{MapFetcher, NoNetwork}};

fn api(pairs: &[(&str, &str)]) -> MapFetcher {
    let mut m = MapFetcher::default();
    for (k, v) in pairs { m.0.insert((*k).into(), (*v).into()); }
    m
}

fn run(html: &str, net: Option<MapFetcher>) -> navigator_prerender::Conversion {
    let mut eng = BoaEngine {
        page_fetcher: net.map(|n| Box::new(n) as Box<dyn navigator_prerender::fetch::Fetcher>),
        ..Default::default()
    };
    convert_with(html, Some("https://example.test/p.html"), &mut eng, &mut NoNetwork)
}

/// The case XHR exists for: pre-fetch-era content loading. Without it these
/// pages produce an empty shell exactly as an SPA does without fetch.
#[test]
fn xhr_content_reaches_the_artifact() {
    let c = run(r#"<html><body><ul id=list></ul><script>
        var x = new XMLHttpRequest();
        x.open('GET', '/api/items');
        x.onload = function () {
          var d = JSON.parse(x.responseText);
          var ul = document.getElementById('list');
          for (var i = 0; i < d.items.length; i++) {
            var li = document.createElement('li');
            li.textContent = d.items[i];
            ul.appendChild(li);
          }
        };
        x.send();
      </script></body></html>"#,
      Some(api(&[("https://example.test/api/items", r#"{"items":["alpha","beta"]}"#)])));
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert_eq!(c.page_fetches, 1, "XHR must be counted as page network");
    assert!(c.html.contains("alpha") && c.html.contains("beta"), "{}", c.html);
}

/// ★ THE ORDERING PROPERTY, and the reason this was not a thin wrapper.
///
/// The network seam is synchronous; async XHR is not. Delivering onload
/// inline from send() would run it BEFORE the statement following send(),
/// inverting the order every async caller is written against. Async posts to
/// the timer queue; sync (the false argument) delivers inline. This test
/// fails if either half regresses to the other's behaviour.
#[test]
fn async_xhr_returns_before_its_callback_sync_does_not() {
    let c = run(r#"<html><body><div id=r></div><script>
        var log = '';
        var a = new XMLHttpRequest();
        a.open('GET', '/api/v', true);
        a.onload = function () { log += 'A'; };
        a.send();
        log += '1';                       // must precede A

        var s = new XMLHttpRequest();
        s.open('GET', '/api/v', false);
        s.onload = function () { log += 'S'; };
        s.send();
        log += '2';                       // must FOLLOW S

        setTimeout(function () {
          document.getElementById('r').setAttribute('data-log', log);
        }, 1);
      </script></body></html>"#,
      Some(api(&[("https://example.test/api/v", "{}")])));
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains(r#"data-log="1S2A""#),
        "async/sync delivery order wrong: {}", c.html);
}

/// A refused request is reported the way a browser reports a network error —
/// status 0 and an `error` event — not as a fabricated success.
#[test]
fn refused_request_is_a_network_error_not_a_fake_200() {
    // Cross-origin: refused by the same-origin policy, not by absence of net.
    let c = run(r#"<html><body><div id=r></div><script>
        var x = new XMLHttpRequest();
        x.open('GET', 'https://analytics.test/collect');
        x.onerror = function () {
          document.getElementById('r').setAttribute('data-err', String(x.status));
        };
        x.onload = function () {
          document.getElementById('r').setAttribute('data-ok', String(x.status));
        };
        x.send();
      </script></body></html>"#,
      Some(api(&[("https://analytics.test/collect", "1")])));
    assert!(c.html.contains(r#"data-err="0""#), "{}", c.html);
    // ★ Match the ATTRIBUTE form, not the bare name: the artifact contains
    // the inline script source too, so `data-ok` also matches the string
    // literal inside the setAttribute call that was never executed.
    assert!(!c.html.contains(r#"data-ok=""#),
        "a refusal must not present as success: {}", c.html);
    assert!(c.page_blocked >= 1, "refusal must be counted: {}", c.page_blocked);
}

/// XHR is the OLDER telemetry transport. It inherits fetch's policy rather
/// than restating it, and this asserts the inheritance actually happened —
/// a POST to the page's own origin is still refused.
#[test]
fn xhr_post_to_own_origin_is_still_refused() {
    let c = run(r#"<html><body><div id=r></div><script>
        var x = new XMLHttpRequest();
        x.open('POST', '/log');
        x.onerror = function () {
          document.getElementById('r').setAttribute('data-blocked','1');
        };
        x.send();
      </script></body></html>"#,
      Some(api(&[("https://example.test/log", "ok")])));
    assert!(c.html.contains(r#"data-blocked="1""#), "{}", c.html);
    assert_eq!(c.page_fetches, 0, "a refused POST is not a fetch");
}

/// readyState transitions and the event sequence pages actually branch on.
#[test]
fn readystate_and_event_sequence() {
    let c = run(r#"<html><body><div id=r></div><script>
        var seq = '';
        var x = new XMLHttpRequest();
        seq += x.readyState;                       // 0 UNSENT
        x.onreadystatechange = function () { seq += 'r' + x.readyState; };
        x.addEventListener('load', function () { seq += 'L'; });
        x.addEventListener('loadend', function () { seq += 'E'; });
        x.open('GET', '/api/v');                   // -> 1 OPENED
        x.send();
        setTimeout(function () {
          document.getElementById('r').setAttribute('data-seq', seq);
        }, 1);
      </script></body></html>"#,
      Some(api(&[("https://example.test/api/v", "{}")])));
    assert!(c.html.contains(r#"data-seq="0r1r4LE""#), "{}", c.html);
}

/// abort() before the queued settle must cancel it — otherwise a page that
/// aborts on teardown gets a callback after it has torn down.
#[test]
fn abort_cancels_the_queued_delivery() {
    let c = run(r#"<html><body><div id=r>base</div><script>
        var x = new XMLHttpRequest();
        x.open('GET', '/api/v');
        x.onload = function () {
          document.getElementById('r').setAttribute('data-late','1');
        };
        x.send();
        x.abort();
      </script></body></html>"#,
      Some(api(&[("https://example.test/api/v", "{}")])));
    // Attribute form — see the note in the refusal test above.
    assert!(!c.html.contains(r#"data-late=""#),
        "aborted XHR still delivered: {}", c.html);
}

/// ★ Recorded limit: the seam does not retain response headers, so XHR
/// reports none. Accurate-and-empty beats plausible-and-invented — a page
/// branching on content-type takes its no-header path instead of a wrong one.
#[test]
fn response_headers_are_absent_not_invented() {
    let c = run(r#"<html><body><div id=r></div><script>
        var x = new XMLHttpRequest();
        x.open('GET', '/api/v');
        x.onload = function () {
          document.getElementById('r').setAttribute('data-h',
            String(x.getResponseHeader('content-type')) + '/' +
            JSON.stringify(x.getAllResponseHeaders()));
        };
        x.send();
      </script></body></html>"#,
      Some(api(&[("https://example.test/api/v", "{}")])));
    assert!(c.html.contains(r#"data-h="null/&quot;&quot;""#), "{}", c.html);
}
