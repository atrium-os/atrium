//! The .conf importer on anything: never panics, and what it accepts stays
//! inside its limits.
#![no_main]
use crypta_proto::conf;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(c) = conf::parse(data) {
        assert!(data.len() <= conf::MAX_FILE);
        assert!(c.addresses4.len() + c.addresses6.len() <= conf::MAX_ADDRESSES);
        assert!(c.allowed4.len() + c.allowed6.len() <= conf::MAX_ALLOWED);
        assert!(c.dns_servers.len() <= conf::MAX_DNS_SERVERS);
        assert!(c.dns_domains.len() <= conf::MAX_DNS_DOMAINS);
        assert!(c.dns_domains.iter().all(|d| conf::valid_name(d)));
        assert!(c.allowed4.iter().all(|p| p.len <= 32 && u32::from(p.addr) & !crypta_proto::net::mask(p.len) == 0));
        if let Some(conf::Endpoint::Host(h, p)) = &c.endpoint {
            assert!(conf::valid_name(h) && *p != 0);
        }
    }
});
