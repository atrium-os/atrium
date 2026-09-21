use navigator_prerender::{boa_impl::BoaEngine, convert, dom, parse};

/// The whole point: an SPA shell whose content exists only after script runs.
#[test]
fn spa_shell_becomes_static_content() {
    let html = r#"<html><body><div id="root"></div>
      <script>
        var r = document.getElementById('root');
        var h = document.createElement('h1');
        h.textContent = 'Hello from script';
        r.appendChild(h);
        var p = document.createElement('p');
        p.setAttribute('class', 'lede');
        p.textContent = 'Body text';
        r.appendChild(p);
      </script></body></html>"#;
    let c = convert(html, &mut BoaEngine::default());
    assert_eq!(c.scripts_failed, 0, "errors: {:?}", c.errors);
    assert!(c.html.contains("Hello from script"), "got: {}", c.html);
    assert!(c.html.contains(r#"class="lede""#), "got: {}", c.html);
    assert!(c.elements_after > c.elements_before, "{} -> {}", c.elements_before, c.elements_after);
    assert!(c.script_mutations >= 4);
}

#[test]
fn textcontent_reads_back() {
    let html = "<html><body><div id=a>original</div><script>\
        var d=document.getElementById('a'); d.textContent = d.textContent + ' + appended';\
        </script></body></html>";
    let c = convert(html, &mut BoaEngine::default());
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains("original + appended"), "got: {}", c.html);
}

#[test]
fn query_and_attributes() {
    let html = "<html><body><p>one</p><p>two</p><script>\
        var ps=document.querySelectorAll('p');\
        for (var i=0;i<ps.length;i++) ps[i].setAttribute('data-i', String(i));\
        </script></body></html>";
    let c = convert(html, &mut BoaEngine::default());
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains(r#"data-i="0""#) && c.html.contains(r#"data-i="1""#), "got: {}", c.html);
}

/// A failing script must not lose the document — conversion degrades to the
/// static content rather than throwing it away.
#[test]
fn script_failure_keeps_the_document() {
    let html = "<html><body><p>static</p><script>noSuchApi.doThing();</script></body></html>";
    let c = convert(html, &mut BoaEngine::default());
    assert_eq!(c.scripts_failed, 1);
    assert!(!c.errors.is_empty());
    assert!(c.html.contains("static"), "got: {}", c.html);
}

/// Determinism: same bytes in, same bytes out (G1's instrument-side analogue).
#[test]
fn conversion_is_deterministic() {
    let html = "<html><body><div id=r></div><script>\
        for(var i=0;i<20;i++){var e=document.createElement('span');e.textContent='n'+i;\
        document.getElementById('r').appendChild(e);}</script></body></html>";
    let a = convert(html, &mut BoaEngine::default()).html;
    let b = convert(html, &mut BoaEngine::default()).html;
    let c = convert(html, &mut BoaEngine::default()).html;
    assert_eq!(a, b); assert_eq!(b, c);
}

/// The arena must refuse a cycle rather than hang the serializer.
#[test]
fn append_cycle_refused() {
    let mut d = parse::parse("<html><body><div id=a><div id=b></div></div></body></html>");
    let a = d.by_id("a").unwrap();
    let b = d.by_id("b").unwrap();
    assert!(!d.append(b, a), "appending an ancestor into its descendant must be refused");
    assert!(d.max_depth() < 100);
}

#[test]
fn inline_scripts_are_found_in_order_and_src_skipped() {
    let d = parse::parse("<html><body><script>1</script><script src=x.js></script><script>2</script></body></html>");
    let s = dom::inline_scripts(&d);
    assert_eq!(s.len(), 2);
    assert!(s[0].contains('1') && s[1].contains('2'));
}

/// A real corpus run reported 9 syntax errors that were not engine gaps at
/// all: `application/ld+json` metadata blocks were being executed as script.
/// Data is not a program.
#[test]
fn non_javascript_script_types_are_not_executed() {
    let html = r#"<html><body>
      <script type="application/ld+json">{"@context":"https://schema.org","name":"x"}</script>
      <script type="text/template"><div>{{not js}}</div></script>
      <script>document.body.setAttribute('ran','yes');</script>
      </body></html>"#;
    let d = navigator_prerender::parse::parse(html);
    assert_eq!(navigator_prerender::dom::inline_scripts(&d).len(), 1,
        "only the real script should run");
    assert_eq!(navigator_prerender::dom::skipped_script_types(&d).len(), 2);
    let c = convert(html, &mut BoaEngine::default());
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains(r#"ran="yes""#), "got: {}", c.html);
}

#[test]
fn console_is_a_sink_not_a_failure() {
    let html = "<html><body><script>console.log('x');console.warn('y');</script></body></html>";
    let c = convert(html, &mut BoaEngine::default());
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
}

/// External script, deterministically: the fetcher seam means a test can
/// exercise the whole path with no network. `NoNetwork` is the default
/// elsewhere precisely so no test can silently acquire one.
#[test]
fn external_script_is_fetched_resolved_and_run_in_order() {
    use navigator_prerender::{convert_with, fetch::MapFetcher};
    let mut f = MapFetcher::default();
    f.0.insert("https://example.test/js/app.js".into(),
               "var made = document.createElement('p'); made.textContent='from bundle'; \
                document.body.appendChild(made);".into());
    let html = r#"<html><body><script src="/js/app.js"></script>
        <script>document.body.setAttribute('after','inline');</script></body></html>"#;
    let c = convert_with(html, Some("https://example.test/page.html"), &mut BoaEngine::default(), &mut f);
    assert_eq!(c.external_total, 1);
    assert_eq!(c.external_fetched, 1);
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains("from bundle"), "external script did not run: {}", c.html);
    assert!(c.html.contains(r#"after="inline""#), "inline script must still run: {}", c.html);
}

/// A `src` that cannot be resolved is reported, never guessed at.
#[test]
fn unresolvable_src_is_reported_not_guessed() {
    use navigator_prerender::{convert_with, fetch::MapFetcher};
    let mut f = MapFetcher::default();
    let html = r#"<html><body><p>kept</p><script src="/app.js"></script></body></html>"#;
    let c = convert_with(html, None, &mut BoaEngine::default(), &mut f); // no base
    assert_eq!(c.external_total, 1);
    assert_eq!(c.external_fetched, 0);
    assert!(c.errors.iter().any(|e| e.contains("unresolved")), "{:?}", c.errors);
    assert!(c.html.contains("kept"));
}

/// Document order across inline and external must be preserved: a bundle
/// usually defines what a later inline script calls.
#[test]
fn document_order_is_preserved_across_inline_and_external() {
    use navigator_prerender::dom::{scripts_in_order, Script};
    let d = navigator_prerender::parse::parse(
        r#"<html><body><script>1</script><script src="a.js"></script><script>2</script></body></html>"#);
    let s = scripts_in_order(&d);
    assert_eq!(s.len(), 3);
    assert!(matches!(&s[0], Script::Inline { text, .. } if text.contains('1')));
    assert!(matches!(&s[1], Script::External { href, .. } if href == "a.js"));
    assert!(matches!(&s[2], Script::Inline { text, .. } if text.contains('2')));
}

/// Attribution: a script reaching for an API we do not have must name it,
/// not merely fail. Before this, the largest failure bucket in a corpus run
/// was "TypeError: not a callable function" with no callee.
#[test]
fn missing_apis_are_named_not_just_failed() {
    let html = "<html><body><div id=a></div><script>\
        try { var x = document.currentScript; } catch (e) {}\
        try { var y = document.getElementById('a').parentNode; } catch (e) {}\
        try { var z = document.childNodes; } catch (e) {}\
        try { var w = document.fonts; } catch (e) {}\
        try { var v = document.fullscreenElement; } catch (e) {}\
        try { document.addEventListener('x', function(){}); } catch (e) {}\
        try { document.querySelector('p'); } catch (e) {}\
        try { document.getElementsByClassName('x'); } catch (e) {}\
        </script></body></html>";
    let c = convert(html, &mut BoaEngine::default());
    let names: Vec<&str> = c.missing.iter().map(|(n, _)| n.as_str()).collect();
    // still missing — these are the next items the corpus report ranks
    assert!(names.contains(&"document.fonts"), "got {names:?}");
    assert!(names.contains(&"document.fullscreenElement"), "got {names:?}");
    // ★ Implemented APIs must DISAPPEAR from the report. Each of these was
    // once the top entry; the assertions are how a regression gets caught,
    // and this test has now flagged its own obsolescence FIVE times now —
    // which is the point of it: each failure meant a gap had been closed.
    for gone in ["document.addEventListener", "document.querySelector",
                 "document.getElementsByClassName", "document.currentScript",
                 "element.classList", "element.style",
                 // tree semantics, added once the corpus proved that no
                 // amount of extra GLOBALS would move the number
                 "element.parentNode", "document.childNodes",
                 // and the collections, added once the cause list named them
                 "document.forms", "document.images"] {
        assert!(!names.contains(&gone), "{gone} is implemented and must not be reported missing: {names:?}");
    }
}

/// What exists must still pass through the probe untouched.
#[test]
fn probe_does_not_break_working_apis() {
    let html = "<html><body><div id=a></div><script>\
        var d=document.getElementById('a'); d.setAttribute('k','v'); d.textContent='t';\
        </script></body></html>";
    let c = convert(html, &mut BoaEngine::default());
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains(r#"k="v""#) && c.html.contains('t'), "got {}", c.html);
    assert!(!c.missing.iter().any(|(n, _)| n.ends_with("getElementById")));
}

/// The point of addEventListener is the FIRING. A page whose content is built
/// in a DOMContentLoaded handler must come out with that content.
#[test]
fn dom_content_loaded_handler_runs_and_produces_content() {
    let html = r#"<html><body><div id="root"></div><script>
        document.addEventListener('DOMContentLoaded', function () {
          var h = document.createElement('h1');
          h.textContent = 'built on ready';
          document.getElementById('root').appendChild(h);
        });
      </script></body></html>"#;
    let c = convert(html, &mut BoaEngine::default());
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.listeners_fired >= 1, "no listener fired");
    assert!(c.html.contains("built on ready"), "handler did not build content: {}", c.html);
}

#[test]
fn window_onload_and_load_listeners_run() {
    let html = r#"<html><body><div id=r></div><script>
        window.onload = function () { document.getElementById('r').setAttribute('onload','yes'); };
        window.addEventListener('load', function () { document.getElementById('r').setAttribute('lis','yes'); });
      </script></body></html>"#;
    let c = convert(html, &mut BoaEngine::default());
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains(r#"onload="yes""#), "window.onload did not run: {}", c.html);
    assert!(c.html.contains(r#"lis="yes""#), "load listener did not run: {}", c.html);
}

/// A handler that throws must not take the conversion with it.
#[test]
fn a_throwing_handler_does_not_lose_the_document() {
    let html = r#"<html><body><p>kept</p><script>
        document.addEventListener('DOMContentLoaded', function(){ missing.thing(); });
        document.addEventListener('DOMContentLoaded', function(){ document.body.setAttribute('second','ran'); });
      </script></body></html>"#;
    let c = convert(html, &mut BoaEngine::default());
    assert!(c.html.contains("kept"));
    assert!(c.html.contains(r#"second="ran""#), "a later handler must still run: {}", c.html);
}

/// readyState must move, since scripts branch on it.
#[test]
fn ready_state_progresses() {
    let html = r#"<html><body><div id=r></div><script>
        var seen = document.readyState;
        document.addEventListener('DOMContentLoaded', function(){
          document.getElementById('r').setAttribute('at-script', seen);
          document.getElementById('r').setAttribute('at-ready', document.readyState);
        });
      </script></body></html>"#;
    let c = convert(html, &mut BoaEngine::default());
    assert!(c.html.contains(r#"at-script="loading""#), "got {}", c.html);
    assert!(c.html.contains(r#"at-ready="interactive""#), "got {}", c.html);
}

/// querySelector against the real engine, from both document and element.
#[test]
fn query_selector_works_from_document_and_element() {
    let html = r#"<html><body>
      <div id="a" class="box"><p class="lede">one</p><p>two</p></div>
      <div id="b"><p class="lede">three</p></div>
      <script>
        var first = document.querySelector('#a p.lede');
        first.setAttribute('hit','1');
        var scoped = document.getElementById('b').querySelectorAll('p');
        for (var i=0;i<scoped.length;i++) scoped[i].setAttribute('scoped', String(i));
        var all = document.querySelectorAll('.lede');
        document.body.setAttribute('lede-count', String(all.length));
        var cls = document.getElementsByClassName('box');
        document.body.setAttribute('box-count', String(cls.length));
      </script></body></html>"#;
    let c = convert(html, &mut BoaEngine::default());
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains(r#"hit="1""#), "descendant selector failed: {}", c.html);
    assert!(c.html.contains(r#"scoped="0""#), "element-scoped query failed: {}", c.html);
    assert!(c.html.contains(r#"lede-count="2""#), "got {}", c.html);
    assert!(c.html.contains(r#"box-count="1""#), "got {}", c.html);
    assert!(!c.missing.iter().any(|(n, _)| n.ends_with("querySelector")));
}

/// Element-scoped queries must not escape their subtree.
#[test]
fn element_query_is_scoped_to_its_subtree() {
    let html = r#"<html><body><div id=a><p>in</p></div><p id=out>out</p><script>
        var n = document.getElementById('a').querySelectorAll('p');
        document.body.setAttribute('n', String(n.length));
      </script></body></html>"#;
    let c = convert(html, &mut BoaEngine::default());
    assert!(c.html.contains(r#"n="1""#), "scope leaked: {}", c.html);
}

/// An unparseable selector loses the query, not the document.
#[test]
fn bad_selector_returns_empty_rather_than_throwing() {
    let html = r#"<html><body><p>kept</p><script>
        var n = document.querySelectorAll('###');
        document.body.setAttribute('n', String(n.length));
      </script></body></html>"#;
    let c = convert(html, &mut BoaEngine::default());
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains(r#"n="0""#) && c.html.contains("kept"), "got {}", c.html);
}

/// A `type="module"` script must evaluate, and its top-level body must have
/// run by the time the conversion finishes.
#[test]
fn module_scripts_evaluate() {
    let html = r#"<html><body><div id=r></div>
      <script type="module">
        const el = document.createElement('p');
        el.textContent = 'from module';
        document.getElementById('r').appendChild(el);
        export const unused = 1;
      </script></body></html>"#;
    let c = convert(html, &mut BoaEngine::default());
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains("from module"), "module body did not run: {}", c.html);
}

/// A classic script carrying module-only syntax is retried as a module,
/// because pages do mislabel their goal type and a converter should not lose
/// the bundle over it.
#[test]
fn classic_script_with_export_is_retried_as_module() {
    let html = r#"<html><body><div id=r></div>
      <script>
        const el = document.createElement('p');
        el.textContent = 'retried';
        document.getElementById('r').appendChild(el);
        export {};
      </script></body></html>"#;
    let c = convert(html, &mut BoaEngine::default());
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert_eq!(c.module_retries, 1, "should have been retried as a module");
    assert!(c.html.contains("retried"), "got {}", c.html);
}

/// A module whose import cannot be resolved is REPORTED, not silently
/// treated as having run.
#[test]
fn unresolved_module_import_is_reported() {
    let html = r#"<html><body><p>kept</p>
      <script type="module">import x from './nowhere.js'; document.body.setAttribute('ran','1');</script>
      </body></html>"#;
    let c = convert(html, &mut BoaEngine::default());
    assert_eq!(c.scripts_failed, 1, "unresolved import must not count as success");
    assert!(c.html.contains("kept"));
    assert!(!c.html.contains(r#"ran="1""#));
}

/// ★ HTML-like comments are STANDARDISED JAVASCRIPT (Annex B.1.1), not broken
/// markup. The `<!-- ... //-->` wrapper around a classic script is accepted by
/// every browser, and Boa needs its `annex-b` feature turned on to match.
/// Without it these scripts were a SyntaxError, which the report then
/// mis-described as "script body is HTML" — the body was JavaScript all along.
#[test]
fn html_like_comments_are_javascript_not_markup() {
    let html = "<html><body><div id=t></div><script type=\"text/javascript\">\n\
        <!--\n\
        document.getElementById('t').setAttribute('ran', '1');\n\
        //-->\n\
        </script></body></html>";
    let c = convert(html, &mut BoaEngine::default());
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains(r#"ran="1""#), "{}", c.html);
}

/// ★ `<!--` is a LINE comment, not a block opener — anything after it on the
/// same line is commented out, and the block does NOT need a closing `-->`.
/// I got this wrong first time and wrote code on the opening line, which the
/// engine correctly ignored; the test now pins the real semantics, including
/// the bare `-->` closing form with no `//` guard.
#[test]
fn html_open_comment_is_a_line_comment() {
    let html = "<html><body><div id=t></div><script>\n\
        <!-- this text is commented out\n\
        var x = 1;\n\
        document.getElementById('t').setAttribute('x', String(x));\n\
        -->\n\
        </script></body></html>";
    let c = convert(html, &mut BoaEngine::default());
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains(r#"x="1""#), "{}", c.html);
}
