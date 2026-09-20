use navigator_prerender::{boa_impl::BoaEngine, convert, dom, parse::parse};

fn run(html: &str) -> navigator_prerender::Conversion { convert(html, &mut BoaEngine::default()) }

#[test]
fn css_name_maps_camel_case() {
    assert_eq!(dom::css_name("backgroundColor"), "background-color");
    assert_eq!(dom::css_name("display"), "display");
    assert_eq!(dom::css_name("Z-Index"), "z-index");
}

#[test]
fn style_helpers_round_trip() {
    let mut d = parse("<html><body><div id=a style='color: red; margin-top: 4px'></div></body></html>");
    let h = d.by_id("a").unwrap();
    assert_eq!(d.style_get(h, "color"), "red");
    assert_eq!(d.style_get(h, "marginTop"), "4px", "camelCase must resolve");
    d.style_set(h, "backgroundColor", "blue");
    assert_eq!(d.style_get(h, "background-color"), "blue");
    d.style_set(h, "color", "");
    assert_eq!(d.style_get(h, "color"), "", "empty value removes the declaration");
    assert!(d.attr(h, "style").unwrap().contains("background-color: blue"));
}

#[test]
fn class_helpers() {
    let mut d = parse("<html><body><div id=a class='x y'></div></body></html>");
    let h = d.by_id("a").unwrap();
    assert_eq!(d.class_list(h), ["x", "y"]);
    d.class_add(h, &["z".into(), "x".into()]);
    assert_eq!(d.class_list(h), ["x", "y", "z"], "no duplicates");
    d.class_remove(h, &["y".into()]);
    assert_eq!(d.class_list(h), ["x", "z"]);
    assert!(!d.class_toggle(h, "x", None));
    assert!(d.class_toggle(h, "x", Some(true)));
}

/// The live view is the point: a script's writes must reach the output.
#[test]
fn classlist_writes_reach_the_serialized_html() {
    let html = "<html><body><div id=a class='one'></div><script>\
        var e = document.getElementById('a');\
        e.classList.add('two','three');\
        e.classList.remove('one');\
        e.setAttribute('has3', String(e.classList.contains('three')));\
        e.setAttribute('n', String(e.classList.length));\
        e.setAttribute('toggled', String(e.classList.toggle('four')));\
        </script></body></html>";
    let c = run(html);
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains(r#"class="two three four""#), "got {}", c.html);
    // length is read BEFORE the toggle adds a fourth, so two is correct here.
    assert!(c.html.contains(r#"has3="true""#) && c.html.contains(r#"n="2""#), "got {}", c.html);
}

/// `el.style.display = 'none'` is a set of an arbitrary property name — the
/// case that forced style to be a Proxy.
#[test]
fn style_assignment_reaches_the_attribute() {
    let html = "<html><body><div id=a></div><script>\
        var e = document.getElementById('a');\
        e.style.display = 'none';\
        e.style.backgroundColor = 'red';\
        e.setAttribute('read-back', e.style.display);\
        e.setAttribute('kebab', e.style.getPropertyValue('background-color'));\
        </script></body></html>";
    let c = run(html);
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains("display: none"), "got {}", c.html);
    assert!(c.html.contains("background-color: red"), "camelCase must write kebab: {}", c.html);
    assert!(c.html.contains(r#"read-back="none""#), "must read its own write: {}", c.html);
    assert!(c.html.contains(r#"kebab="red""#), "got {}", c.html);
}

#[test]
fn style_methods_and_csstext() {
    let html = "<html><body><div id=a style='color: red'></div><script>\
        var e = document.getElementById('a');\
        e.style.setProperty('margin-top','8px');\
        e.style.removeProperty('color');\
        e.setAttribute('t', e.style.cssText);\
        </script></body></html>";
    let c = run(html);
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains("margin-top: 8px"), "got {}", c.html);
    assert!(!c.html.contains("color: red"), "removeProperty failed: {}", c.html);
}
