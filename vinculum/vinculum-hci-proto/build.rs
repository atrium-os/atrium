//! Generates the HCI packet parsers from `../pdl/hci_packets.pdl` with
//! pdl-compiler (spec §6.1: no hand-written byte slicing for radio data).
use pdl_compiler::{analyzer, backends, parser};
use std::path::PathBuf;

fn main() {
    generate("hci_packets");
    generate("gap");
}

fn generate(name: &str) {
    let pdl = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("../pdl/{name}.pdl"));
    println!("cargo:rerun-if-changed={}", pdl.display());
    let mut sources = pdl_compiler::ast::SourceDatabase::new();
    let file = parser::parse_file(&mut sources, pdl.to_str().unwrap())
        .unwrap_or_else(|d| panic!("pdl parse: {d:?}"));
    let analyzed = analyzer::analyze(&file).unwrap_or_else(|d| panic!("pdl analyze: {d:?}"));
    let code = backends::rust::generate(&sources, &analyzed, &[]);
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join(format!("{name}.rs"));
    std::fs::write(&out, code).unwrap();
}
