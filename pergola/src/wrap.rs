//! Line breaking for wrapped text ([`crate::text::lines`]).
//!
//! Lines break where UAX #14 allows (after spaces, after hyphens and
//! dashes, around slashes and ideographs, after a soft hyphen; never at a
//! no-break space), greedily: each line takes as much as fits. A word
//! that doesn't fit the room left can be hyphenated (Knuth–Liang, the TeX
//! en-US patterns) when [`Wrap::hyphens`] is on, and a soft hyphen (U+00AD)
//! is always a place to break, showing a hyphen there and nothing
//! elsewhere. A word wider than a whole line is broken by character
//! (grapheme). With [`Wrap::max_lines`], what doesn't fit ends in "…".

use std::sync::OnceLock;

use hyphenation::{Hyphenator, Language, Load, Standard};
use unicode_linebreak::linebreaks;
use unicode_segmentation::UnicodeSegmentation;

const SHY: char = '\u{00AD}';
/// What a line broken inside a word ends with.
pub const HYPHEN: &str = "-";
pub const ELLIPSIS: &str = "…";

/// How text wraps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Wrap {
    /// Hyphenate words by dictionary when that fills a line better
    /// (soft hyphens are used either way).
    pub hyphens: bool,
    /// At most this many lines (0: no limit); the last ends in "…" when the
    /// text goes on. 1 is a truncated single line.
    pub max_lines: u16,
}

fn dictionary() -> Option<&'static Standard> {
    static D: OnceLock<Option<Standard>> = OnceLock::new();
    D.get_or_init(|| Standard::from_embedded(Language::EnglishUS).ok()).as_ref()
}

/// Hyphenation points in `word` (byte offsets, ascending), keeping 2
/// letters before and 3 after (the patterns' own minimums).
fn hyphen_points(word: &str) -> Vec<usize> {
    // Only the letters: leading and trailing punctuation stays on.
    let start = word.find(|c: char| c.is_alphabetic()).unwrap_or(word.len());
    let end = word.rfind(|c: char| c.is_alphabetic()).map(|i| i + word[i..].chars().next().unwrap().len_utf8()).unwrap_or(start);
    if end <= start || !word[start..end].chars().all(char::is_alphabetic) {
        return Vec::new();
    }
    let Some(d) = dictionary() else { return Vec::new() };
    d.hyphenate(&word[start..end]).breaks.into_iter().map(|b| b + start).collect()
}

/// Break `text` (one paragraph, no `\n`) into lines no wider than `width`
/// as `measure` measures. Returned lines are trimmed of the spaces they
/// broke at and carry their visible hyphen.
pub fn break_paragraph(text: &str, width: f32, wrap: &Wrap, measure: &dyn Fn(&str) -> f32) -> Vec<String> {
    let fits = |s: &str| measure(s) <= width + 0.01;
    let shown = |s: &str| -> String { s.trim_end().chars().filter(|c| *c != SHY).collect() };
    // Break opportunities: (end byte of the piece before, after a soft hyphen?).
    let mut ops: Vec<usize> = linebreaks(text).map(|(i, _)| i).collect();
    ops.dedup();
    let mut out: Vec<String> = Vec::new();
    let mut line_start = 0usize;
    // The last opportunity that fit on this line.
    let mut last_fit: Option<usize> = None;
    let mut i = 0;
    while i < ops.len() {
        let b = ops[i];
        let candidate = &text[line_start..b];
        let mut cand = shown(candidate);
        if text[..b].ends_with(SHY) {
            cand.push_str(HYPHEN);
        }
        if fits(&cand) {
            last_fit = Some(b);
            i += 1;
            continue;
        }
        // The piece from the last fit to `b` doesn't fit on this line.
        let piece_start = last_fit.unwrap_or(line_start);
        let piece = &text[piece_start..b];
        // Fill the rest of the line with part of the word, by hyphen.
        if wrap.hyphens {
            let head: String = text[line_start..piece_start].chars().filter(|c| *c != SHY).collect();
            let word = piece.trim_end();
            let mut best = None;
            for p in hyphen_points(word).into_iter().rev() {
                let s = format!("{}{}{HYPHEN}", head, &word[..p]);
                if fits(&s) {
                    best = Some((s, piece_start + p));
                    break;
                }
            }
            if let Some((s, at)) = best {
                out.push(s);
                line_start = at;
                last_fit = None;
                // Re-examine from the same opportunity with the new start.
                continue;
            }
        }
        if let Some(lf) = last_fit {
            let mut l = shown(&text[line_start..lf]);
            if text[..lf].ends_with(SHY) {
                l.push_str(HYPHEN);
            }
            out.push(l);
            line_start = lf;
            last_fit = None;
            continue;
        }
        // A word alone wider than the line: by grapheme, at least one.
        let mut taken = 0usize;
        for (gi, g) in piece.grapheme_indices(true) {
            let end = gi + g.len();
            if taken > 0 && !fits(&shown(&piece[..end])) {
                break;
            }
            taken = end;
        }
        out.push(shown(&piece[..taken]));
        line_start += taken;
        last_fit = None;
    }
    if line_start < text.len() || out.is_empty() {
        out.push(shown(&text[line_start..]));
    }
    out
}

/// Cut `lines` to `max` (0: all), ending the last kept in "…" when some
/// were cut or `tail` (what the line holds) is wider than `width`.
pub fn truncate(mut lines: Vec<String>, max: u16, width: f32, measure: &dyn Fn(&str) -> f32) -> Vec<String> {
    let fits = |s: &str| measure(s) <= width + 0.01;
    let max = max as usize;
    let cut = max > 0 && lines.len() > max;
    if cut {
        lines.truncate(max);
    }
    let Some(last) = lines.last_mut() else { return lines };
    if !cut && fits(last) {
        return lines;
    }
    // Drop graphemes (and a trailing space or hyphen) until "…" fits.
    let mut s = if cut { last.trim_end_matches(HYPHEN).to_string() } else { last.clone() };
    loop {
        let t = format!("{}{ELLIPSIS}", s.trim_end());
        if fits(&t) || s.is_empty() {
            *last = t;
            break;
        }
        let n = s.grapheme_indices(true).last().map(|(i, _)| i).unwrap_or(0);
        s.truncate(n);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One unit per character.
    fn m(s: &str) -> f32 {
        s.chars().count() as f32
    }

    fn lines(t: &str, w: f32, wrap: Wrap) -> Vec<String> {
        truncate(break_paragraph(t, w, &wrap, &m), wrap.max_lines, w, &m)
    }

    #[test]
    fn breaks_at_spaces_hyphens_and_slashes_never_at_a_no_break_space() {
        let off = Wrap::default();
        assert_eq!(lines("one two three four", 9.0, off), ["one two", "three", "four"]);
        assert_eq!(lines("well-known words", 8.0, off), ["well-", "known", "words"]);
        assert_eq!(lines("see a/b/c/d now", 6.0, off), ["see a/", "b/c/d", "now"]);
        assert_eq!(lines("10\u{a0}km away", 6.0, off), ["10\u{a0}km", "away"]);
    }

    #[test]
    fn a_soft_hyphen_breaks_with_a_hyphen_and_is_invisible_otherwise() {
        let off = Wrap::default();
        assert_eq!(lines("in\u{ad}ter\u{ad}na\u{ad}tional", 8.0, off), ["interna-", "tional"]);
        assert_eq!(lines("in\u{ad}ter\u{ad}na\u{ad}tional", 7.0, off), ["inter-", "na-", "tional"]);
        assert_eq!(lines("in\u{ad}ter", 20.0, off), ["inter"]);
    }

    #[test]
    fn hyphenation_fills_a_line_with_part_of_a_word() {
        let on = Wrap { hyphens: true, ..Wrap::default() };
        let t = "the hyphenation algorithm";
        let got = lines(t, 12.0, on);
        assert_eq!(got, ["the hyphena-", "tion algo-", "rithm"]);
        assert!(got.iter().all(|l| m(l) <= 12.0), "{got:?}");
        // Off: the word moves down whole.
        assert_eq!(lines(t, 12.0, Wrap::default())[0], "the");
    }

    #[test]
    fn a_word_wider_than_a_line_breaks_by_character() {
        assert_eq!(lines("abcdefghij xy", 4.0, Wrap::default()), ["abcd", "efgh", "ij", "xy"]);
        assert_eq!(lines("é\u{301}abc", 1.0, Wrap::default())[0], "é\u{301}", "a grapheme stays whole");
    }

    #[test]
    fn max_lines_ends_in_an_ellipsis() {
        let two = Wrap { max_lines: 2, ..Wrap::default() };
        assert_eq!(lines("one two three four five", 9.0, two), ["one two", "three…"]);
        assert_eq!(lines("one two", 9.0, two), ["one two"], "all of it fits: no ellipsis");
        let one = Wrap { max_lines: 1, ..Wrap::default() };
        assert_eq!(lines("a long single line", 8.0, one), ["a long…"]);
    }
}
