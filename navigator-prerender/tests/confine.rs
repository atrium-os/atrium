//! The require-confinement switch and the serve protocol, end to end, on
//! whatever host runs the suite (threat model §2.2). The jailed half — the
//! same binary under `portcullis exec` — is scripts/navigator-converter-jail-e2e.sh.

use std::io::Write;
use std::process::{Command, Stdio};

const EXE: &str = env!("CARGO_BIN_EXE_prerender");

fn scratch(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("prerender-confine-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn corpus_of_one(name: &str, html: &str) -> std::path::PathBuf {
    let d = scratch(name);
    std::fs::write(d.join("doc.html"), html).unwrap();
    d
}

fn driver(dir: &std::path::Path, env: &[(&str, &str)]) -> (i32, String, String) {
    let mut c = Command::new(EXE);
    c.arg(dir);
    for k in ["PRERENDER_LAUNCHER", "PRERENDER_FETCHD", "PRERENDER_REQUIRE_CONFINEMENT", "PRERENDER_NET",
              "PRERENDER_EMIT_DIR", "PRERENDER_BLOB_DIR", "PRERENDER_DEADLINE", "PRERENDER_FETCH_BUDGET",
              "PRERENDER_EXPLORE", "PRERENDER_COVERAGE", "PRERENDER_PROGRESSIVE", "PRERENDER_PARTS_DIR",
              "PRERENDER_TIMING", "PRERENDER_TIER1"] { c.env_remove(k); }
    for (k, v) in env { c.env(k, v); }
    let o = c.output().unwrap();
    (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stdout).into(), String::from_utf8_lossy(&o.stderr).into())
}

const PAGE: &str = "<!doctype html><title>t</title><p id=a>one</p>\
                    <script>document.getElementById('a').textContent='two'</script>";

#[test]
fn required_confinement_refuses_without_a_launcher() {
    let d = corpus_of_one("nolauncher", PAGE);
    let (code, out, err) = driver(&d, &[("PRERENDER_REQUIRE_CONFINEMENT", "1")]);
    assert_eq!(code, 1, "{out}{err}");
    assert!(err.contains("REFUSED") && err.contains("PRERENDER_LAUNCHER"), "{err}");
}

#[test]
fn required_confinement_refuses_a_networked_run_without_fetchd() {
    let d = corpus_of_one("nofetchd", PAGE);
    let (code, _, err) = driver(&d, &[("PRERENDER_REQUIRE_CONFINEMENT", "1"), ("PRERENDER_NET", "1"),
                                      ("PRERENDER_LAUNCHER", EXE)]);
    assert_eq!(code, 1);
    assert!(err.contains("PRERENDER_FETCHD"), "{err}");
}

#[test]
fn a_launcher_that_does_not_jail_is_caught_by_the_child_itself() {
    // ★ The "launcher" here is the bare binary: it claims nothing and jails
    // nothing. With confinement required, the child asks the kernel and
    // refuses — the driver's own config is not evidence of a jail.
    let d = corpus_of_one("fakejail", PAGE);
    let (code, out, _) = driver(&d, &[("PRERENDER_REQUIRE_CONFINEMENT", "1"), ("PRERENDER_LAUNCHER", EXE)]);
    assert_eq!(code, 0, "the batch completes, with the document counted as failed");
    assert!(out.contains("not in a jail"), "{out}");
    assert!(out.contains("TIMED OUT / crashed 1"), "{out}");
}

#[test]
fn unconfined_is_named_as_such() {
    let d = corpus_of_one("named", PAGE);
    let (code, out, _) = driver(&d, &[]);
    assert_eq!(code, 0);
    assert!(out.starts_with("confinement: none — UNCONFINED, for testing"), "{out}");
}

#[test]
fn the_recording_comes_back_over_the_pipe_and_matches_the_one_document_path() {
    let d = corpus_of_one("emit", PAGE);
    let served = scratch("emit-served");
    let (code, out, err) = driver(&d, &[("PRERENDER_EMIT_DIR", served.to_str().unwrap())]);
    assert_eq!(code, 0, "{out}{err}");
    let a = std::fs::read(served.join("doc.json")).expect("a recording");
    // The legacy single-document path writes it from inside the child.
    let direct = scratch("emit-direct");
    let o = Command::new(EXE).args(["--one", d.join("doc.html").to_str().unwrap(), ""])
        .env("PRERENDER_EMIT_DIR", &direct).env_remove("PRERENDER_NET").output().unwrap();
    assert!(o.status.success());
    let b = std::fs::read(direct.join("doc.json")).unwrap();
    assert_eq!(a, b, "serve mode must produce the same recording byte for byte");
    assert!(String::from_utf8_lossy(&a).contains("two"), "the script ran");
}

#[test]
fn launcher_chatter_on_stdout_fails_the_document_loudly() {
    let d = corpus_of_one("chatter", PAGE);
    let sh = d.join("chatty-launcher.sh");
    std::fs::write(&sh, format!("#!/bin/sh\necho 'org_atrium_navigator_converter__1: created'\nexec {EXE}\n")).unwrap();
    let _ = Command::new("chmod").arg("+x").arg(&sh).status();
    std::fs::remove_file(d.join("chatty-launcher.sh.unused")).ok();
    // The launcher script itself lives in the corpus dir; keep it out of the batch.
    let corpus = scratch("chatter-corpus");
    std::fs::write(corpus.join("doc.html"), PAGE).unwrap();
    let (_, out, _) = driver(&corpus, &[("PRERENDER_LAUNCHER", sh.to_str().unwrap())]);
    assert!(out.contains("launcher chatter"), "{out}");
}

#[test]
fn a_page_that_never_finishes_is_stopped_by_its_own_deadline() {
    let d = corpus_of_one("spin", "<script>for(;;){}</script>");
    let t = std::time::Instant::now();
    let (code, out, _) = driver(&d, &[("PRERENDER_DEADLINE", "2")]);
    assert_eq!(code, 0);
    assert!(out.contains("timed out at 2s"), "{out}");
    // The child's own watchdog fires at 2 s; the driver's backstop would be
    // 2 + 10 + 10.
    assert!(t.elapsed() < std::time::Duration::from_secs(10), "{:?}", t.elapsed());
}

#[test]
fn the_serve_entry_reads_only_stdin() {
    // No arguments, no environment: the jail's view.
    let mut c = Command::new(EXE).env_clear().stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().unwrap();
    let job = navigator_prerender::serve::Job { len: PAGE.len(), emit: true, ..Default::default() };
    let mut w = c.stdin.take().unwrap();
    w.write_all(job.header().unwrap().as_bytes()).unwrap();
    w.write_all(PAGE.as_bytes()).unwrap();
    drop(w);
    let o = c.wait_with_output().unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(out.starts_with("R\t"), "{out}");
    assert!(out.contains("\nRECORDING ") && out.ends_with("END\n"), "{out}");
}

#[test]
fn an_inherited_hard_limit_below_the_ask_is_kept_not_refused() {
    // The child is started under a CPU hard limit LOWER than the one it will
    // ask for (deadline 30 + 5 = 35 s, inherited 20 s). It must convert, not
    // fail trying to raise it. `ulimit -H -t` sets the inherited hard limit.
    let d = corpus_of_one("lowhard", PAGE);
    let o = Command::new("sh").arg("-c").arg(format!("ulimit -t 20 && exec {EXE} {}", d.display()))
        .env_remove("PRERENDER_LAUNCHER").env_remove("PRERENDER_REQUIRE_CONFINEMENT").output().unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(o.status.success(), "{out}{}", String::from_utf8_lossy(&o.stderr));
    assert!(!out.contains("TIMED OUT / crashed"), "{out}");
}

/// ★ The per-conversion byte budget (threat model §2.2, P2) is enforced by
/// the DRIVER's relay, not the converter: five 100 KB scripts against a
/// 250 KB budget deliver two, refuse the third after pulling it, and refuse
/// the last two without asking upstream at all. The report names the
/// document and the reason.
#[test]
fn the_relay_enforces_a_per_conversion_byte_budget() {
    let d = scratch("budget");
    let log = d.join("upstream.log");
    let fake = d.join("fetchd.sh");
    std::fs::write(&fake, format!(
        "while read u; do case \"$u\" in 'H '*|'B '*) echo \"$u\" >> {log}.hints; continue;; esac; \
         echo \"$u\" >> {log}; printf 'ok 200 0 100000 %s\\n' \"$u\"; \
         head -c 100000 /dev/zero | tr '\\0' ' '; done\n", log = log.display())).unwrap();
    let docs = d.join("docs");
    std::fs::create_dir_all(&docs).unwrap();
    let mut page = String::from("<!doctype html><title>t</title><p>x</p>");
    for i in 0..5 { page.push_str(&format!("<script src=\"https://a.example/s{i}.js\"></script>")); }
    std::fs::write(docs.join("doc.html"), page).unwrap();
    let fetchd = format!("sh {}", fake.display());
    let (code, out, err) = driver(&docs, &[("PRERENDER_NET", "1"), ("PRERENDER_FETCHD", &fetchd),
                                          ("PRERENDER_FETCH_BUDGET", "250000")]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("FETCH BUDGET refused requests in 1 documents"), "{out}");
    assert!(out.contains("doc.html  (fetch budget exhausted: 300000 bytes pulled of a 250000-byte limit; \
                          3 request(s) refused, first https://a.example/s2.js)"), "{out}");
    let asked = std::fs::read_to_string(&log).unwrap();
    assert_eq!(asked.lines().count(), 3, "upstream is not asked once the budget is spent: {asked}");
    // ★ Hinted, all five, after the budget — which bounds what hints pull.
    let hints = std::fs::read_to_string(format!("{}.hints", log.display())).unwrap();
    assert_eq!(hints.lines().next(), Some("B 250000"), "{hints}");
    assert_eq!(hints.lines().filter(|l| l.starts_with("H https://a.example/s")).count(), 5, "{hints}");
    // Unbounded by a large budget, nothing is refused.
    let _ = std::fs::remove_file(&log);
    let (_, out, _) = driver(&docs, &[("PRERENDER_NET", "1"), ("PRERENDER_FETCHD", &fetchd)]);
    assert!(!out.contains("FETCH BUDGET"), "{out}");
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 5);
}

/// ★ Progressive delivery through the real driver and child (§5.4.5.7): the
/// three parts are written, the whole recording the driver emits is their
/// merge, and it is byte-identical to a non-progressive run's. The timing
/// line says each artifact arrived, the first one before the readable one.
#[test]
fn a_progressive_run_writes_its_parts_and_the_same_whole_recording() {
    let page = "<!doctype html><title>t</title><p id=a>one</p><div id=p class=c>x</div>\
                <script>document.getElementById('a').textContent='two';\
                document.getElementById('a').addEventListener('click',function(){\
                document.getElementById('p').setAttribute('class','o')})</script>";
    let d = corpus_of_one("progressive", page);
    let (plain, prog, parts) = (d.join("plain"), d.join("prog"), d.join("parts"));
    let timing = d.join("t.tsv");
    let (code, out, err) = driver(&d, &[("PRERENDER_EXPLORE", "1"), ("PRERENDER_EMIT_DIR", plain.to_str().unwrap())]);
    assert_eq!(code, 0, "{out}{err}");
    let (code, out, err) = driver(&d, &[("PRERENDER_EXPLORE", "1"), ("PRERENDER_PROGRESSIVE", "1"),
        ("PRERENDER_EMIT_DIR", prog.to_str().unwrap()), ("PRERENDER_PARTS_DIR", parts.to_str().unwrap()),
        ("PRERENDER_TIMING", timing.to_str().unwrap())]);
    assert_eq!(code, 0, "{out}{err}");
    let a = std::fs::read(plain.join("doc.json")).unwrap();
    let b = std::fs::read(prog.join("doc.json")).unwrap();
    assert_eq!(String::from_utf8_lossy(&a), String::from_utf8_lossy(&b));
    let first = std::fs::read_to_string(parts.join("doc.first.json")).unwrap();
    assert!(first.contains("first display: scripts not run yet") && first.contains(">one</p>"), "{first}");
    assert!(std::fs::read_to_string(parts.join("doc.readable.json")).unwrap().contains(">two</p>"));
    assert!(std::fs::read_to_string(parts.join("doc.transitions.json")).unwrap().contains("atrium-navigator-transitions/1"));
    let t = std::fs::read_to_string(&timing).unwrap();
    let field = |k: &str| t.split('\t').find_map(|f| f.strip_prefix(&format!("{k}="))).and_then(|v| v.trim().parse::<f64>().ok());
    let (f, r, x) = (field("arrive_first").unwrap(), field("arrive_recording").unwrap(), field("arrive_transitions").unwrap());
    assert!(f <= r && r <= x, "{t}");
    assert!(t.contains("first_display_normalize=") && t.contains("\tdisplay_normalize="), "{t}");
}
