//! Dynamic arrays, spill, `LET` and `LAMBDA` (docs/spec/officina.md §16.31). Each case
//! is a formula and the grid Excel 365 spills for it (rows split by `;`, columns by
//! `,`, General format), over the same fixture as conformance.rs. Results are Excel's
//! as documented by Microsoft and checked by hand; the .xlsx round trip in
//! officina-convert's tests/arrays.rs reads Excel-saved results for a subset.

use officina_grid::{CellAddr, ErrorKind, Value, Workbook};

fn fixture() -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("S").unwrap();
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
    wb.recalc();
    wb
}

const CASES: &[(&str, &str)] = &[
    // spilling references and operators
    ("=A1:A3", "1;2;3"),
    ("=A1:A3*2", "2;4;6"),
    ("=D1:D4", "10;x;0;20"),
    ("=A1:A2&B1:B2", "1apple;2Banana"),
    ("={1,2;3,4}*10", "10,20;30,40"),
    ("=SUM(A1:A5*2)", "30"),
    ("=A1:A2+A1:A3", "2;4;#N/A"),
    ("={1,2;3,4}+{10,20,30;40,50,60}", "11,22,#N/A;43,54,#N/A"),
    ("=A4:A7*2", "8;10;0;0"),
    ("=SUM(A4:A7*2)", "18"),
    ("=IFERROR(A1:A3/{1;0},99)", "1;99;99"),
    (r#"=IFERROR(E10:E12,"fallback")"#, ";;"),
    (r#"=TEXTAFTER({"a-x";"b:y";"missing"},{"-",":"},1,,,7)"#, "x;y;7"),
    (r#"=--TEXTAFTER({"Sun, 1 January 2023";"Mon, 2 January 2023"},", ")"#, "44927;44928"),
    ("=LARGE(A1:A5,{1;3})", "5;3"),
    ("=SMALL(A1:A5,{1;3})", "1;3"),
    // making arrays
    ("=SEQUENCE(3)", "1;2;3"),
    ("=SEQUENCE(2,3)", "1,2,3;4,5,6"),
    ("=SEQUENCE(2,2,10,-1)", "10,9;8,7"),
    ("=SEQUENCE(0)", "#CALC!"),
    ("=SUM(SEQUENCE(10))", "55"),
    ("=ROWS(RANDARRAY(3,2))", "3"),
    ("=COLUMNS(RANDARRAY(3,2))", "2"),
    ("=MAKEARRAY(2,2,LAMBDA(r,c,r*10+c))", "11,12;21,22"),
    // reshaping
    ("=TRANSPOSE(A1:A3)", "1,2,3"),
    ("=VSTACK({1,2},{3})", "1,2;3,#N/A"),
    ("=HSTACK({1;2},{3})", "1,3;2,#N/A"),
    ("=TAKE(A1:A5,2)", "1;2"),
    ("=TAKE(A1:A5,-2)", "4;5"),
    ("=DROP(A1:A5,3)", "4;5"),
    ("=DROP(A1:A5,9)", "#CALC!"),
    ("=CHOOSEROWS(A1:A5,1,-1)", "1;5"),
    ("=CHOOSECOLS({1,2,3},3,1)", "3,1"),
    ("=CHOOSEROWS(A1:A5,6)", "#VALUE!"),
    ("=TOCOL({1,2;3,4})", "1;2;3;4"),
    ("=TOCOL({1,2;3,4},0,TRUE)", "1;3;2;4"),
    ("=TOROW({1;2})", "1,2"),
    ("=WRAPROWS(SEQUENCE(5),2)", "1,2;3,4;5,#N/A"),
    ("=WRAPCOLS(SEQUENCE(5),2,0)", "1,3,5;2,4,0"),
    ("=EXPAND({1,2},2,3,\"-\")", "1,2,-;-,-,-"),
    ("=EXPAND({1,2},1,1)", "#VALUE!"),
    // filtering, sorting, unique
    ("=FILTER(A1:A5,A1:A5>2)", "3;4;5"),
    ("=FILTER(A1:A5,A1:A5>9)", "#CALC!"),
    ("=FILTER(A1:A5,A1:A5>9,\"none\")", "none"),
    ("=FILTER(B1:B5,LEFT(B1:B5,1)=\"a\")", "apple;apple pie"),
    ("=FILTER(A1:B5,A1:A5<3)", "1,apple;2,Banana"),
    ("=SORT(B1:B5)", "apple;apple pie;Banana;cherry;date"),
    ("=SORT(A1:A5,1,-1)", "5;4;3;2;1"),
    ("=SORT(A1:B3,2,-1)", "3,cherry;2,Banana;1,apple"),
    ("=SORT({3,1,2},,,TRUE)", "1,2,3"),
    ("=SORTBY(B1:B3,A1:A3,-1)", "cherry;Banana;apple"),
    ("=UNIQUE({1;2;1;3})", "1;2;3"),
    ("=UNIQUE({\"a\";\"A\";\"b\"})", "a;b"),
    ("=UNIQUE({1;2;1;3},,TRUE)", "2;3"),
    ("=UNIQUE({1,1,2},TRUE)", "1,2"),
    // lifting one-value functions over arrays
    ("=ABS({-1,2,-3})", "1,2,3"),
    ("=LEN(B1:B3)", "5;6;6"),
    ("=UPPER(B1:B2)", "APPLE;BANANA"),
    ("=IF(A1:A4>2,\"big\",\"small\")", "small;small;big;big"),
    ("=IFERROR(1/(A1:A3-2),\"-\")", "-1;-;1"),
    ("=ROUND(A1:A3/3,1)", "0.3;0.7;1"),
    ("=ROW(A3:A5)", "3;4;5"),
    ("=COLUMN(A1:C1)", "1,2,3"),
    ("=SUM(ROW(A1:A3))", "6"),
    ("=COUNTIF(A1:A5,{1,5})", "1,1"),
    ("=MATCH({3,1},A1:A5,0)", "3,1"),
    ("=INDEX(A1:B5,0,2)", "apple;Banana;cherry;apple pie;date"),
    ("=INDEX(A1:B5,{1;3},2)", "apple;cherry"),
    ("=XLOOKUP(2,A1:A5,B1:B5)", "Banana"),
    // LET and LAMBDA
    ("=LET(x,2,y,x*3,x+y)", "8"),
    ("=LET(x,A1:A3,x*10)", "10;20;30"),
    ("=LET(f,LAMBDA(n,n*2),f(21))", "42"),
    ("=LAMBDA(x,x+1)(41)", "42"),
    ("=LAMBDA(x,x)", "#CALC!"),
    ("=LAMBDA(x,y,IF(ISOMITTED(y),x,x+y))(1,)", "1"),
    ("=LAMBDA(x,y,IF(ISOMITTED(y),x,x+y))(1,2)", "3"),
    ("=LAMBDA(x,x)(1,2)", "#VALUE!"),
    ("=MAP(A1:A3,LAMBDA(v,v*v))", "1;4;9"),
    ("=MAP(A1:A2,B1:B2,LAMBDA(n,s,s&n))", "apple1;Banana2"),
    ("=REDUCE(0,A1:A5,LAMBDA(a,v,a+v))", "15"),
    ("=REDUCE(1,A1:A5,LAMBDA(a,v,a*v))", "120"),
    ("=SCAN(0,A1:A4,LAMBDA(a,v,a+v))", "1;3;6;10"),
    ("=BYROW({1,2;3,4},LAMBDA(r,SUM(r)))", "3;7"),
    ("=BYCOL({1,2;3,4},LAMBDA(c,MAX(c)))", "3,4"),
    ("=BYROW({1,2;3,4},LAMBDA(r,r))", "#CALC!;#CALC!"),
];

fn grid(wb: &Workbook, a: CellAddr) -> String {
    let area = wb.spill_area(a).unwrap_or(officina_grid::Rect::cell(a));
    let mut rows = Vec::new();
    for r in area.r0..=area.r1 {
        let mut cols = Vec::new();
        for c in area.c0..=area.c1 {
            cols.push(wb.value(a.sheet, r, c).display());
        }
        rows.push(cols.join(","));
    }
    rows.join(";")
}

#[test]
fn excel_spills() {
    let mut wb = fixture();
    for (i, (f, _)) in CASES.iter().enumerate() {
        wb.set_input(CellAddr::new(0, 100 + 8 * i as u32, 30), f).unwrap_or_else(|e| panic!("{f}: {e:?}"));
    }
    wb.recalc();
    let mut bad = Vec::new();
    for (i, (f, want)) in CASES.iter().enumerate() {
        let got = grid(&wb, CellAddr::new(0, 100 + 8 * i as u32, 30));
        if got != *want {
            bad.push(format!("{f}: got {got:?}, want {want:?}"));
        }
    }
    assert!(bad.is_empty(), "{} of {} cases differ:\n{}", bad.len(), CASES.len(), bad.join("\n"));
}

#[test]
fn print_parse_round_trip() {
    let wb = fixture();
    for f in CASES.iter().map(|c| c.0).chain(["=H1#", "=SUM(Sheet1!H1#)", "=@A1:A3", "=@(A1:A3*2)", "=LAMBDA(x,x+1)(2)"]) {
        let Ok(e) = wb.parse(f, 0, 3, 25) else {
            assert!(f.contains("Sheet1"), "{f}");
            continue;
        };
        let printed = officina_grid::formula::to_a1(&e, 3, 25, &|s| wb.sheet_name(s));
        assert_eq!(printed, f, "printed back");
        let again = wb.parse(&printed, 0, 3, 25).unwrap();
        assert_eq!(e, again, "{f} printed as {printed}");
    }
}

#[test]
fn files_spell_new_syntax_as_excel_does() {
    let wb = fixture();
    let file = |f: &str| {
        let e = wb.parse(f, 0, 0, 0).unwrap();
        officina_grid::formula::to_a1_file(&e, 0, 0, &|s| wb.sheet_name(s), &|_, _| None)
    };
    assert_eq!(file("=SUM(H1#)"), "=SUM(_xlfn.ANCHORARRAY(H1))");
    assert_eq!(file("=@A1:A3"), "=_xlfn.SINGLE(A1:A3)");
    assert_eq!(file("=LET(x,2,f,LAMBDA(y,x*y),f(x))"), "=LET(_xlpm.x,2,_xlpm.f,LAMBDA(_xlpm.y,_xlpm.x*_xlpm.y),_xlpm.f(_xlpm.x))");
    // Read back: SINGLE and ANCHORARRAY are the operators again.
    assert_eq!(wb.parse("=SUM(ANCHORARRAY(H1))", 0, 0, 0).unwrap(), wb.parse("=SUM(H1#)", 0, 0, 0).unwrap());
    assert_eq!(wb.parse("=SINGLE(A1:A3)", 0, 0, 0).unwrap(), wb.parse("=@A1:A3", 0, 0, 0).unwrap());
}

fn at(wb: &Workbook, a: &str) -> Value {
    let (r, c) = officina_grid::addr::parse_a1(a).unwrap();
    wb.value(0, r, c)
}
fn put(wb: &mut Workbook, a: &str, s: &str) {
    let (r, c) = officina_grid::addr::parse_a1(a).unwrap();
    wb.set_input(CellAddr::new(0, r, c), s).unwrap();
    wb.recalc();
}

#[test]
fn spills_block_unblock_and_follow_their_formula() {
    let mut wb = fixture();
    put(&mut wb, "H1", "=SEQUENCE(3)");
    assert_eq!((at(&wb, "H1"), at(&wb, "H2"), at(&wb, "H3")), (Value::Num(1.0), Value::Num(2.0), Value::Num(3.0)));
    // A reader of the spill, and of a spilled cell.
    put(&mut wb, "I1", "=SUM(H1#)");
    put(&mut wb, "J1", "=H3*10");
    assert_eq!(at(&wb, "I1"), Value::Num(6.0));
    assert_eq!(at(&wb, "J1"), Value::Num(30.0));
    // The spill grows: its readers follow.
    put(&mut wb, "H1", "=SEQUENCE(4)");
    assert_eq!(at(&wb, "H4"), Value::Num(4.0));
    assert_eq!(at(&wb, "I1"), Value::Num(10.0));
    // Something in the way: #SPILL!, the rest cleared, readers see it.
    put(&mut wb, "H3", "x");
    assert_eq!(at(&wb, "H1"), Value::Err(ErrorKind::Spill));
    assert_eq!(at(&wb, "H2"), Value::Empty);
    assert_eq!(at(&wb, "H3"), Value::str("x"));
    assert_eq!(at(&wb, "H4"), Value::Empty);
    assert_eq!(at(&wb, "I1"), Value::Err(ErrorKind::Ref));
    assert_eq!(at(&wb, "J1"), Value::Err(ErrorKind::Value));
    // Out of the way again: it spills.
    put(&mut wb, "H3", "");
    assert_eq!(at(&wb, "H1"), Value::Num(1.0));
    assert_eq!(at(&wb, "H3"), Value::Num(3.0));
    assert_eq!(at(&wb, "I1"), Value::Num(10.0));
    assert_eq!(at(&wb, "J1"), Value::Num(30.0));
    // A formula in the way blocks too.
    put(&mut wb, "H2", "=1");
    assert_eq!(at(&wb, "H1"), Value::Err(ErrorKind::Spill));
    put(&mut wb, "H2", "");
    assert_eq!(at(&wb, "H2"), Value::Num(2.0));
    // Another spill in the way: the later one is blocked.
    put(&mut wb, "G2", "=SEQUENCE(1,3)");
    assert_eq!(at(&wb, "G2"), Value::Err(ErrorKind::Spill));
    put(&mut wb, "G2", "");
    // The input of a spilled cell is its value; its anchor says where it comes from.
    assert_eq!(wb.spilled_from(CellAddr::new(0, 2, 7)), Some(CellAddr::new(0, 0, 7)));
    // The formula goes: so does its spill.
    put(&mut wb, "H1", "");
    assert_eq!((at(&wb, "H2"), at(&wb, "H4")), (Value::Empty, Value::Empty));
    assert_eq!(at(&wb, "I1"), Value::Err(ErrorKind::Ref));
    assert_eq!(at(&wb, "J1"), Value::Num(0.0));
    // A one-cell result doesn't spill; A1# of it is the cell.
    put(&mut wb, "H1", "=5");
    assert_eq!(at(&wb, "I1"), Value::Num(5.0));
}

#[test]
fn a_spill_depending_on_a_spill_settles() {
    let mut wb = fixture();
    put(&mut wb, "H1", "=SEQUENCE(A1+2)");
    put(&mut wb, "I1", "=H1#*2");
    put(&mut wb, "J1", "=SUM(I1#)");
    assert_eq!(at(&wb, "J1"), Value::Num(12.0));
    put(&mut wb, "A1", "3");
    assert_eq!(at(&wb, "I5"), Value::Num(10.0));
    assert_eq!(at(&wb, "J1"), Value::Num(30.0));
    // A full recalculation agrees.
    wb.recalc_all();
    assert_eq!(at(&wb, "J1"), Value::Num(30.0));
    assert_eq!(at(&wb, "I6"), Value::Empty);
}

#[test]
fn implicit_intersection_keeps_one_value() {
    let mut wb = fixture();
    put(&mut wb, "H3", "=@A1:A5");
    assert_eq!(at(&wb, "H3"), Value::Num(3.0));
    assert_eq!(at(&wb, "H4"), Value::Empty);
    put(&mut wb, "H4", "=@A1:A5*2");
    assert_eq!(at(&wb, "H4"), Value::Num(8.0));
    assert_eq!(wb.formula_text(CellAddr::new(0, 3, 7)).as_deref(), Some("=@A1:A5*2"));
    // Files from before dynamic arrays: top-level `@` only where a formula could spill.
    let legacy = |f: &str| {
        let e = officina_grid::eval::arrays::legacy(wb.parse(f, 0, 0, 0).unwrap());
        officina_grid::formula::to_a1(&e, 0, 0, &|s| wb.sheet_name(s))
    };
    assert_eq!(legacy("=A1:A5"), "=@A1:A5");
    assert_eq!(legacy("=A1:A5+1"), "=@A1:A5+1");
    assert_eq!(legacy("=ABS(A1:A5)"), "=ABS(@A1:A5)");
    assert_eq!(legacy("=SUM(A1:A5)"), "=SUM(A1:A5)");
    assert_eq!(legacy("=A1+1"), "=A1+1");
    assert_eq!(legacy(r#"=TEXTAFTER("a:b",A1:A5)"#), r#"=TEXTAFTER("a:b",@A1:A5)"#);
    assert_eq!(legacy(r#"=TEXTAFTER("a:b",{":","-"})"#), r#"=TEXTAFTER("a:b",{":","-"})"#);
}

#[test]
fn named_lambdas_and_recursion() {
    let mut wb = fixture();
    wb.define_name("DOUBLE", None, "=LAMBDA(x,x*2)").unwrap();
    wb.define_name("FACT", None, "=LAMBDA(n,IF(n<=1,1,n*FACT(n-1)))").unwrap();
    wb.define_name("FOREVER", None, "=LAMBDA(n,FOREVER(n+1))").unwrap();
    put(&mut wb, "H1", "=DOUBLE(A2)");
    put(&mut wb, "H2", "=FACT(10)");
    put(&mut wb, "H3", "=FOREVER(1)");
    put(&mut wb, "H4", "=MAP(A1:A3,DOUBLE)");
    assert_eq!(at(&wb, "H1"), Value::Num(4.0));
    assert_eq!(at(&wb, "H2"), Value::Num(3628800.0));
    assert!(matches!(at(&wb, "H3"), Value::Err(_)));
    assert_eq!((at(&wb, "H4"), at(&wb, "H6")), (Value::Num(2.0), Value::Num(6.0)));
    // A named lambda's reads are its user's dependencies.
    put(&mut wb, "A2", "7");
    assert_eq!(at(&wb, "H1"), Value::Num(14.0));
}

#[test]
fn native_files_keep_formulas_not_spilled_cells() {
    let mut wb = fixture();
    put(&mut wb, "H1", "=SEQUENCE(3)*A1");
    put(&mut wb, "I1", "=SUM(H1#)");
    let bytes = officina_grid::native::save_to_vec(&wb);
    let mut back = officina_grid::native::load(&bytes).unwrap();
    // Not stored: the spilled cells.
    assert_eq!(back.value(0, 1, 7), Value::Empty);
    back.recalc();
    assert_eq!(back.value(0, 2, 7), Value::Num(3.0));
    assert_eq!(back.value(0, 0, 8), Value::Num(6.0));
    assert_eq!(back.input(CellAddr::new(0, 2, 7)), officina_grid::Input::Value(Value::Num(3.0)));
    // A file's cached spill is adopted, not taken for something in the way.
    let mut wb2 = fixture();
    put(&mut wb2, "H2", "2");
    put(&mut wb2, "H3", "3");
    wb2.set_input(CellAddr::new(0, 0, 7), "=SEQUENCE(2)").unwrap();
    wb2.claim_spill(CellAddr::new(0, 0, 7), officina_grid::Rect { sheet: 0, r0: 0, c0: 7, r1: 2, c1: 7 });
    wb2.recalc();
    assert_eq!(wb2.value(0, 1, 7), Value::Num(2.0));
    assert_eq!(wb2.value(0, 2, 7), Value::Empty, "the stale third cell is cleared");
}
