use std::io::Write;
use std::process::{Command, Stdio};

use officina_convert::html_clipboard::{self, MAX_NATIVE_BYTES};
use officina_core::history::Op;
use officina_grid::clip::Clip;
use officina_grid::style::{HAlign, Style, VAlign};
use officina_grid::{CellAddr, Rect, Value, Workbook};

fn workbook() -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("Sheet").unwrap();
    wb
}

fn rect(row: u32, col: u32) -> Rect {
    Rect { sheet: 0, r0: row, c0: col, r1: row, c1: col }
}

fn worker(html: &[u8]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_officina-convert"))
        .arg(html_clipboard::WORKER_KIND)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(html).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn html_offer_round_trips_value_and_styles_through_the_worker() {
    let mut source = workbook();
    let mut style = Style { fill: Some(0xfefdcb), h_align: HAlign::Right, v_align: VAlign::Center, wrap: true, ..Style::default() }
        .with_num_fmt("0.00%;[Red]-0.00%");
    style.font.bold = true;
    style.font.italic = true;
    style.font.underline = true;
    style.font.strike = true;
    style.font.name = Some("Plex".into());
    style.font.size = Some(240);
    style.font.color = Some(0x1297be);
    source.set_value(CellAddr::new(0, 0, 0), Value::Num(0.125));
    source.set_value(CellAddr::new(0, 0, 1), Value::str(" =literal()\n<text> & data "));
    source.set_value(CellAddr::new(0, 1, 0), Value::Bool(true));
    source.set_style(&rect(0, 0), style.clone());
    source.set_style(&rect(1, 1), style.clone());
    let range = Rect { r1: 1, c1: 1, ..rect(0, 0) };
    let html = html_clipboard::write(&source, range).unwrap();
    assert!(html.contains(r##"bgcolor="#fefdcb" align="right" valign="middle""##));
    assert!(html.contains(r##"<font face="Plex" color="#1297be"><b><i><u><strike>12.50%</strike></u></i></b></font>"##));
    assert!(html.contains(r#"sdnum="1033;1033;0.00%;[Red]-0.00%""#));
    assert!(html.contains("&lt;text&gt; &amp; data"));
    let out = worker(html.as_bytes());
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let clip = Clip::decode_system(&out.stdout, MAX_NATIVE_BYTES).unwrap();
    let mut target = workbook();
    for op in clip.paste(&target, 0, 4, 3).unwrap().0 {
        op.apply(&mut target);
    }
    let addr = |r, c| CellAddr::new(0, r, c);
    assert_eq!(target.value(0, 4, 3), Value::Num(0.125));
    assert_eq!(target.value(0, 4, 4), Value::str(" =literal()\n<text> & data "));
    assert_eq!(target.value(0, 5, 3), Value::Bool(true));
    assert_eq!(target.value(0, 5, 4), Value::Empty);
    assert_eq!(target.style(addr(4, 3)), &style);
    assert_eq!(target.style(addr(5, 4)), &style);
    assert!(target.formula_text(addr(4, 4)).is_none());
}

#[test]
fn imports_excel_framing_and_css_calc_markup_and_google_types() {
    let body = concat!(
        r#"<html><head><style>.xl65 {mso-number-format:"0.0%;-0.0%";font-weight:700;color:#123;}</style></head><body>"#,
        "<table><tr><td>decoy</td></tr></table><!--StartFragment--><table>",
        r##"<tr><td class="xl65" x:num="0.5" x:fmla="=DDE()">50%</td><td sdval="17" bgcolor="#fea"><b><font face="Liberation Sans">17</font></b></td>"##,
        r#"<td style="background-color:rgb(3, 6, 9)"><span data-sheets-value='{"1":3,"3":12.5}'>12,50</span></td>"#,
        "<td><script>hidden</script>=literal()</td></tr></table><!--EndFragment--></body></html>",
    );
    let header = |start, end| format!("Version:1.0\r\nStartHTML:{start:010}\r\nEndHTML:{end:010}\r\n");
    let start = header(0, 0).len();
    let input = format!("{}{body}", header(start, start + body.len()));
    let markerless = body.replace("<!--StartFragment-->", "").replace("<!--EndFragment-->", "");
    let first = markerless.rfind("<table>").unwrap();
    let last = markerless.rfind("</table>").unwrap() + "</table>".len();
    let offsets = |start, end, first, last| format!("Version:1.0\r\nStartHTML:{start:010}\r\nEndHTML:{end:010}\r\nStartFragment:{first:010}\r\nEndFragment:{last:010}\r\n");
    let start = offsets(0, 0, 0, 0).len();
    let framed = format!("{}{markerless}", offsets(start, start + markerless.len(), start + first, start + last));
    let markerless_clip = html_clipboard::read(framed.as_bytes()).unwrap();
    assert_eq!(markerless_clip.cols(), 4);
    let clip = html_clipboard::read(input.as_bytes()).unwrap();
    let mut target = workbook();
    for op in clip.paste(&target, 0, 0, 0).unwrap().0 {
        op.apply(&mut target);
    }
    let st = |c| target.style(CellAddr::new(0, 0, c));
    assert_eq!(target.value(0, 0, 0), Value::Num(0.5));
    assert_eq!(target.value(0, 0, 1), Value::Num(17.0));
    assert_eq!(target.value(0, 0, 2), Value::Num(12.5));
    assert_eq!(target.value(0, 0, 3), Value::str("=literal()"));
    assert!(st(0).font.bold);
    assert_eq!(st(0).font.color, Some(0x112233));
    assert_eq!(st(0).num_fmt.as_ref(), "0.0%;-0.0%");
    assert_eq!(st(1).fill, Some(0xffeeaa));
    assert!(st(1).font.bold);
    assert_eq!(st(1).font.name.as_deref(), Some("Liberation Sans"));
    assert_eq!(st(2).fill, Some(0x030609));

    let inside = html_clipboard::read(b"<table><tr><td>before</td></tr><tr><td>left</td><!--StartFragment--><td>selected</td><!--EndFragment--><td>right</td></tr><tr><td>after</td></tr></table>").unwrap();
    assert_eq!((inside.rows(), inside.cols()), (1, 1));
    for op in inside.paste(&target, 0, 2, 0).unwrap().0 { op.apply(&mut target); }
    assert_eq!(target.value(0, 2, 0), Value::str("selected"));

    let lone = html_clipboard::read(br#"<span data-sheets-value='{"1":3,"3":1234.56}'>1 234,56</span>"#).unwrap();
    for op in lone.paste(&target, 0, 1, 0).unwrap().0 { op.apply(&mut target); }
    assert_eq!(target.value(0, 1, 0), Value::Num(1234.56));
}

#[test]
fn rejects_oversized_badly_framed_and_merged_tables() {
    assert!(html_clipboard::read(&vec![b'a'; html_clipboard::MAX_HTML_BYTES + 1]).is_err());
    assert!(html_clipboard::read(b"Version:1.0\nStartHTML:99\nEndHTML:3\n").is_err());
    assert!(html_clipboard::read(b"<table><tr><td colspan=2>One</td></tr></table>").is_err());
    let row = "<tr><td>1</td><td>2</td></tr>";
    let too_tall = format!("<table>{}</table>", row.repeat(html_clipboard::MAX_CELLS as usize / 2 + 1));
    assert!(html_clipboard::read(too_tall.as_bytes()).is_err());
    let out = worker(b"<table><tr><td rowspan=0>Unsupported</td></tr></table>");
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
}

#[test]
fn calc_number_format_and_fraction_survive_the_worker() {
    let out = worker(br#"<style>body,table,th,td {font-family:"Liberation Sans";font-size:x-small}</style><table><tr>
        <td sdval="0.125" sdnum="1033;1033;0.00%;[Red]-0.00%" data-sheets-value='{"1":3,"3":0}'>12.50%</td>
        <td sdval="1" sdnum="1033;" data-sheets-value='{"1":4,"4":1}'>TRUE</td>
        <td sdval="0.5" sdnum="1033;" style='mso-number-format:"0.0%"'>50.0%</td>
        </tr></table>"#);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let clip = Clip::decode_system(&out.stdout, MAX_NATIVE_BYTES).unwrap();
    let mut target = workbook();
    for op in clip.paste(&target, 0, 0, 0).unwrap().0 {
        op.apply(&mut target);
    }
    assert_eq!(target.value(0, 0, 0), Value::Num(0.125));
    assert_eq!(target.formatted(CellAddr::new(0, 0, 0)).text, "12.50%");
    assert_eq!(target.style(CellAddr::new(0, 0, 0)).num_fmt.as_ref(), "0.00%;[Red]-0.00%");
    assert_eq!(target.value(0, 0, 1), Value::Bool(true));
    assert_eq!(target.style(CellAddr::new(0, 0, 0)).font.name.as_deref(), Some("Liberation Sans"));
    assert_eq!(target.style(CellAddr::new(0, 0, 2)).num_fmt.as_ref(), "0.0%");
}
