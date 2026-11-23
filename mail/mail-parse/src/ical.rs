//! Calendar invites (iCalendar RFC 5545 + iTIP RFC 5546) into a bounded
//! card (docs/spec/mail.md §14.1). Only what the card shows is kept; no
//! URL in an invite is ever fetched, and none is kept except as text.

use crate::header::clean;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Request,
    Cancel,
    Reply,
    Publish,
    Counter,
    Other,
}

/// A date-time as written: UTC (`Z`), local in a named zone, floating, or
/// an all-day date. Zones are resolved by the engine, not here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum When {
    Utc(i64),
    Zoned { local: String, tzid: String },
    Floating(String),
    Date(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invite {
    pub method: Method,
    pub uid: String,
    pub sequence: u32,
    pub title: String,
    pub start: Option<When>,
    pub end: Option<When>,
    pub location: String,
    /// The organizer's address (`mailto:` stripped).
    pub organizer: String,
    pub attendees: u32,
    /// The person's own PARTSTAT, if they are an attendee named in `me`.
    pub my_status: Option<String>,
    pub cancelled: bool,
    /// Has an RRULE (repeats); the card says so, the rule isn't expanded here.
    pub repeats: bool,
}

const MAX_LINES: usize = 20_000;

fn unfold(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if (line.starts_with(' ') || line.starts_with('\t')) && !out.is_empty() {
            let l = out.last_mut().unwrap();
            if l.len() < 8192 {
                l.push_str(&line[1..]);
            }
        } else if !line.is_empty() {
            if out.len() >= MAX_LINES {
                break;
            }
            out.push(line.to_string());
        }
    }
    out
}

/// (NAME, params, value) of one content line; quoted params respected.
fn split_line(l: &str) -> Option<(String, Vec<(String, String)>, String)> {
    let mut quoted = false;
    let mut colon = None;
    for (i, c) in l.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ':' if !quoted => {
                colon = Some(i);
                break;
            }
            _ => {}
        }
    }
    let colon = colon?;
    let mut head = l[..colon].split(';');
    let name = head.next()?.trim().to_ascii_uppercase();
    let params = head
        .filter_map(|p| {
            let (k, v) = p.split_once('=')?;
            Some((k.trim().to_ascii_uppercase(), v.trim().trim_matches('"').to_string()))
        })
        .take(32)
        .collect();
    Some((name, params, l[colon + 1..].to_string()))
}

fn text_value(v: &str, cap: usize) -> String {
    let mut out = String::new();
    let mut it = v.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('n') | Some('N') => out.push(' '),
                Some(x) => out.push(x),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    clean(out.trim(), cap)
}

fn when(v: &str, params: &[(String, String)]) -> Option<When> {
    let v = v.trim();
    let is_date = params.iter().any(|(k, x)| k == "VALUE" && x.eq_ignore_ascii_case("DATE")) || (v.len() == 8 && v.bytes().all(|c| c.is_ascii_digit()));
    if is_date {
        return Some(When::Date(clean(v, 16)));
    }
    let b = v.as_bytes();
    if b.len() < 15 || b[8] != b'T' || !b[..8].iter().all(u8::is_ascii_digit) || !b[9..15].iter().all(u8::is_ascii_digit) {
        return None;
    }
    if v.ends_with('Z') {
        let n = |a: usize, l: usize| v[a..a + l].parse::<i64>().ok();
        let s = crate::mime::parse_date(&format!("{} {} {} {}:{}:{} +0000", n(6, 2)?, ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"].get((n(4, 2)? - 1) as usize)?, n(0, 4)?, n(9, 2)?, n(11, 2)?, n(13, 2)?))?;
        return Some(When::Utc(s));
    }
    match params.iter().find(|(k, _)| k == "TZID") {
        Some((_, tz)) => Some(When::Zoned { local: clean(&v[..15], 16), tzid: clean(tz, 64) }),
        None => Some(When::Floating(clean(&v[..15], 16))),
    }
}

/// Reads the first VEVENT of an invite. `me` are the person's own
/// addresses (lower case), to find their attendee status.
pub fn invite(text: &str, me: &[String]) -> Option<Invite> {
    let lines = unfold(text);
    let mut method = Method::Other;
    let mut inv = Invite {
        method: Method::Other,
        uid: String::new(),
        sequence: 0,
        title: String::new(),
        start: None,
        end: None,
        location: String::new(),
        organizer: String::new(),
        attendees: 0,
        my_status: None,
        cancelled: false,
        repeats: false,
    };
    let mut in_event = false;
    let mut depth = 0u32;
    let mut seen_event = false;
    for l in &lines {
        let Some((name, params, value)) = split_line(l) else { continue };
        match name.as_str() {
            "BEGIN" => {
                depth += 1;
                if value.eq_ignore_ascii_case("VEVENT") && !seen_event {
                    in_event = true;
                    seen_event = true;
                    depth = 1;
                } else if depth > 64 {
                    return None;
                }
                continue;
            }
            "END" => {
                if in_event && value.eq_ignore_ascii_case("VEVENT") {
                    in_event = false;
                }
                depth = depth.saturating_sub(1);
                continue;
            }
            "METHOD" if !in_event => {
                method = match value.trim().to_ascii_uppercase().as_str() {
                    "REQUEST" => Method::Request,
                    "CANCEL" => Method::Cancel,
                    "REPLY" => Method::Reply,
                    "PUBLISH" => Method::Publish,
                    "COUNTER" => Method::Counter,
                    _ => Method::Other,
                };
            }
            _ => {}
        }
        if !in_event || depth != 1 {
            continue; // skip VALARM and other nested components
        }
        match name.as_str() {
            "UID" => inv.uid = clean(value.trim(), 256),
            "SEQUENCE" => inv.sequence = value.trim().parse().unwrap_or(0),
            "SUMMARY" => inv.title = text_value(&value, 256),
            "LOCATION" => inv.location = text_value(&value, 256),
            "DTSTART" => inv.start = when(&value, &params),
            "DTEND" => inv.end = when(&value, &params),
            "STATUS" => inv.cancelled = value.trim().eq_ignore_ascii_case("CANCELLED"),
            "RRULE" => inv.repeats = true,
            "ORGANIZER" => inv.organizer = mailto(&value),
            "ATTENDEE" => {
                inv.attendees = inv.attendees.saturating_add(1);
                let a = mailto(&value);
                if me.iter().any(|m| *m == a) {
                    inv.my_status = Some(params.iter().find(|(k, _)| k == "PARTSTAT").map_or_else(|| "NEEDS-ACTION".into(), |(_, v)| clean(&v.to_ascii_uppercase(), 32)));
                }
            }
            _ => {}
        }
    }
    if !seen_event {
        return None;
    }
    inv.method = method;
    if method == Method::Cancel {
        inv.cancelled = true;
    }
    Some(inv)
}

/// Return the single scheduling scope. Only an unbounded weekly master or a
/// timed original occurrence can be represented by Calendar.
#[cfg(feature = "jail")]
pub(crate) fn reply_scope(text: &str) -> Option<Option<String>> {
    if text.len() > 4 << 20 {
        return None;
    }
    let mut stack = Vec::new();
    let mut events = 0;
    let mut scope = None;
    for line in unfold(text) {
        let (key, params, value) = split_line(&line)?;
        if key == "BEGIN" {
            if stack.len() >= 8 {
                return None;
            }
            if value.eq_ignore_ascii_case("VEVENT") {
                events += 1;
            }
            stack.push(value.to_ascii_uppercase());
        } else if key == "END" {
            if stack.pop()? != value.to_ascii_uppercase() {
                return None;
            }
        } else if stack.last().is_some_and(|s| s == "VEVENT") {
            match key.as_str() {
                "RRULE"
                    if scope.is_none()
                        && params.is_empty()
                        && value.eq_ignore_ascii_case("FREQ=WEEKLY") =>
                {
                    scope = Some("weekly".into())
                }
                "RECURRENCE-ID" if scope.is_none() => {
                    let mut tz = None;
                    for (key, v) in &params {
                        match key.as_str() {
                            "TZID" if tz.is_none() => tz = Some(v.as_str()),
                            "VALUE" if v.eq_ignore_ascii_case("DATE-TIME") => (),
                            _ => return None,
                        }
                    }
                    let stamp = value.strip_suffix('Z').unwrap_or(&value);
                    if stamp.len() != 15
                        || stamp.as_bytes()[8] != b'T'
                        || !stamp
                            .bytes()
                            .enumerate()
                            .all(|(i, c)| i == 8 || c.is_ascii_digit())
                    {
                        return None;
                    }
                    scope = Some(match tz {
                        Some(_) if stamp != value => return None,
                        Some("UTC") => format!("RECURRENCE-ID:{stamp}Z"),
                        Some(zone) => format!("RECURRENCE-ID;TZID={zone}:{stamp}"),
                        None => format!("RECURRENCE-ID:{value}"),
                    });
                }
                "RECURRENCE-ID" | "RRULE" | "RDATE" | "EXDATE" | "EXRULE" => return None,
                _ => (),
            }
        }
    }
    (events == 1 && stack.is_empty()).then_some(scope)
}

fn mailto(v: &str) -> String {
    let v = v.trim();
    let v = if v.get(..7).is_some_and(|s| s.eq_ignore_ascii_case("mailto:")) { &v[7..] } else { v };
    clean(v, 320).to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    const REQ: &str = "BEGIN:VCALENDAR\r\nPRODID:-//Example//EN\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VTIMEZONE\r\nTZID:Asia/Kolkata\r\nBEGIN:STANDARD\r\nDTSTART:19700101T000000\r\nTZOFFSETFROM:+0530\r\nTZOFFSETTO:+0530\r\nEND:STANDARD\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:abc-123@example.com\r\nSEQUENCE:2\r\nSUMMARY:Q4 review\\, board\r\n  deck\r\nDTSTART;TZID=Asia/Kolkata:20261117T100000\r\nDTEND:20261117T053000Z\r\nLOCATION:Room 4\\nFloor 2\r\nORGANIZER;CN=\"Arun, M\":mailto:Arun@Example.com\r\nATTENDEE;CN=Priya;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:priya@example.com\r\nATTENDEE;PARTSTAT=accepted:MAILTO:you@example.org\r\nRRULE:FREQ=WEEKLY\r\nBEGIN:VALARM\r\nSUMMARY:ignored\r\nTRIGGER:-PT15M\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn request_card() {
        let i = invite(REQ, &["you@example.org".into()]).unwrap();
        assert_eq!(i.method, Method::Request);
        assert_eq!(i.uid, "abc-123@example.com");
        assert_eq!(i.sequence, 2);
        assert_eq!(i.title, "Q4 review, board deck");
        assert_eq!(i.location, "Room 4 Floor 2");
        assert_eq!(i.start, Some(When::Zoned { local: "20261117T100000".into(), tzid: "Asia/Kolkata".into() }));
        assert_eq!(i.end, Some(When::Utc(1_794_893_400)));
        assert_eq!(i.organizer, "arun@example.com");
        assert_eq!(i.attendees, 2);
        assert_eq!(i.my_status.as_deref(), Some("ACCEPTED"));
        assert!(i.repeats && !i.cancelled);
    }

    #[test]
    fn cancel_and_junk() {
        let c = invite("BEGIN:VCALENDAR\nMETHOD:CANCEL\nBEGIN:VEVENT\nUID:x\nDTSTART;VALUE=DATE:20261201\nEND:VEVENT\nEND:VCALENDAR\n", &[]).unwrap();
        assert!(c.cancelled);
        assert_eq!(c.start, Some(When::Date("20261201".into())));
        assert_eq!(invite("not a calendar", &[]), None);
        let deep = "BEGIN:X\n".repeat(100);
        assert_eq!(invite(&deep, &[]), None);
    }
}
