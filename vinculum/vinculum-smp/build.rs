//! Generates the SMP packet parsers from `../pdl/smp.pdl`.
use pdl_compiler::{analyzer, backends, parser};
use std::path::PathBuf;

fn main() {
    let pdl = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../pdl/smp.pdl");
    println!("cargo:rerun-if-changed={}", pdl.display());
    let mut sources = pdl_compiler::ast::SourceDatabase::new();
    let file = parser::parse_file(&mut sources, pdl.to_str().unwrap()).unwrap_or_else(|d| panic!("pdl parse: {d:?}"));
    let analyzed = analyzer::analyze(&file).unwrap_or_else(|d| panic!("pdl analyze: {d:?}"));
    let code = backends::rust::generate(&sources, &analyzed, &[]);
    std::fs::write(PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("smp.rs"), code).unwrap();
}
