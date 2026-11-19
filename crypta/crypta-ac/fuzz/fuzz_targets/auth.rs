//! Any config-auth answer from a gateway (vpn.md §9.4): the XML reader and
//! the form reader never panic, a document read writes back to one that
//! reads the same, and a form read can always be answered.
//!   cargo fuzz run auth
#![no_main]
use crypta_ac::{auth, xml};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(e) = xml::parse(data) {
        let again = xml::parse(e.to_doc().as_bytes()).expect("what we write reads back");
        assert_eq!(again, e);
    }
    match auth::parse(data) {
        Ok(auth::Answer::Form { form, opaque, .. }) => {
            let _ = auth::reply(&form, opaque.as_ref(), &[]);
        }
        Ok(auth::Answer::Sso { opaque, .. }) => {
            let _ = auth::sso_reply(opaque.as_ref(), "token");
        }
        _ => {}
    }
});
