use navigator_prerender::{boa_impl::BoaEngine, convert_with, fetch::MapFetcher, modmirror};

#[test]
fn scan_finds_static_import_specifiers() {
    let src = r#"
        import a from './a.js';
        import { b } from "../lib/b.js";
        export * from './c.js';
        const d = 'import "not-a-real-one.js"';
        import('./dynamic.js');
    "#;
    let found = modmirror::scan_imports(src);
    assert!(found.contains(&"./a.js".to_string()), "{found:?}");
    assert!(found.contains(&"../lib/b.js".to_string()), "{found:?}");
    assert!(found.contains(&"./c.js".to_string()), "{found:?}");
}

#[test]
fn mirror_path_stays_inside_the_root() {
    let root = std::env::temp_dir().join("prerender-test-root");
    std::fs::create_dir_all(&root).unwrap();
    let canon = root.canonicalize().unwrap();
    // Url::parse normalises `..` away, so traversal cannot arrive this way —
    // the guarantee that matters is containment, which is asserted directly.
    let esc = modmirror::mirror_path(&root, "https://h/a/../../etc/passwd").unwrap();
    assert!(esc.starts_with(&canon), "escaped the root: {esc:?}");
    let p = modmirror::mirror_path(&root, "https://h/a/b.js").unwrap();
    assert!(p.ends_with("h/a/b.js"), "{p:?}");
    let idx = modmirror::mirror_path(&root, "https://h/a/").unwrap();
    assert!(idx.ends_with("index.js"), "{idx:?}");
}

/// The point: a module that imports another must get BOTH, with the import
/// resolved relative to the importer's URL.
#[test]
fn module_import_graph_resolves_and_runs() {
    let mut f = MapFetcher::default();
    f.0.insert("https://example.test/js/app.js".into(),
        "import { make } from './helper.js';\nmake('from import');".into());
    f.0.insert("https://example.test/js/helper.js".into(),
        "export function make(t){ var p = document.createElement('p'); p.textContent = t; \
         document.body.appendChild(p); }".into());
    let html = r#"<html><body><script type="module" src="/js/app.js"></script></body></html>"#;
    let c = convert_with(html, Some("https://example.test/page.html"),
                         &mut BoaEngine::default(), &mut f);
    assert_eq!(c.external_fetched, 1);
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains("from import"), "imported module did not run: {}", c.html);
}

/// A bare specifier has no meaning without an import map and must be
/// reported, not guessed at.
#[test]
fn bare_specifier_is_reported() {
    let mut f = MapFetcher::default();
    f.0.insert("https://example.test/js/app.js".into(),
        "import React from 'react'; document.body.setAttribute('ran','1');".into());
    let html = r#"<html><body><script type="module" src="/js/app.js"></script></body></html>"#;
    let c = convert_with(html, Some("https://example.test/p.html"), &mut BoaEngine::default(), &mut f);
    assert!(c.errors.iter().any(|e| e.contains("bare module specifier")), "{:?}", c.errors);
}

/// An import cycle must terminate.
#[test]
fn import_cycle_terminates() {
    let mut f = MapFetcher::default();
    f.0.insert("https://e.test/a.js".into(), "import './b.js'; export const a=1;".into());
    f.0.insert("https://e.test/b.js".into(), "import './a.js'; export const b=2;".into());
    let html = r#"<html><body><script type="module" src="/a.js"></script></body></html>"#;
    let c = convert_with(html, Some("https://e.test/p.html"), &mut BoaEngine::default(), &mut f);
    assert_eq!(c.external_fetched, 1, "cycle must not prevent the fetch");
}
