use navigator_prerender::{boa_impl::BoaEngine, convert_with, fetch::{MapFetcher, NoNetwork}};

/// An SPA that loads its content over the network is the case tier 2 exists
/// for: without fetch it produces an empty shell.
#[test]
fn page_fetch_produces_content() {
    let mut scripts = MapFetcher::default();
    let mut api = MapFetcher::default();
    api.0.insert("https://example.test/api/items".into(),
                 r#"{"items":["alpha","beta"]}"#.into());
    let html = r#"<html><body><ul id="list"></ul><script>
        fetch('/api/items').then(function (r) { return r.json(); }).then(function (d) {
          var ul = document.getElementById('list');
          for (var i = 0; i < d.items.length; i++) {
            var li = document.createElement('li');
            li.textContent = d.items[i];
            ul.appendChild(li);
          }
        });
      </script></body></html>"#;
    let mut eng = BoaEngine { page_fetcher: Some(Box::new(api)), ..Default::default() };
    let c = convert_with(html, Some("https://example.test/p.html"), &mut eng, &mut scripts);
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert_eq!(c.page_fetches, 1, "the page's request must be counted separately");
    assert!(c.html.contains("alpha") && c.html.contains("beta"),
        "fetched content did not reach the artifact: {}", c.html);
}

/// With no page network configured, fetch REJECTS — it does not hang, and it
/// does not silently resolve with nothing.
#[test]
fn fetch_without_network_rejects_and_is_counted() {
    let html = r#"<html><body><div id=r>base</div><script>
        fetch('/api/x').then(function () {
          document.getElementById('r').setAttribute('ok','1');
        }).catch(function () {
          document.getElementById('r').setAttribute('rejected','1');
        });
      </script></body></html>"#;
    let c = convert_with(html, Some("https://example.test/p.html"),
                         &mut BoaEngine::default(), &mut NoNetwork);
    assert_eq!(c.scripts_failed, 0, "a rejection is not a script failure: {:?}", c.errors);
    assert!(c.html.contains(r#"rejected="1""#), "must reject: {}", c.html);
    assert!(!c.html.contains(r#"ok="1""#));
    assert_eq!(c.page_fetch_failures, 1);
}

/// Relative URLs resolve against the document, as they do in a browser.
#[test]
fn fetch_resolves_relative_urls() {
    let mut api = MapFetcher::default();
    api.0.insert("https://example.test/a/data.json".into(), r#"{"v":7}"#.into());
    let html = r#"<html><body><div id=r></div><script>
        fetch('./data.json').then(function (r) { return r.json(); }).then(function (d) {
          document.getElementById('r').setAttribute('v', String(d.v));
        });
      </script></body></html>"#;
    let mut eng = BoaEngine { page_fetcher: Some(Box::new(api)), ..Default::default() };
    let c = convert_with(html, Some("https://example.test/a/page.html"), &mut eng, &mut NoNetwork);
    assert!(c.html.contains(r#"v="7""#), "relative fetch failed: {}", c.html);
}

/// Content arriving through a fetch inside a deferred callback still lands —
/// the timer drain and the promise queue have to cooperate.
#[test]
fn fetch_inside_a_timer_still_lands() {
    let mut api = MapFetcher::default();
    api.0.insert("https://e.test/late.json".into(), r#"{"t":"late"}"#.into());
    let html = r#"<html><body><div id=r></div><script>
        setTimeout(function () {
          fetch('/late.json').then(function (r) { return r.json(); }).then(function (d) {
            document.getElementById('r').setAttribute('t', d.t);
          });
        }, 5);
      </script></body></html>"#;
    let mut eng = BoaEngine { page_fetcher: Some(Box::new(api)), ..Default::default() };
    let c = convert_with(html, Some("https://e.test/p.html"), &mut eng, &mut NoNetwork);
    assert!(c.html.contains(r#"t="late""#), "deferred fetch did not land: {}", c.html);
}

/// The policy: same-origin GET only. Telemetry is overwhelmingly cross-origin
/// or POST, and a converter must not emit it on behalf of a reader who does
/// not exist.
#[test]
fn cross_origin_and_non_get_are_refused_and_counted() {
    let mut api = MapFetcher::default();
    api.0.insert("https://example.test/same.json".into(), r#"{"v":1}"#.into());
    api.0.insert("https://analytics.example.net/collect".into(), "ok".into());
    let html = r#"<html><body><div id=r></div><script>
        fetch('/same.json').then(function(r){ return r.json(); }).then(function(d){
          document.getElementById('r').setAttribute('same', String(d.v));
        });
        fetch('https://analytics.example.net/collect').catch(function(){
          document.getElementById('r').setAttribute('xo','refused');
        });
        fetch('/collect', { method: 'POST' }).catch(function(){
          document.getElementById('r').setAttribute('post','refused');
        });
      </script></body></html>"#;
    let mut eng = BoaEngine { page_fetcher: Some(Box::new(api)), ..Default::default() };
    let c = convert_with(html, Some("https://example.test/p.html"), &mut eng, &mut NoNetwork);
    assert!(c.html.contains(r#"same="1""#), "same-origin GET must work: {}", c.html);
    assert!(c.html.contains(r#"xo="refused""#), "cross-origin must be refused: {}", c.html);
    assert!(c.html.contains(r#"post="refused""#), "POST must be refused: {}", c.html);
    assert_eq!(c.page_fetches, 1);
    assert_eq!(c.page_blocked, 2, "both refusals must be counted");
    assert!(c.blocked_hosts.iter().any(|(k, _)| k.contains("cross-origin")),
        "refusals must say where they aimed: {:?}", c.blocked_hosts);
    assert!(c.blocked_hosts.iter().any(|(k, _)| k.starts_with("POST")));
}

/// sendBeacon is telemetry by definition — there is no response to use. It
/// reports success so a page's teardown does not break, and sends nothing.
#[test]
fn beacons_are_suppressed_not_sent() {
    let html = r#"<html><body><div id=r></div><script>
        var ok = navigator.sendBeacon('https://t.example.net/x', 'data');
        document.getElementById('r').setAttribute('claimed', String(ok));
      </script></body></html>"#;
    let c = convert_with(html, Some("https://example.test/p.html"),
                         &mut BoaEngine::default(), &mut NoNetwork);
    assert!(c.html.contains(r#"claimed="true""#), "must not break teardown: {}", c.html);
    assert_eq!(c.beacons_suppressed, 1, "and must be counted");
    assert_eq!(c.page_fetches, 0, "nothing may go out");
}
