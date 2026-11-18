//! Content operators as one typed enum (folium.md §5.1): what Officina's
//! export writes, through one serializer, instead of format strings. The
//! interpreter reads the same operators; a written stream read back by
//! [`crate::interpret`] draws what was written.

use folium_syntax::write::{num, write_name, write_string};

/// A part of a `TJ` array.
#[derive(Debug, Clone, PartialEq)]
pub enum TjPart {
    /// Glyph codes, as a string (two bytes each for Identity-H).
    Codes(Vec<u8>),
    /// A move left, in thousandths of the text size.
    Adjust(f64),
}

/// The operators written so far; more join as writers need them.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Save,
    Restore,
    Transform([f64; 6]),
    LineWidth(f64),
    /// The dash pattern and its phase (`d`; empty: solid).
    Dash(Vec<f64>, f64),
    FillRgb([f64; 3]),
    StrokeRgb([f64; 3]),
    FillGray(f64),
    MoveTo(f64, f64),
    LineTo(f64, f64),
    CurveTo([f64; 6]),
    ClosePath,
    Rect(f64, f64, f64, f64),
    Fill,
    FillEvenOdd,
    Stroke,
    Clip,
    EndPath,
    XObject(String),
    /// A named graphics state from the resources (`gs`): opacity.
    GState(String),
    BeginText,
    EndText,
    Font(String, f64),
    Rise(f64),
    /// Text rendering mode (`Tr`): 0 fill, 3 invisible (text kept for
    /// copying and search, drawn some other way).
    TextRender(u8),
    TextMatrix([f64; 6]),
    ShowText(Vec<u8>),
    ShowArray(Vec<TjPart>),
}

fn nums(out: &mut Vec<u8>, v: &[f64]) {
    for x in v {
        out.extend(num(*x).bytes());
        out.push(b' ');
    }
}

/// Codes as a hex string (what Identity-H text is usually written as).
fn hex(out: &mut Vec<u8>, b: &[u8]) {
    out.push(b'<');
    for x in b {
        out.extend(format!("{x:02X}").bytes());
    }
    out.push(b'>');
}

impl Op {
    pub fn write(&self, out: &mut Vec<u8>) {
        match self {
            Op::Save => out.extend(b"q"),
            Op::Restore => out.extend(b"Q"),
            Op::Transform(m) => {
                nums(out, m);
                out.extend(b"cm");
            }
            Op::Dash(v, phase) => {
                out.push(b'[');
                for (i, x) in v.iter().enumerate() {
                    if i > 0 {
                        out.push(b' ');
                    }
                    out.extend(num(*x).bytes());
                }
                out.extend(b"] ");
                nums(out, &[*phase]);
                out.extend(b"d");
            }
            Op::LineWidth(w) => {
                nums(out, &[*w]);
                out.extend(b"w");
            }
            Op::FillRgb(c) => {
                nums(out, c);
                out.extend(b"rg");
            }
            Op::StrokeRgb(c) => {
                nums(out, c);
                out.extend(b"RG");
            }
            Op::FillGray(g) => {
                nums(out, &[*g]);
                out.extend(b"g");
            }
            Op::MoveTo(x, y) => {
                nums(out, &[*x, *y]);
                out.extend(b"m");
            }
            Op::LineTo(x, y) => {
                nums(out, &[*x, *y]);
                out.extend(b"l");
            }
            Op::CurveTo(c) => {
                nums(out, c);
                out.extend(b"c");
            }
            Op::ClosePath => out.extend(b"h"),
            Op::Rect(x, y, w, h) => {
                nums(out, &[*x, *y, *w, *h]);
                out.extend(b"re");
            }
            Op::Fill => out.extend(b"f"),
            Op::FillEvenOdd => out.extend(b"f*"),
            Op::Stroke => out.extend(b"S"),
            Op::Clip => out.extend(b"W n"),
            Op::EndPath => out.extend(b"n"),
            Op::XObject(n) => {
                write_name(out, n.as_bytes());
                out.extend(b" Do");
            }
            Op::GState(n) => {
                write_name(out, n.as_bytes());
                out.extend(b" gs");
            }
            Op::BeginText => out.extend(b"BT"),
            Op::EndText => out.extend(b"ET"),
            Op::Font(n, size) => {
                write_name(out, n.as_bytes());
                out.push(b' ');
                nums(out, &[*size]);
                out.extend(b"Tf");
            }
            Op::Rise(r) => {
                nums(out, &[*r]);
                out.extend(b"Ts");
            }
            Op::TextRender(m) => {
                out.extend(format!("{m} Tr").bytes());
            }
            Op::TextMatrix(m) => {
                nums(out, m);
                out.extend(b"Tm");
            }
            Op::ShowText(s) => {
                write_string(out, s);
                out.extend(b" Tj");
            }
            Op::ShowArray(parts) => {
                out.push(b'[');
                for p in parts {
                    match p {
                        TjPart::Codes(c) => hex(out, c),
                        TjPart::Adjust(a) => out.extend(num(*a).bytes()),
                    }
                }
                out.extend(b"]TJ");
            }
        }
    }
}

/// A content stream from operators, one per line.
pub fn write_ops<'a>(ops: impl IntoIterator<Item = &'a Op>) -> Vec<u8> {
    let mut out = Vec::new();
    for op in ops {
        op.write(&mut out);
        out.push(b'\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spells_operators() {
        let s = write_ops(&[
            Op::FillRgb([1.0, 0.5, 0.0]),
            Op::Rect(10.0, 20.0, 30.5, 40.0),
            Op::Fill,
            Op::BeginText,
            Op::Font("F1".into(), 12.0),
            Op::TextRender(3),
            Op::TextMatrix([1.0, 0.0, 0.0, 1.0, 72.0, 700.0]),
            Op::ShowArray(vec![TjPart::Codes(vec![0, 3, 0, 4]), TjPart::Adjust(-12.5), TjPart::Codes(vec![0, 5])]),
            Op::EndText,
            Op::Save,
            Op::Transform([100.0, 0.0, 0.0, 50.0, 10.0, 10.0]),
            Op::XObject("Im 1".into()),
            Op::Restore,
        ]);
        assert_eq!(
            String::from_utf8(s).unwrap(),
            "1 0.5 0 rg\n10 20 30.5 40 re\nf\nBT\n/F1 12 Tf\n3 Tr\n1 0 0 1 72 700 Tm\n[<00030004>-12.5<0005>]TJ\nET\nq\n100 0 0 50 10 10 cm\n/Im#201 Do\nQ\n"
        );
    }
}
