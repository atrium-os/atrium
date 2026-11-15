//! officina-convert: a foreign file in, a native file out, in a sandbox.
//!
//!   officina-convert xlsx < book.xlsx > book.osheet 2> report
//!   officina-convert ods < book.ods > book.osheet 2> report
//!   officina-convert xls < book.xls > book.osheet 2> report
//!   officina-convert doc < letter.doc > letter.owrite 2> report
//!   officina-convert pptx < talk.pptx > talk.oslides 2> report
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
//! on failure `error<TAB>why` with exit status 1.
//!
//! This is the converter process of docs/spec/officina.md §5.2; launching it in a
//! throwaway jail (portcullis one-shot) is the next step, and the sandbox here stays as
//! a second wall.

use std::io::{Read, Write};

fn rl(v: u64) -> libc::rlimit {
    libc::rlimit { rlim_cur: v as libc::rlim_t, rlim_max: v as libc::rlim_t }
}

fn sandbox() {
    // 60 s of CPU, 8 GiB of address space.
    // SAFETY: setrlimit with valid structs.
    unsafe {
        libc::setrlimit(libc::RLIMIT_CPU, &rl(60));
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

fn main() {
    let what = std::env::args().nth(1).unwrap_or_default();
    let with_password = std::env::args().nth(2).is_some_and(|a| a == "--password");
    let mut stdin = std::io::stdin().lock();
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
        "pptx" | "odp" => {
            let got = if what == "pptx" { officina_convert::pptx::read(&input).map_err(|e| e.to_string()) } else { officina_convert::odp::read(&input).map_err(|e| e.to_string()) };
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
            eprintln!("error\tusage: officina-convert xlsx|xls|ods|docx|doc|odt|pptx|odp < in > out (.osheet, .owrite or .oslides)");
            std::process::exit(2);
        }
    }
}
