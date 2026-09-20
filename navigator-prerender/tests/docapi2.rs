use navigator_prerender::{boa_impl::BoaEngine, convert};

fn run(html: &str) -> navigator_prerender::Conversion { convert(html, &mut BoaEngine::default()) }

fn r(c: &navigator_prerender::Conversion) -> String {
    let m = "data-r=\"";
    let i = c.html.find(m).unwrap_or_else(|| panic!("no data-r in {}", c.html)) + m.len();
    c.html[i..].split('"').next().unwrap().to_string()
}

/// All four positions, two of which land OUTSIDE the element — which is why
/// this needs the parent and is only expressible on a navigable tree.
#[test]
fn insert_adjacent_element_places_all_four_positions() {
    let c = run(r#"<html><body><div id=wrap><div id=t>mid</div></div><script>
      var t = document.getElementById('t');
      function mk(id) { var e = document.createElement('i'); e.setAttribute('id', id); return e; }
      t.insertAdjacentElement('beforebegin', mk('bb'));
      t.insertAdjacentElement('afterbegin', mk('ab'));
      t.insertAdjacentElement('beforeend', mk('be'));
      t.insertAdjacentElement('afterend', mk('ae'));
      var wrap = document.getElementById('wrap');
      var outer = [], inner = [];
      for (var i = 0; i < wrap.children.length; i++) outer.push(wrap.children[i].getAttribute('id'));
      for (var j = 0; j < t.children.length; j++) inner.push(t.children[j].getAttribute('id'));
      document.body.setAttribute('data-r', outer.join(',') + ' | ' + inner.join(','));
    </script></body></html>"#);
    assert_eq!(r(&c), "bb,t,ae | ab,be");
}

/// An unknown position is a SyntaxError, as the DOM says — a page that
/// mistypes must find out rather than silently lose its content.
#[test]
fn a_bad_position_throws() {
    let c = run(r#"<html><body><div id=t></div><script>
      var m = 'none';
      try { document.getElementById('t').insertAdjacentElement('middle', document.createElement('i')); }
      catch (e) { m = e.constructor.name; }
      document.body.setAttribute('data-r', m);
    </script></body></html>"#);
    assert_eq!(r(&c), "SyntaxError");
}

/// insertAdjacentHTML goes through the real parser, and several nodes keep
/// their order — the reverse-insertion bug document.write already taught.
#[test]
fn insert_adjacent_html_parses_and_keeps_order() {
    let c = run(r#"<html><body><div id=t></div><script>
      document.getElementById('t')
        .insertAdjacentHTML('beforeend', '<p class="a">one</p><p>two</p><p>three</p>');
      var t = document.getElementById('t');
      var out = [];
      for (var i = 0; i < t.children.length; i++) out.push(t.children[i].textContent);
      document.body.setAttribute('data-r', out.join(',') + '|' + t.firstChild.getAttribute('class'));
    </script></body></html>"#);
    assert_eq!(r(&c), "one,two,three|a");
    // ★ Assert on the TARGET element, not on "everything after </script>":
    // here the div PRECEDES the script, so that trick does not apply. The
    // unescaped markup inside #t is unambiguous either way.
    assert!(c.html.contains(r#"<div id="t"><p class="a">one</p><p>two</p><p>three</p></div>"#),
        "{}", c.html);
}

#[test]
fn insert_adjacent_text_adds_a_text_node() {
    let c = run(r#"<html><body><div id=t><b>x</b></div><script>
      var t = document.getElementById('t');
      t.insertAdjacentText('afterbegin', 'lead ');
      document.body.setAttribute('data-r', t.textContent + '|' + t.firstChild.nodeType);
    </script></body></html>"#);
    assert_eq!(r(&c), "lead x|3");
}

/// title reads the element's text and writing it reaches the artifact.
#[test]
fn title_round_trips_into_the_artifact() {
    let c = run(r#"<html><head><title>Old</title></head><body><div id=t></div><script>
      var before = document.title;
      document.title = 'New';
      document.body.setAttribute('data-r', before + '|' + document.title);
    </script></body></html>"#);
    assert_eq!(r(&c), "Old|New");
    assert!(c.html.contains("<title>New</title>"), "{}", c.html);
    assert!(!c.html.contains(">Old<"), "{}", c.html);
}

/// Setting a title on a document that has none creates the element, as a
/// browser does.
#[test]
fn setting_title_creates_the_element_when_absent() {
    let c = run(r#"<html><body><div id=t></div><script>
      document.title = 'Made';
      document.body.setAttribute('data-r', document.title);
    </script></body></html>"#);
    assert_eq!(r(&c), "Made");
    assert!(c.html.contains("<title>Made</title>"), "{}", c.html);
}

#[test]
fn scrolling_element_is_the_document_element() {
    let c = run(r#"<html><body><div id=t></div><script>
      document.body.setAttribute('data-r',
        (document.scrollingElement === document.documentElement) + '|' +
        document.scrollingElement.tagName);
    </script></body></html>"#);
    assert_eq!(r(&c), "true|HTML");
}

/// styleSheets lists the real sheets with the metadata callers branch on.
#[test]
fn stylesheets_lists_style_and_link_elements() {
    let c = run(r#"<html><head>
        <style id=s1>p{color:red}</style>
        <link id=l1 rel="stylesheet" href="/a.css" media="screen">
        <link id=l2 rel="icon" href="/favicon.ico">
      </head><body><div id=t></div><script>
      var s = document.styleSheets;
      var out = [s.length];
      for (var i = 0; i < s.length; i++) {
        out.push(s[i].ownerNode.getAttribute('id') + ':' + String(s[i].href) + ':' + s[i].media);
      }
      document.body.setAttribute('data-r', out.join(' | '));
    </script></body></html>"#);
    // The icon link is not a stylesheet and must not be listed.
    assert_eq!(r(&c), "2 | s1:null: | l1:/a.css:screen");
}

/// ★ This converter does not parse CSS, and cssRules says so by being EMPTY
/// rather than by throwing. Most corpus readers guard it with try/catch
/// because cross-origin sheets raise SecurityError; raising that here would
/// assert a reason that is false. Empty is the honest "none known", and the
/// guarded callers handle it identically.
#[test]
fn css_rules_is_empty_not_a_false_security_error() {
    let c = run(r#"<html><head><style>p{color:red}</style></head><body><div id=t></div><script>
      var threw = 'no', n = -1;
      try { n = document.styleSheets[0].cssRules.length; } catch (e) { threw = e.name; }
      document.body.setAttribute('data-r', threw + '|' + n + '|' +
        (document.styleSheets[0].cssRules === document.styleSheets[0].rules));
    </script></body></html>"#);
    assert_eq!(r(&c), "no|0|true");
}

/// The guarded idiom the corpus actually uses must complete rather than
/// abort the script.
#[test]
fn the_guarded_corpus_idiom_completes() {
    let c = run(r#"<html><head><style>a{}</style><style>b{}</style></head><body><div id=t></div><script>
      var n = Array.from(document.styleSheets).reduce(function (acc, sheet) {
        try { return acc + (sheet.cssRules ? sheet.cssRules.length : 0); }
        catch (e) { return acc; }
      }, 0);
      document.body.setAttribute('data-r', String(n) + '|' + document.styleSheets.length);
    </script></body></html>"#);
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert_eq!(r(&c), "0|2");
}

/// ★ HOST OBJECTS MUST NOT LOOK LIKE PLAIN OBJECTS. Everything reported
/// `[object Object]`, so jQuery's isPlainObject answered TRUE for the window
/// — and `extend(true, ...)` descended into it, hit the self-reference every
/// global has, and recursed until the engine's limit stopped it. A browser
/// reports `[object Window]` and the recursion never starts.
#[test]
fn host_objects_report_their_own_tags() {
    let c = run(r#"<html><head><style>p{}</style></head><body><div id=t k=v></div><script>
      var s = Object.prototype.toString;
      var e = document.getElementById('t');
      document.body.setAttribute('data-r', [
        s.call(window), s.call(document), s.call(e),
        s.call(e.style), s.call(e.attributes), s.call(e.dataset),
        s.call(document.createComment('c')), s.call(document.createDocumentFragment())
      ].join(' '));
    </script></body></html>"#);
    assert_eq!(r(&c),
        "[object Window] [object HTMLDocument] [object HTMLElement] \
         [object CSSStyleDeclaration] [object NamedNodeMap] [object DOMStringMap] \
         [object Comment] [object DocumentFragment]");
}

/// The consequence that matters: a deep merge over the window must TERMINATE.
/// This is the exact shape that exhausted the engine's call budget.
#[test]
fn a_deep_merge_over_the_window_terminates() {
    let c = run(r#"<html><body><div id=t></div><script>
      // A miniature isPlainObject + deep merge, the jQuery.extend shape.
      function plain(o) {
        return !!o && typeof o === 'object' &&
               Object.prototype.toString.call(o) === '[object Object]';
      }
      var seen = 0;
      function merge(dst, src, depth) {
        if (depth > 40) throw new Error('runaway');
        for (var k in src) {
          seen++;
          if (seen > 5000) return dst;
          var v;
          try { v = src[k]; } catch (e) { continue; }
          if (plain(v)) { dst[k] = merge({}, v, depth + 1); }
        }
        return dst;
      }
      var ok = 'no';
      try { merge({}, { w: window, d: document, b: document.body }, 0); ok = 'terminated'; }
      catch (e) { ok = 'RUNAWAY'; }
      document.body.setAttribute('data-r', ok);
    </script></body></html>"#);
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert_eq!(r(&c), "terminated");
}
