//! The mail courier's relay filter (mail.md §5.2 point 4): a small, fixed,
//! allocation-bounded parser of the client's side of an IMAP or SMTP
//! session, after the courier has logged in.
//!
//! It knows where every command starts, so it can refuse the ones that would
//! change who the session is (`LOGIN`, `AUTHENTICATE`, `STARTTLS`,
//! `UNAUTHENTICATE`, `COMPRESS`; `AUTH`, `STARTTLS`, `BDAT`), and nothing
//! else. To know where commands start it must know what is NOT a command:
//!
//! - IMAP literals (`{n}`, `{n+}`, `~{n}`) are data. A synchronising
//!   literal is data only once the server has said `+`: until then the
//!   client's bytes are held, and if the server answers the command instead,
//!   the literal never happened and the held bytes are commands again.
//! - SMTP message text after `DATA` is data, but only once the server has
//!   answered `DATA` with 354; until then the client's bytes are held, and
//!   pipelined replies are counted to find that answer.
//!
//! That is the only reason it reads the server's side at all: line starts,
//! skipping the server's own literals, and SMTP reply codes.
//!
//! Both halves are pure: bytes in, bytes out, so the I/O loop is trivial and
//! the parser can be fuzzed alone.

/// Longest command line (without literals) the relay accepts from the client.
pub const MAX_LINE: usize = 64 * 1024;
/// Largest literal (an APPEND of a 64 MiB message, with room).
pub const MAX_LITERAL: u64 = 80 << 20;
/// Most bytes held while waiting for the server's go-ahead. A client that
/// waits as the protocol says holds nothing.
pub const MAX_HELD: usize = 64 * 1024;

/// Why the relay ends the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// A refused command: its name, upper-cased (logged as an anomaly).
    Refused(String),
    /// Over a limit or out of the protocol's shape.
    Malformed(&'static str),
}

pub const IMAP_REFUSED: &[&str] = &["LOGIN", "AUTHENTICATE", "STARTTLS", "UNAUTHENTICATE", "COMPRESS"];
pub const SMTP_REFUSED: &[&str] = &["AUTH", "STARTTLS", "BDAT"];

/// `{123}`, `{123+}` or `~{123}` at the end of a line (without its CRLF):
/// the literal's length and whether it is non-synchronising.
fn literal_at_end(line: &[u8]) -> Option<(u64, bool)> {
    let l = line.strip_suffix(b"}")?;
    let (l, plus) = match l.strip_suffix(b"+") { Some(x) => (x, true), None => match l.strip_suffix(b"-") { Some(x) => (x, true), None => (l, false) } };
    let open = l.iter().rposition(|&c| c == b'{')?;
    let digits = &l[open + 1..];
    if digits.is_empty() || digits.len() > 12 || !digits.iter().all(u8::is_ascii_digit) { return None }
    let n: u64 = std::str::from_utf8(digits).ok()?.parse().ok()?;
    Some((n, plus))
}

fn upper_token(b: &[u8]) -> String {
    b.iter().take(32).map(|c| c.to_ascii_uppercase() as char).collect()
}

// ───────────────────────── IMAP ─────────────────────────

#[derive(Debug)]
enum ImapState {
    /// Reading a line. `cont`: the line continues a command after a literal.
    Line { cont: bool },
    /// Inside a literal: this many bytes are data.
    Literal(u64),
    /// A synchronising literal was announced by command `tag`; waiting for
    /// the server's `+` (then `Literal(n)`) or its answer to `tag`.
    AwaitCont { tag: Vec<u8>, n: u64 },
}

/// The server's lines, as far as the filter needs them: where each one
/// starts, skipping the server's literals.
#[derive(Debug, Default)]
struct ServerLines {
    /// The current line's first bytes (enough for a tag) and last bytes
    /// (enough for a literal marker).
    head: Vec<u8>,
    tail: Vec<u8>,
    /// Literal bytes still to skip.
    lit: u64,
    /// The current line continues a response after a literal.
    cont: bool,
}

impl ServerLines {
    /// Feed server bytes; call `on_line(first_bytes)` for each line that
    /// starts a response (not a continuation after a literal).
    fn feed(&mut self, mut b: &[u8], on_line: &mut dyn FnMut(&[u8])) {
        while !b.is_empty() {
            if self.lit > 0 {
                let n = (self.lit.min(b.len() as u64)) as usize;
                self.lit -= n as u64;
                b = &b[n..];
                continue;
            }
            let Some(nl) = b.iter().position(|&c| c == b'\n') else {
                self.take(b);
                return;
            };
            self.take(&b[..nl]);
            b = &b[nl + 1..];
            let mut tail = std::mem::take(&mut self.tail);
            if tail.last() == Some(&b'\r') { tail.pop(); }
            let head = std::mem::take(&mut self.head);
            if !self.cont { on_line(&head) }
            match literal_at_end(&tail) {
                Some((n, _)) => { self.lit = n; self.cont = true }
                None => self.cont = false,
            }
        }
    }

    fn take(&mut self, b: &[u8]) {
        let room = 64usize.saturating_sub(self.head.len());
        self.head.extend_from_slice(&b[..b.len().min(room)]);
        self.tail.extend_from_slice(b);
        if self.tail.len() > 32 { self.tail.drain(..self.tail.len() - 32); }
    }
}

/// The IMAP relay filter.
#[derive(Debug)]
pub struct Imap {
    state: ImapState,
    line: Vec<u8>,
    held: Vec<u8>,
    server: ServerLines,
    /// After `IDLE`: the next client line is `DONE`, not a command.
    idling: bool,
    /// The tag of the command being read (for a literal on a later line).
    cmd_tag: Vec<u8>,
}

impl Default for Imap {
    fn default() -> Self { Imap { state: ImapState::Line { cont: false }, line: Vec::new(), held: Vec::new(), server: ServerLines::default(), idling: false, cmd_tag: Vec::new() } }
}

impl Imap {
    pub fn new() -> Imap { Imap::default() }

    /// Bytes from the client; what may go to the server is appended to `out`.
    pub fn client(&mut self, b: &[u8], out: &mut Vec<u8>) -> Result<(), Stop> {
        let mut i = 0;
        while i < b.len() {
            match &mut self.state {
                ImapState::AwaitCont { .. } => {
                    if self.held.len() + (b.len() - i) > MAX_HELD { return Err(Stop::Malformed("client did not wait for the server")) }
                    self.held.extend_from_slice(&b[i..]);
                    return Ok(());
                }
                ImapState::Literal(n) => {
                    let k = (*n).min((b.len() - i) as u64) as usize;
                    out.extend_from_slice(&b[i..i + k]);
                    *n -= k as u64;
                    i += k;
                    if *n == 0 { self.state = ImapState::Line { cont: true } }
                }
                ImapState::Line { cont } => {
                    let cont = *cont;
                    let rest = &b[i..];
                    match rest.iter().position(|&c| c == b'\n') {
                        None => {
                            if self.line.len() + rest.len() > MAX_LINE { return Err(Stop::Malformed("command line too long")) }
                            self.line.extend_from_slice(rest);
                            return Ok(());
                        }
                        Some(nl) => {
                            if self.line.len() + nl + 1 > MAX_LINE { return Err(Stop::Malformed("command line too long")) }
                            self.line.extend_from_slice(&rest[..=nl]);
                            i += nl + 1;
                            let line = std::mem::take(&mut self.line);
                            self.line_done(&line, cont, out)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn line_done(&mut self, line: &[u8], cont: bool, out: &mut Vec<u8>) -> Result<(), Stop> {
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        let body = body.strip_suffix(b"\r").unwrap_or(body);
        if !cont {
            let mut words = body.split(|&c| c == b' ');
            let tag = words.next().unwrap_or(&[]);
            self.cmd_tag = tag.to_vec();
            let cmd = upper_token(words.next().unwrap_or(&[]));
            if self.idling {
                // The line after IDLE ends it; it has no tag.
                self.idling = false;
            } else {
                if tag.is_empty() || tag.iter().any(|c| matches!(c, b'*' | b'+' | b'{' | b'(' | b')' | b'%' | b'"' | b'\\') || c.is_ascii_control()) {
                    return Err(Stop::Malformed("command without a tag"));
                }
                if let Some(r) = IMAP_REFUSED.iter().find(|r| **r == cmd) { return Err(Stop::Refused(r.to_string())) }
                if cmd == "IDLE" { self.idling = true }
            }
        }
        out.extend_from_slice(line);
        match literal_at_end(body) {
            Some((n, _)) if n > MAX_LITERAL => Err(Stop::Malformed("literal too large")),
            Some((0, _)) => { self.state = ImapState::Line { cont: true }; Ok(()) }
            Some((n, true)) => { self.state = ImapState::Literal(n); Ok(()) }
            Some((n, false)) => {
                // Answered with `+` or with the command's tagged result.
                self.state = ImapState::AwaitCont { tag: self.cmd_tag.clone(), n };
                Ok(())
            }
            None => { self.state = ImapState::Line { cont: false }; Ok(()) }
        }
    }

    /// Bytes from the server (all go to the client unchanged); may release
    /// held client bytes into `to_server`.
    pub fn server(&mut self, b: &[u8], to_server: &mut Vec<u8>) -> Result<(), Stop> {
        let mut events: Vec<Vec<u8>> = Vec::new();
        self.server.feed(b, &mut |head| events.push(head.to_vec()));
        for head in events {
            let ImapState::AwaitCont { tag, n } = &self.state else { continue };
            let first = head.split(|&c| c == b' ').next().unwrap_or(&[]);
            let first = first.strip_suffix(b"\r").unwrap_or(first);
            if first == b"+" {
                self.state = ImapState::Literal(*n);
            } else if first == tag.as_slice() {
                // Answered without a go-ahead: the literal never came; what
                // the client sent meanwhile is commands again.
                self.state = ImapState::Line { cont: false };
            } else {
                continue;
            }
            let held = std::mem::take(&mut self.held);
            self.client(&held, to_server)?;
        }
        Ok(())
    }
}

// ───────────────────────── ManageSieve ─────────────────────────

/// What a ManageSieve client may say after the courier logged in (RFC
/// 5804 §2): the script commands and nothing that signs in again, starts
/// TLS or drops the sign-in.
pub const SIEVE_ALLOWED: &[&str] = &["CAPABILITY", "HAVESPACE", "PUTSCRIPT", "LISTSCRIPTS", "SETACTIVE", "GETSCRIPT", "DELETESCRIPT", "RENAMESCRIPT", "CHECKSCRIPT", "NOOP", "LOGOUT"];
/// Largest script (a literal) the relay passes.
pub const MAX_SCRIPT: u64 = 1 << 20;

#[derive(Debug)]
enum SieveState {
    Line { cont: bool },
    Literal(u64),
}

/// The ManageSieve relay filter: an allowlist of commands, and literals
/// as data. Only non-synchronising literals (`{n+}`), which every server
/// takes (RFC 5804 §1.6), so the server's side needn't be read.
#[derive(Debug)]
pub struct Sieve {
    state: SieveState,
    line: Vec<u8>,
}

impl Default for Sieve {
    fn default() -> Self { Sieve { state: SieveState::Line { cont: false }, line: Vec::new() } }
}

impl Sieve {
    pub fn new() -> Sieve { Sieve::default() }

    pub fn client(&mut self, b: &[u8], out: &mut Vec<u8>) -> Result<(), Stop> {
        let mut i = 0;
        while i < b.len() {
            match &mut self.state {
                SieveState::Literal(n) => {
                    let k = (*n).min((b.len() - i) as u64) as usize;
                    out.extend_from_slice(&b[i..i + k]);
                    *n -= k as u64;
                    i += k;
                    if *n == 0 { self.state = SieveState::Line { cont: true } }
                }
                SieveState::Line { cont } => {
                    let cont = *cont;
                    let rest = &b[i..];
                    match rest.iter().position(|&c| c == b'\n') {
                        None => {
                            if self.line.len() + rest.len() > MAX_LINE { return Err(Stop::Malformed("command line too long")) }
                            self.line.extend_from_slice(rest);
                            return Ok(());
                        }
                        Some(nl) => {
                            if self.line.len() + nl + 1 > MAX_LINE { return Err(Stop::Malformed("command line too long")) }
                            self.line.extend_from_slice(&rest[..=nl]);
                            i += nl + 1;
                            let line = std::mem::take(&mut self.line);
                            self.line_done(&line, cont, out)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn line_done(&mut self, line: &[u8], cont: bool, out: &mut Vec<u8>) -> Result<(), Stop> {
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        let body = body.strip_suffix(b"\r").unwrap_or(body);
        if !cont {
            let word = body.split(|&c| c == b' ').next().unwrap_or(&[]);
            if !word.is_empty() || !body.is_empty() {
                let cmd = upper_token(word);
                if !SIEVE_ALLOWED.contains(&cmd.as_str()) {
                    return Err(Stop::Refused(if cmd.is_empty() { "?".into() } else { cmd }));
                }
            }
        }
        out.extend_from_slice(line);
        match literal_at_end(body) {
            Some((n, _)) if n > MAX_SCRIPT => Err(Stop::Malformed("literal too large")),
            Some((_, false)) => Err(Stop::Malformed("synchronising literal")),
            Some((0, true)) => { self.state = SieveState::Line { cont: true }; Ok(()) }
            Some((n, true)) => { self.state = SieveState::Literal(n); Ok(()) }
            None => { self.state = SieveState::Line { cont: false }; Ok(()) }
        }
    }

    /// The server's bytes: nothing to learn from them.
    pub fn server(&mut self, _: &[u8], _: &mut Vec<u8>) -> Result<(), Stop> {
        Ok(())
    }
}

#[cfg(test)]
mod sieve_tests {
    use super::*;

    fn run(f: &mut Sieve, chunks: &[&[u8]]) -> Result<Vec<u8>, Stop> {
        let mut out = Vec::new();
        for c in chunks {
            f.client(c, &mut out)?;
        }
        Ok(out)
    }

    #[test]
    fn scripts_pass_and_sign_ins_dont() {
        let script = b"require \"vacation\";\r\nAUTHENTICATE \"PLAIN\" \"x\"\r\n";
        let mut cmd = format!("PUTSCRIPT \"mail\" {{{}+}}\r\n", script.len()).into_bytes();
        cmd.extend_from_slice(script);
        cmd.extend_from_slice(b"\r\nSETACTIVE \"mail\"\r\nlistscripts\r\n");
        let mut f = Sieve::new();
        // Split anywhere, the same bytes go through.
        for at in [1, 7, 20, cmd.len() - 3] {
            let mut f = Sieve::new();
            assert_eq!(run(&mut f, &[&cmd[..at], &cmd[at..]]).unwrap(), cmd);
        }
        assert_eq!(run(&mut f, &[&cmd]).unwrap(), cmd);
        for bad in [&b"AUTHENTICATE \"PLAIN\" \"eA==\"\r\n"[..], b"STARTTLS\r\n", b"UNAUTHENTICATE\r\n", b"\"PUTSCRIPT\"\r\n"] {
            let mut f = Sieve::new();
            assert!(matches!(run(&mut f, &[bad]), Err(Stop::Refused(_))), "{}", String::from_utf8_lossy(bad));
        }
        let mut f = Sieve::new();
        assert_eq!(run(&mut f, &[b"PUTSCRIPT \"x\" {5}\r\n"]), Err(Stop::Malformed("synchronising literal")));
        let mut f = Sieve::new();
        assert_eq!(run(&mut f, &[b"PUTSCRIPT \"x\" {99999999+}\r\n"]), Err(Stop::Malformed("literal too large")));
    }
}

// ───────────────────────── SMTP ─────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SmtpState {
    Command,
    /// `DATA` was sent; its reply is the `n`th final reply since the relay
    /// started. Client bytes are held until it arrives.
    AwaitData(u64),
    /// Message text; `at`: how much of "\r\n.\r\n" has just been seen.
    Data { at: u8 },
}

/// The SMTP relay filter.
#[derive(Debug)]
pub struct Smtp {
    state: SmtpState,
    line: Vec<u8>,
    held: Vec<u8>,
    /// Commands (and message ends) sent, and final replies seen.
    sent: u64,
    replied: u64,
    server_line: Vec<u8>,
}

impl Default for Smtp {
    fn default() -> Self { Smtp { state: SmtpState::Command, line: Vec::new(), held: Vec::new(), sent: 0, replied: 0, server_line: Vec::new() } }
}

impl Smtp {
    pub fn new() -> Smtp { Smtp::default() }

    pub fn client(&mut self, b: &[u8], out: &mut Vec<u8>) -> Result<(), Stop> {
        let mut i = 0;
        while i < b.len() {
            match self.state {
                SmtpState::AwaitData(_) => {
                    if self.held.len() + (b.len() - i) > MAX_HELD { return Err(Stop::Malformed("client did not wait for 354")) }
                    self.held.extend_from_slice(&b[i..]);
                    return Ok(());
                }
                SmtpState::Data { mut at } => {
                    // Forward up to and including the end marker.
                    const END: &[u8] = b"\r\n.\r\n";
                    let start = i;
                    while i < b.len() {
                        let c = b[i];
                        i += 1;
                        at = if c == END[at as usize] { at + 1 } else if c == b'\r' { 1 } else { 0 };
                        if at as usize == END.len() { break }
                    }
                    out.extend_from_slice(&b[start..i]);
                    if at as usize == END.len() {
                        self.sent += 1; // the reply to the message
                        self.state = SmtpState::Command;
                    } else {
                        self.state = SmtpState::Data { at };
                    }
                }
                SmtpState::Command => {
                    let rest = &b[i..];
                    let Some(nl) = rest.iter().position(|&c| c == b'\n') else {
                        if self.line.len() + rest.len() > 4096 { return Err(Stop::Malformed("command line too long")) }
                        self.line.extend_from_slice(rest);
                        return Ok(());
                    };
                    if self.line.len() + nl + 1 > 4096 { return Err(Stop::Malformed("command line too long")) }
                    self.line.extend_from_slice(&rest[..=nl]);
                    i += nl + 1;
                    let line = std::mem::take(&mut self.line);
                    let verb = upper_token(line.split(|&c| c == b' ' || c == b'\r' || c == b'\n').next().unwrap_or(&[]));
                    if let Some(r) = SMTP_REFUSED.iter().find(|r| **r == verb) { return Err(Stop::Refused(r.to_string())) }
                    out.extend_from_slice(&line);
                    self.sent += 1;
                    if verb == "DATA" { self.state = SmtpState::AwaitData(self.sent) }
                }
            }
        }
        Ok(())
    }

    pub fn server(&mut self, b: &[u8], to_server: &mut Vec<u8>) -> Result<(), Stop> {
        for &c in b {
            if c != b'\n' {
                if self.server_line.len() < 8 { self.server_line.push(c) }
                continue;
            }
            let l = std::mem::take(&mut self.server_line);
            let code_ok = l.len() >= 3 && l[..3].iter().all(u8::is_ascii_digit);
            let last = l.get(3).is_none_or(|&c| c == b' ' || c == b'\r');
            if !(code_ok && last) { continue }
            self.replied += 1;
            if let SmtpState::AwaitData(n) = self.state {
                if self.replied == n {
                    self.state = if &l[..3] == b"354" { SmtpState::Data { at: 2 } } else { SmtpState::Command };
                    let held = std::mem::take(&mut self.held);
                    self.client(&held, to_server)?;
                }
            }
        }
        Ok(())
    }
}
