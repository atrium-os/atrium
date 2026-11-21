//! Ephemeral test authenticator for the I3 local relying party.
//! By default it prints a public enrollment record. Pass --empty for browser registration.
use sigillum_fido::Driver;
use sigillum_proto::driver::{FromDriver, Pin, ToDriver};
use sigillum_softkey::SoftKey;
use std::os::unix::net::UnixStream;

fn main() {
    let rp = "login.microsoftonline.com";
    let (mut driver, hello) =
        Driver::open(SoftKey::new(Some(b"4711")), 0, 0, "I3 soft key").unwrap();
    if !std::env::args().any(|s| s == "--empty") {
        let enrollment = driver.handle(ToDriver::Register {
            ceremony: 1,
            rp_id: rp.into(),
            rp_name: "Local I3 provider".into(),
            user_id: b"fixture-user".to_vec(),
            user_name: "fixture".into(),
            user_display_name: "Fixture".into(),
            client_data_hash: [1; 32],
            exclude: vec![],
            resident_key: true,
            uv_required: true,
            pin: Some(Pin(b"4711".to_vec().into())),
        });
        let Some(FromDriver::Registered {
            cred_id,
            public_key,
            sign_count,
            ..
        }) = enrollment
        else {
            panic!("fixture enrollment failed")
        };
        let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        println!("{} {} {}", hex(&cred_id), hex(&public_key), sign_count);
    }
    let socket = UnixStream::connect(sigillum_proto::driver::SOCKET).unwrap();
    sigillum_fido::serve(driver, hello, socket).unwrap();
}
