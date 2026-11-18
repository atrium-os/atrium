//! `[app] updates`: the publisher's update timing class (opifex.md §11).

fn manifest(updates: &str) -> String {
    format!("[app]\nid = \"org.example.u\"\nname = \"U\"\nversion = \"1\"\nentry = \"bin/u\"\n{updates}")
}

#[test]
fn timing_classes() {
    for (line, ok) in [("", true), ("updates = \"auto\"\n", true), ("updates = \"notify\"\n", true),
                       ("updates = \"required\"\n", true), ("updates = \"whenever\"\n", false)] {
        let (m, r) = portcullis_toml::parse_and_validate(&manifest(line)).unwrap();
        assert_eq!(r.errors.is_empty(), ok, "{line}: {:?}", r.errors);
        if line.is_empty() {
            assert_eq!(m.app.updates, None);
        }
    }
}
