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
              "PRERENDER_EMIT_DIR", "PRERENDER_BLOB_DIR", "PRERENDER_DEADLINE"] { c.env_remove(k); }
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
