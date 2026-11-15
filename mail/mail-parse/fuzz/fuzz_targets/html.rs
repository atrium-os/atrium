#![no_main]
//! Any text as an HTML body: conversion never panics, nothing that acts
//! survives, and a picture marked hidden is gone from the document.
use libfuzzer_sys::fuzz_target;
use mail_parse::html::{convert, Env, FontSet};
use std::sync::OnceLock;

static FONTS: OnceLock<FontSet> = OnceLock::new();

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else { return };
    let fonts = FONTS.get_or_init(|| FontSet::load().expect("pinned font set"));
    let c = convert(s, &Default::default(), fonts, &Env::default());
    let d = c.document.to_ascii_lowercase();
    for bad in ["<script", "<iframe", "<object", "<embed", "<form", "<base", "<meta"] {
        assert!(!d.contains(bad), "{bad}");
    }
    for r in &c.resources {
        if r.hidden { assert!(!c.document.contains(&format!("\"{}\"", r.id))) }
    }
});
