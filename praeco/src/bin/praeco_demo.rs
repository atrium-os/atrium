//! praeco-demo: a test app for notification actions (tabula.md §5.3).
//!
//! It listens for its actions first: one held for it (pressed while it
//! wasn't running) arrives at once, and it logs it and exits. Otherwise it
//! posts "Ana: lunch on Friday?" with Archive and Reply. Run as `chat` it
//! then waits up to two minutes for a press; run under any other name
//! (`reminder`) it exits at once, so the press finds it stopped and Forum
//! starts it. praecod's socket: `praeco::client::socket()`.

use std::path::PathBuf;
use std::time::Duration;

fn main() {
    let sock = praeco::client::socket();
    // Tests: `--forge <id>` acts on notification `id` with a press frescod
    // never minted, as a subscriber would; praecod must refuse it.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--forge") {
        let id = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(0);
        match praeco::client::act(&sock, id, "archive", [9; 16]) {
            Ok(st) => println!("praeco-demo: forged act on {id}: status {st}"),
            Err(e) => println!("praeco-demo: forged act: {e}"),
        }
        return;
    }
    let name = std::env::args().next().map(PathBuf::from).and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned())).unwrap_or_default();
    let mut ear = match praeco::client::Listener::connect(&sock) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("praeco-demo: listen: {e}");
            std::process::exit(1);
        }
    };
    if let Ok(Some(a)) = ear.next(Duration::from_secs(3)) {
        eprintln!("praeco-demo: {name} started for {:?} on {}", a.action, a.id);
        return;
    }
    match praeco::client::post_with_actions(&sock, praeco::proto::urgency::HIGH, "Ana: lunch on Friday?", "The usual place at one?", &[("archive", "Archive"), ("reply", "Reply")]) {
        Ok(id) => eprintln!("praeco-demo: {name} posted {id} with Archive and Reply"),
        Err(e) => {
            eprintln!("praeco-demo: post: {e}");
            std::process::exit(1);
        }
    }
    if name != "chat" {
        eprintln!("praeco-demo: {name} exits");
        return;
    }
    match ear.next(Duration::from_secs(120)) {
        Ok(Some(a)) => eprintln!("praeco-demo: {name} heard {:?} on {}", a.action, a.id),
        Ok(None) => eprintln!("praeco-demo: {name} heard nothing"),
        Err(e) => eprintln!("praeco-demo: {name}: {e}"),
    }
}
