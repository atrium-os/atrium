use super::*;

fn term() -> Terminal { Terminal::new(20, 5) }

fn feed(cols: u16, rows: u16, bytes: &[u8]) -> Terminal {
    let mut t = Terminal::new(cols, rows);
    t.feed(bytes);
    t
}

// ---- v1 behaviour ----------------------------------------------------------

#[test]
fn scrollback_text_includes_history_and_screen() {
    // 2-row screen; feed 4 lines → 2 scroll into history.
    let mut t = Terminal::new(8, 2);
    t.feed(b"l1\r\nl2\r\nl3\r\nl4");
    let text = t.grid().scrollback_text();
    assert_eq!(text, vec!["l1", "l2", "l3", "l4"]); // history (l1,l2) + screen (l3,l4)
}

#[test]
fn scrollback_restore_round_trip() {
    let mut src = Terminal::new(8, 2);
    src.feed(b"a\r\nb\r\nc\r\nd\r\ne");
    let saved = src.grid().scrollback_text();
    // A fresh terminal gets the scrollback restored into history.
    let mut dst = Terminal::new(8, 2);
    dst.restore_scrollback(&saved);
    // Paging up shows the oldest restored lines.
    let mut view = Terminal::new(8, 2);
    view.feed(&render_scrollback(dst.grid(), dst.grid().history().len()));
    assert_eq!(view.rows_text()[0], "a");
    // All saved lines are present in history (capped at HISTORY_MAX).
    assert_eq!(dst.grid().history().len(), saved.len());
}

#[test]
fn osc7_captures_cwd() {
    let mut t = term();
    // host form, bell-terminated
    t.feed(b"\x1b]7;file://myhost/home/g/src\x07");
    assert_eq!(t.grid().cwd(), Some("/home/g/src"));
    // no-host form, ST-terminated; percent-decoded
    t.feed(b"\x1b]7;file:///var/log/my%20dir\x1b\\");
    assert_eq!(t.grid().cwd(), Some("/var/log/my dir"));
}

#[test]
fn osc7_ignores_malformed() {
    let mut t = term();
    t.feed(b"\x1b]7;not-a-uri\x07");
    assert_eq!(t.grid().cwd(), None);
    // OSC 7 doesn't touch the title; OSC 2 doesn't touch cwd.
    t.feed(b"\x1b]2;my title\x07");
    assert_eq!(t.grid().title(), "my title");
    assert_eq!(t.grid().cwd(), None);
}

#[test]
fn prints_text_and_advances_cursor() {
    let mut t = term();
    t.feed(b"hello");
    assert_eq!(t.grid().cell(0, 0).c, 'h');
    assert_eq!(t.grid().cell(0, 4).c, 'o');
    assert_eq!(t.grid().cursor(), (0, 5));
}

#[test]
fn cr_lf_moves_to_next_line() {
    let mut t = term();
    t.feed(b"a\r\nb");
    assert_eq!(t.grid().cell(0, 0).c, 'a');
    assert_eq!(t.grid().cell(1, 0).c, 'b');
    assert_eq!(t.grid().cursor(), (1, 1));
}

#[test]
fn backspace_and_tab() {
    let mut t = term();
    t.feed(b"ab\x08X"); // a, b, BS over b, X
    assert_eq!(t.grid().cell(0, 1).c, 'X');
    let mut t2 = term();
    t2.feed(b"\tx"); // tab to col 8, then x
    assert_eq!(t2.grid().cursor(), (0, 9));
    assert_eq!(t2.grid().cell(0, 8).c, 'x');
}

#[test]
fn deferred_wrap_at_right_margin() {
    let mut t = Terminal::new(3, 4);
    t.feed(b"abc"); // exactly fills row 0 — no scroll yet (deferred)
    assert_eq!(t.grid().cursor(), (0, 2)); // cursor parked on last col
    t.feed(b"d"); // now wraps to row 1
    assert_eq!(t.grid().cell(0, 2).c, 'c');
    assert_eq!(t.grid().cell(1, 0).c, 'd');
}

#[test]
fn scrolls_at_bottom() {
    let mut t = Terminal::new(5, 2);
    t.feed(b"one\r\ntwo\r\nthree");
    // "one" scrolled off; row 0 = two, row 1 = three
    assert_eq!(t.rows_text(), vec!["two".to_string(), "three".to_string()]);
}

#[test]
fn cup_positions_cursor_1based() {
    let mut t = term();
    t.feed(b"\x1b[3;5HX");
    assert_eq!(t.grid().cell(2, 4).c, 'X'); // row 3 col 5 → (2,4)
}

#[test]
fn sgr_bold_and_color() {
    let mut t = term();
    t.feed(b"\x1b[1;31mR\x1b[0mN");
    let r = t.grid().cell(0, 0);
    assert!(r.flags & flags::BOLD != 0);
    assert_eq!(r.fg, Color::Indexed(1));
    let n = t.grid().cell(0, 1);
    assert_eq!(n.flags, 0);
    assert_eq!(n.fg, Color::Default);
}

#[test]
fn sgr_256_and_truecolor() {
    let mut t = term();
    t.feed(b"\x1b[38;5;200mA\x1b[38;2;10;20;30mB");
    assert_eq!(t.grid().cell(0, 0).fg, Color::Indexed(200));
    assert_eq!(t.grid().cell(0, 1).fg, Color::Rgb(10, 20, 30));
}

#[test]
fn erase_line_and_display() {
    let mut t = term();
    t.feed(b"abcde\r\x1b[2K"); // write, CR to col0, erase whole line
    assert_eq!(t.rows_text()[0], "");
    let mut t2 = term();
    t2.feed(b"x\r\ny\r\nz\x1b[2J"); // erase whole display
    assert!(t2.rows_text().iter().all(|r| r.is_empty()));
}

#[test]
fn osc_sets_window_title() {
    let mut t = term();
    t.feed(b"\x1b]2;my-shell: ~/src\x07text");
    assert_eq!(t.title(), "my-shell: ~/src");
    assert_eq!(t.grid().cell(0, 0).c, 't'); // the text after still prints
    // OSC 0 (icon+title) also sets it; bell- or ST-terminated.
    t.feed(b"\x1b]0;other\x1b\\");
    assert_eq!(t.title(), "other");
}

#[test]
fn resize_preserves_topleft_and_clamps_cursor() {
    let mut t = Terminal::new(10, 4);
    t.feed(b"\x1b[4;8Hhello"); // cursor near bottom-right
    t.resize(5, 2);
    assert_eq!(t.grid().cols(), 5);
    assert_eq!(t.grid().rows(), 2);
    let (r, c) = t.grid().cursor();
    assert!(r < 2 && c < 5);
}

// ---- M1: scroll regions and editing ---------------------------------------

fn lines(t: &Terminal) -> Vec<String> { t.rows_text() }

#[test]
fn decstbm_scrolls_only_the_region() {
    // 5 rows: 1..5; region rows 2..4; LF at row 4 scrolls rows 2..4 only.
    let mut t = feed(10, 5, b"1\r\n2\r\n3\r\n4\r\n5");
    t.feed(b"\x1b[2;4r"); // cursor homes
    assert_eq!(t.grid().cursor(), (0, 0));
    t.feed(b"\x1b[4;1H\nX");
    assert_eq!(lines(&t), vec!["1", "3", "4", "X", "5"]);
    // Nothing went into the scrollback from a region not at the top.
    assert_eq!(t.grid().history().len(), 0);
}

#[test]
fn reverse_index_at_top_margin_scrolls_down() {
    let mut t = feed(10, 4, b"a\r\nb\r\nc\r\nd");
    t.feed(b"\x1b[2;3r\x1b[2;1H\x1bMZ");
    assert_eq!(lines(&t), vec!["a", "Z", "b", "d"]);
}

#[test]
fn reverse_index_at_screen_top_without_region() {
    let mut t = feed(10, 3, b"a\r\nb\r\nc");
    t.feed(b"\x1b[H\x1bMtop");
    assert_eq!(lines(&t), vec!["top", "a", "b"]);
}

#[test]
fn insert_and_delete_lines_inside_region() {
    let mut t = feed(10, 5, b"1\r\n2\r\n3\r\n4\r\n5");
    t.feed(b"\x1b[2;4r\x1b[2;3H\x1b[L");
    assert_eq!(lines(&t), vec!["1", "", "2", "3", "5"]);
    assert_eq!(t.grid().cursor(), (1, 0), "IL homes the column");
    t.feed(b"\x1b[2M");
    assert_eq!(lines(&t), vec!["1", "3", "", "", "5"]);
    // Outside the region: nothing.
    t.feed(b"\x1b[5;1H\x1b[L");
    assert_eq!(lines(&t)[4], "5");
}

#[test]
fn delete_lines_at_top_does_not_feed_scrollback() {
    let mut t = feed(10, 3, b"a\r\nb\r\nc");
    t.feed(b"\x1b[H\x1b[M");
    assert_eq!(lines(&t), vec!["b", "c", ""]);
    assert!(t.grid().history().is_empty());
}

#[test]
fn su_sd() {
    let mut t = feed(10, 3, b"a\r\nb\r\nc");
    t.feed(b"\x1b[S");
    assert_eq!(lines(&t), vec!["b", "c", ""]);
    t.feed(b"\x1b[2T");
    assert_eq!(lines(&t), vec!["", "", "b"]);
}

#[test]
fn ich_dch_ech() {
    let mut t = feed(10, 1, b"abcdef");
    t.feed(b"\x1b[1;3H\x1b[2@");
    assert_eq!(lines(&t)[0], "ab  cdef");
    t.feed(b"\x1b[3P");
    assert_eq!(lines(&t)[0], "abdef");
    t.feed(b"\x1b[1;1H\x1b[2X");
    assert_eq!(lines(&t)[0], "  def");
}

#[test]
fn ich_pushes_off_the_right_edge() {
    let mut t = feed(5, 1, b"abcde");
    t.feed(b"\x1b[1;1H\x1b[2@");
    assert_eq!(lines(&t)[0], "  abc");
}

#[test]
fn tab_stops_set_clear() {
    let mut t = feed(20, 1, b"\x1b[3g"); // clear all
    t.feed(b"\x1b[1;5H\x1bH\x1b[1;1H\tX");
    assert_eq!(t.grid().cell(0, 4).c, 'X');
    // HT with no stops ahead goes to the last column.
    t.feed(b"\t");
    assert_eq!(t.grid().cursor(), (0, 19));
    // CBT back to the stop.
    t.feed(b"\x1b[Z");
    assert_eq!(t.grid().cursor(), (0, 4));
}

#[test]
fn decsc_decrc_restore_position_and_pen() {
    let mut t = term();
    t.feed(b"\x1b[3;4H\x1b[31m\x1b7\x1b[H\x1b[0mx\x1b8y");
    let c = t.grid().cell(2, 3);
    assert_eq!(c.c, 'y');
    assert_eq!(c.fg, Color::Indexed(1));
}

#[test]
fn decawm_off_overwrites_last_column() {
    let mut t = feed(3, 2, b"\x1b[?7labcde");
    assert_eq!(lines(&t), vec!["abe", ""]);
}

#[test]
fn origin_mode_addresses_within_region() {
    let mut t = feed(10, 6, b"\x1b[3;5r\x1b[?6h");
    assert_eq!(t.grid().cursor(), (2, 0));
    t.feed(b"\x1b[2;2HX");
    assert_eq!(t.grid().cell(3, 1).c, 'X');
    // CUP past the region clamps to its bottom.
    t.feed(b"\x1b[9;1H");
    assert_eq!(t.grid().cursor().0, 4);
    // CPR is relative to the region.
    t.feed(b"\x1b[6n");
    assert_eq!(t.take_replies(), b"\x1b[3;1R");
}

#[test]
fn cuu_cud_stop_at_margins() {
    let mut t = feed(10, 6, b"\x1b[2;4r\x1b[3;1H\x1b[9A");
    assert_eq!(t.grid().cursor().0, 1);
    t.feed(b"\x1b[9B");
    assert_eq!(t.grid().cursor().0, 3);
}

#[test]
fn irm_inserts() {
    let mut t = feed(10, 1, b"abc\x1b[1;2H\x1b[4hXY\x1b[4lZ");
    assert_eq!(lines(&t)[0], "aXYZc");
}

#[test]
fn rep_repeats_last_char() {
    let t = feed(10, 1, b"-\x1b[4b|");
    assert_eq!(lines(&t)[0], "-----|");
}

#[test]
fn dec_line_drawing() {
    let t = feed(10, 1, b"\x1b(0lqk\x1b(Bq");
    assert_eq!(lines(&t)[0], "┌─┐q");
    let t = feed(10, 1, b"\x1b)0a\x0eq\x0fq");
    assert_eq!(lines(&t)[0], "a─q");
}

#[test]
fn decaln_fills() {
    let t = feed(3, 2, b"\x1b#8");
    assert_eq!(lines(&t), vec!["EEE", "EEE"]);
}

#[test]
fn ed3_clears_scrollback_only() {
    let mut t = feed(5, 2, b"a\r\nb\r\nc");
    assert_eq!(t.grid().history().len(), 1);
    t.feed(b"\x1b[3J");
    assert!(t.grid().history().is_empty());
    assert_eq!(lines(&t), vec!["b", "c"]);
}

// ---- alternate screen ------------------------------------------------------

#[test]
fn alt_screen_1049_saves_and_restores() {
    let mut t = feed(10, 3, b"shell$ ");
    let before = t.grid().cursor();
    t.feed(b"\x1b[?1049h");
    assert!(t.grid().mode(mode::ALT_SCREEN));
    assert!(lines(&t).iter().all(|l| l.is_empty()), "alternate screen starts clear");
    t.feed(b"\x1b[2;1Heditor\r\nmore\r\nlines\r\nscroll");
    t.feed(b"\x1b[?1049l");
    assert!(!t.grid().mode(mode::ALT_SCREEN));
    assert_eq!(lines(&t)[0], "shell$");
    assert_eq!(t.grid().cursor(), before);
    // The editor's scrolling never reached the scrollback.
    assert!(t.grid().history().is_empty());
}

#[test]
fn alt_screen_1047_clears_on_leave_and_47_keeps() {
    let mut t = feed(10, 2, b"main");
    t.feed(b"\x1b[?47h\x1b[Halt\x1b[?47l\x1b[?47h");
    assert_eq!(lines(&t)[0], "alt", "47 keeps the alternate screen");
    t.feed(b"\x1b[?47l\x1b[?1047h\x1b[Hx\x1b[?1047l\x1b[?1047h");
    assert_eq!(lines(&t)[0], "", "1047 clears it on leaving");
}

#[test]
fn scrollback_text_is_the_main_screen_while_on_alt() {
    let mut t = feed(10, 2, b"one\r\ntwo");
    t.feed(b"\x1b[?1049hEDITOR");
    assert_eq!(t.grid().scrollback_text(), vec!["one", "two"]);
}

#[test]
fn resize_while_on_alt_keeps_both() {
    let mut t = feed(10, 3, b"main");
    t.feed(b"\x1b[?1049h\x1b[Halt");
    t.resize(6, 2);
    assert_eq!(lines(&t)[0], "alt");
    t.feed(b"\x1b[?1049l");
    assert_eq!(lines(&t)[0], "main");
}

// ---- modes -----------------------------------------------------------------

#[test]
fn dec_modes_track() {
    let mut t = term();
    assert_eq!(t.grid().modes(), mode::DEFAULT);
    t.feed(b"\x1b[?1h\x1b=\x1b[?25l\x1b[?2004h\x1b[?1004h\x1b[?1002;1006h");
    let m = t.grid().modes();
    for bit in [mode::CURSOR_KEYS, mode::APP_KEYPAD, mode::HIDE_CURSOR, mode::BRACKETED_PASTE,
                mode::FOCUS, mode::MOUSE_DRAG, mode::MOUSE_SGR] {
        assert!(m & bit != 0, "bit {bit:#x}");
    }
    // A new mouse mode replaces the old one.
    t.feed(b"\x1b[?1000h");
    assert!(t.grid().mode(mode::MOUSE_CLICK) && !t.grid().mode(mode::MOUSE_DRAG));
    t.feed(b"\x1b[?1l\x1b>\x1b[?25h\x1b[?2004l\x1b[?1000l");
    assert!(!t.grid().mode(mode::CURSOR_KEYS | mode::APP_KEYPAD | mode::HIDE_CURSOR
                           | mode::BRACKETED_PASTE | mode::MOUSE_ANY));
}

#[test]
fn decscusr_shape() {
    let mut t = term();
    t.feed(b"\x1b[6 q");
    assert_eq!(mode::cursor_shape(t.grid().modes()), 6);
    t.feed(b"\x1b[0 q");
    assert_eq!(mode::cursor_shape(t.grid().modes()), 0);
}

#[test]
fn private_sgr_like_sequences_are_not_sgr() {
    // XTMODKEYS (`CSI > 4;1 m`) must not set italic/bold.
    let mut t = term();
    t.feed(b"\x1b[>4;1mX");
    assert_eq!(t.grid().cell(0, 0).flags, 0);
}

#[test]
fn decstr_soft_reset() {
    let mut t = term();
    t.feed(b"\x1b[?1h\x1b[?25l\x1b[2;3r\x1b[31m\x1b[!pX");
    assert!(!t.grid().mode(mode::CURSOR_KEYS));
    assert!(!t.grid().mode(mode::HIDE_CURSOR));
    assert_eq!(t.grid().cell(0, 0).fg, Color::Default);
}

#[test]
fn ris_keeps_scrollback_and_title() {
    let mut t = feed(5, 2, b"\x1b]2;tt\x07a\r\nb\r\nc\x1b[?1h");
    t.feed(b"\x1bc");
    assert_eq!(t.grid().modes(), mode::DEFAULT);
    assert_eq!(t.title(), "tt");
    assert_eq!(t.grid().history().len(), 1);
    assert_eq!(lines(&t), vec!["", ""]);
}

// ---- SGR -------------------------------------------------------------------

#[test]
fn sgr_colon_forms() {
    let mut t = term();
    t.feed(b"\x1b[38:2::1:2:3mA\x1b[38:2:4:5:6mB\x1b[48:5:9mC\x1b[4:3mD\x1b[4:0mE");
    assert_eq!(t.grid().cell(0, 0).fg, Color::Rgb(1, 2, 3));
    assert_eq!(t.grid().cell(0, 1).fg, Color::Rgb(4, 5, 6));
    assert_eq!(t.grid().cell(0, 2).bg, Color::Indexed(9));
    assert!(t.grid().cell(0, 3).flags & flags::UNDERLINE != 0);
    // `4:3` is curly underline, not underline + italic.
    assert!(t.grid().cell(0, 3).flags & flags::ITALIC == 0);
    assert!(t.grid().cell(0, 4).flags & flags::UNDERLINE == 0);
}

#[test]
fn sgr_extended_then_more() {
    let mut t = term();
    t.feed(b"\x1b[38;5;3;1mA\x1b[0;58;2;1;2;3;7mB");
    let a = t.grid().cell(0, 0);
    assert_eq!(a.fg, Color::Indexed(3));
    assert!(a.flags & flags::BOLD != 0);
    let b = t.grid().cell(0, 1);
    assert!(b.flags & flags::REVERSE != 0, "underline colour consumed its parameters");
}

#[test]
fn sgr_strike_hidden() {
    let mut t = term();
    t.feed(b"\x1b[9;8mA\x1b[29;28mB");
    assert_eq!(t.grid().cell(0, 0).flags, flags::STRIKE | flags::HIDDEN);
    assert_eq!(t.grid().cell(0, 1).flags, 0);
}

#[test]
fn erase_uses_pen_background() {
    let mut t = term();
    t.feed(b"\x1b[44m\x1b[2J");
    assert_eq!(t.grid().cell(3, 3).bg, Color::Indexed(4));
}

// ---- wide characters -------------------------------------------------------

#[test]
fn wide_char_takes_two_cells() {
    let t = feed(10, 1, "a漢b".as_bytes());
    let g = t.grid();
    assert_eq!(g.cell(0, 1).c, '漢');
    assert!(g.cell(0, 1).flags & flags::WIDE != 0);
    assert!(g.cell(0, 2).flags & flags::WIDE_SPACER != 0);
    assert_eq!(g.cell(0, 3).c, 'b');
    assert_eq!(lines(&t)[0], "a漢b");
}

#[test]
fn wide_char_wraps_when_no_room() {
    let t = feed(3, 2, "ab漢".as_bytes());
    assert_eq!(lines(&t), vec!["ab", "漢"]);
}

#[test]
fn overwriting_half_a_wide_char_clears_the_other_half() {
    let mut t = feed(5, 1, "漢".as_bytes());
    t.feed(b"\x1b[1;2Hx");
    let g = t.grid();
    assert_eq!(g.cell(0, 0).c, ' ');
    assert_eq!(g.cell(0, 0).flags & flags::WIDE, 0);
    assert_eq!(g.cell(0, 1).c, 'x');
    assert_eq!(g.cell(0, 1).flags & flags::WIDE_SPACER, 0);
}

#[test]
fn combining_marks_are_dropped_not_spaced() {
    let t = feed(10, 1, "e\u{301}x".as_bytes());
    assert_eq!(lines(&t)[0], "ex");
}

// ---- replies ---------------------------------------------------------------

#[test]
fn device_attributes_and_status() {
    let mut t = term();
    t.feed(b"\x1b[c");
    assert_eq!(t.take_replies(), b"\x1b[?62;22c");
    t.feed(b"\x1b[>c");
    assert_eq!(t.take_replies(), b"\x1b[>1;10;0c");
    t.feed(b"\x1b[5n\x1b[2;3H\x1b[6n");
    assert_eq!(t.take_replies(), b"\x1b[0n\x1b[2;3R");
    assert!(t.take_replies().is_empty(), "taken once");
}

#[test]
fn decrqm_reports() {
    let mut t = term();
    t.feed(b"\x1b[?2004h\x1b[?2004$p\x1b[?1$p\x1b[?9999$p");
    assert_eq!(t.take_replies(), b"\x1b[?2004;1$y\x1b[?1;2$y\x1b[?9999;0$y");
}

#[test]
fn colour_queries_answer_in_kind() {
    let mut t = term();
    t.feed(b"\x1b]11;?\x1b\\");
    assert_eq!(t.take_replies(), b"\x1b]11;rgb:1212/1414/1717\x1b\\");
    t.feed(b"\x1b]10;?\x07");
    assert_eq!(t.take_replies(), b"\x1b]10;rgb:f2f2/f2f2/f2f2\x07");
}

#[test]
fn xtversion_and_size() {
    let mut t = feed(20, 5, b"\x1b[>q");
    assert!(t.take_replies().starts_with(b"\x1bP>|stoa-term"));
    t.feed(b"\x1b[18t");
    assert_eq!(t.take_replies(), b"\x1b[8;5;20t");
}

#[test]
fn unknown_queries_get_no_reply() {
    let mut t = term();
    t.feed(b"\x1b[?u\x1b[>4;1m\x1bP+q544e\x1b\\");
    assert!(t.take_replies().is_empty());
}

// ---- OSC -------------------------------------------------------------------

#[test]
fn title_is_bounded_and_stripped() {
    let mut t = term();
    let mut seq = b"\x1b]2;a\x7fb".to_vec();
    seq.extend(std::iter::repeat(b'x').take(1000));
    seq.push(0x07);
    t.feed(&seq);
    assert!(t.title().starts_with("abx"));
    assert_eq!(t.title().chars().count(), TITLE_MAX);
}

#[test]
fn osc52_writes_and_never_reads() {
    let mut t = term();
    t.feed(b"\x1b]52;c;aGVsbG8gd29ybGQ=\x07");
    assert_eq!(t.take_clipboard().as_deref(), Some("hello world"));
    assert_eq!(t.take_clipboard(), None);
    t.feed(b"\x1b]52;c;?\x07");
    assert_eq!(t.take_clipboard(), None);
    assert!(t.take_replies().is_empty(), "a read is never answered");
    t.feed(b"\x1b]52;c;!!!\x07");
    assert_eq!(t.take_clipboard(), None, "bad base64 is dropped");
}

#[test]
fn base64() {
    assert_eq!(base64_decode(b"").unwrap(), b"");
    assert_eq!(base64_decode(b"Zg==").unwrap(), b"f");
    assert_eq!(base64_decode(b"Zm8").unwrap(), b"fo");
    assert_eq!(base64_decode(b"Zm9v").unwrap(), b"foo");
    assert!(base64_decode(b"Zm=9").is_none());
}

// ---- robustness ------------------------------------------------------------

/// Arbitrary bytes never panic, at any size, and every invariant holds.
#[test]
fn noise_never_panics() {
    let mut seed = 0x1234_5678u32;
    let mut rnd = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    let pieces: &[&[u8]] = &[
        b"\x1b[", b"\x1b[?", b"\x1b]", b"\x1b(0", b"\x1b7", b"\x1b8", b"\x1bM", b"\x1bD", b"\x1b#8",
        b";", b"1", b"9", b"99999", b"h", b"l", b"r", b"L", b"M", b"@", b"P", b"X", b"S", b"T",
        b"b", b"J", b"K", b"H", b"m", b"\n", b"\r", b"\t", b"x", "漢".as_bytes(), b"\x07",
        b"1049", b"47", b"6", b"7", b"$p", b" q", b"\x0e", b"\x0f", b"52;c;", b"Zm9v",
    ];
    for size in [(1u16, 1u16), (2, 1), (3, 3), (80, 24)] {
        let mut t = Terminal::new(size.0, size.1);
        for _ in 0..20_000 {
            let p = pieces[(rnd() as usize) % pieces.len()];
            t.feed(p);
            if rnd() % 997 == 0 {
                t.resize((rnd() % 90) as u16, (rnd() % 30) as u16);
            }
        }
        let g = t.grid();
        let (r, c) = g.cursor();
        assert!(r < g.rows() && c < g.cols());
        let _ = t.take_replies();
    }
}

#[test]
fn osc_7770_pkg_requests() {
    let mut t = Terminal::new(20, 2);
    t.feed(b"\x1b]7770;pkg;install  helix\x07");
    assert_eq!(t.take_ask().as_deref(), Some("install helix"));
    assert_eq!(t.take_ask(), None, "a one-shot");
    // Anything but package-name words is dropped.
    t.feed(b"\x1b]7770;pkg;install helix; rm -rf /\x07");
    assert_eq!(t.take_ask(), None);
    t.feed(b"\x1b]7770;pkg;install $(id)\x07");
    assert_eq!(t.take_ask(), None);
    t.feed(b"\x1b]7770;pkg;   \x1b\\");
    assert_eq!(t.take_ask(), None);
    t.feed(format!("\x1b]7770;pkg;{}\x07", "a".repeat(300)).as_bytes());
    assert_eq!(t.take_ask(), None);
    t.feed(b"\x1b]7770;pkg;install -y py311-pip devel/git py3*\x1b\\");
    assert_eq!(t.take_ask().as_deref(), Some("install -y py311-pip devel/git py3*"));
    // Nothing shows on the screen.
    assert!(t.rows_text().iter().all(|r| r.trim().is_empty()));
}
