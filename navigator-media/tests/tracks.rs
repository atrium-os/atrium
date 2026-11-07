//! ★ A VIDEO'S CAPTION TRACKS (fresco-video P3): each `<track>` the slot
//! carries is fetched beside the video (Nuntius `Fd`, capped), sent to the
//! player as `TextTrack` with its WebVTT file only after `Load`, and then
//! `ShowText` picks the default track, or the media's own when there is no
//! default or it failed. A track that fails never stops playback.

use limen::media_player::{TextChoice, ToChild, ToParent, TEXT_TRACKS_MAX};
use navigator_media::fake::FakePlayer;
use navigator_media::session::{MediaHost, Sessions, TrackStatus, TRACK_BYTES_MAX};
use navigator_media::slots::Slots;
use navigator_render::fontset::FontSet;
use navigator_render::source::{ContentSource, DocumentSource, GroupId, SceneSink, Want};
use navigator_render::Scene;
use std::collections::BTreeMap;

const PX: i64 = 64;

struct Ui<'a> { fonts: &'a FontSet, slots: Slots }
impl SceneSink for Ui<'_> {
    fn put(&mut self, g: GroupId, _: Option<(i64, i64)>, s: &Scene) { self.slots.put(g, &navigator_render::nsg::write(s, self.fonts)) }
    fn clear(&mut self, g: GroupId) { self.slots.clear(g) }
}

#[derive(Default)]
struct Host { players: BTreeMap<u64, FakePlayer>, track_fetches: Vec<(u8, String, u64)>, fds: Vec<String>, refuse_tracks: bool }
impl MediaHost for Host {
    fn request(&mut self, key: u64) -> Result<(), String> { self.players.insert(key, FakePlayer::new()); Ok(()) }
    fn send(&mut self, key: u64, msg: &ToChild) -> Result<(), String> { self.players.get_mut(&key).ok_or("no embed")?.receive(&msg.encode()); Ok(()) }
    fn send_fd(&mut self, key: u64, msg: &ToChild, _fd: std::os::fd::OwnedFd) -> Result<(), String> {
        self.fds.push(match msg { ToChild::Load { .. } => "load".into(), ToChild::TextTrack { id, .. } => format!("track {id}"), other => format!("{other:?}") });
        self.send(key, msg)
    }
    fn release(&mut self, key: u64) { self.players.remove(&key); }
    fn fetch(&mut self, _: u64, _: &str) -> Result<(), String> { Ok(()) }
    fn fetch_track(&mut self, _: u64, id: u8, url: &str, max_bytes: u64) -> Result<(), String> {
        if self.refuse_tracks { return Err("refused".into()) }
        self.track_fetches.push((id, url.to_string(), max_bytes));
        Ok(())
    }
    fn cancel_fetch(&mut self, _: u64) {}
}

fn fd() -> std::os::fd::OwnedFd { navigator_fetch::nuntius::anon_file().unwrap().1 }

/// One video on the first screen, with these children; its slot key.
fn slot(fonts: &FontSet, inner: &str) -> (Slots, u64) {
    let html = format!(r#"<html><head><style>body {{ margin: 8px }}</style></head><body><video src="v.webm" width="320" height="180">{inner}</video></body></html>"#);
    let mut src = DocumentSource::new(html, fonts, Default::default(), (800.0, 600.0), 600 * PX);
    let mut ui = Ui { fonts, slots: Slots::new() };
    src.want(Want { range: (0, 600 * PX), zoom_64: 64, prefetch: 0 }, &mut ui);
    let key = ui.slots.entries().next().expect("a slot").key;
    (ui.slots, key)
}

/// Start it, attach it, hand over the whole video file, play: what the
/// player then received, in order (the Load and what follows it).
fn play(s: &mut Sessions, slots: &Slots, key: u64, host: &mut Host) {
    s.press(slots, key, host).unwrap();
    s.attached(key, host);
    s.fetched(key, Some(fd()), true, host);
    let answers = host.players.get_mut(&key).unwrap().deliver();
    for a in answers { s.from_player(key, &a, host) }
}

fn after_load(host: &Host, key: u64) -> Vec<ToChild> {
    let r = &host.players[&key].received;
    let at = r.iter().position(|m| matches!(m, ToChild::Load { .. })).expect("Load went");
    r[at + 1..].iter().filter(|m| matches!(m, ToChild::TextTrack { .. } | ToChild::ShowText { .. })).cloned().collect()
}

#[test]
fn tracks_go_after_load_with_their_files_and_the_default_shows() {
    let fonts = FontSet::load().unwrap();
    let (slots, key) = slot(&fonts, r#"<track src="/subs/en.vtt" srclang="en" label="English"><track kind="captions" src="cc.vtt" srclang="en" label="CC" default>"#);
    let mut s = Sessions::new("https://a.test/watch/1");
    let mut host = Host::default();
    s.press(&slots, key, &mut host).unwrap();
    // Fetched beside the video, resolved like it, capped.
    assert_eq!(host.track_fetches, vec![(0, "https://a.test/subs/en.vtt".into(), TRACK_BYTES_MAX), (1, "https://a.test/watch/cc.vtt".into(), TRACK_BYTES_MAX)]);
    // A track whole BEFORE the player has Load is held.
    s.track_fetched(key, 0, Some(fd()), true, &mut host);
    assert!(host.fds.is_empty(), "nothing goes before the embed attaches and loads");
    s.attached(key, &mut host);
    s.fetched(key, Some(fd()), true, &mut host);
    assert_eq!(host.fds, vec!["load", "track 0"], "Load, then the track already in hand");
    assert!(host.players[&key].showing.is_none(), "the default is still coming: nothing chosen yet");
    s.track_fetched(key, 1, Some(fd()), true, &mut host);
    assert_eq!(after_load(&host, key), vec![
        ToChild::TextTrack { id: 0, lang: "en".into(), label: "English".into() },
        ToChild::TextTrack { id: 1, lang: "en".into(), label: "CC".into() },
        ToChild::ShowText { text: TextChoice::Page(1) },
    ]);
    assert_eq!(s.tracks(key), vec![(0, TrackStatus::Sent), (1, TrackStatus::Sent)]);
    assert!(host.players[&key].violations.is_empty(), "{:?}", host.players[&key].violations);
}

#[test]
fn no_default_shows_the_medias_own_text() {
    let fonts = FontSet::load().unwrap();
    let (slots, key) = slot(&fonts, r#"<track src="en.vtt" srclang="en" label="English">"#);
    let mut s = Sessions::new("https://a.test/");
    let mut host = Host::default();
    play(&mut s, &slots, key, &mut host);
    assert_eq!(host.players[&key].showing, Some(TextChoice::InBand));
    s.track_fetched(key, 0, Some(fd()), true, &mut host);
    assert_eq!(host.players[&key].text_tracks, vec![0], "a track that arrives later still goes");
    assert_eq!(host.players[&key].received.iter().filter(|m| matches!(m, ToChild::ShowText { .. })).count(), 1, "the choice is made once");
}

#[test]
fn a_failed_track_never_stops_playback() {
    let fonts = FontSet::load().unwrap();
    let (slots, key) = slot(&fonts, r#"<track src="en.vtt" srclang="en" label="English" default><track src="fr.vtt" srclang="fr">"#);
    let mut s = Sessions::new("https://a.test/");
    let mut host = Host::default();
    play(&mut s, &slots, key, &mut host);
    assert!(host.players[&key].received.contains(&ToChild::Play), "the video plays while its tracks are fetched");
    // The default's fetch fails (refused, past the cap, retries spent):
    // the media's own text shows, and the other track still goes.
    s.track_fetched(key, 0, None, true, &mut host);
    assert_eq!(host.players[&key].showing, Some(TextChoice::InBand));
    s.track_fetched(key, 1, Some(fd()), false, &mut host);
    assert_eq!(host.players[&key].text_tracks, Vec::<u8>::new(), "not whole yet: not sent");
    s.track_fetched(key, 1, None, true, &mut host);
    assert_eq!(host.players[&key].text_tracks, vec![1], "whole: sent");
    assert_eq!(s.tracks(key), vec![(0, TrackStatus::Failed), (1, TrackStatus::Sent)]);
    s.from_player(key, &ToParent::Playing, &mut host);
    assert!(s.is_live(key));
    assert!(host.players[&key].violations.is_empty(), "{:?}", host.players[&key].violations);
    // A fetcher that refuses every track: the video still starts.
    let mut s = Sessions::new("https://a.test/");
    let mut host = Host { refuse_tracks: true, ..Default::default() };
    play(&mut s, &slots, key, &mut host);
    assert!(host.players[&key].received.contains(&ToChild::Play));
    assert_eq!(host.players[&key].showing, Some(TextChoice::InBand));
}

#[test]
fn at_most_sixteen_tracks_and_only_fetchable_urls() {
    let fonts = FontSet::load().unwrap();
    let many: String = (0..20).map(|i| format!(r#"<track src="t{i}.vtt" srclang="x{i}" label="{}">"#, "L".repeat(80))).collect();
    let (slots, key) = slot(&fonts, &many);
    let mut s = Sessions::new("https://a.test/");
    let mut host = Host::default();
    play(&mut s, &slots, key, &mut host);
    assert_eq!(host.track_fetches.len(), TEXT_TRACKS_MAX as usize);
    for id in 0..TEXT_TRACKS_MAX { s.track_fetched(key, id, Some(fd()), true, &mut host) }
    let p = &host.players[&key];
    assert_eq!(p.text_tracks.len(), TEXT_TRACKS_MAX as usize);
    assert!(p.violations.is_empty(), "every TextTrack within the player's limits: {:?}", p.violations);
    assert!(p.received.iter().all(|m| !matches!(m, ToChild::TextTrack { label, .. } if label.len() > 64)));
    // A src that is not http(s) is never fetched; the video plays.
    let (slots, key) = slot(&fonts, r#"<track src="javascript:alert(1)" default><track src="data:text/vtt,WEBVTT">"#);
    let mut s = Sessions::new("https://a.test/");
    let mut host = Host::default();
    play(&mut s, &slots, key, &mut host);
    assert!(host.track_fetches.is_empty());
    assert_eq!(s.tracks(key), vec![(0, TrackStatus::Failed), (1, TrackStatus::Failed)]);
    assert_eq!(host.players[&key].showing, Some(TextChoice::InBand));
}
