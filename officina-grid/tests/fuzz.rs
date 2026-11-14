//! Fuzzing without external tools: random bytes and token soup into the formula
//! parser, then whatever parses into a workbook and through recalculation; mutated
//! files into the native reader; mutated records into the op decoder. The gate is "no
//! panic, no hang". `FUZZ_ITERS` raises the count (the O1 gate ran it at 2,000,000).

use officina_grid::ops::GridOp;
use officina_grid::{native, CellAddr, Workbook};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn iters(default: u64) -> u64 {
    std::env::var("FUZZ_ITERS").ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

const TOKENS: &[&str] = &[
    "1", "0", "-", "+", "*", "/", "^", "&", "%", "=", "<>", "<=", ">=", "<", ">", "(", ")", ",", ";", ":", "{", "}", "\"a\"", "\"\"", "\"\"\"\"", "A1", "$B$2",
    "C:C", "3:3", "A1:B9", "Z99", "XFD1048576", "AAAAA1", "S!A1", "'Other sheet'!B2", "Nope!A1", "SUM", "IF", "INDEX", "MATCH", "VLOOKUP", "XLOOKUP",
    "SUMIF", "COUNTIFS", "TEXTJOIN", "MID", "DATE", "RAND", "CHOOSE", "SUMPRODUCT", "FOO", "_xlfn.CONCAT", "TRUE", "FALSE", "#N/A", "#REF!", "#DIV/0!",
    "1E+308", "1e-400", "0.5", ".5", "9999999999", " ", "  ", "é", "\u{0}", "A", "1048577", "2147483648",
];

fn soup(g: &mut Rng) -> String {
    let mut s = String::from("=");
    for _ in 0..g.below(40) {
        s.push_str(TOKENS[g.below(TOKENS.len() as u64) as usize]);
    }
    s
}

fn bytes(g: &mut Rng) -> String {
    let n = g.below(64) as usize;
    let b: Vec<u8> = (0..n).map(|_| g.below(256) as u8).collect();
    String::from_utf8_lossy(&b).into_owned()
}

#[test]
fn parser_and_evaluator() {
    let mut g = Rng(0x1234_5678_9abc_def1);
    let mut wb = Workbook::new();
    wb.add_sheet("S").unwrap();
    wb.add_sheet("Other sheet").unwrap();
    for r in 0..20 {
        wb.set_input(CellAddr::new(0, r, 0), &format!("{r}")).unwrap();
        wb.set_input(CellAddr::new(0, r, 1), ["x", "", "TRUE", "3", "#N/A"][r as usize % 5]).unwrap();
    }
    let mut parsed = 0u64;
    let n = iters(200_000);
    for i in 0..n {
        let src = if i % 4 == 0 { bytes(&mut g) } else { soup(&mut g) };
        let r = g.below(40) as u32;
        let c = 2 + g.below(6) as u32;
        if wb.set_input(CellAddr::new(0, r, c), &src).is_ok() {
            parsed += 1;
        }
        if i % 64 == 0 {
            wb.recalc();
        }
        if i % 4096 == 0 {
            // Print, reparse, save, load: all must hold up on whatever got in.
            let bytes = native::save_to_vec(&wb);
            native::load(&bytes).expect("own file loads");
        }
    }
    wb.recalc();
    assert!(parsed > n / 20, "the soup should parse sometimes ({parsed} of {n})");
}

#[test]
fn deep_nesting_is_refused_not_overflowed() {
    let wb = {
        let mut w = Workbook::new();
        w.add_sheet("S").unwrap();
        w
    };
    for depth in [63usize, 64, 65, 200, 5000] {
        let src = format!("={}1{}", "(".repeat(depth), ")".repeat(depth));
        let _ = wb.parse(&src, 0, 0, 0);
        let src = format!("={}1", "-".repeat(depth));
        let _ = wb.parse(&src, 0, 0, 0);
        let src = format!("={}1{}", "SUM(".repeat(depth), ")".repeat(depth));
        let _ = wb.parse(&src, 0, 0, 0);
        let src = format!("=1{}", "+1".repeat(depth));
        let _ = wb.parse(&src, 0, 0, 0);
    }
}

#[test]
fn native_reader() {
    let mut wb = Workbook::new();
    wb.add_sheet("S").unwrap();
    wb.add_sheet("T").unwrap();
    wb.set_numbers(0, 0, 0, &(0..5000).map(|i| i as f64).collect::<Vec<_>>());
    for r in 0..50 {
        wb.set_input(CellAddr::new(1, r * 7, r % 5), &format!("t{r}")).unwrap();
    }
    wb.fill_formula(0, 1, 0, 4999, "=A1*2").unwrap();
    wb.set_input(CellAddr::new(1, 0, 9), "=SUM(S!B:B)").unwrap();
    wb.recalc();
    let good = native::save_to_vec(&wb);
    let mut g = Rng(0xfeed_beef_dead_cafe);
    let n = iters(200_000) / 20;
    for _ in 0..n {
        let mut b = good.clone();
        match g.below(4) {
            0 => {
                for _ in 0..1 + g.below(8) {
                    let i = g.below(b.len() as u64) as usize;
                    b[i] = g.below(256) as u8;
                }
            }
            1 => b.truncate(g.below(b.len() as u64) as usize),
            2 => {
                let i = g.below(b.len() as u64) as usize;
                let k = g.below(16) as usize;
                b.splice(i..i, (0..k).map(|_| g.below(256) as u8));
            }
            _ => {
                let i = g.below(b.len() as u64) as usize;
                let j = (i + g.below(64) as usize).min(b.len());
                b.drain(i..j);
            }
        }
        if let Ok(mut w) = native::load(&b) {
            w.recalc();
        }
    }
}

#[test]
fn op_decoder() {
    let op = GridOp::block(
        0,
        2,
        3,
        vec![vec![officina_grid::Input::Formula("=A1+1".into()), officina_grid::Input::Value(officina_grid::Value::str("x"))]],
    );
    let good = op.encode();
    let mut g = Rng(42);
    for _ in 0..iters(200_000) / 4 {
        let mut b = good.clone();
        for _ in 0..1 + g.below(4) {
            let i = g.below(b.len() as u64) as usize;
            b[i] = g.below(256) as u8;
        }
        let _ = GridOp::decode(&b);
        let n = g.below(40) as usize;
        let r: Vec<u8> = (0..n).map(|_| g.below(256) as u8).collect();
        let _ = GridOp::decode(&r);
    }
}
