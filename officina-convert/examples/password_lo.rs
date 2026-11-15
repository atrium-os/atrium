//! Password-protected files against LibreOffice, both ways (docs/spec/
//! officina.md §16.40, O7a gate), for .docx, .xlsx, .pptx, .odt, .ods and
//! .odp:
//!
//! - ours to LibreOffice: we write a file and protect it; LibreOffice opens
//!   it with the password (and not with a wrong one) and saves it plain; we
//!   read that and find what we wrote;
//! - LibreOffice to ours: LibreOffice saves our plain file with a password
//!   (Standard encryption for OOXML, AES-CBC for ODF, and AES-GCM with
//!   Argon2id in its newest ODF mode); we open it with the password, refuse a
//!   wrong one and ask when there is none.
//!
//! cargo run --release --example password_lo [OUTDIR]
//!
//! Needs `soffice` and Python with LibreOffice's `uno` module. Prints one
//! line per check and a summary.

use std::path::{Path, PathBuf};
use std::process::Command;

use officina_convert::launch::{protect, unprotect, NEEDS_PASSWORD, WRONG_PASSWORD};
use officina_convert::{docx, ods, odp, odt, pptx, xlsx};

#[path = "../tests/support/slides_sample.rs"]
mod sample;

const MARK_TEXT: &str = "Zanzibar marker paragraph";
const MARK_CELL: &str = "Zanzibar marker cell";
const MARK_SLIDE: &str = "Quarterly review";

/// The UNO driver: one soffice for all jobs. Each job line is
/// `save|load<TAB>in<TAB>password<TAB>out<TAB>filter`; prints `ok` or
/// `fail <why>` per job.
const DRIVER: &str = r#"
import sys, time, subprocess, os, uno
from com.sun.star.beans import PropertyValue
def pv(n, v):
    p = PropertyValue(); p.Name = n; p.Value = v; return p
port, profile, jobs, gcm = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4] == "1"
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
    if gcm:
        cp = ctx.ServiceManager.createInstanceWithContext("com.sun.star.configuration.ConfigurationProvider", ctx)
        # Experimental mode and the newest ODF version: whole-package
        # AES-GCM with Argon2id.
        for node, name, val in (("/org.openoffice.Office.Common/Misc", "ExperimentalMode", True), ("/org.openoffice.Office.Common/Save/ODF", "DefaultVersion", 32767)):
            acc = cp.createInstanceWithArguments("com.sun.star.configuration.ConfigurationUpdateAccess", (pv("nodepath", node),))
            acc.setPropertyValue(name, val); acc.commitChanges()
    desk = ctx.ServiceManager.createInstanceWithContext("com.sun.star.frame.Desktop", ctx)
    url = lambda p: uno.systemPathToFileUrl(os.path.abspath(p))
    for line in open(jobs, encoding="utf-8").read().splitlines():
        what, src, pw, out, flt = line.split("\t")
        try:
            args = [pv("Hidden", True)]
            if what == "load":
                args.append(pv("Password", pw))
            d = desk.loadComponentFromURL(url(src), "_blank", 0, tuple(args))
            if d is None:
                print("fail did not open", flush=True); continue
            props = [pv("FilterName", flt)]
            if what == "save":
                props.append(pv("Password", pw))
            d.storeToURL(url(out), tuple(props))
            d.close(True)
            print("ok", flush=True)
        except Exception as e:
            print("fail " + str(e).splitlines()[0][:120], flush=True)
finally:
    proc.terminate()
"#;

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

/// A plain file of `kind` carrying the kind's marker.
fn plain(kind: &str) -> Vec<u8> {
    match kind {
        "docx" | "odt" => {
            let mut d = officina_text::model::TextDoc::new();
            d.blocks.remove(0);
            d.push_para("Protected document", 0, vec![]);
            d.push_para(MARK_TEXT, 0, vec![]);
            if kind == "docx" {
                docx::write(&d, Vec::new()).unwrap()
            } else {
                odt::write(&d, Vec::new()).unwrap()
            }
        }
        "xlsx" | "ods" => {
            let mut wb = officina_grid::Workbook::new();
            wb.add_sheet("Data").unwrap();
            wb.set_value(officina_grid::addr::CellAddr::new(0, 0, 0), officina_grid::Value::str(MARK_CELL));
            wb.set_value(officina_grid::addr::CellAddr::new(0, 1, 0), officina_grid::Value::Num(42.0));
            if kind == "xlsx" {
                xlsx::write(&wb, Vec::new()).unwrap()
            } else {
                ods::write(&wb, Vec::new()).unwrap()
            }
        }
        _ => {
            let mut deck = sample::sample();
            if kind == "pptx" {
                pptx::write(&mut deck, Vec::new()).unwrap()
            } else {
                odp::write(&mut deck, Vec::new()).unwrap()
            }
        }
    }
}

/// Whether our reader of `kind` finds the marker in `file` (a plain file).
fn has_marker(kind: &str, file: &[u8]) -> Result<bool, String> {
    let found = match kind {
        "docx" | "odt" => {
            let doc = if kind == "docx" { docx::read(file).map_err(|e| e.to_string())?.0 } else { odt::read(file).map_err(|e| e.to_string())?.0 };
            let found = doc.blocks.iter().any(|b| b.as_para().is_some_and(|p| p.text.contains(MARK_TEXT)));
            found
        }
        "xlsx" | "ods" => {
            let wb = if kind == "xlsx" { xlsx::read(file).map_err(|e| e.to_string())?.0 } else { ods::read(file).map_err(|e| e.to_string())?.0 };
            wb.get(officina_grid::addr::CellAddr::new(0, 0, 0)) == officina_grid::Value::str(MARK_CELL) && wb.get(officina_grid::addr::CellAddr::new(0, 1, 0)) == officina_grid::Value::Num(42.0)
        }
        _ => {
            let deck = if kind == "pptx" { pptx::read(file).map_err(|e| e.to_string())?.0 } else { odp::read(file).map_err(|e| e.to_string())?.0 };
            let native = officina_slides::native::save_to_vec(&deck);
            native.windows(MARK_SLIDE.len()).any(|w| w == MARK_SLIDE.as_bytes())
        }
    };
    Ok(found)
}

fn filter(kind: &str) -> &'static str {
    match kind {
        "docx" => "MS Word 2007 XML",
        "xlsx" => "Calc MS Excel 2007 XML",
        "pptx" => "Impress MS PowerPoint 2007 XML",
        "odt" => "writer8",
        "ods" => "calc8",
        _ => "impress8",
    }
}

/// Runs `jobs` in one soffice; one result line each.
fn run_lo(dir: &Path, jobs: &[[String; 5]], gcm: bool) -> Vec<String> {
    let driver = dir.join("driver.py");
    std::fs::write(&driver, DRIVER).unwrap();
    let list = dir.join(if gcm { "jobs-gcm.txt" } else { "jobs.txt" });
    std::fs::write(&list, jobs.iter().map(|j| j.join("\t")).collect::<Vec<_>>().join("\n")).unwrap();
    let profile = dir.join(if gcm { "profile-gcm" } else { "profile" });
    let out = Command::new("python3").arg(&driver).arg(if gcm { "2085" } else { "2084" }).arg(&profile).arg(&list).arg(if gcm { "1" } else { "0" }).output().expect("python3");
    let lines: Vec<String> = String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect();
    if lines.len() != jobs.len() {
        eprintln!("soffice: {}", String::from_utf8_lossy(&out.stderr));
    }
    lines
}

fn main() {
    let dir: PathBuf = std::env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join("password-lo"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut c = Checks { pass: 0, fail: 0 };
    const OURS: &str = "Ours-pw 1 ñ";
    const THEIRS: &str = "LO pw 2";
    let kinds = ["docx", "xlsx", "pptx", "odt", "ods", "odp"];
    let s = |p: PathBuf| p.to_string_lossy().into_owned();
    let mut jobs = Vec::new();
    let mut gcm_jobs = Vec::new();
    for k in kinds {
        let p = plain(k);
        c.check(&format!("{k}: our plain file reads back"), has_marker(k, &p) == Ok(true), || format!("{:?}", has_marker(k, &p)));
        std::fs::write(dir.join(format!("plain.{k}")), &p).unwrap();
        let ours = protect(k, &p, OURS).unwrap();
        std::fs::write(dir.join(format!("ours.{k}")), &ours).unwrap();
        // Ours to LibreOffice: opened with the password and saved plain;
        // a wrong password must not open it.
        jobs.push([
            "load".into(),
            s(dir.join(format!("ours.{k}"))),
            OURS.into(),
            s(dir.join(format!("ours-by-lo.{k}"))),
            filter(k).into(),
        ]);
        jobs.push(["load".into(), s(dir.join(format!("ours.{k}"))), "wrong".into(), s(dir.join(format!("ours-wrong.{k}"))), filter(k).into()]);
        // LibreOffice to ours.
        jobs.push(["save".into(), s(dir.join(format!("plain.{k}"))), THEIRS.into(), s(dir.join(format!("lo.{k}"))), filter(k).into()]);
        if k.starts_with("od") {
            gcm_jobs.push(["save".into(), s(dir.join(format!("plain.{k}"))), THEIRS.into(), s(dir.join(format!("lo-gcm.{k}"))), filter(k).into()]);
        }
    }
    let results = run_lo(&dir, &jobs, false);
    let gcm_results = run_lo(&dir, &gcm_jobs, true);
    let result = |r: &[String], i: usize| r.get(i).cloned().unwrap_or_else(|| "missing".into());
    for (i, k) in kinds.iter().enumerate() {
        let k = *k;
        // Ours opened by LibreOffice.
        let r = result(&results, i * 3);
        let back = std::fs::read(dir.join(format!("ours-by-lo.{k}"))).unwrap_or_default();
        c.check(&format!("{k}: LibreOffice opens ours with the password"), r == "ok" && has_marker(k, &back) == Ok(true), || format!("{r}; {:?}", has_marker(k, &back)));
        let r = result(&results, i * 3 + 1);
        c.check(&format!("{k}: LibreOffice refuses ours with a wrong password"), r.starts_with("fail") && !dir.join(format!("ours-wrong.{k}")).exists(), || r.clone());
        // LibreOffice's opened by us.
        let r = result(&results, i * 3 + 2);
        let theirs = std::fs::read(dir.join(format!("lo.{k}"))).unwrap_or_default();
        c.check(&format!("{k}: LibreOffice saves with a password"), r == "ok" && !theirs.is_empty() && !has_marker(k, &theirs).unwrap_or(false), || r.clone());
        their_file(&mut c, k, &theirs, THEIRS, "");
    }
    for (i, k) in kinds.iter().filter(|k| k.starts_with("od")).enumerate() {
        let r = result(&gcm_results, i);
        let theirs = std::fs::read(dir.join(format!("lo-gcm.{k}"))).unwrap_or_default();
        let whole = officina_convert::zip::Archive::open(&theirs).is_ok_and(|z| z.find("encrypted-package").is_some());
        c.check(&format!("{k}: LibreOffice saves AES-GCM/Argon2id (whole package)"), r == "ok" && whole, || format!("{r}; whole package {whole}"));
        their_file(&mut c, k, &theirs, THEIRS, " (AES-GCM)");
    }
    println!("{} passed, {} failed", c.pass, c.fail);
    if c.fail > 0 {
        std::process::exit(1);
    }
}

/// LibreOffice's protected `file` of `kind` opens with `pw` only.
fn their_file(c: &mut Checks, k: &str, file: &[u8], pw: &str, how: &str) {
    let opened = unprotect(k, file.to_vec(), Some(pw));
    let ok = opened.as_ref().map(|p| has_marker(k, p) == Ok(true)).unwrap_or(false);
    c.check(&format!("{k}: we open LibreOffice's{how} with the password"), ok, || format!("{:?}", opened.as_ref().map(|p| has_marker(k, p)).map_err(|e| e.clone())));
    let wrong = unprotect(k, file.to_vec(), Some("lo pw 2"));
    c.check(&format!("{k}: we refuse LibreOffice's{how} with a wrong password"), wrong.as_ref().err().map(String::as_str) == Some(WRONG_PASSWORD), || format!("{:?}", wrong.err()));
    let none = unprotect(k, file.to_vec(), None);
    c.check(&format!("{k}: we ask for LibreOffice's{how} password"), none.as_ref().err().map(String::as_str) == Some(NEEDS_PASSWORD), || format!("{:?}", none.err()));
}
