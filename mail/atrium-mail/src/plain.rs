//! Plain text as Mail draws it (mail.md §7.2, §11.1): quote levels, links
//! found by a conservative linkifier (`https`, `http` and `mailto` only),
//! the quoted tail of a reply and the signature after `-- ` folded.
//!
//! The text has come out of the parse jail as UTF-8 with control
//! characters other than newlines and tabs already removed.

/// One line of the text, with its quote depth and links.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub depth: u8,
    /// Without its quote marks.
    pub text: String,
    /// Byte ranges in `text`, with the link each opens.
    pub links: Vec<(usize, usize, String)>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Plain {
    pub lines: Vec<Line>,
    /// Where the quoted tail starts (with its "On … wrote:" line), folded
    /// behind "Show quoted text".
    pub quoted_from: Option<usize>,
    /// Where the signature starts (its `-- ` line), folded behind "Show signature".
    pub signature_from: Option<usize>,
}

/// Most quote levels drawn; deeper ones are drawn at this depth.
pub const DEPTH_MAX: u8 = 8;

fn depth_of(line: &str) -> (u8, &str) {
    let mut d = 0u8;
    let mut rest = line;
    loop {
        let t = rest.trim_start_matches(' ');
        // At most three spaces before a mark, so indented text isn't taken for quotes.
        if t.starts_with('>') && rest.len() - t.len() <= 3 {
            d = d.saturating_add(1);
            rest = &t[1..];
        } else {
            break;
        }
    }
    if d > 0 {
        rest = rest.strip_prefix(' ').unwrap_or(rest);
    }
    (d.min(DEPTH_MAX), rest)
}

/// "On Mon, 3 Nov 2026, Arun wrote:", "Am … schrieb …:", "Le … a écrit :",
/// "-----Original Message-----": a line that introduces a quote.
fn introduces_quote(l: &str) -> bool {
    let t = l.trim();
    let lower = t.to_lowercase();
    (t.ends_with(':') && (lower.contains(" wrote") || lower.contains(" schrieb") || lower.contains(" a écrit") || lower.contains(" escribió") || lower.contains(" scritto")))
        || lower.starts_with("-----original message-----")
}

/// The links in `s`: an `https://`, `http://` or `mailto:` run up to a
/// space or a bracket, without trailing punctuation (a closing parenthesis
/// stays when the link opened one).
pub fn links(s: &str) -> Vec<(usize, usize, String)> {
    let lower = s.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let rest = &lower[i..];
        let found = ["https://", "http://", "mailto:"].iter().filter_map(|p| rest.find(p).map(|at| (at, p.len()))).min();
        let Some((at, plen)) = found else { break };
        let start = i + at;
        // Only at a word's start ("xhttps://" is not a link).
        let ok_before = s[..start].chars().next_back().map_or(true, |c| !c.is_alphanumeric());
        let tail = &s[start + plen..];
        let stop = tail.find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '`' | '{' | '}' | '|' | '\\' | '^') || c.is_control()).unwrap_or(tail.len());
        let mut end = start + plen + stop;
        // Trailing punctuation belongs to the sentence.
        loop {
            let t = &s[start..end];
            let Some(c) = t.chars().next_back() else { break };
            let drop = match c {
                '.' | ',' | ';' | ':' | '!' | '?' | '\'' | ']' => true,
                ')' => t.matches('(').count() < t.matches(')').count(),
                _ => false,
            };
            if !drop {
                break;
            }
            end -= c.len_utf8();
        }
        if ok_before && end > start + plen {
            out.push((start, end, s[start..end].to_string()));
        }
        i = end.max(start + plen);
    }
    out
}

pub fn parse(text: &str) -> Plain {
    let mut lines: Vec<Line> = text
        .split('\n')
        .map(|raw| {
            let raw = raw.strip_suffix('\r').unwrap_or(raw).replace('\t', "    ");
            let (depth, body) = depth_of(&raw);
            Line { depth, links: links(body), text: body.to_string() }
        })
        .collect();
    while lines.last().is_some_and(|l| l.text.trim().is_empty() && l.depth == 0) {
        lines.pop();
    }
    // The signature: the last `-- ` line outside quotes, with not too much after it.
    let signature_from = lines.iter().rposition(|l| l.depth == 0 && (l.text == "-- " || l.text == "--")).filter(|&i| lines.len() - i <= 40);
    let end = signature_from.unwrap_or(lines.len());
    // The quoted tail: from the last unquoted line with words (before the
    // signature), everything after that is quotes, blank lines and the
    // line introducing them.
    let mut quoted_from = None;
    let mut i = end;
    while i > 0 {
        let l = &lines[i - 1];
        if l.depth > 0 || l.text.trim().is_empty() {
            i -= 1;
            continue;
        }
        break;
    }
    if (i..end).any(|k| lines[k].depth > 0) {
        let mut from = i;
        if from > 0 && introduces_quote(&lines[from - 1].text) {
            from -= 1;
        }
        // Outlook style: a quoted message with no marks after "Original Message".
        quoted_from = Some(from);
    } else if let Some(k) = (0..end).find(|&k| lines[k].depth == 0 && lines[k].text.trim().to_lowercase().starts_with("-----original message-----")) {
        quoted_from = Some(k);
    }
    // A message that is only a quote (a forward) shows it.
    if quoted_from == Some(0) || quoted_from.is_some_and(|q| lines[..q].iter().all(|l| l.text.trim().is_empty())) {
        quoted_from = None;
    }
    Plain { lines, quoted_from, signature_from }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_are_conservative() {
        let l = links("See https://acme.example/q4?x=1. Or (http://a.example/b_(c)), mailto:p@example.org; not ftp://x or javascript:alert(1) or xhttps://y");
        let urls: Vec<&str> = l.iter().map(|x| x.2.as_str()).collect();
        assert_eq!(urls, ["https://acme.example/q4?x=1", "http://a.example/b_(c)", "mailto:p@example.org"]);
        assert_eq!(links("<https://a.example/x>")[0].2, "https://a.example/x");
        assert!(links("https://").is_empty());
    }

    #[test]
    fn quotes_and_signature() {
        let p = parse("Thanks, see below.\n> inline question?\nAnswer.\n\nOn Mon, 23 Nov 2026, Arun wrote:\n> Here are the numbers\n>> older\n>\n-- \nPriya\nAcme");
        assert_eq!(p.lines[1].depth, 1);
        assert_eq!(p.lines[1].text, "inline question?");
        assert_eq!(p.lines[6].depth, 2);
        assert_eq!(p.quoted_from, Some(4));
        assert_eq!(p.signature_from, Some(8));
        // Inline quotes between answers are not the tail.
        let p = parse("> q1\na1\n> q2\na2");
        assert_eq!(p.quoted_from, None);
        // A forward that is all quote shows it.
        let p = parse("> forwarded\n> text");
        assert_eq!(p.quoted_from, None);
        let p = parse("Sure.\n\n-----Original Message-----\nFrom: Arun\nSubject: Q4\n\nNumbers");
        assert_eq!(p.quoted_from, Some(2));
        // Indented code isn't a quote.
        assert_eq!(parse("    > not a quote").lines[0].depth, 0);
    }
}
