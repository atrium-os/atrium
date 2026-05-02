//! tessera-stat — look up a blob by content hash and print where
//! it lives on disk.
//!
//! USAGE:
//!     tessera-stat <VOLUME> <HEX_HASH>
//!
//! `<HEX_HASH>` is the 64-char hex SHA-256 of the blob you're
//! looking for (manifest hash from a `tessera-debug` dump, or any
//! hash seen in `kern.tessera.cas_loc_*` debugging).
//!
//! Walks the on-disk pack registry and, for each pack, opens it
//! and asks `tessera_pack_lookup` if the target hash is inside.
//! On hit, prints:
//!   - pack_id (registry key)
//!   - pack location (start_sector, length_sectors, multi-extent flag)
//!   - blob size + flags (manifest / chunk)
//!   - first 64 bytes of blob payload as hex (sanity preview)
//!
//! Read-only. Safe against a mounted volume only if the volume is
//! quiescent — pack-registry mutations under our walk could yield
//! a false miss. Prefer running against an unmounted volume.

use std::ffi::c_void;
use std::process::ExitCode;

use tessera_sys::{
    tessera_btree_close, tessera_btree_cursor_free, tessera_btree_cursor_get,
    tessera_btree_cursor_next, tessera_btree_open, tessera_btree_seek_first,
    tessera_btree_t, tessera_pack_close, tessera_pack_lookup,
    tessera_pack_open, tessera_volume_close, tessera_volume_open,
    tessera_volume_pack_registry_root, TESSERA_BLOB_FLAG_CHUNK,
    TESSERA_BLOB_FLAG_MANIFEST,
};
use tessera_tools::{fd_of, make_io, open_file_ro, DiskCtx, SECTOR_SIZE};

const REGISTRY_ENTRY_SIZE: usize = 64;
const REGISTRY_KEY_SIZE: usize = 16;
const HASH_SIZE: usize = 32;
const FETCH_PACK_MAX_SECTORS: u64 = 4096; /* 16 MiB cap, matches kmod */
const FLAG_MULTI_EXTENT: u32 = 1 << 2;

fn usage() -> ! {
    eprintln!("usage: tessera-stat <VOLUME> <HEX_HASH>");
    std::process::exit(2);
}

#[derive(Clone, Copy)]
struct RegistryEntry {
    pack_id:        [u8; 16],
    start_sector:   u64,
    length_sectors: u64,
    blob_count:     u32,
    pack_kind:      u32,
    total_bytes:    u64,
    flags:          u32,
}

fn decode_registry(b: &[u8]) -> Option<RegistryEntry> {
    if b.len() < REGISTRY_ENTRY_SIZE {
        return None;
    }
    let read_u64 = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    let read_u32 = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let mut pack_id = [0u8; 16];
    pack_id.copy_from_slice(&b[0..16]);
    Some(RegistryEntry {
        pack_id,
        start_sector:   read_u64(16),
        length_sectors: read_u64(24),
        blob_count:     read_u32(32),
        pack_kind:      read_u32(36),
        total_bytes:    read_u64(40),
        flags:          read_u32(60),
    })
}

fn parse_hex_hash(s: &str) -> Result<[u8; HASH_SIZE], String> {
    if s.len() != HASH_SIZE * 2 {
        return Err(format!("hash must be {} hex chars, got {}", HASH_SIZE * 2, s.len()));
    }
    let mut out = [0u8; HASH_SIZE];
    for i in 0..HASH_SIZE {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
            .map_err(|_| format!("bad hex at byte {i}"))?;
    }
    Ok(out)
}

fn hex(b: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        write!(s, "{x:02x}").unwrap();
    }
    s
}

fn read_pack_body(
    fd: i32,
    re: &RegistryEntry,
) -> Result<Vec<u8>, String> {
    if re.length_sectors == 0 || re.length_sectors > FETCH_PACK_MAX_SECTORS {
        return Err(format!(
            "pack length {} sectors out of range",
            re.length_sectors
        ));
    }
    if re.flags & FLAG_MULTI_EXTENT != 0 {
        return Err("multi-extent pack — not implemented (use mounted FS or extend tool)".into());
    }
    let pack_len = re.length_sectors * SECTOR_SIZE;
    let mut buf = vec![0u8; pack_len as usize];
    let n = unsafe {
        libc::pread(
            fd,
            buf.as_mut_ptr() as *mut c_void,
            pack_len as usize,
            (re.start_sector * SECTOR_SIZE) as i64,
        )
    };
    if n != pack_len as isize {
        return Err(format!(
            "pread {} bytes at sector {} returned {}",
            pack_len, re.start_sector, n
        ));
    }
    Ok(buf)
}

fn run(vol_path: &str, hash_hex: &str) -> Result<(), String> {
    let target = parse_hex_hash(hash_hex)?;
    let f = open_file_ro(vol_path).map_err(|e| format!("open {vol_path}: {e}"))?;
    let fd = fd_of(&f);

    let mut ctx = DiskCtx { fd };
    let io = make_io(&mut ctx);

    let mut v: *mut tessera_sys::tessera_volume_t = std::ptr::null_mut();
    let r = unsafe { tessera_volume_open(&io, &mut v) };
    if r != 0 {
        return Err(format!("tessera_volume_open failed: errno={r}"));
    }
    let pack_root = unsafe { tessera_volume_pack_registry_root(v) };
    unsafe { tessera_volume_close(v) };
    if pack_root == 0 {
        return Err("volume has no pack_registry_root (empty volume?)".into());
    }

    // tree_kind=1 is pack_registry (matches kmod's mountfs).
    let tree: *mut tessera_btree_t = unsafe {
        tessera_btree_open(
            &io,
            pack_root,
            1,
            REGISTRY_KEY_SIZE as u32,
            REGISTRY_ENTRY_SIZE as u32,
        )
    };
    if tree.is_null() {
        return Err("tessera_btree_open(pack_registry) returned null".into());
    }

    let cursor = unsafe { tessera_btree_seek_first(tree) };
    if cursor.is_null() {
        unsafe { tessera_btree_close(tree) };
        return Err("pack_registry is empty".into());
    }

    let mut packs_scanned = 0u64;
    let mut found = false;
    loop {
        let mut key = [0u8; REGISTRY_KEY_SIZE];
        let mut value = [0u8; REGISTRY_ENTRY_SIZE];
        let r = unsafe { tessera_btree_cursor_get(cursor, key.as_mut_ptr(), value.as_mut_ptr()) };
        if r != 0 {
            break;
        }
        packs_scanned += 1;

        let re = match decode_registry(&value) {
            Some(re) => re,
            None => {
                if unsafe { tessera_btree_cursor_next(cursor) } != 0 { break; }
                continue;
            }
        };
        let body = match read_pack_body(fd, &re) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("  skip pack {}: {}", hex(&re.pack_id), e);
                if unsafe { tessera_btree_cursor_next(cursor) } != 0 { break; }
                continue;
            }
        };
        let pr = unsafe { tessera_pack_open(body.as_ptr(), body.len()) };
        if pr.is_null() {
            if unsafe { tessera_btree_cursor_next(cursor) } != 0 { break; }
            continue;
        }
        let mut bytes_ptr: *const u8 = std::ptr::null();
        let mut blob_len: u32 = 0;
        let lr = unsafe {
            tessera_pack_lookup(pr, target.as_ptr(), &mut bytes_ptr, &mut blob_len)
        };
        if lr == 0 && !bytes_ptr.is_null() {
            // Hit. Snapshot a hex preview before closing the pack reader.
            let preview_len = blob_len.min(64) as usize;
            let preview: Vec<u8> = unsafe {
                std::slice::from_raw_parts(bytes_ptr, preview_len).to_vec()
            };
            unsafe { tessera_pack_close(pr) };

            println!("FOUND blob {hash_hex}");
            println!("  pack_id        : {}", hex(&re.pack_id));
            println!("  pack location  : sector {} ({} sectors, {} bytes)",
                re.start_sector, re.length_sectors, re.total_bytes);
            println!("  pack kind      : {} ({})",
                re.pack_kind, pack_kind_name(re.pack_kind));
            println!("  blob count     : {}", re.blob_count);
            println!("  blob length    : {} bytes", blob_len);
            println!("  flags          : 0x{:08x}{}", re.flags,
                if re.flags & FLAG_MULTI_EXTENT != 0 { " MULTI_EXTENT" } else { "" });
            println!("  preview ({preview_len} B): {}", hex(&preview));
            // Heuristic: blob flag info. We don't get the per-blob
            // flag from pack_lookup, but pack_kind hints at it
            // (manifest pack vs chunk pack vs mixed).
            let _ = (TESSERA_BLOB_FLAG_MANIFEST, TESSERA_BLOB_FLAG_CHUNK);
            found = true;
            break;
        }
        unsafe { tessera_pack_close(pr) };

        if unsafe { tessera_btree_cursor_next(cursor) } != 0 { break; }
    }

    unsafe {
        tessera_btree_cursor_free(cursor);
        tessera_btree_close(tree);
    }

    if !found {
        println!("NOT FOUND — scanned {packs_scanned} pack(s)");
        return Err("blob not in volume".into());
    }
    println!("\n  scanned {packs_scanned} pack(s) before hit");
    Ok(())
}

fn pack_kind_name(k: u32) -> &'static str {
    match k {
        0 => "manifest pack",
        1 => "chunk pack",
        2 => "mixed (chunks + manifest)",
        _ => "unknown",
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        usage();
    }
    match run(&args[1], &args[2]) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tessera-stat: {e}");
            ExitCode::from(1)
        }
    }
}
