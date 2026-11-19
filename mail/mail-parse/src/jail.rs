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
            wire::ToJail::Summarize { authserv_id, me, now, headers_only, roots, batch } => {
                if !roots_ok(&roots) || batch.len() > wire::MAX_BATCH || batch.iter().map(|m| m.bytes.len()).sum::<usize>() > wire::MAX_BATCH_BYTES {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "batch over its limits"));
                }
                let ctx = Ctx { authserv_id: authserv_id.as_deref(), me: &me, now, headers_only, roots: &roots, pgp_keys: &[] };
                for m in &batch {
                    let result = summarize(&m.bytes, &ctx, &port);
                    // Autocrypt (§10.2): a key for From's own address, before its summary.
                    if let Ok((_, s)) = &result {
                        let from = s.from.first().map(|a| a.addr.to_ascii_lowercase());
                        if let Some((addr, key)) = s.autocrypt.as_deref().and_then(mail_pgp::autocrypt) {
                            if Some(&addr) == from.as_ref() && key.len() <= wire::PGP_KEY_MAX {
                                port.borrow_mut().send(&wire::FromJail::Autocrypt { id: m.id, addr, key })?;
                            }
                        }
                    }
                    port.borrow_mut().send(&wire::FromJail::Summary { id: m.id, result: result.map(|(s, _)| s) })?;
                }
            }
            wire::ToJail::Open { authserv_id, me, now, roots, key, pgp_keys, message, loaded } => {
                if pgp_keys.len() > wire::PGP_KEYS_MAX || pgp_keys.iter().any(|k| k.len() > wire::PGP_KEY_MAX) {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "OpenPGP keys over their limits"));
                }
                let ctx = Ctx { authserv_id: authserv_id.as_deref(), me: &me, now, headers_only: false, roots: &roots, pgp_keys: &pgp_keys };
                if !roots_ok(&roots) || loaded.len() > wire::LOADED_MAX || loaded.iter().any(|(id, b)| b.len() > wire::LOADED_HEAD || id.len() > 64) {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "loaded pictures over their limits"));
                }
                // Their sizes from their headers; one that doesn't say keeps the declared space.
                let loaded: BTreeMap<String, (u32, u32)> = loaded
                    .into_iter()
                    .filter_map(|(id, b)| navigator_image::image::natural_size(&b).ok().map(|(_, w, h)| (id, (w, h))))
                    .collect();
                let result = match key.as_deref().map(|k| decrypted(&message.bytes, k)) {
                    None => open(&message.bytes, &ctx, &port, &loaded, fonts),
                    Some(Some(inner)) => open(&inner, &ctx, &port, &loaded, fonts).map(|mut o| {
                        o.summary.encrypted = true;
                        o
                    }),
                    Some(None) => Err(wire::Refusal::NotMail),
                };
                port.borrow_mut().send(&wire::FromJail::Opened { id: message.id, result })?;
            }
            wire::ToJail::HtmlAttachment { message, which, key } => {
                let inner = key.as_deref().map(|k| decrypted(&message.bytes, k));
                let raw = match &inner { None => Some(message.bytes.as_slice()), Some(bytes) => bytes.as_deref() };
                let result = raw.ok_or(wire::Refusal::NotMail).and_then(|raw| {
                    let s = crate::parse(raw, &Limits::default()).map_err(refusal)?;
                    let (bytes, charset) = attachment_bytes(raw, &s, &which)?;
                    if bytes.len() > 4 << 20 { return Err(wire::Refusal::TooLarge); }
                    let (html, _) = crate::decode::to_utf8(&bytes, charset.as_deref());
                    let document = document(raw, &s, &html, &BTreeMap::new(), fonts);
                    Ok(wire::Opened { document: Some(document), summary: Default::default(),
                        attachments: Vec::new(), card: None, pgp: None, pgp_encrypted: false })
                });
                port.borrow_mut().send(&wire::FromJail::Opened { id: message.id, result })?;
            }
            wire::ToJail::Attachment { message, which, key } => {
                let result = match key.as_deref().map(|k| decrypted(&message.bytes, k)) {
                    None => attachment(&message.bytes, &which),
                    Some(Some(inner)) => attachment(&inner, &which),
                    Some(None) => Err(wire::Refusal::NotMail),
                };
                port.borrow_mut().send(&wire::FromJail::Attachment { id: message.id, result })?;
            }
            wire::ToJail::KeyFor { message, certs } => {
                if certs.len() > wire::MY_CERTS_MAX {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "too many certificates"));
                }
                let which = envelope_of(&message.bytes).and_then(|env| {
                    certs.iter().enumerate().find_map(|(i, c)| mail_smime::envelope::wrapped_key(&env, c).filter(|w| w.len() <= wire::WRAPPED_KEY_MAX).map(|w| (i as u32, w)))
                });
                port.borrow_mut().send(&wire::FromJail::KeyFor { id: message.id, which })?;
            }
            wire::ToJail::Answer { .. } => return Err(io::Error::new(io::ErrorKind::InvalidData, "answer with no query")),
        }
        port.borrow_mut().send(&wire::FromJail::Done)?;
    }
}

/// The S/MIME envelope of an encrypted message (its top-level body).
fn envelope_of(raw: &[u8]) -> Option<mail_smime::envelope::EnvelopedData> {
    let s = crate::parse(raw, &Limits::default()).ok()?;
    if !s.encrypted { return None }
    let p = s.parts.iter().find(|p| p.path.is_empty())?;
    mail_smime::envelope::enveloped_data(&mime::part_bytes(raw, p)).ok()
}

/// The message inside an encrypted one: its outer header lines (but the
/// `Content-*` ones), then the decrypted entity.
fn decrypted(raw: &[u8], key: &[u8]) -> Option<Vec<u8>> {
    let env = envelope_of(raw)?;
    let inner = mail_smime::envelope::decrypt(&env, key).ok()?;
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 2).or_else(|| raw.windows(2).position(|w| w == b"\n\n").map(|p| p + 1))?;
    let mut out = Vec::with_capacity(end + inner.len());
    let mut skip = false;
    for line in raw[..end].split_inclusive(|b| *b == b'\n') {
        if line.first().is_some_and(|c| *c == b' ' || *c == b'\t') {
            if !skip { out.extend_from_slice(line) }
            continue;
        }
        skip = line.len() >= 8 && line[..8].eq_ignore_ascii_case(b"content-");
        if !skip { out.extend_from_slice(line) }
    }
    out.extend_from_slice(&inner);
    Some(out)
}

struct Ctx<'a> { authserv_id: Option<&'a str>, me: &'a [String], now: i64, headers_only: bool, roots: &'a [Vec<u8>], pgp_keys: &'a [Vec<u8>] }

fn roots_ok(roots: &[Vec<u8>]) -> bool {
    roots.len() <= wire::ROOTS_MAX && roots.iter().map(|r| r.len()).sum::<usize>() <= wire::ROOTS_BYTES_MAX
}

/// The message's S/MIME signature, checked (mail.md §10.1).
fn smime(raw: &[u8], s: &crate::Summary, ctx: &Ctx) -> Option<wire::Smime> {
    use mail_smime::verify;
    let signed = s.signed.as_ref()?;
    let part = s.parts.iter().find(|p| p.path == signed.signature)?;
    let sig = mime::part_bytes(raw, part);
    let content = &raw[signed.content.0..signed.content.1];
    let from = s.from.first().map(|a| a.addr.as_str()).unwrap_or("");
    let mut c = verify::detached(content, &sig, ctx.roots, ctx.now, from);
    // Signed as CRLF lines; a copy kept with bare LFs is checked as sent.
    if matches!(c.state, verify::State::Invalid(_)) && content.windows(2).any(|w| w[1] == b'\n' && w[0] != b'\r') {
        let mut crlf = Vec::with_capacity(content.len() + content.len() / 32);
        for (i, b) in content.iter().enumerate() {
            if *b == b'\n' && (i == 0 || content[i - 1] != b'\r') { crlf.push(b'\r') }
            crlf.push(*b);
        }
        let again = verify::detached(&crlf, &sig, ctx.roots, ctx.now, from);
        if !matches!(again.state, verify::State::Invalid(_)) { c = again }
    }
    let cut = |t: String| -> String { t.chars().filter(|c| !c.is_control()).take(200).collect() };
    let state = match c.state {
        verify::State::Valid => wire::SmimeState::Valid,
        verify::State::NotTrusted(w) => wire::SmimeState::NotTrusted(cut(w)),
        verify::State::Invalid(w) => wire::SmimeState::Invalid(cut(w)),
        verify::State::WrongAddress(l) => wire::SmimeState::WrongAddress(l.into_iter().take(16).map(cut).collect()),
    };
    let signer = match (c.signer, c.signer_cert) {
        (Some(i), Some(der)) if der.len() <= wire::SMIME_CERT_MAX => Some(wire::SmimeCert { name: cut(i.name), emails: i.emails.into_iter().take(16).map(cut).collect(), issuer: cut(i.issuer), not_before: i.not_before, not_after: i.not_after, sha256: i.sha256, der }),
        _ => None,
    };
    let state = if signer.is_none() && !matches!(state, wire::SmimeState::Invalid(_)) { wire::SmimeState::Invalid("the signer's certificate is too large".into()) } else { state };
    Some(wire::Smime { state, signer, issuers: c.chain.into_iter().take(8).map(|i| cut(i.name)).collect(), signed_at: c.signed_at })
}

/// The message's PGP/MIME signature, checked against the sender's keys
/// the engine sent (mail.md §10.2).
fn pgp(raw: &[u8], s: &crate::Summary, ctx: &Ctx) -> Option<wire::Pgp> {
    use mail_pgp::State;
    let signed = s.pgp_signed.as_ref()?;
    let part = s.parts.iter().find(|p| p.path == signed.signature)?;
    let sig = mime::part_bytes(raw, part);
    let content = &raw[signed.content.0..signed.content.1];
    let from = s.from.first().map(|a| a.addr.as_str()).unwrap_or("");
    let mut c = mail_pgp::verify(content, &sig, ctx.pgp_keys, from);
    // Signed as CRLF lines; a copy kept with bare LFs is checked as sent.
    if matches!(c.state, State::Invalid(_)) && content.windows(2).any(|w| w[1] == b'\n' && w[0] != b'\r') {
        let mut crlf = Vec::with_capacity(content.len() + content.len() / 32);
        for (i, b) in content.iter().enumerate() {
            if *b == b'\n' && (i == 0 || content[i - 1] != b'\r') { crlf.push(b'\r') }
            crlf.push(*b);
        }
        let again = mail_pgp::verify(&crlf, &sig, ctx.pgp_keys, from);
        if !matches!(again.state, State::Invalid(_)) { c = again }
    }
    let cut = |t: String| -> String { t.chars().filter(|c| !c.is_control()).take(200).collect() };
    let state = match c.state {
        State::Valid => wire::PgpState::Valid,
        State::NotForFrom => wire::PgpState::NotForFrom,
        State::NoKey => wire::PgpState::NoKey,
        State::Invalid(w) => wire::PgpState::Invalid(cut(w)),
    };
    Some(wire::Pgp { state, fingerprint: c.signer.as_ref().map(|i| i.fingerprint.clone()), emails: c.signer.map(|i| i.emails.into_iter().take(16).map(cut).collect()).unwrap_or_default() })
}

fn refusal(r: Refused) -> wire::Refusal {
    match r { Refused::Empty => wire::Refusal::NotMail, _ => wire::Refusal::TooLarge }
}

fn summarize<R: Read, W: Write>(raw: &[u8], ctx: &Ctx, port: &RefCell<Port<R, W>>) -> Result<(wire::Summary, crate::Summary), wire::Refusal> {
    let s = crate::parse(raw, &Limits::default()).map_err(refusal)?;
    port.borrow_mut().budget = wire::MAX_QUERIES_PER_MESSAGE;
    // A failed exchange with the engine is not this message's fault; the
    // engine sees the broken stream and retires the jail either way.
    let lookup = |name: String| port.borrow_mut().query(name).ok().flatten();

    let sigs = if s.dkim_signatures > 0 && !ctx.headers_only {
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
        // Signed, but the body isn't here yet: checked when it is.
        dkim::Verdict::NotVerified if ctx.headers_only && s.dkim_signatures > 0 => wire::SenderCheck::Pending,
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
                mime::PartKind::Signature => wire::PartKind::Signature,
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
        smime: if ctx.headers_only { None } else { smime(raw, &s, ctx) },
        encrypted: s.encrypted,
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

/// The bytes of the pictures the message carries itself: `cid:` parts and
/// `data:` URLs, within the schema's limits (the rest stay placeholders).
fn own_pictures(raw: &[u8], s: &crate::Summary, resources: &[html::Resource]) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut total = 0;
    for r in resources {
        if out.len() >= wire::PICTURES_MAX { break }
        if r.hidden { continue }
        let bytes = match &r.source {
            html::Source::Cid(cid) => s.parts.iter().find(|p| p.content_id.as_deref() == Some(cid.as_str()) && p.kind != mime::PartKind::Multipart)
                .map(|p| mime::part_bytes(raw, p)),
            html::Source::Data(d) => html::data_url(d, wire::PICTURE_BYTES_MAX),
            html::Source::Remote(_) => None,
        };
        let Some(b) = bytes else { continue };
        if b.is_empty() || b.len() > wire::PICTURE_BYTES_MAX || b.len() as u64 > raw.len() as u64 || total + b.len() > wire::PICTURES_BYTES_MAX { continue }
        total += b.len();
        out.push((r.id.clone(), b));
    }
    out
}

fn document(raw: &[u8], s: &crate::Summary, h: &str, loaded: &BTreeMap<String, (u32, u32)>, fonts: &html::FontSet) -> wire::Document {
    let env = html::Env::default();
    let mut c = html::convert(h, loaded, fonts, &env);
    let pictures = own_pictures(raw, s, &c.resources);
    // Their real sizes, from their headers, so the space is right the
    // first time: converted again only when one wasn't declared.
    let mut sized = loaded.clone();
    for (id, b) in &pictures {
        if !sized.contains_key(id) {
            if let Ok((_, w, h)) = navigator_image::image::natural_size(b) {
                sized.insert(id.clone(), (w, h));
            }
        }
    }
    if sized.len() > loaded.len() {
        c = html::convert(h, &sized, fonts, &env);
    }
    wire::Document {
        pictures,
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
        links: c.links,
        dropped: c.dropped.into_iter().take(256).map(|(k, v)| (k.chars().take(1024).collect(), v.min(u32::MAX as usize) as u32)).collect(),
    }
}

fn open<R: Read, W: Write>(raw: &[u8], ctx: &Ctx, port: &RefCell<Port<R, W>>, loaded: &BTreeMap<String, (u32, u32)>,
                           fonts: &html::FontSet) -> Result<wire::Opened, wire::Refusal> {
    let (summary, s) = summarize(raw, ctx, port)?;
    let document = s.html.as_deref().map(|h| document(raw, &s, h, loaded, fonts));
    let shown: Vec<&str> = document.as_ref().map_or(Vec::new(), |d| d.resources.iter().filter_map(|r| match &r.source {
        wire::Source::Cid(c) if !r.hidden => Some(c.as_str()),
        _ => None,
    }).collect());
    let attachments = attachments(raw, &s, &shown);
    let card = s.html.as_deref().and_then(crate::cards::card);
    let pgp = pgp(raw, &s, ctx);
    Ok(wire::Opened { summary, document, attachments, card, pgp, pgp_encrypted: s.pgp_encrypted })
}

/// The most files unpacked from a message's `winmail.dat` parts.
const UNPACKED_MAX: usize = 64;

fn is_tnef(p: &mime::Part) -> bool {
    p.content_type.eq_ignore_ascii_case("application/ms-tnef") || p.filename.as_deref().is_some_and(|f| f.eq_ignore_ascii_case("winmail.dat"))
}

/// Every file unpacked from the message's `winmail.dat` parts, in order:
/// (the part it came from, the file).
fn unpacked(raw: &[u8], s: &crate::Summary) -> Vec<(usize, tnef::Attachment)> {
    let mut out = Vec::new();
    for (i, p) in s.parts.iter().enumerate().filter(|(_, p)| is_tnef(p)) {
        if let Ok(t) = tnef::unpack(&mime::part_bytes(raw, p), UNPACKED_MAX - out.len().min(UNPACKED_MAX)) {
            out.extend(t.attachments.into_iter().map(|a| (i, a)));
        }
    }
    out.truncate(UNPACKED_MAX);
    out
}

fn head(b: &[u8]) -> Vec<u8> { b[..b.len().min(wire::ATTACHMENT_HEAD)].to_vec() }

/// What the strip shows (§8.1): attached files, forwarded messages, and
/// pictures the HTML doesn't show; a `winmail.dat` that unpacks is
/// replaced by its files.
fn attachments(raw: &[u8], s: &crate::Summary, shown: &[&str]) -> Vec<wire::Attachment> {
    let files = unpacked(raw, s);
    let mut out = Vec::new();
    for (i, p) in s.parts.iter().enumerate() {
        let listed = match p.kind {
            mime::PartKind::Attachment | mime::PartKind::Message => true,
            // A picture for the HTML is shown there; one no HTML shows is a file.
            mime::PartKind::Inline => !p.content_id.as_deref().is_some_and(|c| shown.contains(&c)),
            _ => false,
        };
        if !listed { continue }
        if is_tnef(p) && files.iter().any(|(at, _)| *at == i) {
            for (n, (_, f)) in files.iter().enumerate().filter(|(_, (at, _))| *at == i) {
                let name = mime::sanitize_filename(&f.name);
                out.push(wire::Attachment {
                    which: wire::Which::Unpacked(n as u32),
                    name: if name.is_empty() { "attachment".into() } else { name },
                    content_type: "application/octet-stream".into(),
                    size: f.data.len() as u64,
                    head: head(&f.data),
                });
            }
            continue;
        }
        let bytes = mime::part_bytes(raw, p);
        let fallback = if p.kind == mime::PartKind::Message { "message.eml" } else { "attachment" };
        out.push(wire::Attachment {
            which: wire::Which::Part(p.path.clone()),
            name: p.filename.clone().filter(|f| !f.is_empty() && f != "..").unwrap_or_else(|| fallback.into()),
            content_type: p.content_type.clone(),
            size: bytes.len() as u64,
            head: head(&bytes),
        });
    }
    out
}

/// One attachment's bytes, and its text for search.
fn attachment(raw: &[u8], which: &wire::Which) -> Result<wire::Attached, wire::Refusal> {
    let s = crate::parse(raw, &Limits::default()).map_err(refusal)?;
    let (bytes, charset) = attachment_bytes(raw, &s, which)?;
    let text = text_of(&bytes, charset.as_deref());
    Ok(wire::Attached { bytes, text })
}

fn attachment_bytes(raw: &[u8], s: &crate::Summary, which: &wire::Which) -> Result<(Vec<u8>, Option<String>), wire::Refusal> {
    Ok(match which {
        wire::Which::Part(path) => {
            let p = s.parts.iter().find(|p| p.path == *path && p.kind != mime::PartKind::Multipart).ok_or(wire::Refusal::NotMail)?;
            (mime::part_bytes(raw, p), p.charset.clone())
        }
        wire::Which::Unpacked(n) => (unpacked(raw, s).into_iter().nth(*n as usize).ok_or(wire::Refusal::NotMail)?.1.data, None),
    })
}

/// An attachment's text, when it is text (UTF-8 or its declared charset,
/// no NULs); cut to the schema's limit at a character.
fn text_of(bytes: &[u8], charset: Option<&str>) -> Option<String> {
    let probe = &bytes[..bytes.len().min(wire::ATTACHMENT_HEAD)];
    if probe.contains(&0) { return None }
    if charset.is_none() && std::str::from_utf8(probe).is_err_and(|e| e.error_len().is_some()) { return None }
    let (mut t, _) = crate::decode::to_utf8(bytes, charset);
    // Text as atrium-types counts it: no control characters but layout ones.
    if t.chars().take(wire::ATTACHMENT_HEAD).any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r' | '\x0c' | '\x1b')) { return None }
    if t.len() > wire::ATTACHMENT_TEXT_MAX {
        let mut at = wire::ATTACHMENT_TEXT_MAX;
        while !t.is_char_boundary(at) { at -= 1 }
        t.truncate(at);
    }
    Some(t)
}
