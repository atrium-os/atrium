//! officina-convert: a foreign file in, a native file out, in a sandbox.
//!
//!   officina-convert xlsx < book.xlsx > book.osheet 2> report
//!   officina-convert ods < book.ods > book.osheet 2> report
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
    let mut input = Vec::new();
    if let Err(e) = std::io::stdin().take(2 << 30).read_to_end(&mut input) {
        eprintln!("error\t{e}");
        std::process::exit(1);
    }
    sandbox();
    match what.as_str() {
        "xlsx" | "xlsm" | "ods" => match if what == "ods" { officina_convert::ods::read(&input).map_err(|e| e.to_string()) } else { officina_convert::xlsx::read(&input).map_err(|e| e.to_string()) } {
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
        "docx" | "odt" => {
            let got = if what == "docx" { officina_convert::docx::read(&input).map_err(|e| e.to_string()) } else { officina_convert::odt::read(&input).map_err(|e| e.to_string()) };
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
        _ => {
            eprintln!("error\tusage: officina-convert xlsx|ods|docx|odt < in > out (.osheet or .owrite)");
            std::process::exit(2);
        }
    }
}
