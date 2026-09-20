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
    let c = convert(html, &mut BoaEngine);
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
    let c = convert(html, &mut BoaEngine);
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains("original + appended"), "got: {}", c.html);
}

#[test]
fn query_and_attributes() {
    let html = "<html><body><p>one</p><p>two</p><script>\
        var ps=document.querySelectorAll('p');\
        for (var i=0;i<ps.length;i++) ps[i].setAttribute('data-i', String(i));\
        </script></body></html>";
    let c = convert(html, &mut BoaEngine);
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains(r#"data-i="0""#) && c.html.contains(r#"data-i="1""#), "got: {}", c.html);
}

/// A failing script must not lose the document — conversion degrades to the
/// static content rather than throwing it away.
#[test]
fn script_failure_keeps_the_document() {
    let html = "<html><body><p>static</p><script>noSuchApi.doThing();</script></body></html>";
    let c = convert(html, &mut BoaEngine);
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
    let a = convert(html, &mut BoaEngine).html;
    let b = convert(html, &mut BoaEngine).html;
    let c = convert(html, &mut BoaEngine).html;
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
    let c = convert(html, &mut BoaEngine);
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains(r#"ran="yes""#), "got: {}", c.html);
}

#[test]
fn console_is_a_sink_not_a_failure() {
    let html = "<html><body><script>console.log('x');console.warn('y');</script></body></html>";
    let c = convert(html, &mut BoaEngine);
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
}
