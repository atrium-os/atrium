use navigator_prerender::{boa_impl::BoaEngine, convert, Tier, TierPolicy};

fn run(html: &str) -> navigator_prerender::Conversion { convert(html, &mut BoaEngine::default()) }

/// Enough text that the policy's floor does not exempt the document. The
/// default floor is 200 characters of visible text and one copy of this
/// sentence is ~130, so it is repeated — a test that silently sat under the
/// floor would assert nothing at all.
const SENTENCE: &str = "Server rendered prose that a reader came here to read, repeated \
so the document clears the policy floor and the ratio means something. ";

fn filler() -> String { SENTENCE.repeat(3) }

/// Look only AFTER the script element: the artifact carries the script
/// SOURCE too, so searching the whole document finds string literals from
/// the very code under test.
fn after_script(html: &str) -> &str {
    match html.find("</script>") { Some(i) => &html[i..], None => html }
}

/// ★ THE CONVERTER MEASURES AND DOES NOT DECIDE. It returns BOTH artifacts
/// and the numbers; nothing in it picks one. A converter that silently
/// returned a different artifact than its scripts produced would be making a
/// product judgement inside an instrument.
#[test]
fn the_converter_returns_both_artifacts_and_judges_neither() {
    let f = filler();
    let c = run(&format!(
        "<html><body><div id=r>{f}</div><script>\
         document.getElementById('r').textContent = 'replaced';\
         </script></body></html>"));
    assert!(c.html.contains("replaced"), "tier 2 is the post-script document");
    assert!(c.html_tier1.contains("Server rendered prose"),
        "tier 1 is the pre-script document");
    assert!(!after_script(&c.html_tier1).contains("replaced"),
        "tier 1 must not contain what the scripts did: {}", c.html_tier1);
    // The measurements are present; no decision is.
    assert!(c.text_before > 0 && c.text_after > 0);
}

/// A conversion that ADDS content keeps tier 2 — the case tier 2 exists for.
#[test]
fn a_conversion_that_adds_content_is_published() {
    let f = filler();
    let c = run(&format!(
        "<html><body><div id=r>{f}</div><script>\
         var p = document.createElement('p');\
         p.textContent = 'content that only exists after the script ran';\
         document.getElementById('r').appendChild(p);\
         </script></body></html>"));
    let (html, d) = TierPolicy::default().artifact(&c);
    assert_eq!(d.tier, Tier::Two, "{}", d.reason);
    assert!(html.contains("only exists after"));
    assert!(c.text_retained() > 1.0);
}

/// ★ A conversion that REMOVES reader-visible content is demoted, and the
/// artifact published is the pre-script one. This is the bbc.co.uk shape:
/// every script runs clean while the document empties.
#[test]
fn a_conversion_that_removes_content_is_demoted_to_tier_one() {
    let f = filler();
    let c = run(&format!(
        "<html><body><div id=r>{f}</div><script>\
         document.getElementById('r').textContent = '';\
         </script></body></html>"));
    assert_eq!(c.scripts_failed, 0, "the scripts ran CLEAN: {:?}", c.errors);
    let (html, d) = TierPolicy::default().artifact(&c);
    assert_eq!(d.tier, Tier::One, "{}", d.reason);
    assert_eq!(d.reason, "conversion removed reader-visible content");
    assert!(html.contains("Server rendered prose"),
        "the published artifact must be the pre-script one");
}

/// The threshold is the OPERATOR'S, not the converter's: the same conversion
/// is published or demoted depending on the policy it is read with.
#[test]
fn the_same_conversion_decides_differently_under_different_policies() {
    // Removes about half the text.
    let f = filler();
    let c = run(&format!(
        "<html><body><div id=a>{f}</div><div id=b>{f}</div><script>\
         document.getElementById('b').textContent = '';\
         </script></body></html>"));
    let strict = TierPolicy { min_text_retained: 0.90, ..Default::default() };
    let lax = TierPolicy { min_text_retained: 0.10, ..Default::default() };
    assert_eq!(strict.decide(&c).tier, Tier::One);
    assert_eq!(lax.decide(&c).tier, Tier::Two);
}

/// ★ A trivial loss is not a regression. The corpus separates cleanly — the
/// catastrophic case retains 0.346 and the only other loss retains 0.996,
/// which is 38 characters of noise. The floor must not fire on the second.
#[test]
fn a_trivial_loss_is_not_treated_as_a_regression() {
    let f = filler();
    let c = run(&format!(
        "<html><body><div id=r>{f}{f}{f}</div><div id=x>tiny</div><script>\
         document.getElementById('x').textContent = '';\
         </script></body></html>"));
    assert!(c.text_retained() > 0.95, "retained {}", c.text_retained());
    assert_eq!(TierPolicy::default().decide(&c).tier, Tier::Two);
}

/// An app shell with no text either way has lost nothing — the ratio would be
/// noise over a handful of characters, so the floor exempts it.
#[test]
fn a_document_with_no_text_is_exempt() {
    let c = run("<html><body><div id=r></div><script>\
        document.getElementById('r').setAttribute('data-x', '1');\
        </script></body></html>");
    let d = TierPolicy::default().decide(&c);
    assert_eq!(d.tier, Tier::Two);
    assert_eq!(d.reason, "too little text to judge; nothing to protect");
}

/// With no scripts there is no tier 2 to publish; the two artifacts are the
/// same document and the decision says so.
#[test]
fn a_document_without_scripts_is_tier_one_by_definition() {
    let f = filler();
    let c = run(&format!("<html><body><div>{f}</div></body></html>"));
    let d = TierPolicy::default().decide(&c);
    assert_eq!(d.tier, Tier::One);
    assert_eq!(d.reason, "no scripts to run");
}

/// ★ The decision must travel WITH the bytes: a reader cannot otherwise tell
/// which pipeline produced what they are looking at.
#[test]
fn the_choice_is_returned_alongside_the_artifact() {
    let f = filler();
    let c = run(&format!("<html><body><div id=r>{f}</div><script>\
        document.getElementById('r').textContent = '';</script></body></html>"));
    let (html, d) = TierPolicy::default().artifact(&c);
    assert_eq!(html, c.html_tier1);
    assert_eq!(d.tier, Tier::One);
    assert!(!d.reason.is_empty(), "a decision without a reason is not inspectable");
}

// ── Origin refusal (spec §5.4.1b) ───────────────────────────────────────

/// ★ A CHALLENGE PAGE IS NOT THE SITE. Converting one publishes an artifact
/// that looks like a successful conversion of a document reading "Just a
/// moment...", which is worse than publishing nothing.
#[test]
fn an_interstitial_challenge_is_marked_refused() {
    let c = run("<html><head><title>Just a moment...</title></head>\
        <body><div class=cf-browser-verification>Checking your browser before \
        accessing the site.</div></body></html>");
    assert_eq!(c.origin_refusal, Some("interstitial challenge"));
}

#[test]
fn an_access_denial_is_marked_refused() {
    let c = run("<html><head><title>Denied</title></head>\
        <body><h1>Access to this page has been denied</h1></body></html>");
    assert_eq!(c.origin_refusal, Some("origin denied access"));
}

/// ★★ THE PRECISION RISK, AND THE REASON THE RULE NEEDS TWO CONDITIONS. An
/// article ABOUT captchas contains the word "captcha"; a news story about
/// Cloudflare quotes its interstitial verbatim. A marker alone must never
/// decide, or the converter refuses to publish exactly the documents that
/// discuss the thing.
#[test]
fn an_article_about_challenges_is_not_a_challenge() {
    let c = run(&format!("<html><head><title>How CAPTCHAs work</title></head><body>\
        <article><p>{}</p><p>Cloudflare's interstitial says \"Just a moment...\" \
        while it runs its checks, and some sites show \"Access denied\" instead. \
        Are you a robot? The question is harder than it looks.</p></article>\
        </body></html>",
        "Real prose about bot detection, long enough that this is plainly a document \
         with an article behind it rather than a wall with a sentence on it. ".repeat(4)));
    assert_eq!(c.origin_refusal, None,
        "a document that DISCUSSES challenges must still be published");
}

/// An ordinary document is not a refusal, obviously — pinned because the
/// cheap version of this rule would fire on anything short.
#[test]
fn a_short_ordinary_document_is_not_a_refusal() {
    let c = run("<html><head><title>Notes</title></head><body><p>A brief note.</p></body></html>");
    assert_eq!(c.origin_refusal, None);
}
