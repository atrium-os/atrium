//! Treat all gateway responses, including challenge text, as data.
#![no_main]

use crypta_gp::gateway;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = gateway::prelogin(data);
    let _ = gateway::challenge(data);
    let _ = gateway::gateways(data);
    let _ = gateway::config(data, 1406);
    let _ = gateway::hip_check(data);
    let _ = gateway::hip_interval(data);
    let _ = gateway::hip_accepted(data);
    if let Ok(session) = gateway::login(data) {
        let stored = session.stored();
        let restored = gateway::Session::restore(&stored).expect("valid login can be restored");
        assert!(restored.stored().as_str() == stored.as_str());
    }
});
