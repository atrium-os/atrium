//! `cargo run --example show -- NAME...`: print each instruction's encodings,
//! fields and operands.
use atrium_rdna4_isa::{ENCODINGS, INSTRUCTIONS};
fn main() {
    for want in std::env::args().skip(1) {
        let mut found = false;
        for i in INSTRUCTIONS.iter().filter(|i| i.name.eq_ignore_ascii_case(&want) || (want.ends_with('*') && i.name.starts_with(&want[..want.len()-1].to_uppercase()))) {
            found = true;
            for e in i.encodings {
                let enc = &ENCODINGS[e.encoding];
                let fields: Vec<_> = enc.fields.iter().map(|f| f.name).collect();
                let ops: Vec<_> = e.operands.iter().map(|o| format!("{}:{}{}", o.field, o.kind, if o.implicit { "(imp)" } else { "" })).collect();
                println!("{} {} [{}] op={} fields={:?}\n    operands={:?}", i.name, enc.name, e.condition, e.opcode, fields, ops);
            }
        }
        if !found { println!("{want}: not found"); }
    }
}
