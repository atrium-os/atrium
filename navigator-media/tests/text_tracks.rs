//! ★ `video.textTracks` AND `addTextTrack` ON THE PLAYER (fresco-video P3):
//! the page's track modes map onto `ShowText`, checked by the fake player
//! as the player checks it (no `TextTrack` before the source, no
//! `ShowText Page` for a track it never got). Cues a page adds by script
//! are out of scope: such a track is listed and keeps its mode, but shows
//! no page text.

use limen::media_player::{TextChoice, ToChild, TEXT_TRACKS_MAX};
use navigator_media::fake::FakePlayer;
use navigator_media::mse::{DomError, Fire, Media, TrackMode};
use navigator_render::{TextTrack, TrackKind};

fn send(m: &mut Media, p: &mut FakePlayer) -> Vec<ToChild> {
    let out = m.take_out();
    for msg in &out { p.receive(&msg.encode()) }
    out
}

fn track(id: u8, lang: &str, default: bool) -> TextTrack {
    TextTrack { id, src: format!("{lang}.vtt"), kind: TrackKind::Subtitles, srclang: lang.into(), label: lang.to_uppercase(), default }
}

fn shows(out: &[ToChild]) -> Vec<TextChoice> {
    out.iter().filter_map(|m| match m { ToChild::ShowText { text } => Some(*text), _ => None }).collect()
}

#[test]
fn the_default_track_shows_once_its_file_is_with_the_player() {
    let mut m = Media::new();
    let mut p = FakePlayer::new();
    m.element_text_track(&track(0, "en", false));
    m.element_text_track(&track(1, "fr", true));
    assert_eq!(m.text_tracks().iter().map(|t| t.mode).collect::<Vec<_>>(), vec![TrackMode::Disabled, TrackMode::Showing], "HTML honours default");
    assert_eq!(m.take_events(), vec![Fire::TextTracks("addtrack"), Fire::TextTracks("addtrack")]);
    // A file before the source waits for it.
    m.text_track_file(1).unwrap();
    assert!(send(&mut m, &mut p).is_empty(), "nothing before the source");
    m.load_fd("video/webm").unwrap();
    let out = send(&mut m, &mut p);
    assert!(matches!(out[0], ToChild::Load { .. }));
    assert_eq!(out[1], ToChild::TextTrack { id: 1, lang: "fr".into(), label: "FR".into() }, "the file goes right after Load");
    assert_eq!(shows(&out), vec![TextChoice::Page(1)]);
    assert!(p.violations.is_empty(), "{:?}", p.violations);
}

#[test]
fn modes_map_onto_what_the_player_shows() {
    let mut m = Media::new();
    let mut p = FakePlayer::new();
    m.element_text_track(&track(0, "en", true));
    m.element_text_track(&track(1, "de", false));
    m.load_fd("video/webm").unwrap();
    assert!(shows(&send(&mut m, &mut p)).is_empty(), "the default's file is still coming: wait, don't say None first");
    m.text_track_file(0).unwrap();
    m.text_track_file(1).unwrap();
    assert_eq!(shows(&send(&mut m, &mut p)), vec![TextChoice::Page(0)]);
    m.take_events();
    // The reader (through the page) picks German; English goes off.
    m.set_text_track_mode(1, "showing").unwrap();
    m.set_text_track_mode(0, "disabled").unwrap();
    // German showing alongside English changes nothing (the first showing
    // track with a file is still English); English off makes it German.
    assert_eq!(shows(&send(&mut m, &mut p)), vec![TextChoice::Page(1)], "one change, sent once");
    assert_eq!(m.take_events(), vec![Fire::TextTracks("change"), Fire::TextTracks("change")]);
    // Hidden is not shown: no page text at all.
    m.set_text_track_mode(1, "hidden").unwrap();
    assert_eq!(shows(&send(&mut m, &mut p)), vec![TextChoice::None]);
    m.take_events();
    // An unknown mode is ignored (an enum attribute), with no event; the
    // same mode again changes nothing.
    m.set_text_track_mode(1, "bogus").unwrap();
    m.set_text_track_mode(1, "hidden").unwrap();
    assert!(m.take_events().is_empty());
    assert!(send(&mut m, &mut p).is_empty());
    assert_eq!(m.set_text_track_mode(9, "showing"), Err(DomError::NotSupported), "no such track");
    assert!(p.violations.is_empty(), "{:?}", p.violations);
}

#[test]
fn add_text_track_is_listed_but_its_script_cues_are_out_of_scope() {
    let mut m = Media::new();
    let mut p = FakePlayer::new();
    m.element_text_track(&track(0, "en", false));
    m.load_fd("video/webm").unwrap();
    assert!(shows(&send(&mut m, &mut p)).is_empty(), "none showing and the page chose nothing: the media's own text stays");
    let id = m.add_text_track("captions", "Live captions", "en").unwrap();
    assert_eq!(id, 1, "the next free id");
    let t = m.text_tracks().iter().find(|t| t.id == id).unwrap().clone();
    assert_eq!((t.mode, t.element, t.kind.as_str()), (TrackMode::Hidden, false, "captions"), "a script's track starts hidden");
    m.set_text_track_mode(id, "showing").unwrap();
    assert_eq!(shows(&send(&mut m, &mut p)), vec![TextChoice::None], "showing it shows no page text: its cues are the script's");
    assert!(m.text_track_file(id).is_err(), "a script's track has no file");
    assert_eq!(m.add_text_track("karaoke", "", ""), Err(DomError::Type));
    // Within the player's limits: 16 tracks, names of 64 bytes.
    for _ in 2..TEXT_TRACKS_MAX { m.add_text_track("subtitles", &"x".repeat(100), "en\u{7}").unwrap(); }
    assert_eq!(m.add_text_track("subtitles", "", ""), Err(DomError::NotSupported), "17th");
    let last = m.text_tracks().last().unwrap();
    assert_eq!((last.label.len(), last.language.as_str()), (64, "en"));
    assert!(p.violations.is_empty(), "{:?}", p.violations);
}

#[test]
fn a_video_with_no_page_tracks_keeps_the_players_own_text() {
    let mut m = Media::new();
    let mut p = FakePlayer::new();
    m.load_fd("video/webm").unwrap();
    assert!(shows(&send(&mut m, &mut p)).is_empty(), "InBand is the player's default: nothing to send");
}
