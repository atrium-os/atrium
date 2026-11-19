//! ManageSieve (RFC 5804) after the Arca courier has logged in (mail.md
//! §11.6): the server's responses read into lines of strings, and the few
//! commands Mail sends (list, put, activate, log out). The courier starts
//! TLS and signs in; it hands over the server's capabilities, then its own
//! `OK`, and refuses any sign-in from us.

/// The most a response may hold (Mail never fetches a script).
pub const RESPONSE_MAX: usize = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    No,
    Bye,
}

/// One word of a response line: a quoted string or literal, or an atom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Atom(String),
    Str(String),
}

impl Item {
    pub fn text(&self) -> &str {
        match self {
            Item::Atom(s) | Item::Str(s) => s,
        }
    }
}

/// A whole response: its data lines, then how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub lines: Vec<Vec<Item>>,
    pub status: Status,
    /// The response code, as written inside its parentheses.
    pub code: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    TooLong,
    Malformed(String),
}

/// Reads responses from the bytes as they come.
#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

enum Got<T> {
    Done(T, usize),
    More,
}

fn more<T>() -> Result<Got<T>, DecodeError> {
    Ok(Got::More)
}

/// One line of items from `b[at..]`: the items and where the next line
/// starts.
fn line(b: &[u8], mut at: usize) -> Result<Got<Vec<Item>>, DecodeError> {
    let mut items = Vec::new();
    loop {
        match b.get(at) {
            None => return more(),
            Some(b'\r') => {
                return match b.get(at + 1) {
                    None => more(),
                    Some(b'\n') => Ok(Got::Done(items, at + 2)),
                    Some(_) => Err(DecodeError::Malformed("a CR without LF".into())),
                };
            }
            Some(b'\n') => return Ok(Got::Done(items, at + 1)),
            Some(b' ') => at += 1,
            Some(b'"') => {
                let mut s = Vec::new();
                at += 1;
                loop {
                    match b.get(at) {
                        None => return more(),
                        Some(b'\\') => match b.get(at + 1) {
                            None => return more(),
                            Some(c) => {
                                s.push(*c);
                                at += 2;
                            }
                        },
                        Some(b'"') => {
                            at += 1;
                            break;
                        }
                        Some(b'\r' | b'\n') => return Err(DecodeError::Malformed("a line break in a quoted string".into())),
                        Some(c) => {
                            s.push(*c);
                            at += 1;
                        }
                    }
                }
                items.push(Item::Str(String::from_utf8_lossy(&s).into_owned()));
            }
            Some(b'{') => {
                let end = match b[at..].iter().position(|c| *c == b'}') {
                    Some(e) => at + e,
                    None if b.len() - at > 16 => return Err(DecodeError::Malformed("a literal's length".into())),
                    None => return more(),
                };
                let n: usize = std::str::from_utf8(&b[at + 1..end]).ok().map(|s| s.trim_end_matches('+')).and_then(|s| s.parse().ok()).ok_or_else(|| DecodeError::Malformed("a literal's length".into()))?;
                if n > RESPONSE_MAX {
                    return Err(DecodeError::TooLong);
                }
                let start = match (b.get(end + 1), b.get(end + 2)) {
                    (Some(b'\r'), Some(b'\n')) => end + 3,
                    (Some(b'\n'), _) => end + 2,
                    (None, _) | (Some(b'\r'), None) => return more(),
                    _ => return Err(DecodeError::Malformed("a literal without its line break".into())),
                };
                if b.len() < start + n {
                    return more();
                }
                items.push(Item::Str(String::from_utf8_lossy(&b[start..start + n]).into_owned()));
                at = start + n;
            }
            Some(b'(') => {
                // A response code: kept as written, quoted strings and all.
                let mut depth = 0usize;
                let mut quoted = false;
                let mut i = at;
                loop {
                    match b.get(i) {
                        None => return more(),
                        Some(b'"') => quoted = !quoted,
                        Some(b'\\') if quoted => i += 1,
                        Some(b'(') if !quoted => depth += 1,
                        Some(b')') if !quoted => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        Some(b'\r' | b'\n') => return Err(DecodeError::Malformed("an unclosed response code".into())),
                        _ => {}
                    }
                    i += 1;
                }
                items.push(Item::Atom(String::from_utf8_lossy(&b[at..=i]).into_owned()));
                at = i + 1;
            }
            Some(_) => {
                let start = at;
                while let Some(c) = b.get(at) {
                    if matches!(c, b' ' | b'\r' | b'\n' | b'(' | b'"') {
                        break;
                    }
                    at += 1;
                }
                if at == b.len() {
                    return more();
                }
                items.push(Item::Atom(String::from_utf8_lossy(&b[start..at]).into_owned()));
            }
        }
    }
}

fn status_of(items: &[Item]) -> Option<Status> {
    match items.first()? {
        Item::Atom(a) if a.eq_ignore_ascii_case("OK") => Some(Status::Ok),
        Item::Atom(a) if a.eq_ignore_ascii_case("NO") => Some(Status::No),
        Item::Atom(a) if a.eq_ignore_ascii_case("BYE") => Some(Status::Bye),
        _ => None,
    }
}

impl Decoder {
    pub fn feed(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    /// The next whole response, if one has come.
    pub fn next(&mut self) -> Option<Result<Response, DecodeError>> {
        let mut at = 0;
        let mut lines = Vec::new();
        loop {
            match line(&self.buf, at) {
                Err(e) => return Some(Err(e)),
                Ok(Got::More) if self.buf.len() > RESPONSE_MAX => return Some(Err(DecodeError::TooLong)),
                Ok(Got::More) => return None,
                Ok(Got::Done(items, next)) => {
                    at = next;
                    if let Some(status) = status_of(&items) {
                        let mut rest = items.into_iter().skip(1).peekable();
                        let code = match rest.peek() {
                            Some(Item::Atom(a)) if a.starts_with('(') => {
                                let a = a[1..a.len() - 1].to_string();
                                rest.next();
                                Some(a)
                            }
                            _ => None,
                        };
                        let text = rest.map(|i| i.text().to_string()).collect::<Vec<_>>().join(" ");
                        self.buf.drain(..at);
                        return Some(Ok(Response { lines, status, code, text }));
                    }
                    if !items.is_empty() {
                        lines.push(items);
                    }
                }
            }
        }
    }
}

/// What the server offers, from its capability lines.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Caps {
    pub implementation: String,
    /// Its Sieve extensions, lower case (`fileinto`, `vacation`, `date`, …).
    pub sieve: Vec<String>,
}

impl Caps {
    pub fn from_lines(lines: &[Vec<Item>]) -> Caps {
        let mut c = Caps::default();
        for l in lines {
            let (Some(name), value) = (l.first(), l.get(1).map(Item::text).unwrap_or("")) else { continue };
            match name.text().to_ascii_uppercase().as_str() {
                "IMPLEMENTATION" => c.implementation = value.chars().take(100).collect(),
                "SIEVE" => c.sieve = value.split_whitespace().map(str::to_ascii_lowercase).collect(),
                _ => {}
            }
        }
        c
    }

    pub fn has(&self, ext: &str) -> bool {
        self.sieve.iter().any(|e| e == ext)
    }
}

/// A script's name from `LISTSCRIPTS`, and whether it is the active one.
pub fn scripts(r: &Response) -> Vec<(String, bool)> {
    r.lines
        .iter()
        .filter_map(|l| {
            let name = l.first()?.text().to_string();
            Some((name, l.get(1).is_some_and(|a| a.text().eq_ignore_ascii_case("ACTIVE"))))
        })
        .collect()
}

/// `s` as a quoted string (Sieve and ManageSieve quote the same way).
pub fn quoted(s: &str) -> String {
    let mut q = String::with_capacity(s.len() + 2);
    q.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            q.push('\\');
        }
        q.push(c);
    }
    q.push('"');
    q
}

fn name_ok(name: &str) -> bool {
    !name.is_empty() && name.len() <= 128 && !name.chars().any(char::is_control)
}

pub fn listscripts() -> Vec<u8> {
    b"LISTSCRIPTS\r\n".to_vec()
}

/// Store `script` as `name` (a literal that needs no go-ahead, `{n+}`).
pub fn putscript(name: &str, script: &[u8]) -> Option<Vec<u8>> {
    if !name_ok(name) {
        return None;
    }
    let mut b = format!("PUTSCRIPT {} {{{}+}}\r\n", quoted(name), script.len()).into_bytes();
    b.extend_from_slice(script);
    b.extend_from_slice(b"\r\n");
    Some(b)
}

pub fn setactive(name: &str) -> Option<Vec<u8>> {
    name_ok(name).then(|| format!("SETACTIVE {}\r\n", quoted(name)).into_bytes())
}

pub fn logout() -> Vec<u8> {
    b"LOGOUT\r\n".to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(b: &[u8]) -> Vec<Response> {
        let mut d = Decoder::default();
        let mut out = Vec::new();
        // A byte at a time: every split point is a "more".
        for c in b {
            d.feed(&[*c]);
            while let Some(r) = d.next() {
                out.push(r.unwrap());
            }
        }
        out
    }

    #[test]
    fn responses_read_a_byte_at_a_time() {
        let caps = b"\"IMPLEMENTATION\" \"Dovecot Pigeonhole\"\r\n\"SIEVE\" \"fileinto reject envelope vacation imap4flags date relational\"\r\n\"NOTIFY\" \"mailto\"\r\n\"VERSION\" \"1.0\"\r\nOK \"Logged in by Arca\"\r\n";
        let list = b"\"atrium-mail\" ACTIVE\r\n{6}\r\nold\"et\r\nOK \"Listscripts completed.\"\r\n";
        let no = b"NO (QUOTA/MAXSIZE) \"Script is too big\"\r\nNO \"line 3: error: unknown command \\\"fileinot\\\"\"\r\nBYE (TRYLATER) \"down\"\r\n";
        let mut b = caps.to_vec();
        b.extend_from_slice(list);
        b.extend_from_slice(no);
        let r = all(&b);
        assert_eq!(r.len(), 5);
        let c = Caps::from_lines(&r[0].lines);
        assert_eq!(c.implementation, "Dovecot Pigeonhole");
        assert!(c.has("vacation") && c.has("date") && !c.has("include"));
        assert_eq!((r[0].status, r[0].text.as_str()), (Status::Ok, "Logged in by Arca"));
        assert_eq!(scripts(&r[1]), vec![("atrium-mail".to_string(), true), ("old\"et".to_string(), false)]);
        assert_eq!((r[2].status, r[2].code.as_deref(), r[2].text.as_str()), (Status::No, Some("QUOTA/MAXSIZE"), "Script is too big"));
        assert_eq!(r[3].text, "line 3: error: unknown command \"fileinot\"");
        assert_eq!((r[4].status, r[4].code.as_deref()), (Status::Bye, Some("TRYLATER")));
    }

    #[test]
    fn commands_and_refusals() {
        assert_eq!(putscript("atrium-mail", b"keep;").unwrap(), b"PUTSCRIPT \"atrium-mail\" {5+}\r\nkeep;\r\n");
        assert_eq!(setactive("a\"b").unwrap(), b"SETACTIVE \"a\\\"b\"\r\n");
        assert!(putscript("a\r\nLOGOUT", b"").is_none() && setactive("").is_none());
        let mut d = Decoder::default();
        d.feed(b"{99999999}\r\n");
        assert_eq!(d.next(), Some(Err(DecodeError::TooLong)));
        let mut d = Decoder::default();
        d.feed(b"\"a\rb\"\r\n");
        assert!(matches!(d.next(), Some(Err(DecodeError::Malformed(_)))));
    }
}
