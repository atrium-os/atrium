//! Read a Save As result from the two-workbook desktop recovery fixture.
use atrium_sheets::files::{load, load_with};
use std::{env, fs};

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    let [path, expected, mode] = args.as_slice() else {
        panic!("expected path, CSV contents and plain/protected");
    };
    let bytes = fs::read(path).unwrap();
    let doc = match mode.as_str() {
        "plain" => load(path, &bytes).unwrap(),
        "protected" => {
            assert!(load(path, &bytes).is_err());
            assert!(matches!(
                load_with(path, &bytes, Some("wrong")),
                Err(officina_convert::launch::ConvertError::WrongPassword)
            ));
            load_with(path, &bytes, Some("test")).unwrap()
        }
        _ => panic!("unknown fixture mode"),
    };
    let csv = String::from_utf8(doc.write_csv(Vec::new()).unwrap()).unwrap();
    assert_eq!(csv.trim_end(), expected);
}
