//! `.xls` against LibreOffice (docs/spec/officina.md §16.40, O7b gate).
//!
//! 1. Builds officina-grid's conformance workbook (the fixture and its 152
//!    formulas, each with the result Excel shows) and a sheet of formats,
//!    and writes it as `.xlsx`.
//! 2. LibreOffice saves it as Excel 97–2003 `.xls` (BIFF8), plain and with a
//!    password (RC4 CryptoAPI).
//! 3. Our reader opens both: every formula is decompiled from LibreOffice's
//!    tokens, recalculated by our engine and compared with Excel's result
//!    and with the result LibreOffice saved; values and formats are compared
//!    with what we wrote; the protected file asks for its password and
//!    refuses a wrong one.
//!
//!   cargo run --release --example xls_lo [OUTDIR]
//!
//! Needs `soffice` and Python with LibreOffice's `uno` module. Prints the
//! disagreements and a summary line.

use std::path::Path;
use std::process::Command;

use officina_convert::xls::{self, XlsError};
use officina_grid::style::{Edge, HAlign, Line, Style, VAlign};
use officina_grid::{CellAddr, Rect, Value, Workbook};

const COL: u32 = 25; // Z, as in tests/conformance.rs
const PASSWORD: &str = "Zanzibar 7";

const DRIVER: &str = r#"
import sys, time, subprocess, os, uno
from com.sun.star.beans import PropertyValue
def pv(n, v):
    p = PropertyValue(); p.Name = n; p.Value = v; return p
port, profile, jobs = sys.argv[1], sys.argv[2], sys.argv[3]
proc = subprocess.Popen(["soffice", "--headless", "--invisible", "--norestore", "-env:UserInstallation=file://" + profile, "--accept=socket,host=localhost,port=%s;urp;" % port], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
try:
    local = uno.getComponentContext()
    res = local.ServiceManager.createInstanceWithContext("com.sun.star.bridge.UnoUrlResolver", local)
    ctx = None
    for _ in range(120):
        try:
            ctx = res.resolve("uno:socket,host=localhost,port=%s;urp;StarOffice.ComponentContext" % port); break
        except Exception:
            time.sleep(0.5)
    if ctx is None:
        raise SystemExit("no soffice")
    desk = ctx.ServiceManager.createInstanceWithContext("com.sun.star.frame.Desktop", ctx)
    url = lambda p: uno.systemPathToFileUrl(os.path.abspath(p))
    for line in open(jobs, encoding="utf-8").read().splitlines():
        src, pw, out, flt = line.split("\t")
        try:
            d = desk.loadComponentFromURL(url(src), "_blank", 0, (pv("Hidden", True),))
            if d is None:
                print("fail did not open", flush=True); continue
            d.calculateAll()
            props = [pv("FilterName", flt)]
            if pw:
                props.append(pv("Password", pw))
            d.storeToURL(url(out), tuple(props))
            d.close(True)
            print("ok", flush=True)
        except Exception as e:
            print("fail " + str(e).splitlines()[0][:120], flush=True)
finally:
    proc.terminate()
"#;

/// The conformance cases, read from officina-grid's test (one source of truth).
fn cases() -> Vec<(String, String)> {
    let src = include_str!("../../officina-grid/tests/conformance.rs");
    let unq = |s: &str| s.replace("\\\"", "\"").replace("\\\\", "\\");
    let mut out = Vec::new();
    for line in src.lines() {
        let l = line.trim();
        let Some(rest) = l.strip_prefix("(\"") else { continue };
        let Some(mid) = find_sep(rest) else { continue };
        let f = &rest[..mid];
        let tail = &rest[mid + 4..];
        let Some(end) = tail.rfind("\")") else { continue };
        out.push((unq(f), unq(&tail[..end])));
    }
    out
}

fn find_sep(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 0;
    while i + 3 < b.len() {
        if b[i] == b'\\' {
            i += 2;
            continue;
        }
        if &b[i..i + 4] == b"\", \"" {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn set(wb: &mut Workbook, s: u16, a: &str, v: &str) {
    let (r, c) = officina_grid::addr::parse_a1(a).unwrap();
    wb.set_input(CellAddr::new(s, r, c), v).unwrap();
}

/// The formats sheet: (cell, value, style).
fn formats() -> Vec<(&'static str, &'static str, Style)> {
    let st = Style::default;
    let mut v = Vec::new();
    let mut s = st();
    s.font.bold = true;
    v.push(("A1", "bold", s));
    let mut s = st();
    s.font.italic = true;
    s.font.underline = true;
    v.push(("A2", "italic underlined", s));
    let mut s = st();
    s.font.strike = true;
    s.font.color = Some(0xC00000);
    v.push(("A3", "struck red", s));
    let mut s = st();
    s.fill = Some(0xFFFF00);
    v.push(("A4", "yellow", s));
    let mut s = st();
    s.font.size = Some(280);
    s.font.name = Some("DejaVu Serif".into());
    v.push(("A5", "14 pt serif", s));
    let mut s = st();
    s.borders = [Edge { line: Line::Thin, color: None }, Edge { line: Line::Medium, color: Some(0x0070C0) }, Edge { line: Line::Double, color: None }, Edge { line: Line::Dashed, color: Some(0x00B050) }];
    v.push(("B2", "borders", s));
    for (a, val, code) in [("C1", "1234.5", "0.00"), ("C2", "0.25", "0%"), ("C3", "45000", "dd/mm/yyyy"), ("C4", "1234567", "#,##0"), ("C5", "-3", "0.0;[Red]-0.0")] {
        v.push((a, val, st().with_num_fmt(code)));
    }
    let mut s = st();
    s.h_align = HAlign::Center;
    s.v_align = VAlign::Top;
    v.push(("D1", "centred top", s));
    let mut s = st();
    s.h_align = HAlign::Right;
    s.wrap = true;
    v.push(("D2", "right wrapped text in a cell", s));
    let mut s = st();
    s.indent = 2;
    s.h_align = HAlign::Left;
    v.push(("D3", "indented", s));
    v
}

fn build(cases: &[(String, String)]) -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("S").unwrap();
    wb.add_sheet("Other sheet").unwrap();
    wb.add_sheet("Formats").unwrap();
    for (i, v) in ["1", "2", "3", "4", "5"].iter().enumerate() {
        set(&mut wb, 0, &format!("A{}", i + 1), v);
    }
    for (i, v) in ["apple", "Banana", "cherry", "apple pie", "date"].iter().enumerate() {
        set(&mut wb, 0, &format!("B{}", i + 1), v);
    }
    set(&mut wb, 0, "D1", "10");
    set(&mut wb, 0, "D2", "x");
    set(&mut wb, 0, "D4", "20");
    set(&mut wb, 0, "E1", "=1/0");
    wb.set_input(CellAddr::new(1, 0, 0), "7").unwrap();
    for (i, (f, _)) in cases.iter().enumerate() {
        wb.set_input(CellAddr::new(0, 100 + i as u32, COL), f).unwrap();
    }
    for (a, val, s) in formats() {
        set(&mut wb, 2, a, val);
        let (r, c) = officina_grid::addr::parse_a1(a).unwrap();
        wb.set_style(&Rect { sheet: 2, r0: r, c0: c, r1: r, c1: c }, s);
    }
    let sh = wb.sheet_mut(2);
    sh.col_sizes = officina_grid::column::Sizes::from_overrides(sh.col_sizes.default, vec![(3, 180.0)]);
    sh.row_sizes = officina_grid::column::Sizes::from_overrides(sh.row_sizes.default, vec![(1, 40.0)]);
    wb.recalc();
    wb
}

struct Checks {
    pass: usize,
    fail: usize,
}

impl Checks {
    fn check(&mut self, what: &str, ok: bool, detail: impl FnOnce() -> String) {
        if ok {
            self.pass += 1;
            println!("PASS {what}");
        } else {
            self.fail += 1;
            println!("FAIL {what}: {}", detail());
        }
    }
}

fn close(a: &str, b: &str) -> bool {
    match (a.parse::<f64>(), b.parse::<f64>()) {
        (Ok(x), Ok(y)) => (x - y).abs() <= 1e-12 * x.abs().max(y.abs()),
        _ => false,
    }
}

fn run_lo(dir: &Path, jobs: &[[String; 4]]) -> Vec<String> {
    let driver = dir.join("driver.py");
    std::fs::write(&driver, DRIVER).unwrap();
    let list = dir.join("jobs.txt");
    std::fs::write(&list, jobs.iter().map(|j| j.join("\t")).collect::<Vec<_>>().join("\n")).unwrap();
    let out = Command::new("python3").arg(&driver).arg("2086").arg(dir.join("profile")).arg(&list).output().expect("python3");
    let lines: Vec<String> = String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect();
    if lines.len() != jobs.len() {
        eprintln!("soffice: {}", String::from_utf8_lossy(&out.stderr));
    }
    lines
}

fn main() {
    let cases = cases();
    assert!(cases.len() >= 150, "read {} cases", cases.len());
    let wb = build(&cases);
    let dir = match std::env::args().nth(1) {
        Some(d) => std::path::PathBuf::from(d),
        None => std::env::temp_dir().join(format!("xls-lo-{}", std::process::id())),
    };
    std::fs::create_dir_all(&dir).unwrap();
    let ours = dir.join("ours.xlsx");
    std::fs::write(&ours, officina_convert::xlsx::write(&wb, Vec::new()).unwrap()).unwrap();
    let s = |p: std::path::PathBuf| p.to_string_lossy().into_owned();
    let jobs = vec![
        [s(ours.clone()), String::new(), s(dir.join("lo.xls")), "MS Excel 97".into()],
        [s(ours.clone()), PASSWORD.into(), s(dir.join("lo-protected.xls")), "MS Excel 97".into()],
    ];
    let res = run_lo(&dir, &jobs);
    let mut c = Checks { pass: 0, fail: 0 };
    c.check("LibreOffice saved both .xls files", res.iter().filter(|l| *l == "ok").count() == 2, || format!("{res:?}"));
    let Ok(plain) = std::fs::read(dir.join("lo.xls")) else {
        println!("summary: no file from LibreOffice");
        std::process::exit(1);
    };
    let (back, loaded) = match xls::read(&plain, None) {
        Ok(x) => x,
        Err(e) => {
            println!("FAIL our reader on LibreOffice's .xls: {e}");
            std::process::exit(1);
        }
    };
    // Formulas: our engine's results on LibreOffice's tokens are Excel's,
    // except where LibreOffice can't write the formula to .xls at all (it
    // saves an error constant instead).
    let (saved, _) = xls::read_saved(&plain, None).expect("saved results");
    let (mut wrong, mut lo_cant, mut lo_not_excel) = (Vec::new(), Vec::new(), 0);
    for (i, (f, excel)) in cases.iter().enumerate() {
        let a = CellAddr::new(0, 100 + i as u32, COL);
        let got = back.get(a).display();
        let text = back.formula_text(a).unwrap_or_default();
        let lo = saved.get(a).display();
        if lo != *excel && !close(&lo, excel) {
            lo_not_excel += 1;
        }
        if got == *excel || close(&got, excel) {
            continue;
        }
        if text.starts_with("=#") && !f.starts_with("=#") {
            lo_cant.push(format!("{f} (saved as {text})"));
        } else {
            wrong.push(format!("{f} shows {got}, Excel {excel}, LibreOffice {lo} (read back as {text})"));
        }
    }
    c.check(&format!("{} conformance formulas through .xls give Excel's results", cases.len() - lo_cant.len()), wrong.is_empty(), || wrong.join("\n    "));
    println!("     LibreOffice can't write {} of them to .xls: {}", lo_cant.len(), lo_cant.join("; "));
    println!("     LibreOffice's own results differ from Excel's on {lo_not_excel}");
    c.check(&format!("every formula placed ({} of {})", loaded.formulas, cases.len() + 1), loaded.formulas as usize >= cases.len() + 1, || loaded.report.lines().join("\n    "));
    // Where LibreOffice's saved result is Excel's, ours is too.
    let mut vs_lo = Vec::new();
    for (i, (f, excel)) in cases.iter().enumerate() {
        let a = CellAddr::new(0, 100 + i as u32, COL);
        let (got, lo) = (back.get(a).display(), saved.get(a).display());
        let text = back.formula_text(a).unwrap_or_default();
        let cant = text.starts_with("=#") && !f.starts_with("=#");
        if !cant && (lo == *excel || close(&lo, excel)) && got != lo && !close(&got, &lo) {
            vs_lo.push(format!("{f}: ours {got}, LibreOffice {lo}"));
        }
    }
    c.check("ours equals LibreOffice's saved result wherever that is Excel's", vs_lo.is_empty(), || vs_lo.join("\n    "));
    // Values.
    let mut kept = Vec::new();
    for s in 0..2u16 {
        for r in 0..5 {
            for col in 0..5 {
                let a = CellAddr::new(s, r, col);
                if back.get(a) != wb.get(a) {
                    kept.push(format!("sheet {s} {}: {:?} vs {:?}", officina_grid::addr::a1(r, col), back.get(a), wb.get(a)));
                }
            }
        }
    }
    c.check("the fixture's values", kept.is_empty(), || kept.join(", "));
    let names: Vec<String> = (0..back.sheet_count() as u16).map(|s| back.sheet_name(s).to_string()).collect();
    c.check("sheet names", names == ["S", "Other sheet", "Formats"], || format!("{names:?}"));
    // Formats.
    for (a, val, want) in formats() {
        let (r, col) = officina_grid::addr::parse_a1(a).unwrap();
        let at = CellAddr::new(2, r, col);
        let got = back.style(at).clone();
        let mut want = want;
        // LibreOffice names the default font in every format it writes.
        let mut g2 = got.clone();
        if want.font.name.is_none() {
            g2.font.name = None;
        }
        if want.font.size.is_none() {
            g2.font.size = None;
        }
        // Borders without a colour come back black.
        for e in g2.borders.iter_mut() {
            if e.color == Some(0) {
                e.color = None;
            }
        }
        // LibreOffice spells codes its own way (`[RED]`, `\\-`).
        let norm = |c: &str| c.to_ascii_lowercase().replace('\\', "");
        if norm(&want.num_fmt) == norm(&g2.num_fmt) {
            want.num_fmt = g2.num_fmt.clone();
        }
        let shown = back.get(at).display();
        let _ = val;
        c.check(&format!("format of {a} ({val})"), g2 == want, || format!("{got:?}\n    wanted {want:?} (shows {shown})"));
    }
    let sh = back.sheet(2);
    c.check("column width", (sh.col_sizes.size(3) - 180.0).abs() <= 3.0, || format!("{}", sh.col_sizes.size(3)));
    c.check("row height", (sh.row_sizes.size(1) - 40.0).abs() <= 2.0, || format!("{}", sh.row_sizes.size(1)));
    println!("fidelity report on LibreOffice's file:");
    for l in loaded.report.human() {
        println!("  {l}");
    }
    // The protected copy.
    match std::fs::read(dir.join("lo-protected.xls")) {
        Ok(p) => {
            c.check("protected: asks for the password", matches!(xls::read(&p, None), Err(XlsError::NeedsPassword)), || "opened or failed otherwise".into());
            c.check("protected: refuses a wrong password", matches!(xls::read(&p, Some("wrong")), Err(XlsError::WrongPassword)), || "opened or failed otherwise".into());
            match xls::read(&p, Some(PASSWORD)) {
                Ok((b2, l2)) => {
                    let same = (0..cases.len() as u32).all(|i| b2.get(CellAddr::new(0, 100 + i, COL)) == back.get(CellAddr::new(0, 100 + i, COL)));
                    c.check("protected: opens with the password, same results", same && l2.differ == loaded.differ, || format!("differ {}", l2.differ));
                }
                Err(e) => c.check("protected: opens with the password", false, || e.to_string()),
            }
            let via = officina_convert::launch::unprotect("xls", p.clone(), Some(PASSWORD));
            c.check("the converter leaves .xls decryption to the reader", via.as_deref() == Ok(&p[..]), || "changed".into());
        }
        Err(e) => c.check("LibreOffice's protected .xls", false, || e.to_string()),
    }
    println!("summary: {} passed, {} failed", c.pass, c.fail);
    if c.fail > 0 {
        std::process::exit(1);
    }
}
