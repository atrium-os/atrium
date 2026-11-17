//! R0 cross-check against an independent IPP printer (printing.md §11).
//!
//! Usage: `oracle <host:port> <pdf|raster>`. Talks plain IPP over HTTP/1.1 to a
//! test printer (CUPS's `ippeveprinter`, used only as a test oracle, never
//! shipped; `scripts/prelum-r0-oracle.sh` starts it), using only this crate
//! and `prelum-raster`:
//!
//! - `Get-Printer-Attributes` decoded into `PrinterInfo`;
//! - `Validate-Job`, `Identify-Printer`;
//! - `raster`: a 2-page PWG Raster job and a 1-page URF job by `Print-Job`,
//!   which the printer's command decodes with its own raster reader, so a
//!   completed job means an independent decoder accepted our stream;
//! - `pdf`: `Create-Job` + `Send-Document`, and `Cancel-Job` on a second job.
//!
//! Prints one line per check and exits non-zero on the first failure.

use prelum_ipp_codec::ops::{self, ColorMode, Format, JobIdentity, Sides, Ticket};
use prelum_ipp_codec::printer::{PrinterInfo, PrinterState};
use prelum_ipp_codec::{tag, Limits, Message, Value};
use prelum_raster::{ColorSpace, Duplex, PageHeader, PwgWriter, UrfWriter};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::exit;
use std::time::{Duration, Instant};

fn post(addr: &str, body: &[u8]) -> Message {
    let mut s = TcpStream::connect(addr).unwrap_or_else(|e| fail(&format!("connect {addr}: {e}")));
    s.set_read_timeout(Some(Duration::from_secs(20))).ok();
    let head = format!(
        "POST /ipp/print HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/ipp\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    let mut r = BufReader::new(s);
    let mut status = String::new();
    r.read_line(&mut status).unwrap();
    if !status.contains(" 200 ") {
        fail(&format!("HTTP status {}", status.trim()));
    }
    let mut len: Option<usize> = None;
    let mut chunked = false;
    loop {
        let mut l = String::new();
        r.read_line(&mut l).unwrap();
        let l = l.trim_end();
        if l.is_empty() {
            break;
        }
        let lower = l.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            len = v.trim().parse().ok();
        }
        if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
            chunked = true;
        }
    }
    let mut body = Vec::new();
    if chunked {
        loop {
            let mut l = String::new();
            r.read_line(&mut l).unwrap();
            let n = usize::from_str_radix(l.trim().split(';').next().unwrap_or(""), 16).unwrap_or(0);
            if n == 0 {
                break;
            }
            let mut c = vec![0; n + 2];
            r.read_exact(&mut c).unwrap();
            body.extend_from_slice(&c[..n]);
        }
    } else if let Some(n) = len {
        body.resize(n, 0);
        r.read_exact(&mut body).unwrap();
    } else {
        r.read_to_end(&mut body).unwrap();
    }
    match Message::decode(&body, &Limits::default()) {
        Ok((m, _)) => m,
        Err(e) => fail(&format!("decode response: {e}")),
    }
}

fn fail(why: &str) -> ! {
    println!("FAIL {why}");
    exit(1)
}

fn ok(what: &str) {
    println!("ok   {what}");
}

fn expect_ok(m: &Message, what: &str) {
    if m.code != 0 {
        let msg = m.attribute(tag::OPERATION, "status-message").and_then(|a| a.first()).and_then(Value::as_str).unwrap_or("");
        if !m.is_success() {
            fail(&format!("{what}: status {:#06x} {msg}", m.code));
        }
        fail(&format!("{what}: printer substituted or ignored something ({:#06x} {msg})", m.code));
    }
    if !m.is_success() {
        fail(&format!("{what}: status {:#06x}", m.code));
    }
    ok(what);
}

fn job_id(m: &Message) -> i32 {
    m.attribute(tag::JOB, "job-id").and_then(|a| a.first()).and_then(Value::as_int).unwrap_or_else(|| fail("no job-id"))
}

fn wait_done(addr: &str, uri: &str, job: i32, what: &str) {
    let start = Instant::now();
    loop {
        let r = post(addr, &ops::get_job_attributes(90, uri, job, "atrium").encode());
        let state = r.attribute(tag::JOB, "job-state").and_then(|a| a.first()).and_then(Value::as_int).unwrap_or(0);
        match state {
            9 => return ok(&format!("{what}: job {job} completed")),
            7 | 8 => {
                let reasons: Vec<String> = r
                    .attribute(tag::JOB, "job-state-reasons")
                    .map(|a| a.values.iter().filter_map(Value::as_str).map(str::to_string).collect())
                    .unwrap_or_default();
                fail(&format!("{what}: job {job} ended in state {state} {reasons:?}"))
            }
            _ if start.elapsed() > Duration::from_secs(60) => fail(&format!("{what}: job {job} still in state {state}")),
            _ => std::thread::sleep(Duration::from_millis(300)),
        }
    }
}

fn test_page(w: u32, h: u32, color: ColorSpace, page: u32) -> Vec<u8> {
    let bpp = color.bytes_per_pixel();
    let mut v = vec![0xFF; (w * h) as usize * bpp];
    for y in 0..h as usize {
        for x in 0..w as usize {
            // a frame, a diagonal and a band whose position depends on the page
            let ink = x < 8 || y < 8 || x + 8 >= w as usize || y + 8 >= h as usize || x * h as usize / w as usize == y || (y / 40) as u32 == page + 2;
            if ink {
                for k in 0..bpp {
                    v[(y * w as usize + x) * bpp + k] = (k as u8) * 60;
                }
            }
        }
    }
    v
}

fn header(w: u32, h: u32, dpi: u32, color: ColorSpace, pages: u32) -> PageHeader {
    PageHeader {
        width: w,
        height: h,
        dpi: (dpi, dpi),
        color,
        duplex: Duplex::Simplex,
        quality: 4,
        media_name: "na_letter_8.5x11in".into(),
        media_type: "stationery".into(),
        total_pages: pages,
        transform: (1, 1),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: oracle <host:port> <pdf|raster>");
        exit(2);
    }
    let addr = &args[1];
    let uri = format!("ipp://{addr}/ipp/print");
    let id = JobIdentity { user: "atrium".into(), job_name: "Prelum R0".into(), job_password: None };

    let r = post(addr, &ops::get_printer_attributes(1, &uri, &["all", "media-col-database"]).encode());
    expect_ok(&r, "Get-Printer-Attributes");
    let p = PrinterInfo::from_response(&r);
    println!(
        "     model={:?} uuid={:?} state={:?} formats={:?} pwg={:?} urf={:?} sides={:?} ops={}",
        p.make_and_model, p.uuid, p.state, p.formats, p.pwg_resolutions, p.urf, p.sides_supported, p.operations.len()
    );
    if p.uuid.is_none() {
        fail("printer-uuid missing or not accepted");
    }
    if p.state.is_none() || p.state == Some(PrinterState::Unknown) {
        fail("printer-state");
    }
    if !p.supports(ops::op::CREATE_JOB) {
        fail("operations-supported lacks Create-Job");
    }

    if p.supports(ops::op::IDENTIFY_PRINTER) {
        let r = post(addr, &ops::identify_printer(2, &uri, &[]).encode());
        expect_ok(&r, "Identify-Printer");
    }

    let media = p.media_default.clone();
    match args[2].as_str() {
        "raster" => {
            let dpi = *p.pwg_resolutions.first().map(|(x, _)| x).unwrap_or_else(|| fail("no PWG resolution"));
            let (w, h) = (dpi as u32 * 17 / 2, dpi as u32 * 11);
            let t = Ticket { color: ColorMode::Color, media: media.clone(), resolution: p.ticket_resolution((dpi, dpi)), ..Ticket::default() };
            expect_ok(&post(addr, &ops::validate_job(3, &uri, &id, Format::PwgRaster, &t).encode()), "Validate-Job (PWG Raster)");

            let mut wr = PwgWriter::new(ops::print_job(4, &uri, &id, Format::PwgRaster, &t).encode()).unwrap();
            for page in 0..2 {
                wr.begin_page(&header(w, h, dpi as u32, ColorSpace::SRgb8, 2)).unwrap();
                for row in test_page(w, h, ColorSpace::SRgb8, page).chunks(w as usize * 3) {
                    wr.write_row(row).unwrap();
                }
                wr.end_page().unwrap();
            }
            let body = wr.finish().unwrap();
            println!("     PWG Raster request {} bytes", body.len());
            let r = post(addr, &body);
            expect_ok(&r, "Print-Job (PWG Raster, 2 pages sRGB)");
            wait_done(addr, &uri, job_id(&r), "PWG Raster");

            let urf_dpi = p.urf_resolutions().next().unwrap_or_else(|| fail("no URF resolution")) as u32;
            let (w, h) = (urf_dpi * 17 / 2, urf_dpi * 11);
            let t = Ticket { color: ColorMode::Monochrome, sides: Sides::OneSided, media, resolution: p.ticket_resolution((urf_dpi as i32, urf_dpi as i32)), ..Ticket::default() };
            let mut wr = UrfWriter::new(ops::print_job(5, &uri, &id, Format::Urf, &t).encode(), 1).unwrap();
            wr.begin_page(&header(w, h, urf_dpi, ColorSpace::SGray8, 1)).unwrap();
            for row in test_page(w, h, ColorSpace::SGray8, 0).chunks(w as usize) {
                wr.write_row(row).unwrap();
            }
            wr.end_page().unwrap();
            let r = post(addr, &wr.finish().unwrap());
            expect_ok(&r, "Print-Job (URF, 1 page sGray)");
            wait_done(addr, &uri, job_id(&r), "URF");
        }
        "pdf" => {
            let t = Ticket { copies: 2, sides: Sides::TwoSidedLongEdge, media, ..Ticket::default() };
            let r = post(addr, &ops::create_job(6, &uri, &id, Format::Pdf, &t).encode());
            expect_ok(&r, "Create-Job (PDF, 2 copies, two-sided)");
            let job = job_id(&r);
            let mut body = ops::send_document(7, &uri, job, "atrium", Format::Pdf, true).encode();
            body.extend_from_slice(include_bytes!("min.pdf"));
            expect_ok(&post(addr, &body), "Send-Document");
            wait_done(addr, &uri, job, "PDF");

            let r = post(addr, &ops::create_job(8, &uri, &id, Format::Pdf, &Ticket::default()).encode());
            let job = job_id(&r);
            expect_ok(&post(addr, &ops::cancel_job(9, &uri, job, "atrium").encode()), "Cancel-Job");
        }
        _ => fail("mode"),
    }
    println!("PASS");
}
