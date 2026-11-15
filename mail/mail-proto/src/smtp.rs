//! SMTP submission (RFC 6409, RFC 5321), client side, sans-I/O.
//!
//! The Arca mail courier has done TLS, `EHLO` and `AUTH` before mail-sync
//! sees the connection. It then sends mail-sync the server's post-login
//! `EHLO` reply, which is where [`Caps`] come from (mail.md §5.2). This
//! module has no AUTH, STARTTLS or EHLO of its own.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub code: u16,
    pub lines: Vec<String>,
}

impl Reply {
    pub fn positive(&self) -> bool {
        (200..400).contains(&self.code)
    }
    pub fn transient(&self) -> bool {
        (400..500).contains(&self.code)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Syntax,
    TooLong,
}

/// Collects multi-line replies (`250-…` … `250 …`).
#[derive(Default)]
pub struct ReplyDecoder {
    buf: Vec<u8>,
    lines: Vec<String>,
    code: Option<u16>,
}

const MAX_LINE: usize = 4096;
const MAX_LINES: usize = 128;

impl ReplyDecoder {
    pub fn feed(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    pub fn next(&mut self) -> Option<Result<Reply, Error>> {
        loop {
            let Some(nl) = self.buf.iter().position(|&c| c == b'\n') else {
                if self.buf.len() > MAX_LINE {
                    return Some(Err(Error::TooLong));
                }
                return None;
            };
            let raw: Vec<u8> = self.buf.drain(..=nl).collect();
            if raw.len() > MAX_LINE {
                return Some(Err(Error::TooLong));
            }
            let line = String::from_utf8_lossy(&raw);
            let line = line.trim_end_matches(['\r', '\n']);
            let b = line.as_bytes();
            if b.len() < 3 || !b[..3].iter().all(u8::is_ascii_digit) {
                return Some(Err(Error::Syntax));
            }
            let code: u16 = line[..3].parse().unwrap();
            if !(200..600).contains(&code) {
                return Some(Err(Error::Syntax));
            }
            if self.code.is_some_and(|c| c != code) {
                return Some(Err(Error::Syntax));
            }
            self.code = Some(code);
            let sep = b.get(3).copied();
            let text: String = line.get(4..).unwrap_or("").chars().filter(|c| !c.is_control()).collect();
            if self.lines.len() >= MAX_LINES {
                return Some(Err(Error::TooLong));
            }
            self.lines.push(text);
            match sep {
                Some(b'-') => continue,
                None | Some(b' ') => {
                    let r = Reply { code, lines: std::mem::take(&mut self.lines) };
                    self.code = None;
                    return Some(Ok(r));
                }
                _ => return Some(Err(Error::Syntax)),
            }
        }
    }
}

/// What the server offers, from its EHLO reply.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Caps {
    pub size: Option<u64>,
    pub eightbit: bool,
    pub smtputf8: bool,
    pub pipelining: bool,
    pub chunking: bool,
    /// FUTURERELEASE (RFC 4865): the longest hold in seconds.
    pub future_release: Option<u64>,
}

impl Caps {
    pub fn from_ehlo(r: &Reply) -> Caps {
        let mut c = Caps::default();
        for l in r.lines.iter().skip(1) {
            let mut it = l.split_whitespace();
            let kw = it.next().unwrap_or("").to_ascii_uppercase();
            match kw.as_str() {
                "SIZE" => c.size = Some(it.next().and_then(|n| n.parse().ok()).unwrap_or(0)).filter(|&n| n > 0),
                "8BITMIME" => c.eightbit = true,
                "SMTPUTF8" => c.smtputf8 = true,
                "PIPELINING" => c.pipelining = true,
                "CHUNKING" => c.chunking = true,
                "FUTURERELEASE" => c.future_release = it.next().and_then(|n| n.parse().ok()),
                _ => {}
            }
        }
        c
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// An address with characters that can't go on the wire.
    BadAddress(String),
    /// A non-ASCII address and no SMTPUTF8.
    NeedsSmtpUtf8(String),
    /// The message is over the server's SIZE.
    TooBig { size: u64, limit: u64 },
    /// A hold longer than the server allows, or no FUTURERELEASE.
    HoldNotOffered,
    NoRecipients,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Send(Vec<u8>),
    /// Accepted. `rejected` lists recipients the server refused (the rest
    /// got it).
    Sent { rejected: Vec<(String, Reply)> },
    /// Not sent. `transient` means try again later.
    Failed { stage: &'static str, reply: Reply, transient: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Mail,
    Rcpt(usize),
    Data,
    Body,
    Reset,
    Done,
}

/// One message, from `MAIL FROM` to the reply after the final dot.
pub struct Submission {
    from: String,
    rcpts: Vec<String>,
    body: Vec<u8>,
    params: String,
    state: State,
    rejected: Vec<(String, Reply)>,
    failure: Option<(&'static str, Reply)>,
}

fn check_addr(a: &str, caps: &Caps) -> Result<(), Refusal> {
    if a.len() > 320 || a.chars().any(|c| c.is_control() || c.is_whitespace() || c == '<' || c == '>') || (!a.is_empty() && !a.contains('@')) {
        return Err(Refusal::BadAddress(a.to_string()));
    }
    if !a.is_ascii() && !caps.smtputf8 {
        return Err(Refusal::NeedsSmtpUtf8(a.to_string()));
    }
    Ok(())
}

impl Submission {
    /// `hold_for`: seconds to hold on the server (send later, mail.md §9.3).
    pub fn new(caps: &Caps, from: &str, rcpts: &[&str], message: &[u8], hold_for: Option<u64>) -> Result<Self, Refusal> {
        check_addr(from, caps)?;
        if rcpts.is_empty() {
            return Err(Refusal::NoRecipients);
        }
        for r in rcpts {
            if r.is_empty() {
                return Err(Refusal::BadAddress(String::new()));
            }
            check_addr(r, caps)?;
        }
        let body = dot_stuff(message);
        let size = message.len() as u64;
        let mut params = String::new();
        if let Some(limit) = caps.size {
            if size > limit {
                return Err(Refusal::TooBig { size, limit });
            }
            params.push_str(&format!(" SIZE={size}"));
        }
        let eight = message.iter().any(|&c| c >= 0x80);
        let utf8 = !from.is_ascii() || rcpts.iter().any(|r| !r.is_ascii());
        if eight && caps.eightbit {
            params.push_str(" BODY=8BITMIME");
        }
        if utf8 {
            params.push_str(" SMTPUTF8");
        }
        if let Some(h) = hold_for {
            match caps.future_release {
                Some(max) if h <= max => params.push_str(&format!(" HOLDFOR={h}")),
                _ => return Err(Refusal::HoldNotOffered),
            }
        }
        Ok(Submission {
            from: from.to_string(),
            rcpts: rcpts.iter().map(|s| s.to_string()).collect(),
            body,
            params,
            state: State::Mail,
            rejected: Vec::new(),
            failure: None,
        })
    }

    /// The first command.
    pub fn start(&mut self) -> Vec<u8> {
        self.state = State::Mail;
        format!("MAIL FROM:<{}>{}\r\n", self.from, self.params).into_bytes()
    }

    pub fn on_reply(&mut self, r: Reply) -> Step {
        match self.state {
            State::Mail => {
                if !r.positive() {
                    return self.fail("MAIL FROM", r);
                }
                self.state = State::Rcpt(0);
                self.rcpt(0)
            }
            State::Rcpt(i) => {
                if !r.positive() {
                    self.rejected.push((self.rcpts[i].clone(), r.clone()));
                }
                if i + 1 < self.rcpts.len() {
                    self.state = State::Rcpt(i + 1);
                    return self.rcpt(i + 1);
                }
                if self.rejected.len() == self.rcpts.len() {
                    return self.fail("RCPT TO", r);
                }
                self.state = State::Data;
                Step::Send(b"DATA\r\n".to_vec())
            }
            State::Data => {
                if r.code != 354 {
                    return self.fail("DATA", r);
                }
                self.state = State::Body;
                Step::Send(std::mem::take(&mut self.body))
            }
            State::Body => {
                self.state = State::Done;
                if r.positive() {
                    Step::Sent { rejected: std::mem::take(&mut self.rejected) }
                } else {
                    let transient = r.transient();
                    Step::Failed { stage: "message", reply: r, transient }
                }
            }
            State::Reset => {
                self.state = State::Done;
                let (stage, reply) = self.failure.take().unwrap();
                let transient = reply.transient();
                Step::Failed { stage, reply, transient }
            }
            State::Done => Step::Failed { stage: "done", reply: r, transient: false },
        }
    }

    fn rcpt(&self, i: usize) -> Step {
        Step::Send(format!("RCPT TO:<{}>\r\n", self.rcpts[i]).into_bytes())
    }

    /// After a refusal mid-transaction: `RSET`, then report the refusal.
    fn fail(&mut self, stage: &'static str, r: Reply) -> Step {
        self.failure = Some((stage, r));
        self.state = State::Reset;
        Step::Send(b"RSET\r\n".to_vec())
    }
}

/// CRLF line ends, a leading dot doubled, and the final `.` line
/// (RFC 5321 §4.5.2).
pub fn dot_stuff(m: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(m.len() + m.len() / 50 + 5);
    let mut at_line_start = true;
    let mut i = 0;
    while i < m.len() {
        let c = m[i];
        if at_line_start && c == b'.' {
            out.push(b'.');
        }
        match c {
            b'\r' if m.get(i + 1) == Some(&b'\n') => {
                out.extend_from_slice(b"\r\n");
                i += 2;
                at_line_start = true;
                continue;
            }
            b'\r' | b'\n' => {
                out.extend_from_slice(b"\r\n");
                at_line_start = true;
            }
            _ => {
                out.push(c);
                at_line_start = false;
            }
        }
        i += 1;
    }
    if !at_line_start {
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b".\r\n");
    out
}
