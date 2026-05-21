//! Entitlements plist (XML) generation.
//!
//! macOS entitlements are key/value pairs embedded in
//! the app's code signature. They unlock capability
//! tiers the App Sandbox SBPL profile cannot grant on
//! its own — most notably network, hardware device
//! access (camera, microphone, location), and
//! arbitrary-file access via user-selected pickers.
//!
//! Each Insula manifest maps to a set of entitlements
//! at install time; `codesign --entitlements ent.plist
//! …` embeds them into the signed binary.
//!
//! # What this generates
//!
//! A minimal Apple-plist v1.0 XML document with the
//! entitlements derived from the Insula manifest.
//!
//! # What this does NOT do
//!
//! - Sign anything. `codesign` is invoked separately
//!   at install time, consuming the generated XML.
//! - Validate entitlements against Apple's known list.
//!   An unknown entitlement key passes through; macOS
//!   silently ignores it at launch.

use insula_manifest::Manifest;
use std::fmt::Write;

/// Render the entitlements plist (XML) for a manifest.
pub fn render_entitlements(manifest: &Manifest) -> String {
    let mut out = String::new();

    let _ = writeln!(out, "<?xml version=\"1.0\" encoding=\"UTF-8\"?>");
    let _ = writeln!(
        out,
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">"
    );
    let _ = writeln!(out, "<plist version=\"1.0\">");
    let _ = writeln!(out, "<dict>");

    // App Sandbox is always enabled for Insula apps.
    bool_entry(&mut out, "com.apple.security.app-sandbox", true);

    // Network — outbound client + (rarely) server.
    if let Some(net) = &manifest.network {
        if net.raw_network || !net.hosts.is_empty() {
            bool_entry(&mut out, "com.apple.security.network.client", true);
        }
        // We don't generate a server entitlement
        // automatically; that's a separate manifest
        // capability not yet typed.
    }

    // Device access — these come from [capabilities]
    // since insula.md §18 routes them through powerbox,
    // but the macOS prerequisite is an entitlement
    // (TCC then prompts at first use).
    if let Some(caps) = &manifest.capabilities {
        if caps.contains_key("camera") {
            bool_entry(&mut out, "com.apple.security.device.camera", true);
        }
        if caps.contains_key("microphone") {
            bool_entry(&mut out, "com.apple.security.device.microphone", true);
        }
        if caps.contains_key("location") {
            bool_entry(&mut out, "com.apple.security.personal-information.location", true);
        }
    }

    // File access via Scrinium picker → user-selected
    // file/folder entitlement.
    // Always granted by default — every Insula app can
    // receive an fd from the system picker.
    bool_entry(
        &mut out,
        "com.apple.security.files.user-selected.read-write",
        true,
    );

    let _ = writeln!(out, "</dict>");
    let _ = writeln!(out, "</plist>");

    out
}

fn bool_entry(out: &mut String, key: &str, value: bool) {
    let _ = writeln!(out, "    <key>{}</key>", key);
    let _ = writeln!(out, "    <{}/>", if value { "true" } else { "false" });
}
