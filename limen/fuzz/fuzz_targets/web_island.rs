//! A live page's engine channel (navigator-islands.md I6.6): what the
//! engine reports, and the control messages navigatord sends it. Every
//! byte string is hostile; none may panic, and whatever is taken must be
//! what an honest sender could have sent.
#![no_main]
use libfuzzer_sys::fuzz_target;
use limen::web_island::{ChapterSeek, FilePicked, Lifecycle, PageSize, Pasted, Report, UnloadAnswer, MAX_FRAME};

fuzz_target!(|data: &[u8]| {
    // Frames split on 0xff 0xfe, fed in order to one session: every state
    // of the lifecycle sees hostile reports.
    let mut life = Lifecycle::default();
    for frame in data.split(|&b| b == 0xff).filter(|f| !f.is_empty()) {
        if let Some(report) = life.receive(frame) {
            assert!(frame.len() <= MAX_FRAME);
            // Taken only if an honest engine could have encoded it.
            let again = report.encode().expect("a report taken is one that encodes");
            let mut back = Lifecycle::Running { last_query: 0 };
            if matches!(report, Report::Started { .. }) { back = Lifecycle::default(); }
            if let Report::BeforeUnload { .. } = report { back = Lifecycle::Running { last_query: 0 }; }
            assert_eq!(back.receive(&again), Some(report), "re-encoded report reads the same");
        }
        let _ = life.accepts_control(frame, frame.first() == Some(&1));
        if let Some(p) = Pasted::decode(frame) { assert_eq!(Pasted::decode(&p.encode().expect("taken paste encodes")), Some(p)); }
        if let Some(f) = FilePicked::decode(frame) { assert_eq!(FilePicked::decode(&f.encode().expect("taken pick encodes")), Some(f)); }
        if let Some(a) = UnloadAnswer::decode(frame) { assert_eq!(UnloadAnswer::decode(&a.encode()), Some(a)); }
        if let Some(c) = ChapterSeek::decode(frame) { assert_eq!(ChapterSeek::decode(&c.encode()), Some(c)); }
        if let Some(s) = PageSize::decode(frame) { assert_eq!(PageSize::decode(&s.encode()), Some(s)); }
    }
});
