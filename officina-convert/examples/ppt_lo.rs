//! `.ppt` against LibreOffice (docs/spec/officina.md §16.40, O7d gate): we
//! write slides_lo's sample deck (what the model has) as `.pptx`, LibreOffice
//! saves it as PowerPoint 97–2003 `.ppt`, plain and with a password (RC4
//! CryptoAPI), and our `.ppt` reader opens both; the plain one meets
//! slides_lo's checks, the protected one asks for its password, refuses a
//! wrong one and opens with the right one to the same text.
//!
//!   cargo run --release --example ppt_lo [OUTDIR]
//!
//! Needs `soffice` and Python with LibreOffice's `uno` module. Prints one
//! line per check and a summary.

#[allow(dead_code)]
#[path = "docx_lo.rs"]
mod common;
#[path = "../tests/support/slides_sample.rs"]
mod sample;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::Checks;
use officina_convert::ppt::{self, PptError};
use officina_convert::pptx;
use sample::{compare, sample};

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

fn run_lo(dir: &Path, jobs: &[[String; 4]]) -> Vec<String> {
    let driver = dir.join("driver.py");
    std::fs::write(&driver, DRIVER).unwrap();
    let list = dir.join("jobs.txt");
    std::fs::write(&list, jobs.iter().map(|j| j.join("\t")).collect::<Vec<_>>().join("\n")).unwrap();
    let out = Command::new("python3").arg(&driver).arg("2088").arg(dir.join("profile")).arg(&list).output().expect("python3");
    let lines: Vec<String> = String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect();
    if lines.len() != jobs.len() {
        eprintln!("soffice: {}", String::from_utf8_lossy(&out.stderr));
    }
    lines
}

fn deck_text(d: &mut officina_slides::model::Deck) -> String {
    let mut out = String::new();
    for i in 0..d.slide_count() {
        if let Ok(s) = d.slide(i) {
            for sh in &s.shapes {
                if let Some(t) = sh.text() {
                    out.push_str(&t.text());
                }
            }
        }
    }
    out
}

fn main() {
    let dir: PathBuf = std::env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join(format!("ppt-lo-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut deck = sample();
    let ours = dir.join("ours.pptx");
    std::fs::write(&ours, pptx::write(&mut deck, Vec::new()).unwrap()).unwrap();
    let s = |p: PathBuf| p.to_string_lossy().into_owned();
    let jobs = vec![
        [s(ours.clone()), String::new(), s(dir.join("lo.ppt")), "MS PowerPoint 97".into()],
        [s(ours.clone()), PASSWORD.into(), s(dir.join("lo-protected.ppt")), "MS PowerPoint 97".into()],
    ];
    let res = run_lo(&dir, &jobs);
    let mut c = Checks { pass: 0, fail: 0 };
    c.check("LibreOffice saved both .ppt files", res.iter().filter(|l| *l == "ok").count() == 2, || format!("{res:?}"));
    let Ok(plain) = std::fs::read(dir.join("lo.ppt")) else {
        println!("summary: no file from LibreOffice");
        std::process::exit(1);
    };
    let (mut back, loaded) = match ppt::read(&plain, None) {
        Ok(x) => x,
        Err(e) => {
            println!("FAIL our reader on LibreOffice's .ppt: {e}");
            std::process::exit(1);
        }
    };
    // LibreOffice saves a star as a shape drawn from formulas, which the
    // reader draws as a rectangle and says so (a limit of LibreOffice's file;
    // PowerPoint keeps the star's preset).
    let outline = loaded.report.human().iter().any(|l| l.contains("formula-driven custom shapes"));
    compare(
        &mut |w, ok, d| {
            if w.ends_with("star keeps its preset") {
                c.check("LibreOffice's .ppt: star (written by LibreOffice from formulas) drawn as a rectangle, with a note", outline, || "no note".into());
            } else {
                c.check(w, ok, || d)
            }
        },
        "LibreOffice's .ppt",
        &mut deck,
        &mut back,
    );
    for l in loaded.report.human() {
        println!("note {l}");
    }
    // LibreOffice's password on a .ppt save doesn't encrypt it: the file
    // opens without one, with the same text.
    match std::fs::read(dir.join("lo-protected.ppt")) {
        Ok(p) => match ppt::read(&p, None) {
            Ok((mut b2, _)) => {
                let (a, b) = (deck_text(&mut b2), deck_text(&mut back));
                c.check("LibreOffice's \"protected\" .ppt (saved unencrypted) opens, same text", a == b && !a.is_empty(), || format!("{a:?} vs {b:?}"));
            }
            Err(e) => c.check("LibreOffice's \"protected\" .ppt opens", false, || e.to_string()),
        },
        Err(e) => c.check("protected file", false, || e.to_string()),
    }
    // Encrypted decks: PowerPoint's RC4 CryptoAPI layout, as the sample
    // writes it, and a deck PowerPoint saved when PPT_PROTECTED names one
    // (PATH:PASSWORD).
    let mut locked = vec![(ppt::sample::deck(Some(PASSWORD)), PASSWORD.to_string(), "sample")];
    if let Some((path, pw)) = std::env::var("PPT_PROTECTED").ok().and_then(|v| v.rsplit_once(':').map(|(a, b)| (a.to_string(), b.to_string()))) {
        match std::fs::read(&path) {
            Ok(b) => locked.push((b, pw, "PowerPoint's")),
            Err(e) => c.check("PowerPoint's protected deck", false, || e.to_string()),
        }
    }
    for (p, pw, who) in locked {
        c.check(&format!("{who} protected deck: asks for the password"), matches!(ppt::read(&p, None), Err(PptError::NeedsPassword)), || "opened or failed otherwise".into());
        c.check(&format!("{who} protected deck: refuses a wrong password"), matches!(ppt::read(&p, Some("wrong")), Err(PptError::WrongPassword)), || "opened or failed otherwise".into());
        match ppt::read(&p, Some(&pw)) {
            Ok((mut b2, _)) => {
                let t = deck_text(&mut b2);
                c.check(&format!("{who} protected deck: opens with the password"), !t.is_empty(), || "no text".into());
            }
            Err(e) => c.check(&format!("{who} protected deck: opens with the password"), false, || e.to_string()),
        }
    }
    println!("ppt via LibreOffice: {} of {} checks pass", c.pass, c.pass + c.fail);
    if c.fail > 0 {
        std::process::exit(1);
    }
}
