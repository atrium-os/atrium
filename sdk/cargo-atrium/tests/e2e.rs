//! `cargo atrium check` on a real project: cargo metadata, the manifest,
//! the findings, the exit status.

use std::process::Command;

fn write(p: &std::path::Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

#[test]
fn check_flags_image_in_a_networked_core() {
    let d = std::env::temp_dir().join(format!("cargo-atrium-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    // A stand-in crate named `image`, so no network is needed.
    write(&d.join("image/Cargo.toml"), "[package]\nname = \"image\"\nversion = \"0.25.0\"\nedition = \"2021\"\n");
    write(&d.join("image/src/lib.rs"), "");
    write(&d.join("Cargo.toml"), "[workspace]\nmembers = [\"photos\"]\nresolver = \"2\"\n");
    write(&d.join("photos/Cargo.toml"), "[package]\nname = \"photos\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nimage = { path = \"../image\" }\n");
    write(&d.join("photos/src/main.rs"), "fn main() {}\n");
    write(&d.join("atrium.toml"), "[app]\nid = \"com.example.photos\"\nname = \"Photos\"\nversion = \"1\"\nentry = \"bin/photos\"\n\n[capabilities]\nnet-fetch = true\n");
    let out = Command::new(env!("CARGO_BIN_EXE_cargo-atrium")).args(["atrium", "check", "--json"]).current_dir(&d).output().unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{s}\n{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_str(&s).unwrap();
    let f = v["findings"].as_array().unwrap();
    assert!(f.iter().any(|x| x["code"] == "parser-in-core" && x["message"].as_str().unwrap().contains("links image 0.25.0")), "{s}");
    // A broken manifest fails the command.
    write(&d.join("atrium.toml"), "[app]\nid = 3\n");
    let out = Command::new(env!("CARGO_BIN_EXE_cargo-atrium")).args(["check"]).current_dir(&d).output().unwrap();
    assert!(!out.status.success());
    let _ = std::fs::remove_dir_all(&d);
}
