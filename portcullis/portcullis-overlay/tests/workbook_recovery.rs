//! FreeBSD guest gate. Give this test a new directory on a disposable volume
//! and a dense native fixture. For UFS use a PATH without mkfs-tessera.
use portcullis_overlay::{mount_recovery, resize_empty_recovery, unmount_recovery, Place, WORKBOOK_RECOVERY_BYTES};
use std::{env, fs, io, path::PathBuf, process::Command};

#[test]
#[ignore = "requires root, md(4), a dense fixture and a disposable FreeBSD volume"]
fn legacy_recovery_and_two_dense_bases() {
    let root = PathBuf::from(env::var_os("RECOVERY_TEST_DIR").unwrap());
    fs::create_dir(&root).unwrap();
    let fixture = PathBuf::from(env::var_os("RECOVERY_TEST_WORKBOOK").unwrap());
    let size = fs::metadata(&fixture).unwrap().len();
    assert!(size >= 1_065_968_177);
    let kind = env::var("RECOVERY_TEST_FS").unwrap();
    let pl = Place { dir: root.join("slot"), img: root.join("slot.img"), person: None };
    let image = match kind.as_str() {
        "ufs" => pl.img.with_extension("ufs"),
        "tessera" => pl.img.clone(),
        _ => panic!("expected ufs or tessera"),
    };
    let data = pl.dir.join("autosave");
    let journal = data.join("journal.1");
    mount_recovery(&pl, 128 << 20).unwrap();
    let old_size = fs::metadata(&image).unwrap().len();
    fs::create_dir(&data).unwrap();
    fs::write(&journal, b"pending edits").unwrap();
    unmount_recovery(&pl).unwrap();
    mount_recovery(&pl, WORKBOOK_RECOVERY_BYTES).unwrap();
    assert!(resize_empty_recovery(&pl).is_err());
    assert_eq!(fs::metadata(&image).unwrap().len(), old_size);
    assert_eq!(fs::read(&journal).unwrap(), b"pending edits");
    fs::remove_file(&journal).unwrap();
    resize_empty_recovery(&pl).unwrap();
    let expected_size = if kind == "ufs" {
        WORKBOOK_RECOVERY_BYTES
    } else {
        portcullis_overlay::image_mib_for_quota(WORKBOOK_RECOVERY_BYTES) << 20
    };
    assert_eq!(fs::metadata(&image).unwrap().len(), expected_size);
    for name in ["base.1.osheet", "base.2.osheet"] {
        let mut dest = fs::File::create(data.join(name)).unwrap();
        assert_eq!(io::copy(&mut fs::File::open(&fixture).unwrap(), &mut dest).unwrap(), size);
        dest.sync_all().unwrap();
    }
    fs::write(&journal, b"edits after checkpoint").unwrap();
    unmount_recovery(&pl).unwrap();
    mount_recovery(&pl, WORKBOOK_RECOVERY_BYTES).unwrap();
    assert_eq!(fs::read(&journal).unwrap(), b"edits after checkpoint");
    for name in ["base.1.osheet", "base.2.osheet"] {
        assert!(Command::new("cmp").arg(&fixture).arg(data.join(name)).status().unwrap().success());
    }
    unmount_recovery(&pl).unwrap();
    println!("{kind} preserved legacy image ({old_size} bytes), then held two {size}-byte bases and a journal in a bounded {expected_size}-byte image");
}
