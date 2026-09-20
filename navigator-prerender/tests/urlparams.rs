use navigator_prerender::{boa_impl::BoaEngine, convert, convert_with, fetch::NoNetwork};

fn attr(html: &str, name: &str) -> String {
    let c = convert(html, &mut BoaEngine::default());
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    let pat = format!("{name}=\"");
    let i = c.html.find(&pat).unwrap_or_else(|| panic!("no {name} in {}", c.html)) + pat.len();
    c.html[i..].split('"').next().unwrap().to_string()
}

#[test]
fn self_is_the_global() {
    assert_eq!(attr("<html><body><div id=r></div><script>\
        document.getElementById('r').setAttribute('t', String(self === globalThis));\
        </script></body></html>", "t"), "true");
}

/// Order and duplicates are observable, so the backing store is a pair list.
#[test]
fn search_params_preserve_order_and_duplicates() {
    assert_eq!(attr("<html><body><div id=r></div><script>\
        var p = new URLSearchParams('b=2&a=1&b=3');\
        document.getElementById('r').setAttribute('t', p.getAll('b').join(',') + '|' + p.toString());\
        </script></body></html>", "t"), "2,3|b=2&amp;a=1&amp;b=3");
}

#[test]
fn search_params_decode_and_encode() {
    assert_eq!(attr("<html><body><div id=r></div><script>\
        var p = new URLSearchParams('q=hello+world&x=a%26b');\
        document.getElementById('r').setAttribute('t', p.get('q') + '|' + p.get('x'));\
        </script></body></html>", "t"),
        // The value decoded to `a&b`; the serializer then escapes `&` in the
        // attribute, as it must. Both halves are correct.
        "hello world|a&amp;b");
}

/// set() replaces in place and appends only when absent — the subtle part.
#[test]
fn search_params_set_append_delete() {
    assert_eq!(attr("<html><body><div id=r></div><script>\
        var p = new URLSearchParams('a=1&b=2&a=3');\
        p.set('a','9'); p.append('c','4'); p['delete']('b');\
        document.getElementById('r').setAttribute('t', p.toString());\
        </script></body></html>", "t"),
        // `&` escaped by the serializer, as in the decode test above.
        "a=9&amp;c=4");
}

#[test]
fn url_and_location_expose_real_search_params() {
    let html = "<html><body><div id=r></div><script>\
        var u = new URL('https://h.test/p?x=1&y=2');\
        document.getElementById('r').setAttribute('u', u.searchParams.get('y'));\
        document.getElementById('r').setAttribute('l', location.searchParams.get('q'));\
        </script></body></html>";
    let c = convert_with(html, Some("https://example.test/p.html?q=found"),
                         &mut BoaEngine::default(), &mut NoNetwork);
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains(r#"u="2""#), "got {}", c.html);
    assert!(c.html.contains(r#"l="found""#), "location must carry its own query: {}", c.html);
}
