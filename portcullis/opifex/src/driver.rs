//! Driver and firmware packages (opifex.md §8.2; build step C6). The
//! manifests are opifex_catalog::driver's.
//!
//! ```text
//! opifex driver pack <tree> --manifest <atrium.toml> [--sign <key.pem>]
//! opifex firmware pack <tree> --manifest <atrium.toml> [--sign <key.pem>]
//! opifex driver-for usb <vvvv:pppp> [<cc/ss/pp>] | hid <page>
//! ```
//!
//! Under the system root (`/`):
//!
//! - a driver's files go to `usr/local/libexec/atrium/drivers/<id>/`, and its
//!   `devices.toml` to `usr/local/etc/atrium/devices.d/60-<id>.toml`
//!   (root-owned, 0644: what dispensatord's strict loading takes). The rules
//!   may claim only `usb` and `hid` devices, only what the signed `[[match]]`
//!   tables show, only for driver jails the same file defines, each run from
//!   the package's own folder;
//! - firmware goes to `usr/local/share/atrium/firmware/<dir>/` (jailed
//!   drivers, mounted read-only into their jail) or `boot/firmware/<dir>/`
//!   (kernel drivers), device-assignment §4.4.
//!
//! dispensatord reads `devices.d` when it starts; a new driver takes devices
//! from its next start (`service atrium_dispensatord restart`), which the
//! administrator's install does.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::content::{index_text, index_tree, own, place, read_index, Kind, CONTENT_INDEX, MANIFEST, SIG, TOP};
use opifex_catalog::driver::{DriverManifest, FirmwareManifest, Match, DEVICES_FILE, IDX_HEADER};

pub const DRIVERS: &str = "usr/local/libexec/atrium/drivers";
pub const DEVICES_D: &str = "usr/local/etc/atrium/devices.d";
pub const FIRMWARE_JAILED: &str = "usr/local/share/atrium/firmware";
pub const FIRMWARE_KERNEL: &str = "boot/firmware";
pub const IDX: &str = "devices.idx";

fn write_signed(tree: &Path, idx: &str, text: &str, key_pem: Option<&str>) -> Result<(), String> {
    fs::write(tree.join(CONTENT_INDEX), idx).map_err(|e| format!("write {CONTENT_INDEX}: {e}"))?;
    fs::write(tree.join(MANIFEST), text).map_err(|e| format!("write {MANIFEST}: {e}"))?;
    let _ = fs::remove_file(tree.join(SIG));
    if let Some(k) = key_pem {
        use base64::Engine;
        let sig = portcullis_sig::sign(text.as_bytes(), k).map_err(|e| format!("sign: {e:?}"))?;
        fs::write(tree.join(SIG), base64::engine::general_purpose::STANDARD.encode(sig) + "\n").map_err(|e| format!("write {SIG}: {e}"))?;
    }
    Ok(())
}

pub fn pack_driver(tree: &Path, template: &str, key_pem: Option<&str>) -> Result<DriverManifest, String> {
    let mut m: DriverManifest = toml::from_str(template).map_err(|e| format!("driver manifest: {e}"))?;
    let entries = index_tree(tree)?;
    let devices = fs::read_to_string(tree.join(DEVICES_FILE)).map_err(|e| format!("{DEVICES_FILE}: {e}"))?;
    check_devices(&m, &devices)?;
    let idx = index_text(&entries);
    m.driver.content = crate::content::digest_text(&idx);
    m.validate()?;
    write_signed(tree, &idx, &toml::to_string(&m).map_err(|e| e.to_string())?, key_pem)?;
    Ok(m)
}

pub fn pack_firmware(tree: &Path, template: &str, key_pem: Option<&str>) -> Result<FirmwareManifest, String> {
    let mut m: FirmwareManifest = toml::from_str(template).map_err(|e| format!("firmware manifest: {e}"))?;
    let entries = index_tree(tree)?;
    if entries.iter().any(|e| e.kind == Kind::Link) {
        return Err("firmware holds files and folders, no links".into());
    }
    let idx = index_text(&entries);
    m.firmware.content = crate::content::digest_text(&idx);
    m.validate()?;
    write_signed(tree, &idx, &toml::to_string(&m).map_err(|e| e.to_string())?, key_pem)?;
    Ok(m)
}

/// The package's `devices.toml` claims only what its signed matches show.
pub fn check_devices(m: &DriverManifest, text: &str) -> Result<(), String> {
    let id = &m.driver.id;
    let v: toml::Value = text.parse().map_err(|e| format!("{DEVICES_FILE}: {e}"))?;
    let t = v.as_table().ok_or(DEVICES_FILE)?;
    if let Some(k) = t.keys().find(|k| !["rule", "driver"].contains(&k.as_str())) {
        return Err(format!("{DEVICES_FILE}: [{k}]: a driver package defines rules and driver jails only"));
    }
    let drivers = t.get("driver").and_then(|d| d.as_table()).cloned().unwrap_or_default();
    let home = format!("/{DRIVERS}/{id}/");
    for (name, d) in &drivers {
        let exec = d.get("exec").and_then(|x| x.as_str()).unwrap_or("");
        if !exec.starts_with(&home) || exec.contains("/../") {
            return Err(format!("{DEVICES_FILE}: driver {name}: exec must be under {home}"));
        }
    }
    let rules = t.get("rule").and_then(|r| r.as_array()).cloned().unwrap_or_default();
    if rules.is_empty() {
        return Err(format!("{DEVICES_FILE}: no [[rule]]"));
    }
    for r in &rules {
        let s = |k: &str| r.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let name = s("name");
        let owner = s("owner");
        if !owner.strip_prefix("driver:").is_some_and(|d| drivers.contains_key(d)) {
            return Err(format!("{DEVICES_FILE}: rule {name}: owner must be a driver this file defines"));
        }
        let strs = |k: &str| r.get(k).and_then(|x| x.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
        let nums = |k: &str| -> Vec<i64> { r.get(k).and_then(|x| x.as_array()).map(|a| a.iter().filter_map(|x| x.as_integer()).collect()).unwrap_or_default() };
        let (ids, iface, pages): (Vec<String>, Vec<u8>, Vec<u16>) =
            (strs("usb_ids"), nums("usb_interface").iter().map(|&x| x as u8).collect(), nums("any_page").iter().map(|&x| x as u16).collect());
        let class = s("class");
        if !m.matches.iter().any(|x: &Match| x.covers(&class, &ids, &iface, &pages)) {
            return Err(format!("{DEVICES_FILE}: rule {name} ({class}) claims devices the manifest's [[match]] tables don't show"));
        }
    }
    Ok(())
}

/// Copy a checked tree from `src` into `dest` through a staging folder and
/// swap it in; returns the folder that was there before (gone).
fn swap_in(src: &Path, entries: &[crate::content::Entry], dest: &Path) -> Result<(), String> {
    let parent = dest.parent().ok_or("no parent")?;
    fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let name = dest.file_name().unwrap().to_string_lossy().to_string();
    let stage = parent.join(format!(".staging-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&stage);
    fs::create_dir(&stage).map_err(|e| format!("{}: {e}", stage.display()))?;
    let r = (|| {
        place(src, &stage, entries)?;
        for f in TOP {
            if src.join(f).is_file() {
                fs::copy(src.join(f), stage.join(f)).map_err(|e| format!("{f}: {e}"))?;
                let _ = fs::set_permissions(stage.join(f), fs::Permissions::from_mode(0o444));
            }
        }
        own(&stage)?;
        fs::set_permissions(&stage, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())
    })();
    if let Err(e) = r {
        let _ = fs::remove_dir_all(&stage);
        return Err(e);
    }
    let old = parent.join(format!(".old-{name}-{}", std::process::id()));
    let had = fs::symlink_metadata(dest).is_ok();
    if had {
        fs::rename(dest, &old).map_err(|e| format!("{}: {e}", dest.display()))?;
    }
    if let Err(e) = fs::rename(&stage, dest) {
        if had {
            let _ = fs::rename(&old, dest);
        }
        let _ = fs::remove_dir_all(&stage);
        return Err(format!("{}: {e}", dest.display()));
    }
    if had {
        let _ = fs::remove_dir_all(&old);
    }
    Ok(())
}

fn signed(src: &Path, keys: &[String], what: &str) -> Result<String, String> {
    let text = fs::read_to_string(src.join(MANIFEST)).map_err(|e| format!("{MANIFEST}: {e}"))?;
    let raw = fs::read(src.join(SIG)).map_err(|_| format!("{what} is not signed (no {SIG})"))?;
    let sig = std::str::from_utf8(&raw).ok().and_then(|s| portcullis_sig::sig_from_base64(s).ok()).ok_or_else(|| format!("{SIG}: not base64"))?;
    portcullis_sig::verify_trusted(text.as_bytes(), &sig, keys).map_err(|e| format!("{what} is not signed by its publisher ({e:?})"))?;
    Ok(text)
}

pub fn devices_file(system: &Path, id: &str) -> PathBuf { system.join(DEVICES_D).join(format!("60-{id}.toml")) }

pub fn install_driver(src: &Path, system: &Path, keys: &[String]) -> Result<DriverManifest, String> {
    let m = DriverManifest::from_str(&signed(src, keys, "driver")?)?;
    let id = m.driver.id.clone();
    let entries = read_index(src, m.require_content()?)?;
    let dest = system.join(DRIVERS).join(&id);
    swap_in(src, &entries, &dest)?;
    // The rules from the copy just checked against the index, not the source.
    let devices = fs::read_to_string(dest.join(DEVICES_FILE)).map_err(|e| format!("{DEVICES_FILE}: {e}"))?;
    if let Err(e) = check_devices(&m, &devices) {
        let _ = fs::remove_dir_all(&dest);
        return Err(e);
    }
    let file = devices_file(system, &id);
    let dir = file.parent().unwrap();
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = dir.join(format!(".60-{id}.{}", std::process::id()));
    fs::write(&tmp, &devices).map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644)).map_err(|e| e.to_string())?;
    own(&tmp)?;
    fs::rename(&tmp, &file).map_err(|e| format!("{}: {e}", file.display()))?;
    Ok(m)
}

pub fn remove_driver(system: &Path, id: &str) -> Result<(), String> {
    if !opifex_catalog::font::id_ok(id) {
        return Err(format!("{id:?}: not a package id"));
    }
    let _ = fs::remove_file(devices_file(system, id));
    let d = system.join(DRIVERS).join(id);
    if fs::symlink_metadata(&d).is_ok() {
        fs::remove_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    Ok(())
}

pub fn firmware_dir(system: &Path, m: &FirmwareManifest) -> PathBuf {
    system.join(if m.firmware.place == "kernel" { FIRMWARE_KERNEL } else { FIRMWARE_JAILED }).join(&m.firmware.dir)
}

pub fn install_firmware(src: &Path, system: &Path, keys: &[String]) -> Result<FirmwareManifest, String> {
    let m = FirmwareManifest::from_str(&signed(src, keys, "firmware")?)?;
    let entries = read_index(src, m.require_content()?)?;
    if entries.iter().any(|e| e.kind == Kind::Link) {
        return Err("firmware holds files and folders, no links".into());
    }
    let dest = firmware_dir(system, &m);
    // A folder another package placed is not this one's to replace.
    if let Ok(t) = fs::read_to_string(dest.join(MANIFEST)) {
        if FirmwareManifest::from_str(&t).map(|o| o.firmware.id != m.firmware.id).unwrap_or(true) {
            return Err(format!("{}: another package's folder", dest.display()));
        }
    } else if fs::symlink_metadata(&dest).is_ok() {
        return Err(format!("{}: there already, not from a package", dest.display()));
    }
    swap_in(src, &entries, &dest)?;
    Ok(m)
}

pub fn remove_firmware(system: &Path, manifest: &str) -> Result<(), String> {
    let m = FirmwareManifest::from_str(manifest)?;
    let d = firmware_dir(system, &m);
    if fs::symlink_metadata(&d).is_ok() {
        fs::remove_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    Ok(())
}

/// Rewrite `<state>/devices.idx` from the checked catalog: every driver's
/// newest release for this machine.
pub fn write_idx(state: &Path, local: &opifex_catalog::Local, arch: &str) -> Result<usize, String> {
    let mut rows = Vec::new();
    for (id, rs) in opifex_catalog::by_id(local) {
        if !rs.iter().any(|r| r.kind == "driver") {
            continue;
        }
        if let Some(r) = local.latest(&id, arch) {
            if let Ok(m) = DriverManifest::from_str(&r.manifest) {
                rows.extend(opifex_catalog::driver::idx_rows(&m));
            }
        }
    }
    let text = format!("{IDX_HEADER}\n{}", rows.iter().map(|r| format!("{r}\n")).collect::<String>());
    let tmp = state.join(format!(".{IDX}.{}", std::process::id()));
    fs::create_dir_all(state).map_err(|e| e.to_string())?;
    fs::write(&tmp, text).and_then(|_| fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644)))
        .and_then(|_| fs::rename(&tmp, state.join(IDX))).map_err(|e| format!("{IDX}: {e}"))?;
    Ok(rows.len())
}

/// Look a device up in devices.idx text: the packages that say they drive
/// it, best first (exact ids, then the longest interface class).
pub fn lookup(idx: &str, usb: Option<((u16, u16), [u8; 3])>, hid_pages: &[u16]) -> Vec<String> {
    let mut hits: Vec<(usize, String)> = Vec::new();
    let mut lines = idx.lines();
    if lines.next() != Some(IDX_HEADER) {
        return Vec::new();
    }
    for l in lines {
        let f: Vec<&str> = l.split('\t').collect();
        let [what, key, id] = f[..] else { continue };
        let score = match (what, usb) {
            ("usb-id", Some(((v, p), _))) if key == format!("{v:04x}:{p:04x}") => 10,
            ("usb-class", Some((_, c))) => {
                let want: Vec<String> = c.iter().map(|b| format!("{b:02x}")).collect();
                let parts: Vec<&str> = key.split('/').collect();
                if !parts.is_empty() && parts.iter().zip(&want).all(|(a, b)| a == b) && parts.len() <= 3 { parts.len() } else { 0 }
            }
            ("hid-page", _) if hid_pages.iter().any(|p| key == format!("{p:04x}")) => 5,
            _ => 0,
        };
        if score > 0 && !hits.iter().any(|(_, i)| i == id) {
            hits.push((score, id.to_string()));
        }
    }
    hits.sort_by(|a, b| b.0.cmp(&a.0));
    hits.into_iter().map(|(_, i)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    pub const MANIFEST_T: &str = "kind = \"driver\"\n[driver]\nid = \"org.atrium.driver.rtl\"\nname = \"Realtek USB Ethernet\"\nversion = \"1.0\"\n\
        license = \"BSD-2-Clause\"\n\n[[match]]\nclass = \"usb\"\nusb_ids = [\"0bda:8153\"]\n";
    pub const DEVICES_T: &str = "[[rule]]\nname = \"rtl\"\nclass = \"usb\"\nusb_ids = [\"0bda:8153\"]\ndetach_kernel = true\n\
        nodes = [\"ugen{bus}.{addr}\"]\nowner = \"driver:rtl\"\n\n[driver.rtl]\nexec = \"/usr/local/libexec/atrium/drivers/org.atrium.driver.rtl/rtl\"\n\
        argv = [\"rtl\"]\nuid = 65060\ngid = 65060\njail = \"atrium-rtl-{usb}\"\nroot = \"/var/lib/atrium/jails/rtl-{usb}\"\ndevfs_ruleset = 30\nsysfil = \"device\"\n";

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("opifex-driver-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn devices_claim_only_what_is_shown() {
        let m = DriverManifest::from_str(MANIFEST_T).unwrap();
        assert!(check_devices(&m, DEVICES_T).is_ok());
        for (a, b, why) in [
            ("usb_ids = [\"0bda:8153\"]\ndetach", "usb_ids = [\"0bda:8153\", \"1234:0001\"]\ndetach", "don't show"),
            ("class = \"usb\"\nusb_ids", "class = \"disk\"\nusb_ids", "don't show"),
            ("drivers/org.atrium.driver.rtl/rtl", "drivers/org.atrium.driver.other/rtl", "exec must be under"),
            ("owner = \"driver:rtl\"", "owner = \"service:storage\"", "owner must be"),
        ] {
            let e = check_devices(&m, &DEVICES_T.replace(a, b)).unwrap_err();
            assert!(e.contains(why), "{b}: {e}");
        }
        assert!(check_devices(&m, &format!("{DEVICES_T}\n[service.x]\ndevfs = \"/x\"\n")).unwrap_err().contains("rules and driver jails only"));
    }

    #[test]
    fn driver_and_firmware_install() {
        let d = tmp("install");
        let (sk, pk) = portcullis_sig::new_key([5; 32]).unwrap();
        let sys = d.join("sys");
        let t = d.join("drv");
        fs::create_dir_all(&t).unwrap();
        fs::write(t.join("rtl"), b"\x7fELF driver").unwrap();
        fs::write(t.join(DEVICES_FILE), DEVICES_T).unwrap();
        pack_driver(&t, MANIFEST_T, Some(&sk)).unwrap();
        install_driver(&t, &sys, &[pk.clone()]).unwrap();
        let f = devices_file(&sys, "org.atrium.driver.rtl");
        assert_eq!(fs::read_to_string(&f).unwrap(), DEVICES_T);
        assert_eq!(fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o644);
        assert!(sys.join(DRIVERS).join("org.atrium.driver.rtl/rtl").is_file());
        remove_driver(&sys, "org.atrium.driver.rtl").unwrap();
        assert!(!f.exists());

        let fw = d.join("fw");
        fs::create_dir_all(fw.join("sub")).unwrap();
        fs::write(fw.join("sub/rtl8153a-4.fw"), b"blob").unwrap();
        let ft = "kind = \"firmware\"\n[firmware]\nid = \"org.atrium.firmware.rtl\"\nname = \"RTL firmware\"\nversion = \"1\"\n\
                  license = \"LicenseRef-Realtek\"\nplace = \"kernel\"\ndir = \"realtek\"\n";
        pack_firmware(&fw, ft, Some(&sk)).unwrap();
        let m = install_firmware(&fw, &sys, &[pk.clone()]).unwrap();
        assert_eq!(fs::read(sys.join("boot/firmware/realtek/sub/rtl8153a-4.fw")).unwrap(), b"blob");
        install_firmware(&fw, &sys, &[pk.clone()]).unwrap();
        // Someone else's folder.
        fs::create_dir_all(sys.join("boot/firmware/amdgpu")).unwrap();
        let fw2 = d.join("fw2");
        fs::create_dir_all(&fw2).unwrap();
        fs::write(fw2.join("x.bin"), b"x").unwrap();
        pack_firmware(&fw2, &ft.replace("realtek", "amdgpu").replace(".rtl", ".amd"), Some(&sk)).unwrap();
        assert!(install_firmware(&fw2, &sys, &[pk]).unwrap_err().contains("not from a package"));
        remove_firmware(&sys, &toml::to_string(&m).unwrap()).unwrap();
        assert!(!sys.join("boot/firmware/realtek").exists());
    }

    #[test]
    fn lookups() {
        let idx = format!("{IDX_HEADER}\nusb-class\tff\torg.a.generic\nusb-id\t0bda:8153\torg.a.rtl\nusb-class\tff/00\torg.a.vendor\nhid-page\tff00\torg.a.hid\n");
        assert_eq!(lookup(&idx, Some(((0x0bda, 0x8153), [0xff, 0, 0])), &[]), vec!["org.a.rtl", "org.a.vendor", "org.a.generic"]);
        assert_eq!(lookup(&idx, Some(((1, 2), [0xff, 1, 0])), &[]), vec!["org.a.generic"]);
        assert_eq!(lookup(&idx, None, &[0xff00]), vec!["org.a.hid"]);
        assert!(lookup("junk", None, &[0xff00]).is_empty());
    }
}
