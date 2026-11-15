//! The parse jail's side of the engine conversation (mail.md §4, §7.1):
//! read `ToJail` frames, parse, verify, convert, answer with bounded
//! `FromJail` frames. Everything the jail needs from outside — DKIM keys and
//! DMARC records — it asks the engine for with `Query`, and the engine has
//! mail-sync look them up; the jail itself has no network and no files.

use crate::{dkim, html, ical, mime, tnef, Limits, Refused};
use mail_schema as wire;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{self, Read, Write};

/// The jail's two ends, with the lookup counter for the current message.
struct Port<R, W> { r: R, w: W, next: u32, budget: u32 }

impl<R: Read, W: Write> Port<R, W> {
    fn send(&mut self, m: &wire::FromJail) -> io::Result<()> { wire::write_frame(&mut self.w, m) }

    /// One TXT lookup through the engine; `None` past the per-message budget.
    fn query(&mut self, name: String) -> io::Result<Option<String>> {
        if self.budget == 0 || !wire::query_allowed(&name) { return Ok(None) }
        self.budget -= 1;
        let q = self.next;
        self.next = self.next.wrapping_add(1);
        self.send(&wire::FromJail::Query { query: q, name })?;
        match wire::read_frame::<wire::ToJail>(&mut self.r)? {
            Some(wire::ToJail::Answer { query, txt }) if query == q => Ok(txt.filter(|t| t.len() <= 4096)),
            _ => Err(io::Error::new(io::ErrorKind::InvalidData, "expected the answer to the last query")),
        }
    }
}

/// Serve requests until the engine closes the stream.
pub fn serve(r: impl Read, w: impl Write, fonts: &html::FontSet) -> io::Result<()> {
    let port = RefCell::new(Port { r, w, next: 0, budget: 0 });
    loop {
        let req = { let mut p = port.borrow_mut(); wire::read_frame::<wire::ToJail>(&mut p.r)? };
        let Some(req) = req else { return Ok(()) };
        match req {
            wire::ToJail::Summarize { authserv_id, me, now, batch } => {
                if batch.len() > wire::MAX_BATCH || batch.iter().map(|m| m.bytes.len()).sum::<usize>() > wire::MAX_BATCH_BYTES {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "batch over its limits"));
                }
                let ctx = Ctx { authserv_id: authserv_id.as_deref(), me: &me, now };
                for m in &batch {
                    let result = summarize(&m.bytes, &ctx, &port).map(|(s, _)| s);
                    port.borrow_mut().send(&wire::FromJail::Summary { id: m.id, result })?;
                }
            }
            wire::ToJail::Open { authserv_id, me, now, message, loaded } => {
                let ctx = Ctx { authserv_id: authserv_id.as_deref(), me: &me, now };
                let loaded: BTreeMap<String, (u32, u32)> = loaded.into_iter().map(|(id, w, h)| (id, (w, h))).collect();
                let result = open(&message.bytes, &ctx, &port, &loaded, fonts);
                port.borrow_mut().send(&wire::FromJail::Opened { id: message.id, result })?;
            }
            wire::ToJail::Answer { .. } => return Err(io::Error::new(io::ErrorKind::InvalidData, "answer with no query")),
        }
        port.borrow_mut().send(&wire::FromJail::Done)?;
    }
}

struct Ctx<'a> { authserv_id: Option<&'a str>, me: &'a [String], now: i64 }

fn refusal(r: Refused) -> wire::Refusal {
    match r { Refused::Empty => wire::Refusal::NotMail, _ => wire::Refusal::TooLarge }
}

fn summarize<R: Read, W: Write>(raw: &[u8], ctx: &Ctx, port: &RefCell<Port<R, W>>) -> Result<(wire::Summary, crate::Summary), wire::Refusal> {
    let s = crate::parse(raw, &Limits::default()).map_err(refusal)?;
    port.borrow_mut().budget = wire::MAX_QUERIES_PER_MESSAGE;
    // A failed exchange with the engine is not this message's fault; the
    // engine sees the broken stream and retires the jail either way.
    let lookup = |name: String| port.borrow_mut().query(name).ok().flatten();

    let sigs = if s.dkim_signatures > 0 {
        dkim::verify(raw, &|sel, dom| lookup(format!("{sel}._domainkey.{dom}")), ctx.now)
    } else { Vec::new() };
    let from_domain = s.from.first().and_then(|a| a.addr.rsplit_once('@')).map(|(_, d)| d.to_ascii_lowercase()).unwrap_or_default();
    // DMARC only decides strict alignment, so it is looked up only when a
    // signature passed and could count.
    let dmarc = if sigs.iter().any(|g| g.outcome == dkim::Outcome::Pass) && !from_domain.is_empty() {
        let rec = |d: &str| lookup(format!("_dmarc.{d}")).filter(|t| t.trim_start().starts_with("v=DMARC1"));
        rec(&from_domain).or_else(|| {
            let org = dkim::org_domain(&from_domain);
            if org != from_domain { rec(&org) } else { None }
        })
    } else { None };
    let provider = ctx.authserv_id.and_then(|id| dkim::provider_result(raw, id));
    let sender_check = match dkim::verdict(&from_domain, &sigs, dmarc.as_deref(), provider.as_ref()) {
        dkim::Verdict::Verified { domain, by_provider } => wire::SenderCheck::Verified { domain, by_provider },
        dkim::Verdict::NotVerified => wire::SenderCheck::NotVerified,
        dkim::Verdict::Pending => wire::SenderCheck::Pending,
    };

    let invite = s.parts.iter().find(|p| p.kind == mime::PartKind::Calendar).and_then(|p| {
        let bytes = mime::part_bytes(raw, p);
        let (text, _) = crate::decode::to_utf8(&bytes, p.charset.as_deref());
        ical::invite(&text, ctx.me)
    }).map(invite_wire);

    let addr = |a: &crate::address::Address| wire::Address { name: a.name.clone(), addr: a.addr.clone(), name_looks_like_address: a.name_looks_like_address };
    let addrs = |l: &[crate::address::Address]| l.iter().map(addr).collect::<Vec<_>>();
    let out = wire::Summary {
        message_id: s.message_id.clone(),
        in_reply_to: s.in_reply_to.clone(),
        references: s.references.clone(),
        subject: s.subject.clone(),
        date: s.date,
        from: addrs(&s.from),
        sender: s.sender.as_ref().map(addr),
        reply_to: addrs(&s.reply_to),
        to: addrs(&s.to),
        cc: addrs(&s.cc),
        bcc: addrs(&s.bcc),
        list_id: s.list_id.clone(),
        list_unsubscribe: s.list_unsubscribe.iter().take(8).cloned().collect(),
        one_click_unsubscribe: s.one_click_unsubscribe,
        wants_receipt: s.wants_receipt,
        automated: s.automated,
        parts: s.parts.iter().map(|p| wire::Part {
            path: p.path.clone(),
            kind: match p.kind {
                mime::PartKind::Multipart => wire::PartKind::Multipart,
                mime::PartKind::Text => wire::PartKind::Text,
                mime::PartKind::Html => wire::PartKind::Html,
                mime::PartKind::Calendar => wire::PartKind::Calendar,
                mime::PartKind::Inline => wire::PartKind::Inline,
                mime::PartKind::Message => wire::PartKind::Message,
                mime::PartKind::Attachment => wire::PartKind::Attachment,
            },
            content_type: p.content_type.clone(),
            filename: p.filename.clone(),
            content_id: p.content_id.clone(),
            size: p.size as u64,
            body: (p.body.0 as u64, p.body.1 as u64),
        }).collect(),
        text: s.text.clone(),
        preview: s.preview.clone(),
        sender_check,
        invite,
        unknown_charset: s.unknown_charset,
        truncated: s.truncated,
    };
    Ok((out, s))
}

fn when(w: ical::When) -> wire::When {
    match w {
        ical::When::Utc(t) => wire::When::Utc(t),
        ical::When::Zoned { local, tzid } => wire::When::Zoned { local, tzid },
        ical::When::Floating(s) => wire::When::Floating(s),
        ical::When::Date(s) => wire::When::Date(s),
    }
}

fn invite_wire(i: ical::Invite) -> wire::Invite {
    wire::Invite {
        method: match i.method {
            ical::Method::Request => wire::InviteMethod::Request,
            ical::Method::Cancel => wire::InviteMethod::Cancel,
            ical::Method::Reply => wire::InviteMethod::Reply,
            ical::Method::Publish => wire::InviteMethod::Publish,
            ical::Method::Counter => wire::InviteMethod::Counter,
            ical::Method::Other => wire::InviteMethod::Other,
        },
        uid: i.uid, sequence: i.sequence, title: i.title,
        start: i.start.map(when), end: i.end.map(when),
        location: i.location, organizer: i.organizer, attendees: i.attendees,
        my_status: i.my_status, cancelled: i.cancelled, repeats: i.repeats,
    }
}

fn open<R: Read, W: Write>(raw: &[u8], ctx: &Ctx, port: &RefCell<Port<R, W>>, loaded: &BTreeMap<String, (u32, u32)>,
                           fonts: &html::FontSet) -> Result<wire::Opened, wire::Refusal> {
    let (summary, s) = summarize(raw, ctx, port)?;
    let document = s.html.as_deref().map(|h| {
        let c = html::convert(h, loaded, fonts, &html::Env::default());
        wire::Document {
            html: c.document,
            resources: c.resources.into_iter().map(|r| wire::Resource {
                id: r.id,
                source: match r.source {
                    html::Source::Cid(s) => wire::Source::Cid(s),
                    html::Source::Remote(s) => wire::Source::Remote(s),
                    html::Source::Data(s) => wire::Source::Data(s),
                },
                used_as: match r.used_as { html::Use::Image => wire::Use::Image, html::Use::Background => wire::Use::Background, html::Use::Other => wire::Use::Other },
                hidden: r.hidden,
            }).collect(),
            dropped: c.dropped.into_iter().take(256).map(|(k, v)| (k.chars().take(1024).collect(), v.min(u32::MAX as usize) as u32)).collect(),
        }
    });
    let mut unpacked = Vec::new();
    for p in &s.parts {
        let is_tnef = p.content_type.eq_ignore_ascii_case("application/ms-tnef")
            || p.filename.as_deref().is_some_and(|f| f.eq_ignore_ascii_case("winmail.dat"));
        if !is_tnef { continue }
        if let Ok(t) = tnef::unpack(&mime::part_bytes(raw, p), 64 - unpacked.len().min(64)) {
            unpacked.extend(t.attachments.into_iter().map(|a| wire::Unpacked { name: a.name, data: a.data }));
        }
    }
    unpacked.truncate(64);
    Ok(wire::Opened { summary, document, unpacked })
}
