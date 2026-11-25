use super::*;

fn buf(text: &str) -> CodeBuffer {
    CodeBuffer::new(text)
}

/// A buffer whose selections are marked in `text`: `|` a caret, `[...]` a
/// selection (anchor at `[`, head at `]`), `]...[` a backward one.
fn marked(text: &str) -> CodeBuffer {
    let mut plain = String::new();
    let mut sels = Vec::new();
    let mut open: Option<(usize, char)> = None;
    let mut n = 0;
    for c in text.chars() {
        match c {
            '|' => sels.push(Selection::caret(n)),
            '[' if open.is_none() => open = Some((n, '[')),
            ']' if open.is_none() => open = Some((n, ']')),
            ']' if matches!(open, Some((_, '['))) => { let (a, _) = open.take().unwrap(); sels.push(Selection { anchor: a, head: n, goal: None }) }
            '[' if matches!(open, Some((_, ']'))) => { let (a, _) = open.take().unwrap(); sels.push(Selection { anchor: n, head: a, goal: None }) }
            c => { plain.push(c); n += 1; }
        }
    }
    let mut b = CodeBuffer::new(&plain);
    let p = sels.len().saturating_sub(1);
    b.set_selections(sels, p);
    b
}

/// The buffer with its selections marked as [`marked`] reads them.
fn show(b: &CodeBuffer) -> String {
    let text: Vec<char> = b.to_string().chars().collect();
    let mut out = String::new();
    for i in 0..=text.len() {
        for s in b.selections() {
            if s.is_empty() && s.head == i { out.push('|') }
            else if !s.is_empty() && s.range().start == i { out.push(if s.head > s.anchor { '[' } else { ']' }) }
            else if !s.is_empty() && s.range().end == i { out.push(if s.head > s.anchor { ']' } else { '[' }) }
        }
        if i < text.len() {
            out.push(text[i]);
        }
    }
    out
}

#[test]
fn marks_round_trip() {
    for t in ["a|b", "[ab]c", "a]bc[", "|x\n|y"] {
        assert_eq!(show(&marked(t)), t);
    }
}

#[test]
fn typing_at_every_caret() {
    let mut b = marked("|a\n|b\n|c");
    b.insert("x");
    assert_eq!(show(&b), "x|a\nx|b\nx|c");
}

#[test]
fn typing_replaces_selections() {
    let mut b = marked("[foo] and [foo]");
    b.insert("bar");
    assert_eq!(show(&b), "bar| and bar|");
}

#[test]
fn paste_of_a_column() {
    let mut b = marked("|\n|\n|");
    b.insert("one\ntwo\nthree");
    assert_eq!(show(&b), "one|\ntwo|\nthree|");
}

#[test]
fn backspace_and_delete() {
    let mut b = marked("ab|c\nde|f");
    b.backspace(false);
    assert_eq!(show(&b), "a|c\nd|f");
    b.delete(false);
    assert_eq!(show(&b), "a|\nd|");
}

#[test]
fn backspace_in_indent_takes_a_level() {
    let mut b = marked("        |x");
    b.backspace(false);
    assert_eq!(show(&b), "    |x");
    let mut b = marked("      |x");
    b.backspace(false);
    assert_eq!(show(&b), "    |x");
}

#[test]
fn word_deletes() {
    let mut b = marked("let foo_bar = 1|");
    b.backspace(true);
    b.backspace(true);
    assert_eq!(show(&b), "let foo_bar |");
    let mut b = marked("|  hello world");
    b.delete(true);
    assert_eq!(show(&b), "| world");
}

#[test]
fn carets_deleting_into_each_other_merge() {
    let mut b = marked("ab|c|d");
    b.backspace(false);
    b.backspace(false);
    assert_eq!(show(&b), "|d");
    assert_eq!(b.selections().len(), 1);
}

#[test]
fn newline_keeps_indent_and_opens_blocks() {
    let mut b = marked("    let x = 1;|");
    b.newline();
    assert_eq!(show(&b), "    let x = 1;\n    |");
    let mut b = marked("fn main() {|");
    b.newline();
    assert_eq!(show(&b), "fn main() {\n    |");
}

#[test]
fn tab_and_shift_tab() {
    let mut b = marked("ab|");
    b.indent();
    assert_eq!(show(&b), "ab  |", "to the next tab stop");
    let mut b = marked("[a\nb]\nc");
    b.indent();
    assert_eq!(show(&b), "[    a\n    b]\nc");
    b.outdent();
    assert_eq!(show(&b), "[a\nb]\nc");
    b.use_tabs = true;
    b.indent();
    assert_eq!(show(&b), "[\ta\n\tb]\nc");
}

#[test]
fn toggle_comment() {
    let mut b = marked("[fn a() {\n    x();\n}]");
    b.comment = Some("//".into());
    b.toggle_comment();
    assert_eq!(b.to_string(), "// fn a() {\n//     x();\n// }");
    b.toggle_comment();
    assert_eq!(b.to_string(), "fn a() {\n    x();\n}");
}

#[test]
fn undo_redo_and_typing_runs() {
    let mut b = marked("|");
    for c in "hello".chars() {
        b.insert(&c.to_string());
    }
    b.move_by(Motion::LineStart, false);
    b.insert("X");
    assert_eq!(b.to_string(), "Xhello");
    assert!(b.undo());
    assert_eq!(b.to_string(), "hello");
    assert!(b.undo(), "the word is one step");
    assert_eq!(show(&b), "|");
    assert!(!b.undo());
    assert!(b.redo());
    assert_eq!(b.to_string(), "hello");
    assert!(b.redo());
    assert_eq!(show(&b), "X|hello");
    assert!(!b.redo());
}

#[test]
fn undo_restores_multi_cursor_edit() {
    let mut b = marked("[foo] [foo]");
    b.insert("x");
    b.undo();
    assert_eq!(show(&b), "[foo] [foo]");
}

#[test]
fn vertical_motion_keeps_its_column() {
    let mut b = marked("abcdef|\nab\nabcdefgh");
    b.move_by(Motion::Down, false);
    assert_eq!(show(&b), "abcdef\nab|\nabcdefgh");
    b.move_by(Motion::Down, false);
    assert_eq!(show(&b), "abcdef\nab\nabcdef|gh");
}

#[test]
fn vertical_motion_through_tabs_and_wide() {
    let mut b = marked("\tx|\n漢字abc");
    b.move_by(Motion::Down, false);
    // Display column 5: past 漢字 (4 cells) and one char.
    assert_eq!(show(&b), "\tx\n漢字a|bc");
}

#[test]
fn smart_home_and_end() {
    let mut b = marked("    let x|;");
    b.move_by(Motion::LineStart, false);
    assert_eq!(show(&b), "    |let x;");
    b.move_by(Motion::LineStart, false);
    assert_eq!(show(&b), "|    let x;");
    b.move_by(Motion::LineEnd, true);
    assert_eq!(show(&b), "[    let x;]");
}

#[test]
fn left_right_collapse_selection() {
    let mut b = marked("a[bc]d");
    b.move_by(Motion::Left, false);
    assert_eq!(show(&b), "a|bcd");
    let mut b = marked("a[bc]d");
    b.move_by(Motion::Right, false);
    assert_eq!(show(&b), "abc|d");
}

#[test]
fn words() {
    let mut b = marked("foo.bar_baz  qux|");
    b.move_by(Motion::WordLeft, false);
    assert_eq!(show(&b), "foo.bar_baz  |qux");
    b.move_by(Motion::WordLeft, false);
    assert_eq!(show(&b), "foo.|bar_baz  qux");
    b.move_by(Motion::WordRight, false);
    assert_eq!(show(&b), "foo.bar_baz|  qux");
    assert_eq!(b.word_at(5), 4..11);
}

#[test]
fn add_cursor_below_and_escape() {
    let mut b = marked("ab|c\nd\nefg");
    b.add_cursor_vertical(false);
    b.add_cursor_vertical(false);
    assert_eq!(show(&b), "ab|c\nd|\nef|g");
    assert!(b.single());
    assert_eq!(b.selections().len(), 1);
    assert_eq!(show(&b), "abc\nd\nef|g", "the newest is the primary");
}

#[test]
fn select_next_occurrence() {
    let mut b = marked("f|oo bar foo baz foo");
    b.select_next();
    assert_eq!(show(&b), "[foo] bar foo baz foo");
    b.select_next();
    b.select_next();
    assert_eq!(show(&b), "[foo] bar [foo] baz [foo]");
    assert!(!b.select_next(), "all taken");
    b.insert("q");
    assert_eq!(b.to_string(), "q bar q baz q");
}

#[test]
fn find_and_replace() {
    let mut b = buf("Alpha beta alpha");
    assert_eq!(b.find_all("alpha", true), vec![11..16]);
    assert_eq!(b.find_all("alpha", false), vec![0..5, 11..16]);
    assert!(b.find_next("beta", true));
    assert_eq!(show(&b), "Alpha [beta] alpha");
    assert_eq!(b.replace_all("alpha", "gamma", false), 2);
    assert_eq!(b.to_string(), "gamma beta gamma");
    b.undo();
    assert_eq!(b.to_string(), "Alpha beta alpha");
}

#[test]
fn copy_and_cut_whole_lines_from_a_caret() {
    let mut b = marked("one\nt|wo\nthree");
    let o = b.key(Key::Char, Modifiers { ctrl: true, ..Default::default() }, "c", 10);
    assert_eq!(o, KeyOutcome::Copy("two\n".into()));
    let o = b.key(Key::Char, Modifiers { ctrl: true, ..Default::default() }, "x", 10);
    assert_eq!(o, KeyOutcome::Copy("two\n".into()));
    assert_eq!(b.to_string(), "one\nthree");
}

#[test]
fn edit_log_bytes_and_points() {
    let mut b = marked("é|\nab");
    b.take_edits();
    b.insert("ü");
    let e = b.take_edits();
    assert_eq!(e, vec![Edit {
        start_byte: 2, old_end_byte: 2, new_end_byte: 4,
        start: Point { row: 0, col_byte: 2 }, old_end: Point { row: 0, col_byte: 2 }, new_end: Point { row: 0, col_byte: 4 },
    }]);
    b.undo();
    let e = b.take_edits();
    assert_eq!(e[0].old_end_byte, 4);
    assert_eq!(e[0].new_end_byte, 2);
}

#[test]
fn edit_log_for_several_carets_is_in_order() {
    let mut b = marked("|a|b");
    b.take_edits();
    b.insert("xy");
    let e = b.take_edits();
    assert_eq!(e.len(), 2);
    assert_eq!((e[0].start_byte, e[0].new_end_byte), (0, 2));
    assert_eq!((e[1].start_byte, e[1].new_end_byte), (3, 5), "after the first insert");
    assert_eq!(b.to_string(), "xyaxyb");
}

#[test]
fn dirty_tracking() {
    let mut b = buf("x");
    assert!(!b.is_dirty());
    b.insert("y");
    assert!(b.is_dirty());
    b.mark_saved();
    assert!(!b.is_dirty());
}

#[test]
fn keys() {
    let none = Modifiers::default();
    let mut b = marked("ab|");
    assert_eq!(b.key(Key::Char, none, "c", 10), KeyOutcome::Changed);
    assert_eq!(b.key(Key::Enter, none, "", 10), KeyOutcome::Changed);
    assert_eq!(b.key(Key::Char, Modifiers { ctrl: true, ..none }, "v", 10), KeyOutcome::Paste);
    assert_eq!(b.key(Key::Escape, none, "", 10), KeyOutcome::Ignored, "one caret: Escape bubbles");
    assert_eq!(show(&b), "abc\n|");
}

/// Random edits never panic and keep the selections valid and sorted.
#[test]
fn random_edits_hold_invariants() {
    let mut seed = 0x9e37_79b9u32;
    let mut rnd = move || { seed ^= seed << 13; seed ^= seed >> 17; seed ^= seed << 5; seed };
    let mut b = buf("fn main() {\n    let x = 1;\n}\n");
    b.comment = Some("//".into());
    for _ in 0..5000 {
        match rnd() % 16 {
            0 => { b.insert(["a", "\n", "é", "漢", "\t", "x\ny"][rnd() as usize % 6]); }
            1 => { b.backspace(rnd() % 2 == 0); }
            2 => { b.delete(rnd() % 2 == 0); }
            3 => { b.newline(); }
            4 => { b.indent(); }
            5 => { b.outdent(); }
            6 => { b.toggle_comment(); }
            7 => { b.undo(); }
            8 => { b.redo(); }
            9 => b.move_by([Motion::Up, Motion::Down, Motion::Left, Motion::Right, Motion::WordLeft, Motion::LineEnd][rnd() as usize % 6], rnd() % 2 == 0),
            10 => b.add_cursor_vertical(rnd() % 2 == 0),
            11 => { b.select_next(); }
            12 => { b.single(); }
            13 => { let n = b.len_chars(); b.add_caret(rnd() as usize % (n + 1)); }
            14 => { b.replace_all("x", "yy", true); }
            _ => b.set_caret(rnd() as usize % (b.len_chars() + 1), rnd() % 2 == 0),
        }
        let n = b.len_chars();
        let s = b.selections();
        assert!(!s.is_empty());
        assert!(b.primary_index() < s.len());
        for w in s.windows(2) {
            assert!(w[0].range().end <= w[1].range().start, "sorted, apart: {:?}", s);
        }
        assert!(s.iter().all(|x| x.anchor <= n && x.head <= n));
    }
    // Undo all the way back gives the start.
    while b.undo() {}
    assert_eq!(b.to_string(), "fn main() {\n    let x = 1;\n}\n");
}

#[test]
fn replace_is_one_step_and_skips_overlaps() {
    let mut b = CodeBuffer::new("let x = 1;\nx + x\n");
    b.set_caret(b.len_chars(), false);
    assert!(b.replace(vec![(15..16, "count".into()), (4..5, "count".into()), (11..12, "count".into()), (4..6, "no".into()), (90..99, "!".into())]));
    assert_eq!(b.to_string(), "let count = 1;\ncount + count\n!");
    assert!(b.undo());
    assert_eq!(b.to_string(), "let x = 1;\nx + x\n", "one undo undoes all of it");
}
