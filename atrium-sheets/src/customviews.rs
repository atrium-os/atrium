//! Custom Views… (docs/spec/officina.md §16.158): a bar that adds, shows,
//! deletes and lists the workbook's named views ([`officina_grid::views`]).

use officina_grid::ops::GridOp;
use officina_grid::views::{valid_name, CustomView, MAX_VIEWS};

use crate::doc::Doc;

/// What the bar did.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    Said(String),
    /// Rows, filter and columns are shown already (one undo step); the
    /// window goes to `sheet`, zooms and selects `cursor`.
    Show { sheet: u16, cursor: (u32, u32), zoom: u16, say: String },
}

/// The bar's text to start with.
pub fn start(d: &Doc) -> String {
    match d.wb.custom_views().first() {
        Some(v) => format!("show {}", v.name),
        None => "add View1".into(),
    }
}

fn find(d: &Doc, name: &str) -> Option<usize> {
    d.wb.custom_views().iter().position(|v| v.name.to_lowercase() == name.to_lowercase())
}

fn list(d: &Doc) -> String {
    let views = d.wb.custom_views();
    if views.is_empty() {
        return "No custom views yet: add NAME saves this sheet's view.".into();
    }
    let names: Vec<String> = views.iter().map(|v| format!("{} ({})", v.name, d.wb.sheet_name(v.sheet))).collect();
    format!("Views: {}.", names.join(", "))
}

/// `add NAME`, `show NAME` (or the name alone), `delete NAME`, `list`.
pub fn run(d: &mut Doc, cursor: (u32, u32), typed: &str) -> Result<Outcome, String> {
    let t = typed.trim();
    let (verb, rest) = t.split_once(char::is_whitespace).map_or((t, ""), |(v, r)| (v, r.trim()));
    let verb = verb.to_ascii_lowercase();
    let named = |rest: &str| -> Result<String, String> {
        if valid_name(rest) { Ok(rest.to_string()) } else { Err(format!("Name the view: 1 to {} bytes, no control characters.", officina_grid::views::MAX_NAME)) }
    };
    match verb.as_str() {
        "list" | "" if rest.is_empty() => Ok(Outcome::Said(list(d))),
        "add" => {
            let name = named(rest)?;
            let view = CustomView::capture(&d.wb, &name, d.sheet, cursor).ok_or("That view can't be saved.")?;
            let mut views = d.wb.custom_views().to_vec();
            let replaced = match find(d, &name) {
                Some(i) => { views[i] = view; true }
                None if views.len() >= MAX_VIEWS => return Err(format!("A workbook keeps at most {MAX_VIEWS} views.")),
                None => { views.push(view); false }
            };
            d.status.clear();
            if !d.run_action("Add View", vec![GridOp::CustomViews(views)]) {
                return Err(std::mem::take(&mut d.status));
            }
            Ok(Outcome::Said(format!("{} view “{name}” of {}: its hidden rows and filter, columns, zoom and selected cell.",
                if replaced { "Replaced the" } else { "Saved the" }, d.wb.sheet_name(d.sheet))))
        }
        "delete" | "remove" => {
            let name = named(rest)?;
            let i = find(d, &name).ok_or_else(|| format!("No view is named “{name}”. {}", list(d)))?;
            let mut views = d.wb.custom_views().to_vec();
            let gone = views.remove(i);
            d.status.clear();
            if !d.run_action("Delete View", vec![GridOp::CustomViews(views)]) {
                return Err(std::mem::take(&mut d.status));
            }
            Ok(Outcome::Said(format!("Deleted the view “{}”.", gone.name)))
        }
        _ => {
            let name = if verb == "show" { named(rest)? } else { t.to_string() };
            let i = find(d, &name).ok_or_else(|| format!("No view is named “{name}”. {}", list(d)))?;
            let view = d.wb.custom_views()[i].clone();
            let ops = view.show_ops(&d.wb).ok_or("That view's sheet is gone; delete the view.")?;
            d.status.clear();
            if !d.run_action("Show View", ops) {
                return Err(std::mem::take(&mut d.status));
            }
            Ok(Outcome::Show { sheet: view.sheet, cursor: view.cursor, zoom: view.zoom, say: format!("Showing the view “{}”.", view.name) })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_show_delete_and_list_views() {
        let mut d = Doc::from_csv("v.csv", b"a,b,c\n1,2,3\n4,5,6\n7,8,9\n").unwrap().0;
        assert_eq!(start(&d), "add View1");
        assert!(matches!(run(&mut d, (0, 0), "list"), Ok(Outcome::Said(s)) if s.starts_with("No custom views")));
        // The whole view: rows 2-3 hidden, column B hidden, zoom 150.
        let mut st = d.wb.filter_state(0);
        st.hidden = vec![(1, 2)];
        d.run("Hide Rows", vec![GridOp::Filter(st)]);
        d.run("Hide Columns", vec![GridOp::Sizes { sheet: 0, rows: false, over: vec![(1, 0.0)] }]);
        d.wb.sheet_mut(0).zoom = 150;
        assert!(matches!(run(&mut d, (3, 2), "add Summary"), Ok(Outcome::Said(s)) if s.starts_with("Saved the view “Summary”")));
        assert_eq!(start(&d), "show Summary");
        // Everything shown again, then the view brings it back in one step.
        d.run("Unhide", vec![GridOp::Filter(officina_grid::filter::FilterState { sheet: 0, filter: None, hidden: Vec::new() }), GridOp::Sizes { sheet: 0, rows: false, over: Vec::new() }]);
        d.wb.sheet_mut(0).zoom = 100;
        let shown = run(&mut d, (0, 0), "summary").unwrap();
        assert_eq!(shown, Outcome::Show { sheet: 0, cursor: (3, 2), zoom: 150, say: "Showing the view “Summary”.".into() });
        assert_eq!(d.wb.filter_state(0).hidden, vec![(1, 2)]);
        assert_eq!(d.wb.sheet(0).col_sizes.overrides(), &[(1, 0.0)]);
        assert!(d.undo());
        assert!(d.wb.filter_state(0).hidden.is_empty());
        // Add again replaces; names are checked; delete, then it's gone.
        assert!(matches!(run(&mut d, (0, 0), "add SUMMARY"), Ok(Outcome::Said(s)) if s.starts_with("Replaced")));
        assert_eq!(d.wb.custom_views().len(), 1);
        assert!(run(&mut d, (0, 0), "add").is_err());
        assert!(run(&mut d, (0, 0), &format!("add {}", "v".repeat(65))).is_err());
        assert!(run(&mut d, (0, 0), "show Nope").unwrap_err().contains("Views: SUMMARY (Sheet1)"));
        assert!(matches!(run(&mut d, (0, 0), "delete summary"), Ok(Outcome::Said(_))));
        assert!(d.wb.custom_views().is_empty());
        assert!(d.undo());
        assert_eq!(d.wb.custom_views().len(), 1);
    }
}
