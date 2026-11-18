//! prelumctl: prelumd from the console (docs/spec/printing.md §6.4).
//!
//! ```text
//! prelumctl [--socket PATH] printers
//! prelumctl nearby [--wait SECONDS]       printers on the LAN (mDNS), confirmed over IPP
//! prelumctl add URI [NAME]                 ipps://host[:port]/path, ipp://…, or a host
//! prelumctl remove|repin|identify|refresh PRINTER
//! prelumctl rename PRINTER NAME
//! prelumctl print FILE.pdf --printer PRINTER [--copies N] [--sides one|long|short]
//!     [--gray] [--media KEYWORD] [--quality draft|normal|high] [--pages 1-3]
//!     [--per-sheet 1|2|4] [--scale fit|shrink|PERCENT] [--title T] [--wait]
//! prelumctl jobs
//! prelumctl cancel|release JOB
//! prelumctl preview FILE.pdf OUT_DIR [--media KEYWORD] [--pages 1-3] [--per-sheet N]
//!     [--first N] [--count N] [--longest PX]  sheets as OUT_DIR/sheet-N.ppm
//! prelumctl save-pdf FILE.pdf OUT.pdf [--media KEYWORD] [--pages 1-3] [--per-sheet N]
//! ```
//!
//! `--for APP_UID` (before the command) asks prelumd to act for that app's
//! person first, as prelum-sheet does; only prelum-sheet's uid may.
//!
//! PRINTER is a printer's number or name. `print --wait` follows the job
//! and exits 0 only when it is done.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use prelum_ipp_codec::ops::{ColorMode, Quality, Sides};
use prelum_proto::print::{client::Client, Add, Layout, Phase, PreviewReq, Print, Printer, Rename, Scale};

const SOCKET: &str = "/atrium/sockets/print/print.sock";

fn die(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("prelumctl: {msg}");
    ExitCode::from(1)
}

fn show(p: &Printer) {
    let state = match (&p.changed, p.elsewhere, p.state) {
        (Some(_), _, _) => "IDENTITY CHANGED".to_string(),
        (_, true, _) => "not on this network".to_string(),
        (_, _, None) => "offline".to_string(),
        (_, _, Some(s)) => format!("{s:?}").to_lowercase(),
    };
    println!("{:>3}  {}  [{}]{}", p.id, p.name, state, if p.new { " new" } else { "" });
    println!("     {}  {}", p.uri, if p.tls { "encrypted" } else { "NOT ENCRYPTED" });
    if let Some(f) = &p.fingerprint {
        println!("     key {f}");
    }
    if let Some(u) = &p.uuid {
        println!("     uuid {u}");
    }
    if !p.make_and_model.is_empty() {
        println!("     {}", p.make_and_model);
    }
    if let Some(c) = &p.changed {
        println!("     now shows key {} uuid {}", c.fingerprint.as_deref().unwrap_or("-"), c.uuid.as_deref().unwrap_or("-"));
    }
    for r in &p.reasons {
        println!("     {:?} ({:?})", r.reason, r.severity);
    }
}

fn find(c: &mut Client, which: &str) -> Result<u32, String> {
    if let Ok(n) = which.parse() {
        return Ok(n);
    }
    let ps = c.printers().map_err(|e| e.to_string())?;
    ps.iter().find(|p| p.name == which).map(|p| p.id).ok_or_else(|| format!("no printer {which}"))
}

fn main() -> ExitCode {
    let mut a: Vec<String> = std::env::args().skip(1).collect();
    let mut socket = PathBuf::from(SOCKET);
    if a.first().map(String::as_str) == Some("--socket") && a.len() > 1 {
        socket = a[1].clone().into();
        a.drain(..2);
    }
    let mut act_for: Option<u32> = None;
    if a.first().map(String::as_str) == Some("--for") && a.len() > 1 {
        act_for = a[1].parse().ok();
        a.drain(..2);
    }
    let Some(cmd) = a.first().cloned() else {
        return die("usage: prelumctl printers|nearby|add|remove|rename|repin|identify|refresh|print|jobs|cancel|release …");
    };
    let mut c = match Client::connect(&socket) {
        Ok(c) => c,
        Err(e) => return die(format!("{}: {e}", socket.display())),
    };
    if let Some(app) = act_for {
        match c.act_for(app) {
            Ok(persona) => eprintln!("prelumctl: acting for app uid {app} (persona {:?})", if persona.is_empty() { "Personal" } else { &persona }),
            Err(e) => return die(e),
        }
    }
    let arg = |i: usize| a.get(i).cloned().unwrap_or_default();
    let r: Result<(), String> = (|| match cmd.as_str() {
        "printers" => {
            for p in c.printers().map_err(|e| e.to_string())? {
                show(&p);
            }
            Ok(())
        }
        "nearby" => {
            let secs = match arg(1).as_str() {
                "" => 4,
                "--wait" => arg(2).parse::<u8>().map_err(|_| "--wait SECONDS (at most 10)".to_string())?,
                x => return Err(format!("nearby [--wait SECONDS], not {x}")),
            };
            let list = c.nearby(secs).map_err(|e| e.to_string())?;
            if list.is_empty() {
                println!("no printers found");
            }
            for n in list {
                let saved = n.saved.map(|i| format!("saved as {i}")).unwrap_or_else(|| "not saved".into());
                println!("{}  [{}]  {}", n.uri, if n.tls { "encrypted" } else { "NOT ENCRYPTED" }, saved);
                println!("    name {}  (announced as {:?})", n.name, n.announced);
                if !n.make_and_model.is_empty() {
                    println!("    model {}", n.make_and_model);
                }
                if let Some(f) = &n.fingerprint {
                    println!("    key {f}");
                }
                if let Some(u) = &n.uuid {
                    println!("    uuid {u}");
                }
            }
            Ok(())
        }
        "add" => {
            let p = c.add(&Add { uri: arg(1), name: arg(2) }).map_err(|e| e.to_string())?;
            show(&p);
            Ok(())
        }
        "remove" | "repin" | "identify" | "refresh" => {
            let id = find(&mut c, &arg(1))?;
            match cmd.as_str() {
                "remove" => println!("{}", c.remove(id).map_err(|e| e.to_string())?),
                "identify" => println!("{}", c.identify(id).map_err(|e| e.to_string())?),
                "repin" => show(&c.repin(id).map_err(|e| e.to_string())?),
                _ => show(&c.refresh(id).map_err(|e| e.to_string())?),
            }
            Ok(())
        }
        "rename" => {
            let id = find(&mut c, &arg(1))?;
            println!("{}", c.rename(&Rename { printer: id, name: arg(2) }).map_err(|e| e.to_string())?);
            Ok(())
        }
        "jobs" => {
            for j in c.jobs().map_err(|e| e.to_string())? {
                println!("{:>4}  printer {}  {:?}  {}/{}  {}  {}", j.id, j.printer, j.phase, j.sheets, j.of, j.title, j.message);
            }
            Ok(())
        }
        "cancel" | "release" => {
            let id: u64 = arg(1).parse().map_err(|_| "JOB is a number")?;
            let m = if cmd == "cancel" { c.cancel(id) } else { c.release(id) };
            println!("{}", m.map_err(|e| e.to_string())?);
            Ok(())
        }
        "print" => {
            let file = arg(1);
            let mut printer = None;
            let mut wait = false;
            let title = std::path::Path::new(&file).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let mut p = Print::new(0, &title);
            let mut i = 2;
            while i < a.len() {
                let k = a[i].as_str();
                let v = a.get(i + 1).cloned().unwrap_or_default();
                i += 2;
                match k {
                    "--printer" => printer = Some(v),
                    "--copies" => p.copies = v.parse().map_err(|_| "bad --copies")?,
                    "--sides" => {
                        p.sides = match v.as_str() {
                            "long" => Sides::TwoSidedLongEdge,
                            "short" => Sides::TwoSidedShortEdge,
                            _ => Sides::OneSided,
                        }
                    }
                    "--gray" => {
                        p.color = ColorMode::Monochrome;
                        i -= 1;
                    }
                    "--wait" => {
                        wait = true;
                        i -= 1;
                    }
                    "--media" => p.media = Some(v),
                    "--quality" => {
                        p.quality = Some(match v.as_str() {
                            "draft" => Quality::Draft,
                            "high" => Quality::High,
                            _ => Quality::Normal,
                        })
                    }
                    "--pages" => p.pages = Some(v),
                    "--per-sheet" => p.per_sheet = v.parse().map_err(|_| "bad --per-sheet")?,
                    "--scale" => {
                        p.scale = match v.as_str() {
                            "fit" => Scale::Fit,
                            "shrink" => Scale::ShrinkToFit,
                            n => Scale::Percent(n.parse().map_err(|_| "bad --scale")?),
                        }
                    }
                    "--title" => p.title = v,
                    _ => return Err(format!("unknown option {k}")),
                }
            }
            p.printer = find(&mut c, &printer.ok_or("--printer is needed")?)?;
            let f = std::fs::File::open(&file).map_err(|e| format!("{file}: {e}"))?;
            let job = c.print(&p, &f).map_err(|e| e.to_string())?;
            println!("job {job}");
            if !wait {
                return Ok(());
            }
            let mut last = String::new();
            loop {
                std::thread::sleep(Duration::from_millis(300));
                let jobs = c.jobs().map_err(|e| e.to_string())?;
                let Some(j) = jobs.into_iter().find(|j| j.id == job) else { return Err("the job disappeared".into()) };
                let line = format!("{:?} {}/{} {}", j.phase, j.sheets, j.of, j.message);
                if line != last {
                    println!("{line}");
                    last = line;
                }
                match j.phase {
                    Phase::Done => return Ok(()),
                    Phase::Failed | Phase::Cancelled | Phase::Held => return Err(format!("job {job}: {:?} {}", j.phase, j.message)),
                    _ => {}
                }
            }
        }
        "preview" | "save-pdf" => {
            let (file, out) = (arg(1), arg(2));
            let mut l = Layout::new("iso_a4_210x297mm");
            let (mut first, mut count, mut longest) = (1u32, 8u8, 300u16);
            let mut i = 3;
            while i < a.len() {
                let (k, v) = (a[i].as_str(), arg(i + 1));
                i += 2;
                match k {
                    "--media" => l.media = v,
                    "--pages" => l.pages = Some(v),
                    "--per-sheet" => l.per_sheet = v.parse().map_err(|_| "bad --per-sheet")?,
                    "--first" => first = v.parse().map_err(|_| "bad --first")?,
                    "--count" => count = v.parse().map_err(|_| "bad --count")?,
                    "--longest" => longest = v.parse().map_err(|_| "bad --longest")?,
                    _ => return Err(format!("unknown option {k}")),
                }
            }
            let f = std::fs::File::open(&file).map_err(|e| format!("{file}: {e}"))?;
            if cmd == "save-pdf" {
                let (n, mut pdf) = c.save_pdf(&l, &f).map_err(|e| e.to_string())?;
                let mut o = std::fs::File::create(&out).map_err(|e| format!("{out}: {e}"))?;
                std::io::copy(&mut pdf, &mut o).map_err(|e| format!("{out}: {e}"))?;
                println!("saved {n} bytes");
                return Ok(());
            }
            let p = c.preview(&PreviewReq { layout: l, first, count, longest }, &f).map_err(|e| e.to_string())?;
            println!("{} sheets", p.sheets);
            std::fs::create_dir_all(&out).map_err(|e| format!("{out}: {e}"))?;
            for im in p.images {
                let mut b = format!("P6\n{} {}\n255\n", im.w, im.h).into_bytes();
                b.extend_from_slice(&im.rgb);
                let path = format!("{out}/sheet-{}.ppm", im.sheet);
                std::fs::write(&path, b).map_err(|e| format!("{path}: {e}"))?;
                println!("sheet {} {}x{}", im.sheet, im.w, im.h);
            }
            Ok(())
        }
        _ => Err(format!("unknown command {cmd}")),
    })();
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => die(e),
    }
}
