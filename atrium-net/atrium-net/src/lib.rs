//! atrium-net's logic, kept free of system calls so it runs in host tests.

pub mod egress;
pub mod plan;
pub mod spawn;
pub mod wifi;
pub mod wrap;

#[cfg(target_os = "freebsd")]
pub mod sys;

/// The DNS-over-TLS check's question (atrium-net.md §8.1 P3): `name` A,
/// recursion desired. Empty if `name` is not a usable host name.
pub fn dot_query(id: u16, name: &str) -> Vec<u8> {
    let name = name.trim_end_matches('.');
    if name.is_empty() || name.len() > 253 || name.split('.').any(|l| l.is_empty() || l.len() > 63 || !l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')) {
        return Vec::new();
    }
    let mut q = Vec::with_capacity(18 + name.len());
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
    for l in name.split('.') {
        q.push(l.len() as u8);
        q.extend_from_slice(l.as_bytes());
    }
    q.extend_from_slice(&[0, 0, 1, 0, 1]);
    q
}

#[cfg(test)]
mod dot_tests {
    #[test]
    fn the_check_asks_the_servers_own_name() {
        let q = super::dot_query(0x1234, "dot.atrium.test");
        assert_eq!(&q[..12], &[0x12, 0x34, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&q[12..], b"\x03dot\x06atrium\x04test\x00\x00\x01\x00\x01");
        assert!(super::dot_query(1, "").is_empty());
        assert!(super::dot_query(1, "a..b").is_empty());
        assert!(super::dot_query(1, "bad name").is_empty());
    }
}
