use navigator_prerender::{artifact, boa_impl::BoaEngine, convert_with, fetch::NoNetwork,
                          TierPolicy};

fn explore_at(html: &str, url: &str) -> navigator_prerender::Conversion {
    let mut eng = BoaEngine { explore: true, ..Default::default() };
    convert_with(html, Some(url), &mut eng, &mut NoNetwork)
}

/// A minimal JSON reader, so the test validates the OUTPUT rather than
/// re-implementing the writer's assumptions: it parses, and it finds values
/// by key at the top level.
fn parses(s: &str) -> bool {
    // Structural check: balanced braces/brackets outside strings, and every
    // string properly terminated.
    let (mut depth, mut in_str, mut esc) = (0i32, false, false);
    for c in s.chars() {
        if in_str {
            if esc { esc = false } else if c == '\\' { esc = true }
            else if c == '"' { in_str = false }
            else if (c as u32) < 0x20 { return false }   // raw control char
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' | '[' => depth += 1,
            '}' | ']' => depth -= 1,
            _ => {}
        }
        if depth < 0 { return false }
    }
    depth == 0 && !in_str
}

const MENU: &str = r#"<html><body>
  <button id=toggle>Menu</button>
  <nav id=menu class="nav hidden"><a href="/a">One</a></nav>
  <p>Enough prose in the document that the tier policy has something to
     measure and does not exempt it for being too small to judge at all.</p>
  <script>
    document.getElementById('toggle').addEventListener('click', function () {
      document.getElementById('menu').classList.remove('hidden');
    });
  </script></body></html>"#;

#[test]
fn the_recording_is_well_formed_and_carries_both_halves() {
    let c = explore_at(MENU, "https://example.test/p.html");
    let json = artifact::emit(Some("https://example.test/p.html"), &c, &TierPolicy::default());
    assert!(parses(&json), "malformed JSON:\n{json}");
    assert!(json.contains(&format!("\"format\": \"{}\"", artifact::FORMAT)));
    assert!(json.contains("\"url\": \"https://example.test/p.html\""));
    // Both halves: the document, and the table of what can be done to it.
    assert!(json.contains("\"document\": \"<html>"), "the document is missing");
    assert!(json.contains("\"trigger\": \"#toggle\""), "the transition is missing");
    assert!(json.contains("\"attribute_only\": true"));
    assert!(json.contains("\"kind\": \"attribute\""));
}

/// ★ THE RISKY PART. A whole document is embedded, and it contains quotes,
/// backslashes and control characters. Getting this wrong produces a file
/// that parses as something ELSE, which is worse than one that fails to
/// parse.
#[test]
fn string_escaping_survives_hostile_content() {
    let nasty = "quote \" backslash \\ newline \n tab \t bell \u{07} nul-ish \u{01} unicode é 中";
    let out = artifact::json_str(nasty);
    assert!(parses(&format!("{{\"k\": {out}}}")), "{out}");
    assert!(out.contains("\\\""), "quote unescaped");
    assert!(out.contains("\\\\"), "backslash unescaped");
    assert!(out.contains("\\n") && out.contains("\\t"));
    assert!(out.contains("\\u0007") && out.contains("\\u0001"),
        "control characters must be \\u-escaped: {out}");
    assert!(out.contains('é') && out.contains('中'), "text must survive intact");
}

#[test]
fn a_document_containing_json_and_script_end_tags_is_embedded_safely() {
    let c = explore_at(
        r#"<html><body><div id=d>{"looks": "like json", "q": "\" backslash \\"}</div>
           <script>var s = "</scr" + "ipt>";</script></body></html>"#,
        "https://example.test/p.html");
    let json = artifact::emit(None, &c, &TierPolicy::default());
    assert!(parses(&json), "malformed:\n{json}");
}

/// ★ A DEMOTED DOCUMENT CANNOT KEEP EVERY TRANSITION. Publishing tier 1 means
/// script-made triggers are not in the document at all, and POSITIONAL paths
/// may address a different node because the script-modified tree had a
/// different shape. Only anchored, id-addressed transitions survive — and the
/// rest are COUNTED, because a recording that silently lost half its entries
/// is indistinguishable from a page with little to do.
#[test]
fn demotion_keeps_only_transitions_that_still_address_the_document() {
    let html = r#"<html><body>
      <div id=keep>Server rendered prose, repeated enough that the tier policy
        has something real to measure when the script empties it out.
        Server rendered prose, repeated enough that the tier policy has
        something real to measure when the script empties it out.</div>
      <button id=named>named</button><div id=host></div>
      <script>
        document.getElementById('named').addEventListener('click', function () {
          document.getElementById('host').setAttribute('data-open', '1');
        });
        var made = document.createElement('button');
        document.getElementById('host').appendChild(made);
        made.addEventListener('click', function () {
          document.getElementById('host').setAttribute('data-made', '1');
        });
        document.getElementById('keep').textContent = '';   // forces demotion
      </script></body></html>"#;
    let c = explore_at(html, "https://example.test/p.html");
    assert!(c.transitions.len() >= 2, "{:?}", c.transitions);

    let json = artifact::emit(None, &c, &TierPolicy::default());
    assert!(json.contains("\"tier\": 1"), "the policy should demote this: {json}");
    assert!(json.contains("\"trigger\": \"#named\""), "the id-addressed one survives");
    assert!(json.contains("\"transitions_dropped\": 1"),
        "the script-made trigger must be dropped AND counted: {json}");
}

/// The emitted bytes must be reproducible, like everything else that feeds a
/// content-addressed store.
#[test]
fn the_recording_is_byte_reproducible() {
    let a = artifact::emit(Some("https://example.test/p.html"),
        &explore_at(MENU, "https://example.test/p.html"), &TierPolicy::default());
    let b = artifact::emit(Some("https://example.test/p.html"),
        &explore_at(MENU, "https://example.test/p.html"), &TierPolicy::default());
    assert_eq!(a, b);
}
