use super::*;
use pergola::code::CodeBuffer;

fn tags(line: &[Span], text: &str) -> Vec<(String, Tag)> {
    let chars: Vec<char> = text.chars().collect();
    line.iter().map(|s| (chars[s.cols.clone()].iter().collect(), s.tag)).collect()
}

fn has(spans: &[(String, Tag)], word: &str, tag: Tag) -> bool {
    spans.iter().any(|(w, t)| w == word && *t == tag)
}

#[test]
fn every_query_compiles() {
    for lang in Lang::ALL {
        let mut s = Syntax::new(lang).unwrap_or_else(|e| panic!("{}: {e}", lang.name()));
        assert!(s.parse(&Rope::from_str("")), "{}", lang.name());
    }
}

#[test]
fn languages_from_paths() {
    assert_eq!(Lang::from_path(Path::new("src/main.rs")), Some(Lang::Rust));
    assert_eq!(Lang::from_path(Path::new("a/b.H")), Some(Lang::C));
    assert_eq!(Lang::from_path(Path::new("x.hpp")), Some(Lang::Cpp));
    assert_eq!(Lang::from_path(Path::new("Cargo.lock")), Some(Lang::Toml));
    assert_eq!(Lang::from_path(Path::new("atrium.toml")), Some(Lang::Toml));
    assert_eq!(Lang::from_path(Path::new("README.md")), Some(Lang::Markdown));
    assert_eq!(Lang::from_path(Path::new("Makefile")), None);
    assert_eq!(Lang::from_path(Path::new("noext")), None);
    assert_eq!(Lang::from_shebang("#!/bin/sh"), Some(Lang::Shell));
    assert_eq!(Lang::from_shebang("#!/usr/bin/env -S python3 -u"), Some(Lang::Python));
    assert_eq!(Lang::from_shebang("#!/usr/bin/perl"), None);
    assert_eq!(Lang::Rust.comment(), Some("//"));
    assert_eq!(Lang::Json.comment(), None);
}

#[test]
fn rust_highlights() {
    let src = "// hi\nfn main() {\n    let x = \"s\";\n    let n = 42;\n}\n";
    let mut s = Syntax::new(Lang::Rust).unwrap();
    assert!(s.parse(&Rope::from_str(src)));
    let rope = Rope::from_str(src);
    let h = s.highlight(&rope, 0..5);
    let l0 = tags(&h.line(0), "// hi");
    assert_eq!(l0, vec![("// hi".to_string(), Tag::Comment)]);
    let l1 = tags(&h.line(1), "fn main() {");
    assert!(has(&l1, "fn", Tag::Keyword), "{l1:?}");
    assert!(has(&l1, "main", Tag::Function), "{l1:?}");
    let l2 = tags(&h.line(2), "    let x = \"s\";");
    assert!(has(&l2, "let", Tag::Keyword), "{l2:?}");
    assert!(has(&l2, "\"s\"", Tag::String), "{l2:?}");
    let l3 = tags(&h.line(3), "    let n = 42;");
    assert!(has(&l3, "42", Tag::Number) || has(&l3, "42", Tag::Constant), "{l3:?}");
    // Outside the asked range: nothing.
    assert!(h.line(9).is_empty());
    assert!(h.covers(&(1..4)));
    assert!(!h.covers(&(3..7)));
}

#[test]
fn columns_are_chars_not_bytes() {
    let src = "let é = \"ü\"; // ö\n";
    let mut s = Syntax::new(Lang::Rust).unwrap();
    let rope = Rope::from_str(src);
    s.parse(&rope);
    let h = s.highlight(&rope, 0..1);
    let line = "let é = \"ü\"; // ö";
    let spans = tags(&h.line(0), line);
    assert!(has(&spans, "\"ü\"", Tag::String), "{spans:?}");
    assert!(has(&spans, "// ö", Tag::Comment), "{spans:?}");
}

#[test]
fn incremental_edits_match_a_fresh_parse() {
    let src = "fn a() {}\n\nfn b() -> u32 {\n    1\n}\n";
    let mut buf = CodeBuffer::new(src);
    let mut s = Syntax::new(Lang::Rust).unwrap();
    s.parse(buf.text());

    // Type a string literal and a comment, delete some, add lines.
    buf.set_caret(buf.at_line_col(1, 0), false);
    buf.insert("const S: &str = \"x\";");
    buf.newline();
    buf.insert("// note é");
    buf.set_caret(buf.at_line_col(4, 4), false);
    buf.delete(true);
    buf.insert("2 + 3");
    buf.select_all();
    buf.set_caret(0, false);
    buf.insert("/* c */ ");
    { let e = buf.take_edits(); s.edit(buf.text(), &e); }

    let mut fresh = Syntax::new(Lang::Rust).unwrap();
    fresh.parse(buf.text());
    assert_eq!(
        s.tree().unwrap().root_node().to_sexp(),
        fresh.tree().unwrap().root_node().to_sexp(),
        "{}",
        buf.to_string()
    );
    let n = buf.len_lines();
    let a = s.highlight(buf.text(), 0..n);
    let b = fresh.highlight(buf.text(), 0..n);
    for l in 0..n {
        assert_eq!(a.line(l), b.line(l), "line {l}");
    }
    assert!(!s.stale());

    // Undo back to the start reparses to the original tree.
    while buf.undo() {}
    { let e = buf.take_edits(); s.edit(buf.text(), &e); }
    let mut orig = Syntax::new(Lang::Rust).unwrap();
    orig.parse(&Rope::from_str(src));
    assert_eq!(buf.to_string(), src);
    assert_eq!(s.tree().unwrap().root_node().to_sexp(), orig.tree().unwrap().root_node().to_sexp());
}

#[test]
fn a_parse_out_of_time_keeps_the_old_tree() {
    let mut s = Syntax::new(Lang::C).unwrap();
    let small = Rope::from_str("int x;\n");
    assert!(s.parse(&small));
    // Big enough that a zero budget can't finish it.
    let big: String = (0..20_000).map(|i| format!("int f{i}(void) {{ return {i}; }}\n")).collect();
    let big = Rope::from_str(&big);
    s.set_budget(Duration::ZERO);
    assert!(!s.parse(&big));
    assert!(s.stale());
    // With time again it parses, and the parser isn't wedged.
    s.set_budget(Duration::from_secs(30));
    assert!(s.parse(&big));
    assert!(!s.stale());
    let h = s.highlight(&big, 19_999..20_000);
    assert!(!h.line(19_999).is_empty());
}

#[test]
fn other_languages_colour_something() {
    let cases: &[(Lang, &str, &str, Tag)] = &[
        (Lang::C, "int main(void) { return 0; } // c", "return", Tag::Keyword),
        (Lang::Cpp, "class A { public: int x; }; // c", "class", Tag::Keyword),
        (Lang::Toml, "[package]\nname = \"x\"", "\"x\"", Tag::String),
        (Lang::Shell, "echo \"hi\" # c", "# c", Tag::Comment),
        (Lang::Python, "def f():\n    return 1", "def", Tag::Keyword),
        (Lang::Json, "{\"a\": 1}", "1", Tag::Number),
        (Lang::Markdown, "# Title\n\ntext", "Title", Tag::Plain),
    ];
    for &(lang, src, word, tag) in cases {
        let mut s = Syntax::new(lang).unwrap();
        let rope = Rope::from_str(src);
        assert!(s.parse(&rope));
        let h = s.highlight(&rope, 0..rope.len_lines());
        let found: Vec<(String, Tag)> = (0..rope.len_lines())
            .flat_map(|l| tags(&h.line(l), &rope.line(l).to_string()))
            .collect();
        if tag == Tag::Plain {
            assert!(!found.is_empty(), "{}: nothing coloured", lang.name());
        } else {
            assert!(found.iter().any(|(w, t)| w.trim_end() == word && *t == tag), "{}: {found:?}", lang.name());
        }
    }
}

#[test]
fn garbage_parses_without_panic() {
    let mut seed = 0x2545F4914F6CDD1Du64;
    for lang in Lang::ALL {
        let mut s = Syntax::new(lang).unwrap();
        let mut buf = CodeBuffer::new("");
        for _ in 0..300 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let pieces = ["{", "}", "\"", "'", "/*", "*/", "\n", "#", "é", "fn ", "(", ")", "`", "\\", "x", " "];
            buf.insert(pieces[(seed % pieces.len() as u64) as usize]);
            if seed % 7 == 0 {
                buf.backspace(false);
            }
            if seed % 11 == 0 {
                let at = (seed >> 8) as usize % (buf.len_chars() + 1);
                buf.set_caret(at, false);
            }
            if seed % 5 == 0 {
                { let e = buf.take_edits(); s.edit(buf.text(), &e); }
                let n = buf.len_lines();
                let _ = s.highlight(buf.text(), 0..n);
            }
        }
    }
}
