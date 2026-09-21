use navigator_prerender::{boa_impl::BoaEngine, convert_with, fetch::NoNetwork};

fn run(html: &str) -> navigator_prerender::Conversion {
    convert_with(html, Some("https://example.test/p.html"),
                 &mut BoaEngine::default(), &mut NoNetwork)
}
fn r(c: &navigator_prerender::Conversion) -> String {
    let m = "data-r=\"";
    let i = c.html.find(m).unwrap_or_else(|| panic!("no data-r in {}", c.html)) + m.len();
    c.html[i..].split('"').next().unwrap().to_string()
}

/// ★ A page that builds a script and appends it is RUNNING CODE, and nothing
/// executed it. Most of what the cause list had left was page-owned globals
/// (window.gl, window.session, window.useNuxtApp) set exactly this way.
#[test]
fn an_injected_inline_script_runs() {
    let c = run(r#"<html><body><div id=t></div><script>
      var s = document.createElement('script');
      s.textContent = "window.__from_injected = 'ran';";
      document.head.appendChild(s);
      document.body.setAttribute('data-r', String(window.__from_injected));
    </script></body></html>"#);
    assert_eq!(c.injected_scripts_run, 1);
    // The value is visible after the sweep, not during the injecting script.
    assert_eq!(r(&c), "undefined");
    assert!(c.html.contains("__from_injected"), "{}", c.html);
}

/// ★ ORDERING: a browser runs an injected script when it is appended, which
/// is BEFORE any timer already queued. Sweeping after the drain instead let a
/// setTimeout read a global the injected script had not defined yet.
#[test]
fn an_injected_script_runs_before_already_queued_timers() {
    let c = run(r#"<html><body><div id=t></div><script>
      var s = document.createElement('script');
      s.textContent = "window.__v = 'injected';";
      document.head.appendChild(s);
      setTimeout(function () {
        document.body.setAttribute('data-r', String(window.__v));
      }, 0);
    </script></body></html>"#);
    assert_eq!(r(&c), "injected");
}

/// It feeds itself: an injected script may inject another, and a timer may
/// inject one too. Bounded rounds, so a page that does this forever settles.
#[test]
fn injection_chains_and_terminates() {
    let c = run(r#"<html><body><div id=t></div><script>
      window.__depth = 0;
      window.__inject = function () {
        window.__depth++;
        var s = document.createElement('script');
        s.textContent = "window.__inject();";
        document.head.appendChild(s);
      };
      window.__inject();
      setTimeout(function () {
        document.body.setAttribute('data-r', 'depth=' + (window.__depth > 1));
      }, 0);
    </script></body></html>"#);
    assert_eq!(r(&c), "depth=true");
    assert!(c.injected_scripts_run < 50, "unbounded: {}", c.injected_scripts_run);
}

/// ★★ AN INJECTED SRC IS REFUSED, AND THAT IS A POLICY DECISION. What the
/// corpus actually injects settles it: the src-bearing ones are overwhelmingly
/// third-party TAG LOADERS. Fetching them would execute tracker code for a
/// reader who never asked, reopening from the inside the hole the page-network
/// policy closes from the outside.
#[test]
fn an_injected_external_script_is_refused_and_counted() {
    let c = run(r#"<html><body><div id=t></div><script>
      var s = document.createElement('script');
      s.src = 'https://tag.tracker.test/s1.js';
      s.defer = true;
      document.head.appendChild(s);
      var own = document.createElement('script');
      own.textContent = "window.__own = 1;";
      document.head.appendChild(own);
    </script></body></html>"#);
    assert_eq!(c.injected_scripts_refused, 1, "the tag loader must be refused");
    assert_eq!(c.injected_scripts_run, 1, "the page's own inline code still runs");
    assert_eq!(c.page_fetches, 0, "and nothing was fetched for it");
}

/// The type filter applies to injected scripts too: JSON-LD is data, and
/// feeding it to the engine would manufacture a syntax error.
#[test]
fn injected_json_ld_is_not_executed() {
    let c = run(r#"<html><body><div id=t></div><script>
      var s = document.createElement('script');
      s.setAttribute('type', 'application/ld+json');
      s.textContent = '{"@context":"https://schema.org"}';
      document.head.appendChild(s);
      document.body.setAttribute('data-r', 'done');
    </script></body></html>"#);
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert_eq!(c.injected_scripts_run, 0);
    assert_eq!(r(&c), "done");
}

/// A script that is created but never CONNECTED must not run — appending is
/// what executes it.
#[test]
fn an_unattached_script_never_runs() {
    let c = run(r#"<html><body><div id=t></div><script>
      var s = document.createElement('script');
      s.textContent = "window.__never = 'ran';";
      // deliberately not appended
      document.body.setAttribute('data-r', String(window.__never));
    </script></body></html>"#);
    assert_eq!(c.injected_scripts_run, 0);
    assert_eq!(r(&c), "undefined");
}

/// An injected script that throws must not take the page with it.
#[test]
fn a_throwing_injected_script_is_contained() {
    let c = run(r#"<html><body><div id=t></div><script>
      var bad = document.createElement('script');
      bad.textContent = "throw new Error('boom');";
      document.head.appendChild(bad);
      var good = document.createElement('script');
      good.textContent = "document.body.setAttribute('data-r', 'still here');";
      document.head.appendChild(good);
    </script></body></html>"#);
    assert_eq!(c.scripts_failed, 0, "the page's own scripts all ran: {:?}", c.errors);
    assert_eq!(r(&c), "still here");
}
