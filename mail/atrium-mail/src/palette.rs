//! The command palette (mail.md §15.5, E9): Ctrl-K, type a few letters of
//! what to do, Enter. Every action Mail has, with its key where it has
//! one, and every place to go: folders, views, saved searches. Only what
//! makes sense now is listed (Reply while reading, Restore for a dropped
//! item).

use mail_store::FolderId;

use crate::view::Lane;

/// The most entries shown at once.
pub const SHOWN_MAX: usize = 9;
/// The most typed.
pub const QUERY_MAX: usize = 80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    Write,
    Reply,
    ReplyAll,
    Forward,
    Archive,
    Delete,
    /// To Junk, or (in Junk) Not junk (§7.7).
    Junk,
    /// Report phishing: to Junk, marked `$Phishing` too (§7.7).
    Phishing,
    Later,
    Done,
    Drop,
    Restore,
    Undo,
    Print,
    Search,
    SearchServer,
    GetMail,
    Settings,
    Conversations(bool),
    Quick(u16),
    NextGroup,
    SaveFilters,
    Folder(FolderId),
    View(Lane),
    Saved(u32),
    Export(Export),
    /// Import an mbox file into the folder shown (§6.5).
    Import,
    /// Make the `n`th of the open message's "Always…" rules (§11.6) and
    /// run it over the inbox.
    Always(u16),
    // A compose window's (§9).
    Send,
    SendLater,
    Attach,
    RichText(bool),
    ShowBcc,
    SaveDraft,
    Discard,
}

/// What an export covers (§6.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Export {
    /// The folder shown.
    Folder,
    /// The search or view's results.
    Results,
    Everything,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub label: String,
    /// Its key in the window, if it has one ("e").
    pub key: &'static str,
    pub cmd: Cmd,
}

/// The palette while it is up.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Palette {
    pub query: String,
    /// The chosen entry among those shown.
    pub sel: usize,
}

/// How well `query` matches `label`: its letters in order, case aside;
/// higher is better, `None` if not all are there. A match at the start of
/// a word counts more, and a whole run more again.
pub fn score(label: &str, query: &str) -> Option<i32> {
    let q: Vec<char> = query.trim().to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
    if q.is_empty() {
        return Some(0);
    }
    let l: Vec<char> = label.to_lowercase().chars().collect();
    let mut s = 0;
    let mut qi = 0;
    let mut last: Option<usize> = None;
    for (i, c) in l.iter().enumerate() {
        if qi < q.len() && *c == q[qi] {
            let word_start = i == 0 || !l[i - 1].is_alphanumeric();
            s += if word_start { 10 } else { 1 };
            if last.is_some_and(|p| p + 1 == i) {
                s += 5;
            }
            last = Some(i);
            qi += 1;
        }
    }
    (qi == q.len()).then(|| s - l.len() as i32 / 8)
}

/// The entries matching `query`, best first (ties keep their order).
pub fn filter(all: &[Entry], query: &str) -> Vec<Entry> {
    let mut v: Vec<(i32, usize, &Entry)> = all.iter().enumerate().filter_map(|(i, e)| score(&e.label, query).map(|s| (s, i, e))).collect();
    v.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    v.into_iter().map(|(_, _, e)| e.clone()).collect()
}

/// What a key did to the palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Do shown entry `n`.
    Run(usize),
    Closed,
    /// Taken (typed, moved, or ignored while up).
    Taken,
}

/// A key while the palette is up over `n` shown entries: typing narrows
/// it, Up and Down choose, Enter does it, Escape (or Ctrl-K again)
/// closes it.
pub fn key(p: &mut Palette, n: usize, key: &pergola::Key, modifiers: pergola::event::Modifiers, chars: &str) -> Step {
    use pergola::Key;
    match key {
        Key::Escape => return Step::Closed,
        Key::Char if modifiers.ctrl && chars.eq_ignore_ascii_case("k") => return Step::Closed,
        Key::Down => p.sel = if n == 0 { 0 } else { (p.sel + 1) % n },
        Key::Up => p.sel = if n == 0 { 0 } else { (p.sel + n - 1) % n },
        Key::Enter => return Step::Run(p.sel),
        Key::Backspace => {
            p.query.pop();
            p.sel = 0;
        }
        Key::Char if !modifiers.ctrl && !modifiers.alt && !modifiers.meta => {
            if p.query.chars().count() < QUERY_MAX {
                p.query.extend(chars.chars().filter(|c| !c.is_control()));
            }
            p.sel = 0;
        }
        _ => {}
    }
    Step::Taken
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(label: &str, cmd: Cmd) -> Entry {
        Entry { label: label.into(), key: "", cmd }
    }

    #[test]
    fn letters_in_order_word_starts_first() {
        assert!(score("Archive", "arc").is_some());
        assert!(score("Archive", "xyz").is_none());
        assert!(score("Reply all", "ra").is_some());
        assert!(score("Reply all", "ar").is_none());
        let all = [e("Reply", Cmd::Reply), e("Reply all", Cmd::ReplyAll), e("Forward", Cmd::Forward), e("Go to Archive", Cmd::Folder(3)), e("Archive", Cmd::Archive)];
        assert_eq!(filter(&all, "rep").iter().map(|x| x.cmd).collect::<Vec<_>>(), [Cmd::Reply, Cmd::ReplyAll]);
        assert_eq!(filter(&all, "ra")[0].cmd, Cmd::ReplyAll);
        assert_eq!(filter(&all, "arch")[0].cmd, Cmd::Archive, "the shorter, whole match first");
        assert_eq!(filter(&all, "").len(), 5);
        assert_eq!(filter(&all, "go arch")[0].cmd, Cmd::Folder(3));
    }
}
