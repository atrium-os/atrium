//! sign-in-probe: drive a VPN sign-in window's navigatord as its UI and page
//! child would, without a screen (vpn.md §9.3; scripts/crypta-v4-gate.sh).
//!
//!   sign-in-probe <ctl socket> <page socket> <start url> <final url> <username> <arca handle hex>
//!
//! Opens the start, reads each page's first form as the page child does
//! (`navigator_page::forms`), and sends it as the form panel would on a
//! press: the username typed, the password as Arca's handle (what was typed
//! into Arca's field, held by arca-ui), every other field as the page gave
//! it. Then opens the page the answer leads to, until the final address.
//! Prints one line per step, and `final <url>` or `stuck <why>`.

use std::os::unix::net::UnixStream;
use std::time::Duration;

use limen::doc_viewer::FieldKind;
use navigator_control::{read_frame, write_frame, Event, FormField, Jar, LinkAction, Request};

fn hex(s: &str) -> Option<[u8; 16]> {
    let b: Vec<u8> = (0..s.len()).step_by(2).filter_map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect();
    b.try_into().ok()
}

fn stuck(why: impl std::fmt::Display) -> ! {
    println!("stuck {why}");
    std::process::exit(1)
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() != 6 {
        stuck("usage: sign-in-probe <ctl> <page> <start> <final> <username> <arca handle hex>");
    }
    let (page_sock, start, fin, user) = (&a[1], &a[2], &a[3], &a[4]);
    let mut arca = Some(hex(&a[5]).unwrap_or_else(|| stuck("the handle is 32 hex digits")));
    let mut s = UnixStream::connect(&a[0]).unwrap_or_else(|e| stuck(format!("navigatord: {e}")));
    s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
    write_frame(&mut s, &Request::Hello { chrome: false, limen: false, measure: false, trails: false }).unwrap();
    let _ = read_frame::<Event>(&mut s);
    let jar = Jar::Personal;
    let mut url = start.clone();
    for step in 1..=8u32 {
        let id = step * 2;
        // After a form, the answer navigatord kept (as the window takes it):
        // fetched again it would be the form sent twice.
        write_frame(&mut s, &Request::Open { id, url: url.clone(), saved: None, jar, knows: true, fresh: step == 1 }).unwrap();
        let (page, ticket) = loop {
            match read_frame::<Event>(&mut s) {
                Ok(Some(Event::Opened { id: got, url, ticket, .. })) if got == id => break (url, ticket),
                Ok(Some(Event::Failed { id: got, why, .. })) if got == id => stuck(format!("{url}: {why}")),
                Ok(Some(_)) => {}
                other => stuck(format!("navigatord: {other:?}")),
            }
        };
        println!("step {step} opened {page}");
        let ends = url::Url::parse(&page).ok().zip(url::Url::parse(fin).ok())
            .is_some_and(|(p, f)| p.host_str() == f.host_str() && p.port_or_known_default() == f.port_or_known_default() && p.path() == f.path());
        if ends {
            // SIGN_IN_PROBE_DUMP: the final page as the page child gets it,
            // for a gate to look into (what the page must never have).
            if let Ok(path) = std::env::var("SIGN_IN_PROBE_DUMP") {
                let got = navigator_control::redeem(page_sock, &ticket).unwrap_or_else(|e| stuck(format!("the page socket: {e}")));
                std::fs::write(&path, &got.recording).unwrap_or_else(|e| stuck(format!("{path}: {e}")));
            }
            println!("final {page}");
            return;
        }
        let got = navigator_control::redeem(page_sock, &ticket).unwrap_or_else(|e| stuck(format!("the page socket: {e}")));
        // The page child's reading: the recording's document, then its forms.
        let text = String::from_utf8_lossy(&got.recording);
        let doc = navigator_normalize::recording::parse(&text).map(|r| r.document).unwrap_or_else(|_| stuck(format!("no recording for {page}")));
        let Some(form) = navigator_page::forms(&doc).into_iter().next() else { stuck(format!("no form on {page}")) };
        let mut fields = Vec::new();
        let mut pressed = false;
        for f in &form.fields {
            if f.name.is_empty() {
                continue;
            }
            match f.kind {
                FieldKind::Text => fields.push(FormField { name: f.name.clone(), value: user.clone(), typed: true, arca: None }),
                FieldKind::Password => fields.push(FormField { name: f.name.clone(), value: String::new(), typed: true, arca: arca.take() }),
                FieldKind::Fixed => fields.push(FormField { name: f.name.clone(), value: f.value.clone(), typed: false, arca: None }),
                FieldKind::Checkbox | FieldKind::Radio if f.checked => fields.push(FormField { name: f.name.clone(), value: f.value.clone(), typed: false, arca: None }),
                FieldKind::Submit if !pressed => {
                    pressed = true;
                    fields.push(FormField { name: f.name.clone(), value: f.value.clone(), typed: false, arca: None });
                }
                _ => {}
            }
        }
        let shown: Vec<String> = fields.iter().map(|f| match (f.arca, f.typed) {
            (Some(_), _) => format!("{}=<arca>", f.name),
            (None, true) => format!("{}=<typed>", f.name),
            (None, false) => format!("{}=<{} bytes>", f.name, f.value.len()),
        }).collect();
        println!("step {step} form {} {} [{}]", if form.post { "POST" } else { "GET" }, form.action.split('?').next().unwrap_or(""), shown.join(" "));
        write_frame(&mut s, &Request::SubmitForm { id: id + 1, page: page.clone(), jar, action: form.action.clone(), post: form.post, fields, press: [0; 16] }).unwrap();
        url = loop {
            match read_frame::<Event>(&mut s) {
                Ok(Some(Event::Link { id: got, action: LinkAction::Navigate { url, .. } })) if got == id + 1 => break url,
                Ok(Some(Event::Link { id: got, action })) if got == id + 1 => stuck(format!("the form: {action:?}")),
                Ok(Some(_)) => {}
                other => stuck(format!("navigatord: {other:?}")),
            }
        };
    }
    stuck("more than 8 pages")
}
