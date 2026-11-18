//! {{title}}: what the core and its index worker share (template: indexer).
//!
//! The core sends each opened file's text to the `index` worker as an
//! [`Add`] message; the worker, kept for the app's whole run (`lifetime =
//! "session"`), splits it into words, keeps the counts and answers with the
//! most common words in that text and in all the text so far ([`Top`]). The core only shows answers whose
//! bounds were checked as they arrived.
//!
//! `cargo atrium fuzz worker:index` fuzzes [`workers::index`] with no
//! setup: each input is a message frame, decoded with every bound checked
//! as the worker does, then handled.

use std::collections::HashMap;

use atrium_message::Message;

/// The most text sent in one message.
pub const MAX_TEXT: usize = 1 << 20;
/// The most different words kept.
pub const MAX_WORDS: usize = 100_000;
/// How many words an answer lists.
pub const TOP: usize = 20;

/// The core's message: one file's text.
#[derive(Message, Debug, Clone, PartialEq)]
#[message(version = 1)]
pub struct Add {
    #[message(max_len = 1_048_576)]
    pub text: String,
}

/// One word and how often it came.
#[derive(Message, Debug, Clone, PartialEq)]
#[message(version = 1)]
pub struct Count {
    #[message(max_len = 64, no_control)]
    pub word: String,
    pub count: u64,
}

/// The worker's answer: the most common words in this text, and in all
/// the text so far.
#[derive(Message, Debug, Clone, PartialEq)]
#[message(version = 1)]
pub struct Top {
    #[message(max_len = 20)]
    pub file: Vec<Count>,
    #[message(max_len = 20)]
    pub words: Vec<Count>,
    /// Every word counted so far.
    pub total: u64,
}

/// The counts the worker keeps between messages.
#[derive(Default)]
pub struct Index {
    counts: HashMap<String, u64>,
    total: u64,
}

impl Index {
    pub fn add(&mut self, text: &str) -> Top {
        let mut file: HashMap<String, u64> = HashMap::new();
        for w in text.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()) {
            // At most 64 bytes (Count's bound), lowercased.
            let mut lw = String::new();
            for c in w.chars().flat_map(char::to_lowercase) {
                if lw.len() + c.len_utf8() > 64 {
                    break;
                }
                lw.push(c);
            }
            let w = lw;
            self.total += 1;
            if file.len() < MAX_WORDS {
                *file.entry(w.clone()).or_default() += 1;
            }
            if let Some(n) = self.counts.get_mut(&w) {
                *n += 1;
            } else if self.counts.len() < MAX_WORDS {
                self.counts.insert(w, 1);
            }
        }
        Top { file: top(&file), words: top(&self.counts), total: self.total }
    }
}

/// The TOP most common, most first.
fn top(counts: &HashMap<String, u64>) -> Vec<Count> {
    let mut words: Vec<Count> = counts.iter().map(|(w, n)| Count { word: w.clone(), count: *n }).collect();
    words.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.word.cmp(&b.word)));
    words.truncate(TOP);
    words
}

/// The workers' handlers: `cargo atrium fuzz` finds `workers::<name>` for
/// each `[[workers]]` entry.
pub mod workers {
    use super::*;
    use std::cell::RefCell;
    use std::fs::File;

    thread_local! {
        static INDEX: RefCell<Index> = RefCell::new(Index::default());
    }

    /// The index worker: text in, the most common words so far out.
    pub fn index(add: Add, _file: Option<File>) -> Result<Top, String> {
        Ok(INDEX.with(|i| i.borrow_mut().add(&add.text)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_the_seed() {
        let mut i = Index::default();
        i.add("The cat, the hat.");
        let t = i.add("THE END");
        assert_eq!(t.total, 6);
        assert_eq!(t.words[0], Count { word: "the".into(), count: 3 });
        assert_eq!(t.file, [Count { word: "end".into(), count: 1 }, Count { word: "the".into(), count: 1 }]);
        let seed = include_bytes!("../fuzz/corpus/worker-index/seed");
        assert_eq!(Add::from_frame(seed, MAX_TEXT + 16).unwrap().text, "the cat the hat");
        // A Top always fits its own bounds.
        let long = "\u{130}".repeat(200);
        let t = i.add(&long);
        assert!(Top::from_frame(&t.to_frame(), 1 << 20).is_ok());
    }
}
