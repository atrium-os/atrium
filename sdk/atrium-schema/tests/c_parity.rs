//! The generated C decodes exactly as the Rust decoder does: for valid
//! frames and thousands of damaged ones, the same accept/refuse, the same
//! error, and the same bytes when re-encoded. Built with ASan and UBSan.

use atrium_message::{Error, Message};
use std::io::Write;
use std::process::{Command, Stdio};

#[derive(Message, Debug, Clone, PartialEq)]
#[message(version = 3)]
struct Thumb {
    #[message(min = 1, max = 4096)]
    width: u32,
    height: u16,
    delta: i64,
    #[message(min = -100, max = 100)]
    small: i8,
    ok: bool,
    scale: f32,
    precise: f64,
    #[message(max_len = 64)]
    pixels: Vec<u8>,
    #[message(max_len = 16, no_control)]
    title: String,
    tags: Option<Vec<String>>,
    #[message(max_len = 4, item_max_len = 8)]
    words: Vec<String>,
    shape: Shape,
    shapes: Vec<Shape>,
    pair: Pair,
    rgb: [u8; 3],
    #[message(max = 1000)]
    maybe: Option<u32>,
    grid: Vec<[u16; 2]>,
    next: Option<Box<Thumb>>,
}

#[derive(Message, Debug, Clone, PartialEq)]
enum Shape {
    Dot,
    Circle(u32),
    Rect { w: u32, #[message(min = -10, max = 10)] h: i32 },
    Named { #[message(max_len = 8)] label: String, int: i16 },
}

#[derive(Message, Debug, Clone, PartialEq)]
struct Pair(u64, String);

fn leaf() -> Thumb {
    Thumb {
        width: 1, height: 0, delta: 0, small: 0, ok: false, scale: 0.0, precise: 0.0, pixels: vec![], title: String::new(),
        tags: None, words: vec![], shape: Shape::Dot, shapes: vec![], pair: Pair(0, String::new()), rgb: [0; 3],
        maybe: None, grid: vec![], next: None,
    }
}

fn samples() -> Vec<Thumb> {
    let full = Thumb {
        width: 640, height: 480, delta: -1234567890123, small: -5, ok: true, scale: 1.5, precise: -2.25e300,
        pixels: vec![1, 2, 3, 255], title: "photo\tone é".into(), tags: Some(vec!["a".into(), "bé".into(), "日本".into()]),
        words: vec!["x".into(), "yy".into()], shape: Shape::Rect { w: 9, h: -3 },
        shapes: vec![Shape::Circle(70000), Shape::Named { label: "n".into(), int: -300 }, Shape::Dot],
        pair: Pair(u64::MAX, "p".into()), rgb: [1, 2, 3], maybe: Some(999), grid: vec![[1, 65535], [300, 0]],
        next: Some(Box::new(Thumb { next: Some(Box::new(leaf())), ..leaf() })),
    };
    vec![full.clone(), leaf(), Thumb { scale: f32::NAN, precise: f64::INFINITY, tags: Some(vec![]), ..full }]
}

fn code(e: &Error) -> i32 {
    match e {
        Error::Truncated => -1, Error::TrailingBytes => -2, Error::BadVarint => -3, Error::BadTag => -4,
        Error::TooLong { .. } => -5, Error::OutOfRange { .. } => -6, Error::BadUtf8 { .. } => -7,
        Error::ControlChar { .. } => -8, Error::BadVariant { .. } => -9, Error::TooDeep => -10,
        Error::Version { .. } => -11, Error::FrameTooLarge { .. } => -12,
    }
}

fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{x:02x}")).collect() }

const HARNESS: &str = r#"
#include <stdio.h>
#include "thumb.h"
static int nib(int c) { return c <= '9' ? c - '0' : c - 'a' + 10; }
int main(void) {
    static char line[1 << 16]; static uint8_t in[1 << 15];
    while (fgets(line, sizeof line, stdin)) {
        size_t n = 0;
        for (char *p = line; p[0] && p[0] != '\n' && p[1]; p += 2) in[n++] = (uint8_t)(nib(p[0]) * 16 + nib(p[1]));
        struct t_thumb t; atrium_arena a = { 0 };
        int rc = t_thumb_decode_frame(in, n, 4096, &t, &a);
        if (rc) { printf("E%d\n", rc); atrium_arena_free(&a); continue; }
        atrium_buf b = { 0 };
        rc = t_thumb_encode_frame(&t, &b);
        if (rc) { printf("encode failed\n"); return 1; }
        for (size_t i = 0; i < b.len; i++) printf("%02x", b.ptr[i]);
        printf("\n");
        atrium_buf_free(&b); atrium_arena_free(&a);
    }
    return 0;
}
"#;

#[test]
fn c_decodes_as_rust_does() {
    let dir = std::env::temp_dir().join(format!("atrium-schema-parity-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (h, c) = atrium_schema::generate(&Thumb::schema_json(), "t_", "thumb.h").unwrap();
    std::fs::write(dir.join("thumb.h"), h).unwrap();
    std::fs::write(dir.join("thumb.c"), c).unwrap();
    std::fs::write(dir.join("main.c"), HARNESS).unwrap();
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let out = Command::new(&cc)
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-Wno-unused-function", "-O1", "-g",
               "-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-o"])
        .arg(dir.join("harness")).arg(dir.join("thumb.c")).arg(dir.join("main.c")).output().unwrap();
    assert!(out.status.success(), "cc failed:\n{}", String::from_utf8_lossy(&out.stderr));

    // Inputs: each sample's frame, every truncation, flips, insertions, garbage.
    let mut inputs: Vec<Vec<u8>> = Vec::new();
    let mut seed: u64 = 0x9e3779b97f4a7c15;
    let mut rnd = || { seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17; seed };
    for s in samples() {
        let f = s.to_frame();
        inputs.push(f.clone());
        for k in 0..f.len() {
            inputs.push(f[..k].to_vec());
        }
        for _ in 0..1500 {
            let mut g = f.clone();
            let flips = 1 + rnd() % 3;
            for _ in 0..flips {
                let at = (rnd() as usize) % g.len();
                match rnd() % 4 {
                    0 => g[at] ^= 1 << (rnd() % 8),
                    1 => g[at] = rnd() as u8,
                    2 => g.insert(at, rnd() as u8),
                    _ => { g.remove(at); if g.is_empty() { g.push(3); } }
                }
            }
            inputs.push(g);
        }
    }
    for _ in 0..500 {
        let n = (rnd() % 64) as usize;
        let mut g: Vec<u8> = (0..n).map(|_| rnd() as u8).collect();
        if !g.is_empty() { g[0] = 3; }
        inputs.push(g);
    }
    inputs.push(vec![0u8; 5000]); // over the frame limit

    let expect: Vec<String> = inputs.iter().map(|i| match Thumb::from_frame(i, 4096) {
        Ok(v) => hex(&v.to_frame()),
        Err(e) => format!("E{}", code(&e)),
    }).collect();
    let mut child = Command::new(dir.join("harness")).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    // Fed from a thread: the harness's output would fill its pipe otherwise.
    let mut stdin = child.stdin.take().unwrap();
    let lines: Vec<String> = inputs.iter().map(|i| hex(i)).collect();
    let feeder = std::thread::spawn(move || {
        for l in lines {
            writeln!(stdin, "{l}").unwrap();
        }
    });
    let out = child.wait_with_output().unwrap();
    feeder.join().unwrap();
    assert!(out.status.success(), "harness crashed");
    let got: Vec<String> = String::from_utf8(out.stdout).unwrap().lines().map(str::to_string).collect();
    assert_eq!(got.len(), expect.len());
    let accepted = expect.iter().filter(|e| !e.starts_with('E')).count();
    let mut kinds: Vec<&str> = expect.iter().filter(|e| e.starts_with('E')).map(|s| s.as_str()).collect();
    kinds.sort();
    kinds.dedup();
    for (i, (g, e)) in got.iter().zip(&expect).enumerate() {
        assert_eq!(g, e, "input {i}: {}", hex(&inputs[i]));
    }
    // The test is only worth something if both sides took many and refused many kinds.
    assert!(accepted > 50, "only {accepted} accepted");
    assert!(kinds.len() >= 8, "refusal kinds seen: {kinds:?}");
    eprintln!("{} inputs, {accepted} accepted, refusals {kinds:?}", inputs.len());
    let _ = std::fs::remove_dir_all(&dir);
}
