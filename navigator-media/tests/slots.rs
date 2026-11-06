//! ★ A DOCUMENT'S VIDEOS AS MEDIA EMBEDS: slots read from the band groups a
//! real content source writes, live embeds started on a press or by muted
//! autoplay, at most 8, `Load` with the resolved URL, released with their
//! band — fed through the fake player.

use limen::media_player::{Source, ToChild, ToParent};
use navigator_media::fake::FakePlayer;
use navigator_media::session::{MediaHost, NotStarted, Sessions, LIVE_MAX};
use navigator_media::slots::{Slots, MEDIA_KEY_BIT};
use navigator_render::fontset::FontSet;
use navigator_render::source::{ContentSource, DocumentSource, GroupId, SceneSink, Want};
use navigator_render::{EmbedSource, Scene};
use std::collections::BTreeMap;

const PX: i64 = 64;

/// The UI's sink: band groups as NSG, read into the slot list.
struct Ui<'a> { fonts: &'a FontSet, slots: Slots }
impl SceneSink for Ui<'_> {
    fn put(&mut self, g: GroupId, _: Option<(i64, i64)>, s: &Scene) { self.slots.put(g, &navigator_render::nsg::write(s, self.fonts)) }
    fn clear(&mut self, g: GroupId) { self.slots.clear(g) }
}

/// Limen as the UI sees it, with one fake player per embed.
#[derive(Default)]
struct Host { players: BTreeMap<u64, FakePlayer>, requested: Vec<u64>, released: Vec<u64>, refuse: bool }
impl MediaHost for Host {
    fn request(&mut self, key: u64) -> Result<(), String> {
        if self.refuse { return Err("refused".into()) }
        self.requested.push(key);
        self.players.insert(key, FakePlayer::new());
        Ok(())
    }
    fn send(&mut self, key: u64, msg: &ToChild) -> Result<(), String> {
        self.players.get_mut(&key).ok_or("no embed")?.receive(&msg.encode());
        Ok(())
    }
    fn release(&mut self, key: u64) { self.released.push(key); self.players.remove(&key); }
}

fn page(videos: &[&str]) -> String {
    let mut b = String::from("<html><head><style>body { margin: 8px } p { margin: 0px 0px 8px }</style></head><body>");
    for (i, v) in videos.iter().enumerate() {
        for j in 0..40 { b.push_str(&format!("<p>Paragraph {i}.{j}: some words to fill a line or two at this width, and then some.</p>")) }
        b.push_str(v);
    }
    b.push_str("<p>The end.</p></body></html>");
    b
}

fn want(src: &mut DocumentSource, ui: &mut Ui, y: i64) { src.want(Want { range: (y, y + 600 * PX), zoom_64: 64, prefetch: 600 * PX }, ui); }

#[test]
fn slots_follow_the_bands_and_carry_media_keys() {
    let fonts = FontSet::load().unwrap();
    let html = page(&[r#"<video src="/a.webm" width="320" height="180"></video>"#, r#"<video src="b.mp4" width="320" height="180" controls></video>"#]);
    let mut src = DocumentSource::new(html, &fonts, Default::default(), (800.0, 600.0), 600 * PX);
    let mut ui = Ui { fonts: &fonts, slots: Slots::new() };
    want(&mut src, &mut ui, 0);
    assert!(ui.slots.is_empty(), "no video on the first screen");
    for _ in 0..100 { src.idle(8, &mut ui); }
    let (h, _) = src.banded().extent();
    // Scroll down a screen at a time: each video's slot appears with its
    // band and leaves with it.
    let mut seen = std::collections::BTreeSet::new();
    let mut y = 0;
    while y < h {
        want(&mut src, &mut ui, y);
        for e in ui.slots.entries() {
            assert!(e.key & MEDIA_KEY_BIT != 0, "a media key has its top bit set");
            assert_eq!(e.role, "media-player");
            assert!(e.rect.1 + e.rect.3 > y - 600 * PX && e.rect.1 < y + 1200 * PX, "only slots of resident bands");
            seen.insert(e.key);
        }
        y += 600 * PX;
    }
    assert_eq!(seen.len(), 2, "both videos were slots on the way");
    assert!(ui.slots.len() <= 1, "at the end, only the second's band is resident");
    let ch = ui.slots.take_changes();
    assert!(!ch.removed.is_empty(), "the first video's slot left with its band");
}

#[test]
fn a_press_starts_a_player_that_loads_the_resolved_url() {
    let fonts = FontSet::load().unwrap();
    let html = r#"<html><body><video src="/media/a.webm" width="320" height="180"></video><video src="" width="10" height="10"></video><video width="10" height="10"><source src="x.ogv"></video></body></html>"#.to_string();
    let mut src = DocumentSource::new(html, &fonts, Default::default(), (800.0, 600.0), 600 * PX);
    let mut ui = Ui { fonts: &fonts, slots: Slots::new() };
    want(&mut src, &mut ui, 0);
    let mut s = Sessions::new("https://site.test/page/index.html");
    let mut host = Host::default();
    let ch = ui.slots.take_changes();
    assert_eq!(ch.added.len(), 3);
    assert!(s.sync(&ui.slots, &ch, &mut host).is_empty(), "nothing autoplays");
    assert!(host.requested.is_empty(), "no player until a press");
    let keys: Vec<u64> = ui.slots.entries().map(|e| e.key).collect();
    let by_src = |want: &EmbedSource| ui.slots.entries().find(|e| &e.src == want).map(|e| e.key).unwrap();
    let file = keys.iter().copied().find(|k| matches!(ui.slots.get(*k).unwrap().src, EmbedSource::Url { .. })).unwrap();
    assert_eq!(s.press(&ui.slots, by_src(&EmbedSource::Script), &mut host), Err(NotStarted::NeedsEngine));
    assert_eq!(s.press(&ui.slots, by_src(&EmbedSource::Unplayable), &mut host), Err(NotStarted::Unplayable));
    s.press(&ui.slots, file, &mut host).unwrap();
    assert_eq!(host.requested, vec![file]);
    s.attached(file, &mut host);
    let p = host.players.get_mut(&file).unwrap();
    assert_eq!(p.received, vec![ToChild::Load { source: Source::Url("https://site.test/media/a.webm".into()), mime: "video/webm".into() }]);
    // Ready: the press asked to play.
    for m in p.deliver() { s.from_player(file, &m, &mut host) }
    let p = host.players.get_mut(&file).unwrap();
    assert_eq!(p.received.last(), Some(&ToChild::Play));
    assert!(p.violations.is_empty(), "{:?}", p.violations);
}

#[test]
fn muted_autoplay_starts_by_itself_at_most_eight_live_and_loops() {
    let fonts = FontSet::load().unwrap();
    let mut body = String::from("<html><body>");
    for i in 0..10 { body.push_str(&format!(r#"<video src="https://cdn.test/{i}.mp4" width="40" height="30" autoplay muted loop></video>"#)) }
    body.push_str(r#"<video src="https://cdn.test/loud.mp4" width="40" height="30" autoplay></video></body></html>"#);
    let mut src = DocumentSource::new(body, &fonts, Default::default(), (800.0, 600.0), 600 * PX);
    let mut ui = Ui { fonts: &fonts, slots: Slots::new() };
    want(&mut src, &mut ui, 0);
    let mut s = Sessions::new("https://site.test/");
    let mut host = Host::default();
    let ch = ui.slots.take_changes();
    let refused = s.sync(&ui.slots, &ch, &mut host);
    assert_eq!(s.live(), LIVE_MAX, "eight live");
    assert_eq!(refused.iter().filter(|(_, w)| *w == NotStarted::TooMany).count(), 2, "the ninth and tenth wait");
    let loud = ui.slots.entries().find(|e| matches!(&e.src, EmbedSource::Url { src, .. } if src.contains("loud"))).unwrap().key;
    assert!(!s.is_live(loud), "unmuted autoplay waits for a press");
    let k = host.requested[0];
    s.attached(k, &mut host);
    assert_eq!(host.players[&k].received[0], ToChild::SetVolume { volume: 0.0 }, "muted before it loads");
    for m in host.players.get_mut(&k).unwrap().deliver() { s.from_player(k, &m, &mut host) }
    // At the end, `loop` starts it again.
    s.from_player(k, &ToParent::Ended, &mut host);
    assert!(host.players[&k].received.ends_with(&[ToChild::Seek { secs: 0.0 }, ToChild::Play]));
    assert!(host.players.values().all(|p| p.violations.is_empty()));
}

#[test]
fn a_slot_whose_band_is_evicted_releases_its_player() {
    let fonts = FontSet::load().unwrap();
    let html = page(&[r#"<video src="https://cdn.test/a.webm" width="320" height="180" autoplay muted></video>"#, "<p>x</p>"]);
    let mut src = DocumentSource::new(html, &fonts, Default::default(), (800.0, 600.0), 600 * PX);
    let mut ui = Ui { fonts: &fonts, slots: Slots::new() };
    for _ in 0..100 { src.idle(8, &mut ui); }
    let mut s = Sessions::new("https://site.test/");
    let mut host = Host::default();
    let mut y = 0;
    let (h, _) = src.banded().extent();
    let mut live_seen = false;
    while y < h {
        want(&mut src, &mut ui, y);
        let ch = ui.slots.take_changes();
        s.sync(&ui.slots, &ch, &mut host);
        live_seen |= s.live() == 1;
        y += 600 * PX;
    }
    assert!(live_seen, "it played while its band was resident");
    assert_eq!(s.live(), 0, "and was released with it");
    assert_eq!(host.released, host.requested);
}

#[test]
fn the_embeds_message_holds_rects_only_for_slots_wholly_in_view() {
    let fonts = FontSet::load().unwrap();
    let html = r#"<html><body style="margin:0"><video src="a.webm" width="320" height="180"></video><p>x</p></body></html>"#.to_string();
    let mut src = DocumentSource::new(html, &fonts, Default::default(), (800.0, 600.0), 600 * PX);
    let mut ui = Ui { fonts: &fonts, slots: Slots::new() };
    want(&mut src, &mut ui, 0);
    let e = ui.slots.entries().next().unwrap().clone();
    let m = ui.slots.embeds_message((0, 600 * PX), 64, 0.0);
    assert_eq!(m[0].0, e.key);
    let r = m[0].1.expect("wholly in view");
    assert_eq!((r[2], r[3]), (320.0, 180.0));
    let m = ui.slots.embeds_message((e.rect.1 + 10 * PX, e.rect.1 + 610 * PX), 64, 0.0);
    assert_eq!(m[0].1, None, "partly scrolled out: released");
    let z = ui.slots.embeds_message((0, 600 * PX), 128, 0.0);
    assert_eq!(z[0].1.unwrap()[2], 640.0, "at 200% the rect doubles");
}
