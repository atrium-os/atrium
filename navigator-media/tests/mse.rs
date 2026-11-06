//! ★ THE MSE FRONT END AGAINST A FAKE PLAYER (limen.md §2.2). Every message
//! the front end sends is decoded and checked by the fake as the player
//! checks it, with the order and limits of the role; answers are held back
//! where a test needs to see the front end wait. Verified against the role's
//! contract only: real playback is the native player's (P1).

use limen::media_player::{code, ToChild, ToParent, APPEND_MAX};
use navigator_media::fake::FakePlayer;
use navigator_media::mse::{DomError, Fire, Media, ReadyState};

/// Send what the front end has to the fake (as bytes).
fn send(m: &mut Media, p: &mut FakePlayer) -> Vec<ToChild> {
    let out = m.take_out();
    for msg in &out { p.receive(&msg.encode()) }
    out
}

/// Answer everything, repeatedly, until quiet.
fn settle(m: &mut Media, p: &mut FakePlayer) {
    for _ in 0..1000 {
        send(m, p);
        let ans = p.deliver();
        if ans.is_empty() && m.take_out().is_empty() { return }
        for a in ans { m.from_player(a) }
    }
    panic!("never quiet");
}

fn feed() -> (Media, FakePlayer) {
    let mut m = Media::new();
    let mut p = FakePlayer::new();
    m.prime(&["video/webm; codecs=\"vp9\"", "audio/webm; codecs=\"opus\"", "video/mp4; codecs=\"hev1.1.6.L93.B0\""]);
    settle(&mut m, &mut p);
    m.attach_source().unwrap();
    assert_eq!(m.take_events(), vec![Fire::Source("sourceopen")]);
    assert_eq!(send(&mut m, &mut p), vec![ToChild::OpenSource]);
    (m, p)
}

#[test]
fn is_type_supported_answers_from_the_player() {
    let (mut m, _) = feed();
    assert!(m.is_type_supported("video/webm; codecs=\"vp9\""));
    assert!(!m.is_type_supported("video/mp4; codecs=\"hev1.1.6.L93.B0\""), "the player said no");
    // An unasked type answers false, and is asked.
    assert!(!m.is_type_supported("video/mp4; codecs=\"av01.0.05M.08\""));
    assert!(matches!(m.take_out().as_slice(), [ToChild::CanPlay { .. }]));
}

#[test]
fn a_feed_appends_one_message_in_flight_per_track() {
    let (mut m, mut p) = feed();
    let v = m.add_source_buffer("video/webm; codecs=\"vp9\"").unwrap();
    let a = m.add_source_buffer("audio/webm; codecs=\"opus\"").unwrap();
    assert_eq!(m.add_source_buffer("video/mp4; codecs=\"hev1.1.6.L93.B0\""), Err(DomError::NotSupported));
    m.set_duration(212.0).unwrap();
    m.set_timestamp_offset(v, 0.5).unwrap();
    // 150 KiB of video and 20 KiB of audio, at once.
    m.append_buffer(v, &vec![7u8; 150 * 1024]).unwrap();
    m.append_buffer(a, &vec![9u8; 20 * 1024]).unwrap();
    assert_eq!(m.append_buffer(v, b"more"), Err(DomError::InvalidState), "a second append while updating throws");
    let sent = send(&mut m, &mut p);
    let appends: Vec<(u8, usize, Option<f64>)> = sent.iter().filter_map(|s| match s { ToChild::Append { track, bytes, timestamp_offset } => Some((*track, bytes.len(), *timestamp_offset)), _ => None }).collect();
    assert_eq!(appends, vec![(v, APPEND_MAX, Some(0.5)), (a, 20 * 1024, None)], "one in flight per track, the offset with the first");
    // Nothing more goes until the player answers.
    assert!(send(&mut m, &mut p).is_empty());
    settle(&mut m, &mut p);
    assert!(p.violations.is_empty(), "{:?}", p.violations);
    assert_eq!(p.bytes(v), 150 * 1024);
    let ev = m.take_events();
    let vend: Vec<&Fire> = ev.iter().filter(|e| matches!(e, Fire::Buffer(t, _) if *t == v)).collect();
    assert_eq!(vend, vec![&Fire::Buffer(v, "updatestart"), &Fire::Buffer(v, "update"), &Fire::Buffer(v, "updateend")], "one updateend for the whole append");
    assert!(ev.contains(&Fire::Element("loadedmetadata")) && ev.contains(&Fire::Element("canplay")));
    assert!(!m.updating(v));
    assert!(!m.source_buffered(v).is_empty() && !m.source_buffered(a).is_empty());
    let both = m.buffered();
    assert_eq!(both.len(), 1);
    assert!(both[0].1 <= m.source_buffered(a)[0].1, "the element's buffered is the tracks' intersection");
    assert_eq!(m.duration, 212.0, "a feed's duration is the page's");
}

#[test]
fn too_big_is_quota_exceeded_until_a_remove() {
    let (mut m, mut p) = feed();
    p.cap = 100 * 1024;
    let v = m.add_source_buffer("video/webm; codecs=\"vp9\"").unwrap();
    m.append_buffer(v, &vec![1u8; 90 * 1024]).unwrap();
    settle(&mut m, &mut p);
    m.take_events();
    m.append_buffer(v, &vec![1u8; 30 * 1024]).unwrap();
    settle(&mut m, &mut p);
    let ev = m.take_events();
    assert!(ev.ends_with(&[Fire::Buffer(v, "error"), Fire::Buffer(v, "updateend")]), "{ev:?}");
    assert_eq!(m.append_buffer(v, b"x"), Err(DomError::QuotaExceeded), "QuotaExceededError until the page removes");
    m.remove(v, 0.0, 0.5).unwrap();
    assert_eq!(m.append_buffer(v, b"x"), Err(DomError::InvalidState), "updating while removing");
    settle(&mut m, &mut p);
    assert!(m.take_events().ends_with(&[Fire::Buffer(v, "update"), Fire::Buffer(v, "updateend")]));
    m.append_buffer(v, &vec![1u8; 10 * 1024]).unwrap();
    settle(&mut m, &mut p);
    assert!(m.take_events().ends_with(&[Fire::Buffer(v, "update"), Fire::Buffer(v, "updateend")]));
    assert!(p.violations.is_empty(), "{:?}", p.violations);
}

#[test]
fn abort_drops_the_rest_and_still_waits_for_the_append_in_flight() {
    let (mut m, mut p) = feed();
    let v = m.add_source_buffer("video/webm; codecs=\"vp9\"").unwrap();
    m.append_buffer(v, &vec![1u8; 200 * 1024]).unwrap();
    send(&mut m, &mut p); // the first chunk is in flight, unanswered
    m.abort(v).unwrap();
    assert_eq!(m.take_events()[1..], [Fire::Buffer(v, "abort"), Fire::Buffer(v, "updateend")]);
    m.append_buffer(v, &vec![2u8; 1024]).unwrap();
    let sent = send(&mut m, &mut p);
    assert_eq!(sent, vec![ToChild::Abort { track: v }], "the new append waits for the one in flight");
    settle(&mut m, &mut p);
    assert!(p.violations.is_empty(), "{:?}", p.violations);
    assert_eq!(p.bytes(v), (APPEND_MAX + 1024) as u64, "the first chunk, then the new append; the rest dropped");
}

#[test]
fn end_of_stream_and_reopening() {
    let (mut m, mut p) = feed();
    let v = m.add_source_buffer("video/webm; codecs=\"vp9\"").unwrap();
    m.append_buffer(v, &vec![1u8; 1000]).unwrap();
    assert_eq!(m.end_of_stream(None), Err(DomError::InvalidState), "while updating");
    settle(&mut m, &mut p);
    m.end_of_stream(None).unwrap();
    assert_eq!(m.ready_state(), ReadyState::Ended);
    assert!(m.take_events().ends_with(&[Fire::Source("sourceended")]));
    assert_eq!(m.add_source_buffer("audio/webm; codecs=\"opus\""), Err(DomError::InvalidState));
    // An append after the end opens the source again.
    m.append_buffer(v, b"tail").unwrap();
    assert!(m.take_events().contains(&Fire::Source("sourceopen")));
    settle(&mut m, &mut p);
    assert!(matches!(p.received.iter().filter(|r| matches!(r, ToChild::EndOfStream { .. })).count(), 1));
    assert_eq!(m.end_of_stream(Some("bogus")), Err(DomError::Type));
    assert!(p.violations.is_empty(), "{:?}", p.violations);
}

#[test]
fn the_element_maps_onto_the_role() {
    let mut m = Media::new();
    let mut p = FakePlayer::new();
    assert_eq!(m.play(), Err(DomError::NotSupported), "no source yet");
    m.load_fd("video/webm").unwrap();
    assert_eq!(m.load_fd("video/webm"), Err(DomError::InvalidState), "one source per embed");
    settle(&mut m, &mut p);
    assert_eq!(m.duration, 10.0);
    assert_eq!(m.size, (640, 360));
    let ev = m.take_events();
    assert_eq!(ev, vec![Fire::Element("durationchange"), Fire::Element("loadedmetadata"), Fire::Element("loadeddata"), Fire::Element("canplay"), Fire::Element("canplaythrough")]);
    m.play().unwrap();
    settle(&mut m, &mut p);
    assert_eq!(m.take_events(), vec![Fire::Element("play"), Fire::Element("playing"), Fire::Element("timeupdate")]);
    assert!(!m.paused);
    m.seek(4.0).unwrap();
    assert_eq!(m.take_events(), vec![Fire::Element("seeking")]);
    settle(&mut m, &mut p);
    assert_eq!(m.take_events(), vec![Fire::Element("seeked"), Fire::Element("timeupdate")]);
    assert_eq!(m.current_time, 4.0);
    assert_eq!(m.set_volume(1.5), Err(DomError::IndexSize));
    m.set_volume(0.25).unwrap();
    m.set_muted(true);
    assert_eq!(m.set_rate(8.0), Err(DomError::NotSupported), "the player plays 0.25 to 4 times");
    m.set_rate(1.5).unwrap();
    let sent = send(&mut m, &mut p);
    assert_eq!(sent, vec![ToChild::SetVolume { volume: 0.25 }, ToChild::SetVolume { volume: 0.0 }, ToChild::SetRate { rate: 1.5 }]);
    p.advance(10.0, true);
    for a in p.deliver() { m.from_player(a) }
    assert!(m.take_events().ends_with(&[Fire::Element("volumechange"), Fire::Element("volumechange"), Fire::Element("ratechange"), Fire::Element("timeupdate"), Fire::Element("pause"), Fire::Element("ended")]));
    assert!(m.ended && m.paused);
    // play() after the end starts over.
    m.play().unwrap();
    assert_eq!(send(&mut m, &mut p), vec![ToChild::Seek { secs: 0.0 }, ToChild::Play]);
    assert!(p.violations.is_empty(), "{:?}", p.violations);
}

#[test]
fn player_errors_become_media_errors() {
    let mut m = Media::new();
    m.load_fd("video/ogg").unwrap();
    let mut p = FakePlayer::new();
    settle(&mut m, &mut p);
    assert_eq!(m.error, Some(4), "MEDIA_ERR_SRC_NOT_SUPPORTED");
    assert!(m.take_events().contains(&Fire::Element("error")));
    let mut m2 = Media::new();
    m2.attach_source().unwrap();
    m2.from_player(ToParent::Error { code: code::DECODE, track: None });
    assert_eq!(m2.error, Some(3));
}

#[test]
fn every_message_fits_the_role() {
    // A 1 MiB append becomes appends of at most 60 KiB, each one the fake
    // decodes (the role refuses anything bigger).
    let (mut m, mut p) = feed();
    let v = m.add_source_buffer("video/webm; codecs=\"vp9\"").unwrap();
    m.append_buffer(v, &vec![3u8; 1 << 20]).unwrap();
    settle(&mut m, &mut p);
    let appends = p.received.iter().filter(|r| matches!(r, ToChild::Append { .. })).count();
    assert_eq!(appends, (1usize << 20).div_ceil(APPEND_MAX));
    assert!(p.violations.is_empty(), "{:?}", p.violations);
    // And the fake catches a parent that breaks the rules.
    let mut bad = FakePlayer::new();
    bad.receive(&ToChild::OpenSource.encode());
    bad.receive(&ToChild::Append { track: 0, bytes: vec![1], timestamp_offset: None }.encode());
    bad.receive(&ToChild::AddTrack { track: 0, mime: "video/webm".into() }.encode());
    bad.receive(&ToChild::Append { track: 0, bytes: vec![1], timestamp_offset: None }.encode());
    bad.receive(&ToChild::Append { track: 0, bytes: vec![1], timestamp_offset: None }.encode());
    bad.receive(&[0xff, 0xff]);
    assert_eq!(bad.violations.len(), 3, "{:?}", bad.violations);
}

#[test]
fn picture_in_picture_needs_a_gesture_and_limends_word() {
    let mut m = Media::new();
    assert_eq!(m.request_picture_in_picture(true), Err(DomError::InvalidState), "nothing loaded yet");
    m.load_fd("video/webm").unwrap();
    m.take_out();
    m.from_player(ToParent::Ready { duration_secs: 10.0, width: 640, height: 360 });
    m.take_events();
    assert_eq!(m.request_picture_in_picture(false), Err(DomError::NotAllowed), "no user gesture");
    assert!(m.take_out().is_empty());
    m.request_picture_in_picture(true).unwrap();
    assert_eq!(m.take_out(), vec![ToChild::RequestPip]);
    assert!(!m.in_pip, "the request alone does not put it in PiP");
    m.pip(true);
    assert!(m.in_pip);
    assert_eq!(m.take_events(), vec![Fire::Element("enterpictureinpicture")]);
    m.pip(true);
    assert!(m.take_events().is_empty(), "a repeated event changes nothing");
    m.exit_picture_in_picture().unwrap();
    assert_eq!(m.take_out(), vec![ToChild::ExitPip]);
    m.pip(false);
    assert_eq!(m.take_events(), vec![Fire::Element("leavepictureinpicture")]);
    assert_eq!(m.exit_picture_in_picture(), Err(DomError::InvalidState));
}

#[test]
fn media_session_metadata_and_track_actions() {
    use limen::media_player::{media_action, MEDIA_TEXT_MAX};
    let mut m = Media::new();
    let long = "é".repeat(200);
    m.set_metadata("Song\u{7}", &long, "");
    let out = m.take_out();
    let ToChild::MediaMetadata { title, artist, album } = &out[0] else { panic!("{out:?}") };
    assert_eq!(title, "Song", "control characters dropped");
    assert!(artist.len() <= MEDIA_TEXT_MAX && artist.chars().all(|c| c == 'é'), "cut at a character boundary");
    assert!(album.is_empty());
    assert!(out[0].clone().encode().len() > 0 && ToChild::decode(&out[0].encode()).is_some(), "passes the player's check");
    m.set_action_handler("nexttrack", true);
    m.set_action_handler("seekto", true);
    assert_eq!(m.take_out(), vec![ToChild::MediaActions { actions: media_action::NEXT_TRACK }], "only the role's actions are sent");
    m.from_player(ToParent::Action { action: media_action::NEXT_TRACK });
    m.from_player(ToParent::Action { action: media_action::PREVIOUS_TRACK });
    assert_eq!(m.take_events(), vec![Fire::SessionAction(media_action::NEXT_TRACK)], "only a handler the page set is called");
    m.set_action_handler("nexttrack", false);
    assert_eq!(m.take_out(), vec![ToChild::MediaActions { actions: 0 }]);
    m.from_player(ToParent::Action { action: media_action::NEXT_TRACK });
    assert!(m.take_events().is_empty(), "a handler removed in between is not called");
}
