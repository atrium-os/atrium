//! officina-convert: a foreign file in, a native file out, in a sandbox.
//!
//!   officina-convert xlsx < book.xlsx > book.osheet 2> report
//!   officina-convert ods < book.ods > book.osheet 2> report
//!   officina-convert xls < book.xls > book.osheet 2> report
//!   officina-convert doc < letter.doc > letter.owrite 2> report
//!   officina-convert pptx < talk.pptx > talk.oslides 2> report
//!   officina-convert ppt < talk.ppt > talk.oslides 2> report
//!
//! With `--password` after the kind, stdin starts with the password (its
//! length as a little-endian u32, then UTF-8) and a protected file is
//! decrypted inside the sandbox; a protected file without one, or with the
//! wrong one, ends in `error\tneeds-password` or `error\twrong-password`.
//!
//! Reads the whole input, then (on FreeBSD) enters Capsicum capability mode, so the
//! parser can open no file and no socket whatever the input does; CPU time and memory
//! are capped with rlimits. Writes the native file on stdout. On stderr, one line per
//! fidelity note (`note<TAB>kind<TAB>count<TAB>what<TAB>examples`), `stats<TAB>...`, and
//! on failure `error<TAB>why` with exit status 1; on success the last line is `done`.
//!
//! This is the converter process of docs/spec/officina.md §5.2. Installed, it is
//! the app's `convert` worker (`[[workers]]`, app-composition.md §4): a one-shot jail of
//! its own as a uid of its own, with no capability; started that way it takes its
//! arguments as the first line on stdin. The sandbox here stays as a second wall.

use std::io::{Read, Write};

fn rl(v: u64) -> libc::rlimit {
    libc::rlimit { rlim_cur: v as libc::rlim_t, rlim_max: v as libc::rlim_t }
}

fn sandbox() {
    sandbox_cpu(60)
}

fn sandbox_cpu(cpu_secs: u64) {
    // CPU seconds, 8 GiB of address space.
    // SAFETY: setrlimit with valid structs.
    unsafe {
        libc::setrlimit(libc::RLIMIT_CPU, &rl(cpu_secs));
        libc::setrlimit(libc::RLIMIT_AS, &rl(8 << 30));
    }
    #[cfg(target_os = "freebsd")]
    {
        extern "C" {
            fn cap_enter() -> libc::c_int;
        }
        // SAFETY: no arguments; from here on, no new files or sockets.
        if unsafe { cap_enter() } != 0 {
            eprintln!("error\tcould not enter capability mode");
            std::process::exit(1);
        }
    }
}

/// Started as the app's `convert` worker (dev-tools-plan.md M5), it has no
/// arguments: the first line on stdin is them (`xlsx` or `xlsx --password`).
fn header(stdin: &mut impl Read) -> (String, bool) {
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    while line.len() < 64 && stdin.read(&mut b).is_ok_and(|n| n == 1) && b[0] != b'\n' {
        line.push(b[0]);
    }
    let line = String::from_utf8_lossy(&line).into_owned();
    let mut w = line.split(' ');
    let what = w.next().unwrap_or_default().to_string();
    (what, w.next() == Some("--password"))
}

/// `officina-convert pictures`: decodes pictures until stdin ends, in the
/// sandbox (the protocol is in officina_convert::pictures). Its CPU limit is
/// for the session; when it is spent the process ends and the app starts
/// another.
fn pictures(mut stdin: impl Read) -> ! {
    use navigator_image::image::{decode_unaddressed, natural_size, Ceilings};
    use officina_convert::pictures::{MAX_INPUT, OP_DECODE, OP_SIZE};
    sandbox_cpu(600);
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    let mut head = [0u8; 13];
    let mut bytes = Vec::new();
    loop {
        if stdin.read_exact(&mut head).is_err() {
            std::process::exit(0);
        }
        let u = |i: usize| u32::from_le_bytes([head[i], head[i + 1], head[i + 2], head[i + 3]]);
        let (op, tw, th, len) = (head[0], u(1), u(5), u(9) as usize);
        if len > MAX_INPUT || !matches!(op, OP_SIZE | OP_DECODE) {
            std::process::exit(1);
        }
        bytes.resize(len, 0);
        if stdin.read_exact(&mut bytes).is_err() {
            std::process::exit(1);
        }
        let answer = |out: &mut dyn Write, ok: bool, w: u32, h: u32| {
            let mut a = vec![ok as u8];
            a.extend_from_slice(&w.to_le_bytes());
            a.extend_from_slice(&h.to_le_bytes());
            out.write_all(&a)
        };
        let r = if op == OP_SIZE {
            match natural_size(&bytes) {
                Ok((_, w, h)) => answer(&mut out, true, w, h),
                Err(_) => answer(&mut out, false, 0, 0),
            }
        } else {
            let target = (tw > 0 && th > 0).then_some((tw, th));
            match decode_unaddressed(&bytes, Ceilings { target, ..Ceilings::default() }) {
                Ok(px) if px.rgba.len() as u64 == px.width as u64 * px.height as u64 * 4 => {
                    answer(&mut out, true, px.width, px.height).and_then(|_| out.flush()).and_then(|_| out.get_mut().write_all(&px.rgba))
                }
                _ => answer(&mut out, false, 0, 0),
            }
        };
        if r.and_then(|_| out.flush()).is_err() {
            std::process::exit(0);
        }
    }
}

fn clipboard_html(stdin: impl Read) -> Result<(), String> {
    use officina_convert::html_clipboard::{self, MAX_HTML_BYTES, MAX_NATIVE_BYTES};
    let mut input = Vec::new();
    stdin.take((MAX_HTML_BYTES + 1) as u64).read_to_end(&mut input).map_err(|e| e.to_string())?;
    sandbox_cpu(3);
    let clip = html_clipboard::read(&input)?;
    let encoded = clip.encode_system(&[0; 16], MAX_NATIVE_BYTES)
        .ok_or("The converted clipboard range is too large.")?;
    std::io::stdout().lock().write_all(&encoded).map_err(|e| e.to_string())
}

fn main() {
    let mut stdin = std::io::stdin().lock();
    let (what, with_password) = match std::env::args().nth(1) {
        Some(w) => (w, std::env::args().nth(2).is_some_and(|a| a == "--password")),
        None => header(&mut stdin),
    };
    if what == officina_convert::html_clipboard::WORKER_KIND {
        match clipboard_html(stdin) {
            Ok(()) => eprintln!("done"),
            Err(e) => {
                eprintln!("error\t{e}");
                std::process::exit(1);
            }
        }
        return;
    }
    if what == "pictures" {
        pictures(stdin);
    }
    let mut password = None;
    if with_password {
        let mut n = [0u8; 4];
        let mut p = Vec::new();
        let ok = stdin.read_exact(&mut n).is_ok() && {
            let n = u32::from_le_bytes(n) as usize;
            n <= officina_convert::launch::MAX_PASSWORD && { p.resize(n, 0); stdin.read_exact(&mut p).is_ok() }
        };
        match String::from_utf8(p) {
            Ok(s) if ok => password = Some(s),
            _ => {
                eprintln!("error\tthe password didn't arrive");
                std::process::exit(1);
            }
        }
    }
    let mut input = Vec::new();
    if let Err(e) = stdin.take(2 << 30).read_to_end(&mut input) {
        eprintln!("error\t{e}");
        std::process::exit(1);
    }
    sandbox();
    let input = match officina_convert::launch::unprotect(&what, input, password.as_deref()) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("error\t{e}");
            std::process::exit(1);
        }
    };
    match what.as_str() {
        "xlsx" | "xlsm" | "ods" | "xls" => match match what.as_str() {
            "ods" => officina_convert::ods::read(&input).map_err(|e| e.to_string()),
            // A protected .xls is decrypted by its reader.
            "xls" => officina_convert::xls::read(&input, password.as_deref()).map_err(|e| e.to_string()),
            _ => officina_convert::xlsx::read(&input).map_err(|e| e.to_string()),
        } {
            Ok((wb, loaded)) => {
                let out = std::io::BufWriter::new(std::io::stdout().lock());
                if let Err(e) = officina_grid::native::save(&wb, out).and_then(|mut o| o.flush()) {
                    eprintln!("error\t{e}");
                    std::process::exit(1);
                }
                for l in loaded.report.lines() {
                    eprintln!("note\t{l}");
                }
                eprintln!("stats\tcells {}\tformulas {}\tdiffer {}", loaded.cells, loaded.formulas, loaded.differ);
            }
            Err(e) => {
                eprintln!("error\t{e}");
                std::process::exit(1);
            }
        },
        "docx" | "odt" | "doc" => {
            let got = match what.as_str() {
                "docx" => officina_convert::docx::read(&input).map_err(|e| e.to_string()),
                // A protected .doc is decrypted by its reader.
                "doc" => officina_convert::doc::read(&input, password.as_deref()).map_err(|e| e.to_string()),
                _ => officina_convert::odt::read(&input).map_err(|e| e.to_string()),
            };
            match got {
                Ok((doc, loaded)) => {
                    let out = std::io::BufWriter::new(std::io::stdout().lock());
                    if let Err(e) = officina_text::native::save(&doc, &[], out).and_then(|mut o| o.flush()) {
                        eprintln!("error\t{e}");
                        std::process::exit(1);
                    }
                    for l in loaded.report.lines() {
                        eprintln!("note\t{l}");
                    }
                    eprintln!("stats\tparagraphs {}\tpictures {}", loaded.paragraphs, loaded.pictures);
                }
                Err(e) => {
                    eprintln!("error\t{e}");
                    std::process::exit(1);
                }
            }
        }
        "pptx" | "odp" | "ppt" => {
            let got = match what.as_str() {
                "pptx" => officina_convert::pptx::read(&input).map_err(|e| e.to_string()),
                // A protected .ppt is decrypted by its reader.
                "ppt" => officina_convert::ppt::read(&input, password.as_deref()).map_err(|e| e.to_string()),
                _ => officina_convert::odp::read(&input).map_err(|e| e.to_string()),
            };
            match got {
                Ok((deck, loaded)) => {
                    let out = std::io::BufWriter::new(std::io::stdout().lock());
                    if let Err(e) = officina_slides::native::save(&deck, out).and_then(|mut o| o.flush()) {
                        eprintln!("error\t{e}");
                        std::process::exit(1);
                    }
                    for l in loaded.report.lines() {
                        eprintln!("note\t{l}");
                    }
                    eprintln!("stats\tslides {}\tshapes {}\tpictures {}", loaded.slides, loaded.shapes, loaded.pictures);
                }
                Err(e) => {
                    eprintln!("error\t{e}");
                    std::process::exit(1);
                }
            }
        }
        _ => {
            eprintln!("error\tusage: officina-convert xlsx|xls|ods|docx|doc|odt|pptx|ppt|odp < in > out (.osheet, .owrite or .oslides)");
            std::process::exit(2);
        }
    }
    // Everything is written: the app needn't wait for this process's end
    // (a jail's teardown) to know it went well.
    eprintln!("done");
}
