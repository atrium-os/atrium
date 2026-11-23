//! ODF filter expressions, with a compact form for independent column rules.

use officina_grid::filter::{Cond, FilterExpr, Rank, Test, TextOp, TextTest, MAX_FILTER_DEPTH, MAX_FILTER_NODES, MAX_TESTS};
use quick_xml::{
    events::{BytesEnd, BytesStart, Event},
    Reader,
};

use super::read::attrs;

type Columns = Vec<(u32, Cond)>;

pub(super) fn parse(xml: &[u8], whole_cell: bool) -> Option<Columns> {
    group(&mut Reader::from_reader(xml), None, true, 0, whole_cell).or_else(|| {
        let mut remaining = MAX_FILTER_NODES;
        let expr = expression(&mut Reader::from_reader(xml), None, true, 0, whole_cell, &mut remaining)?;
        Some(vec![(0, Cond::Expression(Box::new(expr)))])
    })
}

fn group(
    reader: &mut Reader<&[u8]>,
    end: Option<BytesEnd<'_>>,
    and: bool,
    depth: u8,
    whole_cell: bool,
) -> Option<Columns> {
    if depth > 16 {
        return None;
    }
    let mut columns = Columns::new();
    loop {
        let (tag, empty) = match reader.read_event().ok()? {
            Event::Start(tag) => (tag, false),
            Event::Empty(tag) => (tag, true),
            Event::End(tag) if end.as_ref() == Some(&tag) => break,
            Event::Eof if end.is_none() => break,
            Event::Text(t) if t.iter().all(u8::is_ascii_whitespace) => continue,
            Event::Comment(_) => continue,
            _ => return None,
        };
        let next = match tag.name().as_ref() {
            b"table:filter-and" | b"table:filter-or" if !empty => {
                let join = tag.name().as_ref() == b"table:filter-and";
                group(reader, Some(tag.to_end()), join, depth + 1, whole_cell)?
            }
            b"table:filter-condition" => vec![condition(reader, &tag, empty, whole_cell)?],
            _ => return None,
        };
        if !and && (next.len() != 1 || columns.first().is_some_and(|(c, _)| *c != next[0].0)) {
            return None;
        }
        for (col, cond) in next {
            match columns.binary_search_by_key(&col, |(c, _)| *c) {
                Ok(i) => {
                    let previous = std::mem::replace(&mut columns[i].1, Cond::Values(Vec::new()));
                    columns[i].1 = join(previous, cond, and)?;
                }
                Err(i) => columns.insert(i, (col, cond)),
            }
        }
    }
    Some(columns)
}

fn expression(
    reader: &mut Reader<&[u8]>, end: Option<BytesEnd<'_>>, and: bool, depth: usize,
    whole_cell: bool, remaining: &mut usize,
) -> Option<FilterExpr> {
    if depth > MAX_FILTER_DEPTH || *remaining == 0 { return None; }
    *remaining -= 1;
    let mut items = Vec::new();
    loop {
        match reader.read_event().ok()? {
            Event::End(tag) if end.as_ref() == Some(&tag) => break,
            Event::Eof if end.is_none() => break,
            Event::Text(t) if t.iter().all(u8::is_ascii_whitespace) => continue,
            Event::Comment(_) => continue,
            event @ (Event::Start(_) | Event::Empty(_)) => {
                let empty = matches!(event, Event::Empty(_));
                let (Event::Start(tag) | Event::Empty(tag)) = event else { unreachable!() };
                let item = match tag.name().as_ref() {
                    b"table:filter-and" | b"table:filter-or" if !empty =>
                        expression(reader, Some(tag.to_end()), tag.name().as_ref() == b"table:filter-and", depth + 1, whole_cell, remaining)?,
                    b"table:filter-condition" if *remaining > 0 && depth < MAX_FILTER_DEPTH => {
                        *remaining -= 1;
                        let (col, cond) = condition(reader, &tag, empty, whole_cell)?;
                        FilterExpr::Column(col, cond)
                    }
                    _ => return None,
                };
                items.push(item);
            }
            _ => return None,
        }
    }
    if items.is_empty() { return None; }
    if items.len() == 1 { return items.pop(); }
    Some(if and { FilterExpr::All(items) } else { FilterExpr::Any(items) })
}

fn truth(value: &str) -> bool {
    matches!(value, "true" | "1")
}

fn condition(
    reader: &mut Reader<&[u8]>,
    tag: &BytesStart<'_>,
    empty: bool,
    whole_cell: bool,
) -> Option<(u32, Cond)> {
    let a = attrs(tag);
    let field = a.get("table:field-number")?.parse().ok()?;
    if field >= officina_grid::addr::MAX_COLS {
        return None;
    }
    let value = a.get("table:value").map(String::as_str).unwrap_or_default();
    let case_sensitive = a.get("table:case-sensitive").is_some_and(|s| truth(s));
    if !empty {
        let mut values = Vec::new();
        loop {
            match reader.read_event().ok()? {
                Event::End(end) if end.name() == tag.name() => break,
                Event::Text(t) if t.iter().all(u8::is_ascii_whitespace) => {}
                Event::Empty(item) if item.name().as_ref() == b"table:filter-set-item" => {
                    if values.len() == 1 << 20 {
                        return None;
                    }
                    values.push(attrs(&item).remove("table:value")?);
                }
                _ => return None,
            }
        }
        if !values.is_empty() {
            return (!case_sensitive).then_some((field, Cond::Values(values)));
        }
    }
    let op = a.get("table:operator")?.as_str();
    let number = match a.get("table:data-type").map(String::as_str) {
        Some(t) => t == "number",
        // LibreOffice also writes numeric order comparisons without a type.
        None => matches!(op, "<" | "<=" | ">" | ">=") && value.parse::<f64>().is_ok(),
    };
    let rank = match op {
        "top values" => Some((false, false)),
        "top percent" => Some((true, false)),
        "bottom values" => Some((false, true)),
        "bottom percent" => Some((true, true)),
        _ => None,
    };
    if let Some((percent, bottom)) = rank {
        return Some((field, Cond::Rank(Rank::parse(value, percent, bottom)?)));
    }

    let symbol = if op == "!=" { "<>" } else { op };
    if number {
        TextOp::comparison(symbol)?;
        let x: f64 = value.parse().ok()?;
        return x
            .is_finite()
            .then(|| (field, Cond::Compare(vec![(symbol.to_string(), x)], true)));
    }
    if op == "=" && !case_sensitive && whole_cell {
        return Some((field, Cond::Values(vec![value.to_string()])));
    }
    use TextOp::*;
    let (op, value) = match op {
        "begins" => (Begins, value),
        "!begins" => (NotBegins, value),
        "ends" => (Ends, value),
        "!ends" => (NotEnds, value),
        "=" if !whole_cell => (Contains, value),
        "!=" if !whole_cell => (NotContains, value),
        "contains" => (Contains, value),
        "!contains" => (NotContains, value),
        "match" if whole_cell => (Regex, value),
        "!match" if whole_cell => (NotRegex, value),
        "match" => (PartialRegex, value),
        "!match" => (NotPartialRegex, value),
        _ => (TextOp::comparison(symbol)?, value),
    };
    let text = TextTest {
        op,
        value: value.to_string(),
        case_sensitive,
    };
    text.valid()
        .then(|| (field, Cond::Custom(vec![Test::Text(text)], true)))
}

fn join(a: Cond, b: Cond, and: bool) -> Option<Cond> {
    match (a, b) {
        (Cond::Values(mut a), Cond::Values(b)) if !and => {
            if a.len() + b.len() > 1 << 20 {
                return None;
            }
            a.extend(b);
            Some(Cond::Values(a))
        }
        (a, b) => {
            let mut tests = clauses(a, and)?;
            tests.extend(clauses(b, and)?);
            (tests.len() <= MAX_TESTS).then(|| Cond::custom(tests, and))
        }
    }
}

fn clauses(cond: Cond, and: bool) -> Option<Vec<Test>> {
    let values = match cond {
        Cond::Compare(tests, join) if tests.len() == 1 || join == and => {
            return Some(
                tests
                    .into_iter()
                    .map(|(op, x)| Test::Number(op, x))
                    .collect(),
            );
        }
        Cond::Custom(tests, join) if tests.len() == 1 || join == and => return Some(tests),
        Cond::Values(values) if values.len() <= MAX_TESTS && (values.len() == 1 || !and) => values,
        _ => return None,
    };
    values
        .into_iter()
        .map(|value| {
            let text = TextTest {
                op: TextOp::Eq,
                value,
                case_sensitive: false,
            };
            text.valid().then_some(Test::Text(text))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_regex_and_escaped_literal_are_same_column_alternatives() {
        let xml = br#"<table:filter-and>
            <table:filter-or>
              <table:filter-condition table:field-number="1" table:operator="!match" table:value="[a-c]+" table:case-sensitive="true"/>
              <table:filter-condition table:field-number="1" table:operator="contains" table:value="&lt;&amp;"/>
            </table:filter-or>
            <table:filter-condition table:field-number="2" table:operator="=" table:value="blue"/>
          </table:filter-and>"#;
        let text = |op, value: &str, case_sensitive| {
            Test::Text(TextTest {
                op,
                value: value.into(),
                case_sensitive,
            })
        };
        assert_eq!(
            parse(xml, false),
            Some(vec![
                (
                    1,
                    Cond::Custom(
                        vec![
                            text(TextOp::NotPartialRegex, "[a-c]+", true),
                            text(TextOp::Contains, "<&", false)
                        ],
                        false
                    )
                ),
                (
                    2,
                    Cond::Custom(vec![text(TextOp::Contains, "blue", false)], true)
                ),
            ])
        );
    }

    #[test]
    fn cross_column_or_keeps_both_branches_and_invalid_regex_is_rejected() {
        let xml = br#"<table:filter-or>
            <table:filter-condition table:field-number="0" table:operator="begins" table:value="A"/>
            <table:filter-condition table:field-number="1" table:operator="begins" table:value="B"/>
          </table:filter-or>"#;
        assert!(matches!(parse(xml, true).unwrap().as_slice(), [(0, Cond::Expression(_))]));
        let xml = br#"<table:filter-condition table:field-number="0" table:operator="match" table:value="(?=A)"/>"#;
        assert_eq!(parse(xml, true), None);
    }
}
