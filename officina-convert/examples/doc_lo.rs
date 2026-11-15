//! `.doc` against LibreOffice (docs/spec/officina.md §16.40, O7c gate): we
//! write docx_lo's sample (everything the model has) as `.docx`, LibreOffice
//! saves it as Word 97–2003 `.doc`, plain and with a password (RC4
//! CryptoAPI), and our `.doc` reader opens both; the plain one meets
//! docx_lo's checks (but for comment threads, which the format as
//! LibreOffice writes it doesn't keep), the protected one asks for its password, refuses a
//! wrong one and opens with the right one to the same text.
//!
//!   cargo run --release --example doc_lo [OUTDIR]
//!
//! Needs `soffice` and Python with LibreOffice's `uno` module. Prints one
//! line per check and a summary.

#[allow(dead_code)]
#[path = "docx_lo.rs"]
mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::{compare, sample, Checks};
use officina_convert::doc::{self, DocError};
use officina_convert::docx;

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
    let out = Command::new("python3").arg(&driver).arg("2087").arg(dir.join("profile")).arg(&list).output().expect("python3");
    let lines: Vec<String> = String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect();
    if lines.len() != jobs.len() {
        eprintln!("soffice: {}", String::from_utf8_lossy(&out.stderr));
    }
    lines
}

fn main() {
    let dir: PathBuf = std::env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join(format!("doc-lo-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let d = sample();
    let ours = dir.join("ours.docx");
    std::fs::write(&ours, docx::write(&d, Vec::new()).unwrap()).unwrap();
    let s = |p: PathBuf| p.to_string_lossy().into_owned();
    let jobs = vec![
        [s(ours.clone()), String::new(), s(dir.join("lo.doc")), "MS Word 97".into()],
        [s(ours.clone()), PASSWORD.into(), s(dir.join("lo-protected.doc")), "MS Word 97".into()],
    ];
    let res = run_lo(&dir, &jobs);
    let mut c = Checks { pass: 0, fail: 0 };
    c.check("LibreOffice saved both .doc files", res.iter().filter(|l| *l == "ok").count() == 2, || format!("{res:?}"));
    let Ok(plain) = std::fs::read(dir.join("lo.doc")) else {
        println!("summary: no file from LibreOffice");
        std::process::exit(1);
    };
    let (back, loaded) = match doc::read(&plain, None) {
        Ok(x) => x,
        Err(e) => {
            println!("FAIL our reader on LibreOffice's .doc: {e}");
            std::process::exit(1);
        }
    };
    // Word 97–2003 files as LibreOffice writes them keep no comment threads
    // or resolved state: the reply comes back as a comment of its own over
    // the same words.
    let mut want = d.clone();
    {
        use officina_core::history::Op;
        use officina_text::ops::TextOp;
        let (id, (sb, so), (eb, eo)) = want.comment_ranges()[0];
        let reply = want.comments.replies(id)[0];
        let mut r = (**want.comments.get(reply).unwrap()).clone();
        TextOp::SetComment { id: reply, comment: None }.apply(&mut want);
        r.parent = None;
        r.done = false;
        for op in want.add_comment_ops(sb, so, eb, eo, r) {
            op.apply(&mut want);
        }
    }
    compare(&mut c, &want, &back);
    for l in loaded.report.human() {
        println!("note {l}");
    }
    match std::fs::read(dir.join("lo-protected.doc")) {
        Ok(p) => {
            c.check("protected: the text isn't in the clear", !p.windows(9).any(|w| w == b"A heading") && !p.windows(18).any(|w| w == "A heading".encode_utf16().flat_map(|u| u.to_le_bytes()).collect::<Vec<u8>>().as_slice()), || "found".into());
            c.check("protected: asks for the password", matches!(doc::read(&p, None), Err(DocError::NeedsPassword)), || "opened or failed otherwise".into());
            c.check("protected: refuses a wrong password", matches!(doc::read(&p, Some("wrong")), Err(DocError::WrongPassword)), || "opened or failed otherwise".into());
            match doc::read(&p, Some(PASSWORD)) {
                Ok((b2, _)) => c.check("protected: opens with the password, same text", b2.plain_text() == back.plain_text(), || format!("{:?}", b2.plain_text())),
                Err(e) => c.check("protected: opens with the password", false, || e.to_string()),
            }
        }
        Err(e) => c.check("protected file", false, || e.to_string()),
    }
    println!("doc via LibreOffice: {} of {} checks pass", c.pass, c.pass + c.fail);
    if c.fail > 0 {
        std::process::exit(1);
    }
}
