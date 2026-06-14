//! The audio capability grant store (§9).
//!
//! Maps an app's identity (its manifest app-id) to the audio capabilities it
//! holds. In production this file is populated by **Portcullis** from each app's
//! manifest plus the user's approval at the grant surface; choragusd only *reads*
//! it and enforces. (Replacing the earlier global `{audio}` stub: grants are now
//! per-app and data-driven; the only remaining gap is that a real deployment
//! has Portcullis write this, gated by user consent.)
//!
//! Format — one app per line, `#` comments and blanks ignored:
//! ```text
//!   org.atrium.player     audio
//!   org.atrium.recorder   audio,audio_monitor
//!   org.atrium.meet       audio,microphone
//! ```

use crate::app::{CAP_AUDIO, CAP_MICROPHONE, CAP_MONITOR};
use std::collections::HashMap;

/// app-id → granted capability bits.
#[derive(Default)]
pub struct GrantStore {
    map: HashMap<String, u8>,
}

fn cap_bit(word: &str) -> Option<u8> {
    Some(match word.trim() {
        "audio" => CAP_AUDIO,
        "microphone" | "mic" => CAP_MICROPHONE,
        "audio_monitor" | "monitor" => CAP_MONITOR,
        "" => return Some(0),
        _ => return None,
    })
}

impl GrantStore {
    /// Parse the store from text (the file's contents).
    pub fn parse(text: &str) -> GrantStore {
        let mut map = HashMap::new();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut it = line.split_whitespace();
            let Some(app) = it.next() else { continue };
            let mut bits = 0u8;
            if let Some(caps) = it.next() {
                for w in caps.split(',') {
                    bits |= cap_bit(w).unwrap_or(0);
                }
            }
            map.insert(app.to_string(), bits);
        }
        GrantStore { map }
    }

    pub fn load(path: &str) -> std::io::Result<GrantStore> {
        Ok(GrantStore::parse(&std::fs::read_to_string(path)?))
    }

    /// The capability bits granted to `app_id` (0 = nothing / unknown app).
    pub fn granted(&self, app_id: &str) -> u8 {
        self.map.get(app_id).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STORE: &str = "
        # demo grants
        org.atrium.player    audio
        org.atrium.recorder  audio,audio_monitor
        org.atrium.meet      audio,microphone
    ";

    #[test]
    fn parses_per_app_grants() {
        let g = GrantStore::parse(STORE);
        assert_eq!(g.granted("org.atrium.player"), CAP_AUDIO);
        assert_eq!(g.granted("org.atrium.recorder"), CAP_AUDIO | CAP_MONITOR);
        assert_eq!(g.granted("org.atrium.meet"), CAP_AUDIO | CAP_MICROPHONE);
    }

    #[test]
    fn unknown_app_gets_nothing() {
        let g = GrantStore::parse(STORE);
        assert_eq!(g.granted("org.evil.spyware"), 0, "default-deny for unlisted apps");
    }

    #[test]
    fn comments_and_blanks_ignored() {
        let g = GrantStore::parse("# just a comment\n\n   \n");
        assert_eq!(g.granted("anything"), 0);
    }
}
