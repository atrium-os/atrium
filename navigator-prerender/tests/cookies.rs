use navigator_prerender::{boa_impl::BoaEngine, convert_with, fetch::MapFetcher, serve::Job};

const PAGE: &str = r#"<html><body><div id=r></div><script>
    var before = document.cookie;
    document.cookie = 'seen=1; Path=/';
    document.cookie = 'consent=; Max-Age=0';
    document.getElementById('r').setAttribute('before', before);
    document.getElementById('r').setAttribute('after', document.cookie);
    document.getElementById('r').setAttribute('on', String(navigator.cookieEnabled));
  </script></body></html>"#;

/// ★ A cookied conversion's script reads what Arca gave it and may write,
/// but the writes stay in this run's scratch jar (legacy-web §5.4.1e step 6).
#[test]
fn a_cookied_scripts_see_their_cookies_and_writes_stay_put() {
    let mut eng = BoaEngine { cookies: Some("consent=v; lang=en".into()), ..Default::default() };
    let c = convert_with(PAGE, Some("https://example.test/p.html"), &mut eng, &mut MapFetcher::default());
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains(r#"before="consent=v; lang=en""#), "{}", c.html);
    assert!(c.html.contains(r#"after="lang=en; seen=1""#), "{}", c.html);
    assert!(c.html.contains(r#"on="true""#), "{}", c.html);
    assert_eq!(eng.cookies.as_deref(), Some("consent=v; lang=en"), "a write reached the engine's jar");
}

/// Without a jar the page sees no cookies at all, and its writes vanish.
#[test]
fn an_anonymous_scripts_see_no_cookies() {
    let mut eng = BoaEngine::default();
    let c = convert_with(PAGE, Some("https://example.test/p.html"), &mut eng, &mut MapFetcher::default());
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.html.contains(r#"before="""#) && c.html.contains(r#"after="""#), "{}", c.html);
    assert!(c.html.contains(r#"on="false""#), "{}", c.html);
}

/// The job header carries the cookies whole, and refuses ones that would
/// break the header apart.
#[test]
fn the_job_header_carries_cookies() {
    let j = Job { cookies: Some("a=1; b=x=y".into()), len: 3, ..Default::default() };
    let back = Job::parse(j.header().unwrap().trim_end()).unwrap();
    assert_eq!(back.cookies.as_deref(), Some("a=1; b=x=y"));
    assert_eq!(Job::parse(Job { len: 3, ..Default::default() }.header().unwrap().trim_end()).unwrap().cookies, None);
    assert!(Job { cookies: Some("a=1\tlen=9".into()), ..Default::default() }.header().is_err());
    assert!(Job { cookies: Some("a=1\nX".into()), ..Default::default() }.header().is_err());
}

const WALL: &str = r#"<html><body>
  <div id=wall><p>We use cookies.</p>
    <button id=yes>Accept all</button>
    <button id=more>More options</button>
    <button id=no>Continue</button>
  </div>
  <script>
    var wall = document.getElementById('wall');
    document.getElementById('yes').addEventListener('click', function () {
      document.cookie = 'consent=yes; Path=/; Max-Age=31536000';
      wall.setAttribute('class', 'gone');
    });
    document.getElementById('more').addEventListener('click', function () { wall.setAttribute('data-open', '1'); });
    document.body.setAttribute('seen', document.cookie || 'none');
  </script>
  </body></html>"#;

/// ★ A consent wall's control is found by pressing it (legacy-web
/// §5.4.1e): the one whose press writes a cookie is marked for the reader
/// to press, and what it wrote is reported; the press itself is put back.
#[test]
fn a_consent_control_is_found_and_marked() {
    for cookies in [None, Some(String::new())] {
        let mut eng = BoaEngine { cookies: cookies.clone(), ..Default::default() };
        let c = convert_with(WALL, Some("https://example.test/p.html"), &mut eng, &mut MapFetcher::default());
        assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
        assert_eq!(c.consents, vec![vec!["consent=yes; Path=/; Max-Age=31536000".to_string()]], "{cookies:?}");
        assert!(c.html.contains(r#"href="atrium-consent:0""#), "{}", c.html);
        // The probe was put back: the wall shows, and the jar is as it was.
        assert!(!c.html.contains(r#"class="gone""#), "{}", c.html);
        assert!(c.html.contains(r#"<a id="yes" href="atrium-consent:0">Accept all</a>"#), "{}", c.html);
        assert!(c.html.contains(r#"<button id="no">Continue</button>"#), "{}", c.html);
        assert_eq!(c.html.matches("atrium-consent:").count(), 1, "{}", c.html);
    }
}
