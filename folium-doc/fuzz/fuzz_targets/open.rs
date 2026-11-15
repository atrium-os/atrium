//! Open any bytes as a PDF and read everything P1 reads: every object,
//! every stream decoded, every page, the outline, the labels, the info.
//! Errors are fine; panics, hangs and allocations past the budgets aren't.
#![no_main]

use folium_doc::Doc;
use folium_syntax::{Bytes, Document, Ref};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = Document::open(Box::new(Bytes(data.to_vec().into()))) else { return };
    for n in 0..s.object_count().min(4096) as u32 {
        if let Ok(o) = s.get(Ref::new(n, 0)) {
            if let Some(st) = o.as_stream() {
                let _ = s.decode(st);
            }
        }
    }
    let Ok(d) = Doc::from_syntax(s) else { return };
    for i in 0..d.page_count().min(4096) {
        if let Ok(p) = d.page(i) {
            let _ = d.contents(&p);
        }
    }
    let _ = d.outline();
    let _ = d.page_labels();
    let _ = d.info();
});
