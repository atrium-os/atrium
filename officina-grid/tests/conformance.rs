//! Formula conformance: each case is a formula and the text Excel shows for it (General
//! format), over a small fixture. Results are as documented by Microsoft or known from
//! Excel behaviour; there is no Excel in this build environment to diff against
//! automatically, so the table is hand-checked. Converters (O4) add a corpus of real
//! workbooks with Excel's stored results, which is the larger differential test.

use officina_grid::{CellAddr, Workbook};

fn fixture() -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("S").unwrap();
    wb.add_sheet("Other sheet").unwrap();
    let set = |wb: &mut Workbook, a: &str, s: &str| {
        let (r, c) = officina_grid::addr::parse_a1(a).unwrap();
        wb.set_input(CellAddr::new(0, r, c), s).unwrap();
    };
    for (i, v) in ["1", "2", "3", "4", "5"].iter().enumerate() {
        set(&mut wb, &format!("A{}", i + 1), v);
    }
    for (i, v) in ["apple", "Banana", "cherry", "apple pie", "date"].iter().enumerate() {
        set(&mut wb, &format!("B{}", i + 1), v);
    }
    set(&mut wb, "D1", "10");
    set(&mut wb, "D2", "x");
    set(&mut wb, "D4", "20");
    set(&mut wb, "E1", "=1/0");
    wb.set_input(CellAddr::new(1, 0, 0), "7").unwrap();
    wb
}

const CASES: &[(&str, &str)] = &[
    // operators and coercion
    ("=1+2", "3"),
    ("=-2^2", "4"),
    ("=2^3^2", "64"),
    ("=10/4", "2.5"),
    ("=1/0", "#DIV/0!"),
    ("=0.1+0.2", "0.3"),
    ("=1/3", "0.333333333333333"),
    ("=10/3", "3.33333333333333"),
    ("=5%", "0.05"),
    ("=123456789*1000000000", "1.23456789E+17"),
    ("=2^-30", "9.31322574615479E-10"),
    ("=1E+300*1E+300", "#NUM!"),
    ("=\"a\"&1", "a1"),
    ("=\"x\"&1/3", "x0.333333333333333"),
    ("=\"1\"+1", "2"),
    ("=\"x\"+1", "#VALUE!"),
    ("=-\"3\"", "-3"),
    ("=TRUE+1", "2"),
    ("=\"A\"=\"a\"", "TRUE"),
    ("=\"abc\"<\"abd\"", "TRUE"),
    ("=1<\"a\"", "TRUE"),
    ("=\"z\"<TRUE", "TRUE"),
    ("=TRUE=1", "FALSE"),
    ("=D3=0", "TRUE"),
    ("=D3=\"\"", "TRUE"),
    ("=SUM({1,2;3,4})", "10"),
    ("=SUM(A1:A5*2)", "30"),
    ("='Other sheet'!A1*2", "14"),
    // math
    ("=SUM(A1:A5)", "15"),
    ("=SUM(A:A)", "15"),
    ("=SUM(A1:A5,\"3\")", "18"),
    ("=SUM(B1:B5)", "0"),
    ("=SUM(D1:D4)", "30"),
    ("=SUM(E1,1)", "#DIV/0!"),
    ("=AVERAGE(D1:D4)", "15"),
    ("=AVERAGE(B1:B5)", "#DIV/0!"),
    ("=COUNT(D1:D4)", "2"),
    ("=COUNTA(D1:D4)", "3"),
    ("=COUNTBLANK(D1:D4)", "1"),
    ("=PRODUCT(A1:A5)", "120"),
    ("=MIN(A1:A5)", "1"),
    ("=MAX(A1:A5)", "5"),
    ("=MAX(B1:B5)", "0"),
    ("=ROUND(2.675,2)", "2.68"),
    ("=ROUND(-2.5,0)", "-3"),
    ("=ROUND(0.5,0)", "1"),
    ("=ROUND(1234.5,-2)", "1200"),
    ("=ROUNDUP(3.2,0)", "4"),
    ("=ROUNDDOWN(-3.7,0)", "-3"),
    ("=INT(-3.5)", "-4"),
    ("=TRUNC(-3.5)", "-3"),
    ("=TRUNC(3.14159,2)", "3.14"),
    ("=MOD(-3,2)", "1"),
    ("=MOD(3,-2)", "-1"),
    ("=MOD(1,0)", "#DIV/0!"),
    ("=POWER(2,10)", "1024"),
    ("=SQRT(-1)", "#NUM!"),
    ("=LN(0)", "#NUM!"),
    ("=LOG(100)", "2"),
    ("=LOG(8,2)", "3"),
    ("=SIGN(-4)", "-1"),
    ("=ABS(-7)", "7"),
    ("=PI()", "3.14159265358979"),
    ("=SUMPRODUCT(A1:A3,A3:A5)", "26"),
    ("=SUMPRODUCT((A1:A5>2)*A1:A5)", "12"),
    ("=RANDBETWEEN(3,3)", "3"),
    // conditional aggregates
    ("=SUMIF(A1:A5,\">2\")", "12"),
    ("=SUMIF(B1:B5,\"apple*\",A1:A5)", "5"),
    ("=COUNTIF(B1:B5,\"?pple\")", "1"),
    ("=COUNTIF(B1:B5,\"a*\")", "2"),
    ("=COUNTIF(A1:A5,3)", "1"),
    ("=COUNTIF(A1:A5,\"3\")", "1"),
    ("=COUNTIF(D1:D4,\"\")", "1"),
    ("=COUNTIF(D1:D4,\"<>\")", "3"),
    ("=COUNTIFS(A1:A5,\">1\",A1:A5,\"<5\")", "3"),
    ("=SUMIFS(A1:A5,A1:A5,\">=2\",B1:B5,\"<>date\")", "9"),
    ("=AVERAGEIF(A1:A5,\"<>3\")", "3"),
    // statistics
    ("=MEDIAN(A1:A4)", "2.5"),
    ("=STDEV(A1:A5)", "1.58113883008419"),
    ("=STDEV.P(A1:A5)", "1.4142135623731"),
    ("=VAR(A1:A5)", "2.5"),
    ("=VAR.P(A1:A5)", "2"),
    // logic and information
    ("=IF(A1>0,\"yes\",\"no\")", "yes"),
    ("=IF(FALSE,1)", "FALSE"),
    ("=IFS(A1>5,\"a\",A1>0,\"b\")", "b"),
    ("=IFERROR(1/0,\"bad\")", "bad"),
    ("=IFNA(NA(),0)", "0"),
    ("=AND(TRUE,1)", "TRUE"),
    ("=OR(FALSE,0)", "FALSE"),
    ("=XOR(TRUE,TRUE)", "FALSE"),
    ("=NOT(0)", "TRUE"),
    ("=ISBLANK(D3)", "TRUE"),
    ("=ISNUMBER(A1)", "TRUE"),
    ("=ISTEXT(B1)", "TRUE"),
    ("=ISLOGICAL(TRUE)", "TRUE"),
    ("=ISERROR(E1)", "TRUE"),
    ("=ISERROR(#REF!)", "TRUE"),
    ("=ISERR(NA())", "FALSE"),
    ("=ISNA(NA())", "TRUE"),
    ("=NA()", "#N/A"),
    ("=ROW(A7)", "7"),
    ("=COLUMN(C1)", "3"),
    ("=ROWS(A1:B5)", "5"),
    ("=COLUMNS(A1:C1)", "3"),
    // text
    ("=LEN(\"héllo\")", "5"),
    ("=LEN(1/3)", "17"),
    ("=LEFT(\"abc\",2)", "ab"),
    ("=LEFT(123.5,3)", "123"),
    ("=RIGHT(\"abc\")", "c"),
    ("=MID(\"abcdef\",2,3)", "bcd"),
    ("=MID(\"abc\",5,1)", ""),
    ("=UPPER(\"aB\")", "AB"),
    ("=LOWER(\"aB\")", "ab"),
    ("=TRIM(\"  a   b \")", "a b"),
    ("=CONCATENATE(\"a\",1,TRUE)", "a1TRUE"),
    ("=CONCAT(A1:A3)", "123"),
    ("=TEXTJOIN(\"-\",TRUE,B1:B2,D3)", "apple-Banana"),
    ("=REPT(\"ab\",3)", "ababab"),
    ("=FIND(\"b\",\"abcb\")", "2"),
    ("=FIND(\"B\",\"abc\")", "#VALUE!"),
    ("=SEARCH(\"B\",\"abc\")", "2"),
    ("=SEARCH(\"?c\",\"abc\")", "2"),
    ("=SUBSTITUTE(\"aaa\",\"a\",\"b\",2)", "aba"),
    ("=SUBSTITUTE(\"aaa\",\"a\",\"b\")", "bbb"),
    ("=VALUE(\"1e3\")", "1000"),
    ("=VALUE(\"abc\")", "#VALUE!"),
    ("=EXACT(\"a\",\"A\")", "FALSE"),
    // lookup
    ("=CHOOSE(2,\"x\",\"y\")", "y"),
    ("=INDEX(A1:A5,3)", "3"),
    ("=INDEX(A1:B5,2,2)", "Banana"),
    ("=MATCH(3,A1:A5,0)", "3"),
    ("=MATCH(3.5,A1:A5)", "3"),
    ("=MATCH(\"banana\",B1:B5,0)", "2"),
    ("=MATCH(\"b*\",B1:B5,0)", "2"),
    ("=MATCH(9,A1:A5,0)", "#N/A"),
    ("=VLOOKUP(3,A1:B5,2,FALSE)", "cherry"),
    ("=VLOOKUP(9,A1:B5,2,FALSE)", "#N/A"),
    ("=VLOOKUP(4.5,A1:B5,2)", "apple pie"),
    ("=VLOOKUP(3,A1:B5,3,FALSE)", "#REF!"),
    ("=HLOOKUP(\"b\",{\"a\",\"b\";1,2},2,FALSE)", "2"),
    ("=XLOOKUP(\"date\",B1:B5,A1:A5)", "5"),
    ("=XLOOKUP(\"zz\",B1:B5,A1:A5,\"none\")", "none"),
    // dates (1900 system, with Excel's 29 Feb 1900)
    ("=DATE(2024,2,29)", "45351"),
    ("=DATE(1900,2,28)", "59"),
    ("=DATE(1900,3,1)", "61"),
    ("=YEAR(45351)", "2024"),
    ("=MONTH(45351)", "2"),
    ("=DAY(45351)", "29"),
    ("=WEEKDAY(45351)", "5"),
    ("=DAY(60)", "29"),
    ("=MONTH(60)", "2"),
    ("=YEAR(DATE(2024,14,1))", "2025"),
];

#[test]
fn excel_results() {
    let mut wb = fixture();
    let col = 25; // Z, away from the fixture
    for (i, (f, _)) in CASES.iter().enumerate() {
        wb.set_input(CellAddr::new(0, 100 + i as u32, col), f).unwrap_or_else(|e| panic!("{f}: {e:?}"));
    }
    wb.recalc();
    let mut bad = Vec::new();
    for (i, (f, want)) in CASES.iter().enumerate() {
        let got = wb.get(CellAddr::new(0, 100 + i as u32, col)).display();
        if got != *want {
            bad.push(format!("{f}: got {got:?}, want {want:?}"));
        }
    }
    assert!(bad.is_empty(), "{} of {} cases differ:\n{}", bad.len(), CASES.len(), bad.join("\n"));
}

#[test]
fn print_parse_round_trip() {
    let wb = fixture();
    for (f, _) in CASES {
        let e = wb.parse(f, 0, 3, 25).unwrap();
        let printed = officina_grid::formula::to_a1(&e, 3, 25, &|s| wb.sheet_name(s));
        let again = wb.parse(&printed, 0, 3, 25).unwrap_or_else(|e| panic!("{f} printed as {printed}: {e:?}"));
        assert_eq!(e, again, "{f} printed as {printed}");
        // Printed at another cell and parsed there, the relative formula is the same.
        let moved = officina_grid::formula::to_a1(&e, 50, 30, &|s| wb.sheet_name(s));
        if !moved.contains("#REF!") {
            assert_eq!(wb.parse(&moved, 0, 50, 30).unwrap(), e, "{f} moved as {moved}");
        }
    }
}
