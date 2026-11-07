//! ATT PDUs from a bonded LE device during and after HOGP discovery: each
//! input is a run of PDUs, each prefixed by one length byte.
#![no_main]
use libfuzzer_sys::fuzz_target;
use vinculum_gatt::{Hogp, Out};

fuzz_target!(|data: &[u8]| {
    let mut c = Hogp::new();
    let _ = c.start(0);
    let mut rest = data;
    while let Some((&n, r)) = rest.split_first() {
        let n = (n as usize).min(r.len());
        for o in c.on_pdu(&r[..n], 0) {
            if let Out::Report { id, data } = o {
                let _ = c.output(id, &data);
            }
        }
        rest = &r[n..];
    }
});
