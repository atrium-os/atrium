//! The command palette (Ctrl+Shift+P; dev-tools.md §3.8): Artifex's own
//! acts and the commands of the plugins running, found by typing.

use crate::ui::{Act, Panel};

/// What a row does.
#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    Act(Act),
    /// A plugin's command: its app, the command's id.
    Plugin { app: String, id: String },
}

/// One row: what it does, its label, and its key or its plugin's name.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub cmd: Cmd,
    pub label: String,
    pub hint: String,
}

/// Rows shown at most.
pub const MAX_ROWS: usize = 12;

/// Artifex's own commands, as the shortcuts have them.
pub fn own() -> Vec<Entry> {
    let e = |a: Act, label: &str, hint: &str| Entry { cmd: Cmd::Act(a), label: label.into(), hint: hint.into() };
    vec![
        e(Act::Save, "Save", "Ctrl+S"),
        e(Act::Task(0), "Build", "Ctrl+B"),
        e(Act::Run, "Run", "Ctrl+R"),
        e(Act::Debug, "Start debugging", "F5"),
        e(Act::StopDebug, "Stop debugging", "Shift+F5"),
        e(Act::Format, "Format the file", "Shift+Alt+F"),
        e(Act::StartRename, "Rename the symbol", "F2"),
        e(Act::Definition, "Go to definition", "F12"),
        e(Act::References, "Find references", "Shift+F12"),
        e(Act::QuickFix, "Quick fix", "Ctrl+."),
        e(Act::Hover, "Show what's under the caret", "Ctrl+K"),
        e(Act::Complete, "Complete the word", "Ctrl+Space"),
        e(Act::Panel(Panel::Output), "Show the output", "Ctrl+J"),
        e(Act::Panel(Panel::Problems), "Show the problems", "Ctrl+Shift+M"),
        e(Act::CloseTab(usize::MAX), "Close the tab", "Ctrl+W"),
    ]
}

/// How well `query` matches `label`: its letters in order, ignoring case;
/// `None` when they aren't all there. Lower is better: a match at a word's
/// start, then letters close together.
pub fn score(label: &str, query: &str) -> Option<usize> {
    let l: Vec<char> = label.to_lowercase().chars().collect();
    let mut at = 0;
    let mut first = None;
    let mut gaps = 0;
    for q in query.to_lowercase().chars().filter(|c| !c.is_whitespace()) {
        let i = (at..l.len()).find(|&i| l[i] == q)?;
        if first.is_none() {
            first = Some(i);
        } else {
            gaps += i - at;
        }
        at = i + 1;
    }
    let first = first.unwrap_or(0);
    let word_start = first == 0 || !l[first - 1].is_alphanumeric();
    Some(if word_start { 0 } else { 1000 } + gaps * 10 + first)
}

/// The rows for `query`, best first, at most [`MAX_ROWS`].
pub fn filter(all: &[Entry], query: &str) -> Vec<usize> {
    let mut v: Vec<(usize, usize)> = all.iter().enumerate().filter_map(|(i, e)| score(&e.label, query).map(|s| (s, i))).collect();
    v.sort();
    v.into_iter().map(|(_, i)| i).take(MAX_ROWS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_finds_commands_by_their_letters() {
        let all = own();
        let label = |q: &str| filter(&all, q).first().map(|&i| all[i].label.clone());
        assert_eq!(label("fmt").as_deref(), Some("Format the file"));
        assert_eq!(label("ren").as_deref(), Some("Rename the symbol"));
        assert_eq!(label("refs").as_deref(), Some("Find references"));
        assert_eq!(filter(&all, "").len(), MAX_ROWS);
        assert!(filter(&all, "zzz").is_empty());
        // A word's start beats letters inside one.
        assert!(score("Run", "r") < score("Format", "r"));
    }
}
