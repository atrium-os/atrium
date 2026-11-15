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

/// Every URL the page's network was asked for.
struct Logged(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
impl navigator_prerender::fetch::Fetcher for Logged {
    fn get(&mut self, url: &str) -> Result<String, String> {
        self.0.lock().unwrap().push(url.to_string());
        Ok("{}".into())
    }
}

/// ★ The converter's own press of a consent control is not the reader's
/// consent: the page network is closed while it runs, so a request the
/// press makes (a consent endpoint) never reaches the site.
#[test]
fn the_converters_press_sends_nothing() {
    let html = r#"<html><body><div id=wall><button id=yes>Accept</button></div><script>
        fetch('/api/at-load');
        document.getElementById('yes').addEventListener('click', function () {
          document.cookie = 'consent=yes; Path=/';
          fetch('/api/consent?accept=1');
          var i = new Image(); i.src = '/api/pixel?accept=1';
        });
      </script></body></html>"#;
    let log = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
    let mut eng = BoaEngine { page_fetcher: Some(Box::new(Logged(log.clone()))), ..Default::default() };
    let c = convert_with(html, Some("https://example.test/p.html"), &mut eng, &mut MapFetcher::default());
    assert_eq!(c.consents.len(), 1, "{:?}", c.errors);
    let asked = log.lock().unwrap().clone();
    assert!(asked.iter().any(|u| u.contains("/api/at-load")), "the page network was not wired: {asked:?}");
    assert!(!asked.iter().any(|u| u.contains("accept=1")), "the converter's press reached the site: {asked:?}");
}

/// ★ Any language: a control in a consent platform's container is tried
/// whatever it says, and every one whose press writes a cookie is marked,
/// Reject as well as Accept. A control outside any wall that says no
/// consent word is not pressed, even if it would write a cookie.
#[test]
fn a_wall_in_any_language_offers_both_answers() {
    let html = r#"<html><body>
      <div id="onetrust-banner-sdk"><p>Wir verwenden Cookies.</p>
        <button id=ja>Alle zulassen</button><button id=nein>Nur notwendige</button><button id=info>Mehr</button>
      </div>
      <div role="dialog"><button id=hi>स्वीकार करें</button></div>
      <main><button id=cart>In den Warenkorb</button></main>
      <script>
        function on(id, f) { document.getElementById(id).addEventListener('click', f); }
        on('ja', function () { document.cookie = 'OptanonConsent=all; Path=/'; });
        on('nein', function () { document.cookie = 'OptanonConsent=necessary; Path=/'; });
        on('hi', function () { document.cookie = 'c=hi; Path=/'; });
        on('cart', function () { document.cookie = 'cart=1; Path=/'; });
      </script></body></html>"#;
    let c = convert_with(html, Some("https://example.de/"), &mut BoaEngine::default(), &mut MapFetcher::default());
    assert_eq!(c.consents, vec![
        vec!["OptanonConsent=all; Path=/".to_string()],
        vec!["OptanonConsent=necessary; Path=/".to_string()],
        vec!["c=hi; Path=/".to_string()],
    ], "{:?}", c.errors);
    assert!(c.html.contains(r#"<a id="ja" href="atrium-consent:0">Alle zulassen</a>"#), "{}", c.html);
    assert!(c.html.contains(r#"<a id="nein" href="atrium-consent:1">Nur notwendige</a>"#), "{}", c.html);
    assert!(c.html.contains(r#"<button id="info">Mehr</button>"#), "{}", c.html);
    assert!(c.html.contains(r#"<button id="cart">In den Warenkorb</button>"#), "{}", c.html);
}

/// ★ A banner, not a wall: the page is readable without its answer, so the
/// banner is taken out of the document and nothing of it is offered or
/// kept (navigator-ui.md §6.2). The same banner over a page with nothing
/// else to read is a wall, and its controls are marked.
#[test]
fn a_banner_is_hidden_and_a_wall_is_offered() {
    let banner = r#"<div id="onetrust-banner-sdk"><p>We use cookies to improve your experience.</p>
        <button id=ok>Accept all</button><button id=no>Reject all</button></div>
      <script>
        document.getElementById('ok').addEventListener('click', function () { document.cookie = 'c=1; Path=/'; });
        document.getElementById('no').addEventListener('click', function () { document.cookie = 'c=0; Path=/'; });
      </script>"#;
    let article = "<article><h1>The atrium</h1>".to_string() + &"<p>An atrium is an open central court in a house, around which the rooms are arranged.</p>".repeat(8) + "</article>";
    let page = format!("<html><body>{article}{banner}</body></html>");
    let c = convert_with(&page, Some("https://news.example/a"), &mut BoaEngine::default(), &mut MapFetcher::default());
    assert_eq!(c.scripts_failed, 0, "{:?}", c.errors);
    assert!(c.consents.is_empty(), "{:?}", c.consents);
    assert_eq!(c.banners_hidden, 1);
    assert!(!c.html.contains("onetrust-banner-sdk") && !c.html.contains("atrium-consent:"), "{}", c.html);
    assert!(c.html.contains("open central court"), "{}", c.html);

    let wall = format!("<html><body><p>Subscribe to read.</p>{banner}</body></html>");
    let c = convert_with(&wall, Some("https://news.example/b"), &mut BoaEngine::default(), &mut MapFetcher::default());
    assert_eq!(c.banners_hidden, 0);
    assert_eq!(c.consents.len(), 2, "{:?}", c.consents);
}
