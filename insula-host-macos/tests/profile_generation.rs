//! Tests for SBPL + entitlements generation.

use insula_host_macos::{entitlements::render_entitlements, sbpl::render_profile};
use insula_manifest::Manifest;

const HELLO_INSULA_MANIFEST: &str = r#"
[app]
name = "com.example.hello-insula"
version = "0.1.0"
sdk-version = "1.x"

[bundle]
form = "native"
arches = ["aarch64-darwin"]
entry = "bin/hello"

[render]
fresco = false
"#;

const NETWORKED_APP_MANIFEST: &str = r#"
[app]
name = "com.example.weather"
version = "1.0.0"
sdk-version = "1.x"

[bundle]
form = "native"
arches = ["aarch64-darwin"]
entry = "bin/weather"

[render]
fresco = true

[ipc]
services = ["fresco-protocol", "clipboard", "vestibulum"]

[network]
hosts = [
  { name = "api.weather.example.com", port = 443, proto = "tcp" },
  { name = "dns.example.com", port = 53, proto = "udp" },
]
"#;

#[test]
fn sbpl_for_hello_insula() {
    let m = Manifest::parse(HELLO_INSULA_MANIFEST).unwrap();
    let sb = render_profile(&m);

    // Bare-minimum invariants for *every* Insula SBPL:
    assert!(sb.contains("(version 1)"));
    assert!(sb.contains("(deny default)"));
    assert!(sb.contains("(allow process-exec"));
    assert!(sb.contains("(allow file* (subpath (param \"CONTAINER_DIR\")))"));
    assert!(sb.contains("(subpath \"/usr/lib\")"));

    // hello-insula has no IPC or network, so those
    // sections should be absent.
    assert!(!sb.contains("mach-lookup (global-name \"atrium."));
    assert!(!sb.contains("network-outbound"));
}

#[test]
fn sbpl_includes_ipc_services_as_mach_lookups() {
    let m = Manifest::parse(NETWORKED_APP_MANIFEST).unwrap();
    let sb = render_profile(&m);

    assert!(sb.contains("(allow mach-lookup (global-name \"atrium.fresco-protocol\"))"));
    assert!(sb.contains("(allow mach-lookup (global-name \"atrium.clipboard\"))"));
    assert!(sb.contains("(allow mach-lookup (global-name \"atrium.vestibulum\"))"));
}

#[test]
fn sbpl_emits_tcp_and_udp_when_hosts_use_both() {
    let m = Manifest::parse(NETWORKED_APP_MANIFEST).unwrap();
    let sb = render_profile(&m);

    assert!(sb.contains("(allow network-outbound (remote tcp))"));
    assert!(sb.contains("(allow network-outbound (remote udp))"));
}

#[test]
fn sbpl_emits_fresco_rendering_grants_when_render_enabled() {
    let m = Manifest::parse(NETWORKED_APP_MANIFEST).unwrap();
    let sb = render_profile(&m);

    assert!(sb.contains("com.apple.windowserver.active"));
    assert!(sb.contains("IOAccelDevice2"));
}

#[test]
fn sbpl_does_not_emit_fresco_grants_when_render_disabled() {
    let m = Manifest::parse(HELLO_INSULA_MANIFEST).unwrap();
    let sb = render_profile(&m);

    assert!(!sb.contains("windowserver"));
}

#[test]
fn sbpl_raw_network_opens_both_directions() {
    let manifest = r#"
[app]
name = "com.example.vpn"
version = "1.0.0"
sdk-version = "1.x"

[bundle]
form = "native"
arches = ["aarch64-darwin"]
entry = "bin/vpn"

[network]
raw-network = true
"#;
    let m = Manifest::parse(manifest).unwrap();
    let sb = render_profile(&m);

    // raw-network = unrestricted outbound + inbound (the
    // escape hatch for tools that genuinely need raw
    // sockets).
    assert!(sb.contains("(allow network-outbound)"));
    assert!(sb.contains("(allow network-inbound)"));
}

#[test]
fn entitlements_always_include_app_sandbox() {
    let m = Manifest::parse(HELLO_INSULA_MANIFEST).unwrap();
    let ent = render_entitlements(&m);

    assert!(ent.contains("<?xml"));
    assert!(ent.contains("com.apple.security.app-sandbox"));
    assert!(ent.contains("com.apple.security.files.user-selected.read-write"));
}

#[test]
fn entitlements_include_network_client_when_hosts_declared() {
    let m = Manifest::parse(NETWORKED_APP_MANIFEST).unwrap();
    let ent = render_entitlements(&m);

    assert!(ent.contains("com.apple.security.network.client"));
}

#[test]
fn entitlements_skip_network_when_no_network_section() {
    let m = Manifest::parse(HELLO_INSULA_MANIFEST).unwrap();
    let ent = render_entitlements(&m);

    assert!(!ent.contains("com.apple.security.network"));
}

#[test]
fn entitlements_include_device_access_per_capability() {
    let manifest = r#"
[app]
name = "com.example.recorder"
version = "1.0.0"
sdk-version = "1.x"

[bundle]
form = "native"
arches = ["aarch64-darwin"]
entry = "bin/rec"

[capabilities]
microphone = "session"
camera = "session"
"#;
    let m = Manifest::parse(manifest).unwrap();
    let ent = render_entitlements(&m);

    assert!(ent.contains("com.apple.security.device.microphone"));
    assert!(ent.contains("com.apple.security.device.camera"));
}
