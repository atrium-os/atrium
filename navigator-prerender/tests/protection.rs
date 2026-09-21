//! ★ PROTECTED-SUBTREE EXECUTION: AN EXPERIMENT THAT FAILED, KEPT SO THE
//! REFUTATION IS REPRODUCIBLE.
//!
//! The idea: mark parser-produced nodes un-removable, so a hydrating page
//! that tears down server-rendered content cannot — making hydration
//! non-destructive BY CONSTRUCTION rather than detected afterwards.
//!
//! Measured on the 104-document corpus (PRERENDER_PROTECT=1 for the CLI):
//!   - documents retaining <80% of their visible text: 1 -> 0
//!   - but bbc.co.uk's rescued content arrives TWICE: "Attribution" 69 -> 99,
//!     "Posted" 55 -> 75, because the client re-render is added alongside the
//!     preserved original.
//!   - and it damages documents that were HEALTHY: perldoc.perl.org gains
//!     935 duplicated words, its code samples printed twice.
//!
//! So it does not prevent the failure, it EXCHANGES it: missing content
//! becomes duplicated content, and the cost lands on pages that had no
//! problem. The tier-1 fallback already shipped is strictly better for the
//! case this was meant to rescue — bbc at tier 1 is the complete server
//! document with nothing duplicated.
//!
//! Kept behind an explicit flag, off by default, because someone will
//! propose it again and this makes answering cheap.

use navigator_prerender::{boa_impl::BoaEngine, convert, convert_with_opts, fetch::NoNetwork};

/// Protection as an explicit argument — never an environment variable. Two
/// tests toggling one process-wide variable in PARALLEL raced, and each read
/// the other's setting, so both "unprotected" assertions failed against a
/// protected run. A test that reads global process state is not isolated,
/// however careful it looks.
fn protected(html: &str) -> navigator_prerender::Conversion {
    convert_with_opts(html, None, &mut BoaEngine::default(), &mut NoNetwork, true)
}

/// The mechanism works as specified: a script cannot remove server content.
#[test]
fn protection_refuses_removal_of_parser_nodes() {
    let html = "<html><body><div id=r>server content that a reader wants</div><script>\
        document.getElementById('r').remove();\
        </script></body></html>";

    let plain = convert(html, &mut BoaEngine::default());
    assert!(!plain.html.contains("server content that a reader wants"),
        "unprotected, the removal succeeds");
    assert_eq!(plain.removals_refused, 0);

    let guarded = protected(html);

    assert!(guarded.html.contains("server content that a reader wants"),
        "protected, the server's content survives");
    assert_eq!(guarded.removals_refused, 1, "and the refusal is counted");
}

/// ★ AND THE REASON IT IS NOT A DEFAULT: a page that legitimately REPLACES
/// its own content ends up showing both copies. This is the perldoc shape —
/// a healthy document made worse.
#[test]
fn protection_duplicates_content_a_page_meant_to_replace() {
    let html = "<html><body><div id=r>placeholder to be replaced</div><script>\
        var d = document.getElementById('r');\
        d.innerHTML = '<p>the real content, fetched and rendered</p>';\
        </script></body></html>";

    let plain = convert(html, &mut BoaEngine::default());
    assert!(plain.html.contains("the real content"));
    assert!(!plain.html.contains("placeholder to be replaced"),
        "unprotected, the replacement is clean");

    let guarded = protected(html);

    // Both are present: the reader sees the placeholder AND the content.
    assert!(guarded.html.contains("the real content"));
    assert!(guarded.html.contains("placeholder to be replaced"),
        "protection turns a replacement into a duplication: {}", guarded.html);
}

/// Reordering is not removal. Blocking moves would break pages that
/// rearrange the server's own markup without destroying anything, so
/// protection must not fire on them.
#[test]
fn protection_allows_reordering_of_server_content() {
    let html = "<html><body><ul id=l><li id=a>first</li><li id=b>second</li></ul><script>\
        var l = document.getElementById('l');\
        l.appendChild(document.getElementById('a'));\
        </script></body></html>";

    let guarded = protected(html);

    assert_eq!(guarded.removals_refused, 0, "a move is not a removal");
    let first = guarded.html.find(">first<").unwrap();
    let second = guarded.html.find(">second<").unwrap();
    assert!(second < first, "the reorder must have happened: {}", guarded.html);
    // And nothing was duplicated by it.
    assert_eq!(guarded.html.matches(">first<").count(), 1);
}
