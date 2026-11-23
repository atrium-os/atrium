//! Reproducible wire and lifecycle checks for both sides of the island channel.
use limen::web_island::{EndReason, Lifecycle, Report, UnloadAnswer, MAX_FRAME};

// xorshift32; keep the seed so a failing iteration can be replayed on the host or guest.
fn draw(seed: &mut u32) -> u32 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 17;
    *seed ^= *seed << 5;
    *seed
}

fn phase(state: Lifecycle) -> (u8, u32) {
    match state {
        Lifecycle::Waiting => (0, 0),
        Lifecycle::Running { last_query } => (1, last_query),
        Lifecycle::AwaitingUnload(query) => (2, query),
        Lifecycle::Ended => (3, 0),
    }
}

#[test]
fn generated_bidirectional_frames_preserve_lifecycle() {
    let mut seed = 0xd91c_04a7;
    let mut accepted_reports = [0; 5];
    let mut accepted_answers = [0; 2];
    for case in 0..32768 {
        let query = draw(&mut seed).max(1);
        let kind = draw(&mut seed) % 5;
        let report = match kind {
            0 => Report::Started {
                origin: "https://widget.example",
            },
            1 => Report::Title("Counter 世界"),
            2 => Report::Resize {
                width: 1 + (query % 16384) as u16,
                height: 360,
            },
            3 => Report::BeforeUnload { query },
            _ => Report::Ended(if query & 1 == 0 {
                EndReason::Closed
            } else {
                EndReason::Failed
            }),
        };
        let wire = report.encode().unwrap();
        let last = query - 1;
        let initial = match draw(&mut seed) % 5 {
            0 => Lifecycle::Waiting,
            1 => Lifecycle::Running { last_query: last },
            2 => Lifecycle::Running { last_query: query },
            3 => Lifecycle::AwaitingUnload(query),
            _ => Lifecycle::Ended,
        };
        let mut state = initial;
        let expected = match (initial, report) {
            (Lifecycle::Waiting, Report::Started { .. }) => Some((1, 0)),
            (Lifecycle::Running { .. }, Report::Title(_) | Report::Resize { .. }) => {
                Some(phase(initial))
            }
            (Lifecycle::Running { last_query }, Report::BeforeUnload { query })
                if query > last_query =>
            {
                Some((2, query))
            }
            (Lifecycle::Running { .. }, Report::Ended(_)) => Some((3, 0)),
            _ => None,
        };
        assert_eq!(
            state.receive(&wire),
            expected.map(|_| report),
            "report case {case}"
        );
        assert_eq!(phase(state), expected.unwrap_or(phase(initial)));
        if expected.is_some() {
            accepted_reports[kind as usize] += 1;
        }

        let mut mutated = wire;
        let n = draw(&mut seed) as usize;
        let mutation = n % 4;
        match mutation {
            0 => mutated.truncate(n % mutated.len()),
            1 => mutated.push(draw(&mut seed) as u8),
            2 => mutated.resize(MAX_FRAME + 1, n as u8),
            _ => {
                let offset = n % mutated.len();
                mutated[offset] ^= 1 << (draw(&mut seed) % 8);
            }
        }
        let mut probe = initial;
        if let Some(decoded) = probe.receive(&mutated) {
            assert_eq!(mutation, 3, "invalid framing accepted at {case}");
            let mut replay = initial;
            assert_eq!(replay.receive(&decoded.encode().unwrap()), Some(decoded));
            assert_eq!(phase(probe), phase(replay));
        } else {
            assert_eq!(
                phase(probe),
                phase(initial),
                "rejection changed state at {case}"
            );
        }

        // Answer immediately after the report. Include replies for a different
        // question, both decisions, malformed frames and a duplicate after acceptance.
        let answer = UnloadAnswer {
            query: if draw(&mut seed) & 1 == 0 {
                query
            } else {
                last
            },
            leave: draw(&mut seed) & 1 == 0,
        };
        let mut bytes = answer.encode();
        let before = phase(state);
        if draw(&mut seed) % 3 == 0 {
            let offset = (draw(&mut seed) as usize) % bytes.len();
            bytes[offset] ^= 1 << (draw(&mut seed) % 8);
        }
        let decoded = UnloadAnswer::decode(&bytes);
        let valid = bytes[0] == b'U' && bytes[5] <= 1;
        assert_eq!(decoded.is_some(), valid);
        if let Some(reply) = decoded {
            assert_eq!(reply.encode(), bytes);
            let matching = before == (2, reply.query);
            assert_eq!(state.answer(reply), matching, "answer case {case}");
            if matching {
                accepted_answers[usize::from(reply.leave)] += 1;
                assert_eq!(phase(state), (1, reply.query));
                assert!(!state.answer(reply));
            } else {
                assert_eq!(phase(state), before);
            }
        }
        assert!(UnloadAnswer::decode(&bytes[..5]).is_none());
        bytes.push(0);
        assert!(UnloadAnswer::decode(&bytes).is_none());
    }
    assert!(accepted_reports.iter().all(|&n| n > 0));
    assert!(accepted_answers.iter().all(|&n| n > 0));
}

#[test]
fn chapter_controls_require_live_media_and_an_exact_finite_payload() {
    use limen::web_island::ChapterSeek;
    let seek = ChapterSeek::from_link("#t=1.5").unwrap();
    let valid = seek.encode();
    let mut running = Lifecycle::Running { last_query: 3 };
    assert_eq!(ChapterSeek::decode(&valid), Some(seek));
    assert!(running.accepts_control(&valid, true));
    assert!(!running.accepts_control(&valid, false));
    for mut state in [Lifecycle::Waiting, Lifecycle::AwaitingUnload(4), Lifecycle::Ended] {
        assert!(!state.accepts_control(&valid, true));
    }
    for value in [f64::NAN, f64::INFINITY, -1.0, 1e9 + 1.0] {
        let mut bad = vec![b'S'];
        bad.extend(value.to_le_bytes());
        assert!(!running.accepts_control(&bad, true));
    }
    assert!(!running.accepts_control(&valid[..8], true));
    let mut padded = valid;
    padded.push(0);
    assert!(!running.accepts_control(&padded, true));
}
