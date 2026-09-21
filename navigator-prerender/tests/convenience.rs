use navigator_prerender::{boa_impl::BoaEngine, convert};

fn run(html: &str) -> navigator_prerender::Conversion { convert(html, &mut BoaEngine::default()) }

fn r(c: &navigator_prerender::Conversion) -> String {
    let m = "data-r=\"";
    let i = c.html.find(m).unwrap_or_else(|| panic!("no data-r in {}", c.html)) + m.len();
    c.html[i..].split('"').next().unwrap().to_string()
}

/// ★ An array-like host collection must also be ITERABLE. Both of these threw
/// "value with type `object` is not iterable" — the first failure of a corpus
/// document — because the objects had length and indices but no iterator.
#[test]
fn host_collections_are_iterable() {
    let c = run(r#"<html><body><div id=t class="a b c" x=1 y=2><p>k</p></div><script>
      var e = document.getElementById('t');
      var classes = [...e.classList];
      var names = []; for (var a of e.attributes) names.push(a.name);
      document.body.setAttribute('data-r',
        classes.join('') + '|' + names.sort().join(',') + '|' +
        e.classList[1] + '|' + e.classList.item(9));
    </script></body></html>"#);
    assert_eq!(r(&c), "abc|class,id,x,y|b|null");
}

/// classList indexing is LIVE, not captured when the wrapper was built.
#[test]
fn class_list_indexing_is_live() {
    let c = run(r#"<html><body><div id=t class="one"></div><script>
      var l = document.getElementById('t').classList;
      var before = l[0] + ':' + l.length;
      l.add('two');
      document.body.setAttribute('data-r', before + '|' + l[1] + ':' + l.length);
    </script></body></html>"#);
    assert_eq!(r(&c), "one:1|two:2");
}

/// append/prepend take nodes OR STRINGS, and a string becomes a text node —
/// dropping that would silently lose text a page appended.
#[test]
fn append_and_prepend_accept_nodes_and_strings() {
    let c = run(r#"<html><body><div id=t><b>mid</b></div><script>
      var t = document.getElementById('t');
      t.append(' tail', document.createElement('i'));
      t.prepend('head ', document.createElement('em'));
      document.body.setAttribute('data-r',
        t.textContent + '|' + t.childNodes.length + '|' + t.children.length);
    </script></body></html>"#);
    assert_eq!(r(&c), "head mid tail|5|3");
    assert!(c.html.contains("<div id=\"t\">head <em></em><b>mid</b> tail<i></i></div>"), "{}", c.html);
}

/// before/after/replaceWith act on the element's PARENT, and a multi-argument
/// run must keep its order rather than reversing.
#[test]
fn before_after_and_replace_with_keep_order() {
    let c = run(r#"<html><body><div id=w><p id=t>mid</p></div><script>
      var t = document.getElementById('t');
      t.before('A', 'B');
      t.after('C', 'D');
      document.body.setAttribute('data-r', document.getElementById('w').textContent);
    </script></body></html>"#);
    assert_eq!(r(&c), "ABmidCD");
}

#[test]
fn replace_with_removes_the_original() {
    let c = run(r#"<html><body><div id=w><p id=t>gone</p></div><script>
      var t = document.getElementById('t');
      var n = document.createElement('span');
      n.textContent = 'kept';
      t.replaceWith(n);
      var w = document.getElementById('w');
      document.body.setAttribute('data-r',
        w.textContent + '|' + w.children.length + '|' + String(document.getElementById('t')));
    </script></body></html>"#);
    assert_eq!(r(&c), "kept|1|null");
}

#[test]
fn reflected_attributes_round_trip() {
    let c = run(r#"<html><body><div id=t dir=rtl lang=de title=hint></div><script>
      var t = document.getElementById('t');
      var before = [t.dir, t.lang, t.title].join(',');
      t.dir = 'ltr'; t.nonce = 'abc123';
      document.body.setAttribute('data-r',
        before + '|' + t.dir + '|' + t.getAttribute('dir') + '|' + t.nonce);
    </script></body></html>"#);
    assert_eq!(r(&c), "rtl,de,hint|ltr|ltr|abc123");
    assert!(c.html.contains(r#"nonce="abc123""#), "{}", c.html);
}

#[test]
fn select_options_is_a_live_collection() {
    let c = run(r#"<html><body><select id=s><option value=a>A</option><option value=b>B</option></select><script>
      var s = document.getElementById('s');
      var before = s.options.length;
      var o = document.createElement('option');
      o.setAttribute('value', 'c');
      s.appendChild(o);
      document.body.setAttribute('data-r',
        before + '|' + s.options.length + '|' + s.options[1].value + '|' +
        Object.prototype.toString.call(s.options));
    </script></body></html>"#);
    assert_eq!(r(&c), "2|3|b|[object HTMLCollection]");
}

/// ★ window.frames is EMPTY and length is 0, because this converter creates
/// no child browsing contexts — the same truth contentWindow reports as null.
#[test]
fn the_window_has_no_frames() {
    let c = run(r#"<html><body><iframe></iframe><div id=t></div><script>
      document.body.setAttribute('data-r', [
        frames.length, window.length,
        (window.top === window), (window.parent === window), (window.self === window)
      ].join('|'));
    </script></body></html>"#);
    assert_eq!(r(&c), "0|0|true|true|true");
}

#[test]
fn document_interface_matches_only_documents() {
    let c = run(r#"<html><body><div id=t></div><script>
      document.body.setAttribute('data-r', [
        document instanceof Document,
        document.getElementById('t') instanceof Document,
        document.implementation.createHTMLDocument('') instanceof Document
      ].join('|'));
    </script></body></html>"#);
    assert_eq!(r(&c), "true|false|true");
}
