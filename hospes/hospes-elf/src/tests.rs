//! Tests for the parser. Synthetic libraries are built at test time with clang + lld
//! (`-nostdlib`, both machines) when clang is on PATH; the real CDM / OpenH264 binaries are
//! parsed only when `HOSPES_REAL_LIBS` names a directory holding them (they are never
//! committed).
extern crate std;

use super::*;
use std::path::PathBuf;
use std::process::Command;
use std::vec;
use std::vec::Vec as StdVec;

const SRC: &str = r#"
extern int imported_fn(int);
extern int imported_data;
__thread int tls_counter = 7;
static int initialised;
int table[4] = { 1, 2, 3, 4 };
int *ptr_to_table = &table[2];
int (*fn_ptr)(int) = imported_fn;
__attribute__((constructor)) static void ctor(void) { initialised = imported_fn(1); }
int exported(int x) { return imported_fn(x) + imported_data + tls_counter + initialised; }
int old_impl(void) { return 1; }
int new_impl(void) { return 2; }
__asm__(".symver old_impl, versioned@V1");
__asm__(".symver new_impl, versioned@@V2");
"#;

const MAP: &str = "V1 { global: versioned; local: *; }; V2 { global: exported; } V1;";

fn clang() -> Option<PathBuf> {
    let ok = Command::new("clang").arg("--version").output().map(|o| o.status.success()).unwrap_or(false);
    ok.then(|| PathBuf::from("clang"))
}

fn build(target: &str, extra: &[&str]) -> Option<StdVec<u8>> {
    let cc = clang()?;
    let tag = std::format!("{}-{}", target, extra.join("_").replace(['-', ',', '='], ""));
    let dir = std::env::temp_dir().join(std::format!("hospes-elf-test-{}-{tag}-{:?}", std::process::id(), std::thread::current().id()));
    std::fs::create_dir_all(&dir).ok()?;
    let c = dir.join("t.c");
    let map = dir.join("t.map");
    let out = dir.join(std::format!("{tag}.so"));
    std::fs::write(&c, SRC).ok()?;
    std::fs::write(&map, MAP).ok()?;
    let st = Command::new(cc)
        .arg(std::format!("--target={target}")).args(["-fuse-ld=lld", "-shared", "-nostdlib", "-fPIC", "-O1"])
        .arg(std::format!("-Wl,--version-script={}", map.display()))
        .args(["-Wl,-z,relro,-z,now", "-Wl,--hash-style=gnu"])
        .args(extra)
        .arg("-o")
        .arg(&out)
        .arg(&c)
        .status()
        .ok()?;
    assert!(st.success(), "clang failed for {target}");
    std::fs::read(&out).ok()
}

fn check_fixture(img: &Image, m: Machine) {
    assert_eq!(img.machine, Some(m));
    assert!(img.bind_now);
    assert!(img.relro.is_some());
    let tls = img.tls.as_ref().expect("TLS segment");
    assert!(tls.filesz >= 4);
    let e = img.export("exported", None).expect("exported");
    assert!(e.defined && e.kind == SymKind::Func);
    let v2 = img.export("versioned", Some("V2")).expect("versioned@@V2");
    assert!(!v2.hidden);
    let v1 = img.export("versioned", Some("V1")).expect("versioned@V1");
    assert!(v1.hidden);
    assert_eq!(img.export("versioned", None).map(|s| s.value), Some(v2.value));
    let imports: StdVec<&str> = img.imports().map(|(_, s)| s.name.as_str()).collect();
    assert!(imports.contains(&"imported_fn"), "{imports:?}");
    assert!(imports.contains(&"imported_data"), "{imports:?}");
    assert!(img.init_array.map_or(false, |(_, n)| n >= 1));
    let kinds: StdVec<RelocKind> = img.relocs.iter().map(|r| r.kind).collect();
    assert!(kinds.contains(&RelocKind::GlobDat) || kinds.contains(&RelocKind::JumpSlot), "{kinds:?}");
    assert!(kinds.contains(&RelocKind::Abs64), "{kinds:?}");
    assert!(kinds.contains(&RelocKind::Relative) || !img.relr.is_empty(), "{kinds:?}");
    for s in &img.segments {
        assert!(!(s.flags.w && s.flags.x));
    }
}

#[test]
fn fixture_x86_64() {
    let Some(b) = build("x86_64-linux-gnu", &[]) else { return };
    check_fixture(&parse(&b, Some(Machine::X86_64)).unwrap(), Machine::X86_64);
    assert_eq!(parse(&b, Some(Machine::Aarch64)), Err(Error::WrongMachine { found: 62 }));
}

#[test]
fn fixture_aarch64() {
    let Some(b) = build("aarch64-linux-gnu", &[]) else { return };
    check_fixture(&parse(&b, Some(Machine::Aarch64)).unwrap(), Machine::Aarch64);
}

#[test]
fn fixture_relr_and_sysv_hash() {
    for t in ["x86_64-linux-gnu", "aarch64-linux-gnu"] {
        let Some(b) = build(t, &["-Wl,-z,pack-relative-relocs", "-Wl,--hash-style=sysv"]) else { return };
        let img = parse(&b, None).unwrap();
        assert!(!img.relr.is_empty(), "{t}: RELR expected");
        assert!(img.relocs.iter().all(|r| r.kind != RelocKind::Relative), "{t}");
        assert!(img.export("exported", None).is_some());
    }
}

#[test]
fn fixture_tlsdesc_aarch64() {
    // aarch64 uses TLS descriptors by default for global-dynamic TLS.
    let src = "extern __thread int ext_tls; int get(void) { return ext_tls; }";
    let Some(cc) = clang() else { return };
    let dir = std::env::temp_dir().join(std::format!("hospes-elf-tlsdesc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("d.c"), src).unwrap();
    // (x86_64 TLS descriptors need -mtls-dialect=gnu2, which this clang lacks; the real
    // x86_64 libraries use __tls_get_addr.)
    for (t, want) in [("aarch64-linux-gnu", vec!["-O1"])] {
        let out = dir.join(std::format!("{t}.so"));
        let st = Command::new(&cc)
            .arg(std::format!("--target={t}")).args(["-fuse-ld=lld", "-shared", "-nostdlib", "-fPIC"])
            .args(&want)
            .arg("-o")
            .arg(&out)
            .arg(dir.join("d.c"))
            .status()
            .unwrap();
        assert!(st.success());
        let img = parse(&std::fs::read(&out).unwrap(), None).unwrap();
        assert!(img.relocs.iter().any(|r| r.kind == RelocKind::TlsDesc), "{t}: {:?}", img.relocs);
    }
}

// ---- refusals, by patching a good fixture -------------------------------------------

fn phdrs(b: &[u8]) -> (usize, usize) {
    let phoff = u64::from_le_bytes(b[32..40].try_into().unwrap()) as usize;
    let phnum = u16::from_le_bytes(b[56..58].try_into().unwrap()) as usize;
    (phoff, phnum)
}

fn find_phdr(b: &[u8], ty: u32) -> Option<usize> {
    let (o, n) = phdrs(b);
    (0..n).map(|i| o + i * 56).find(|&p| u32::from_le_bytes(b[p..p + 4].try_into().unwrap()) == ty)
}

#[test]
fn refuses_interp_wx_execstack_exec_and_rel() {
    let Some(good) = build("x86_64-linux-gnu", &[]) else { return };
    // PT_INTERP: turn PT_GNU_EH_FRAME into PT_INTERP.
    let mut b = good.clone();
    let p = find_phdr(&b, PT_GNU_EH_FRAME).unwrap();
    b[p..p + 4].copy_from_slice(&PT_INTERP.to_le_bytes());
    assert_eq!(parse(&b, None), Err(Error::HasInterp));
    // W+X: make the executable PT_LOAD writable too.
    let mut b = good.clone();
    let (o, n) = phdrs(&b);
    let p = (0..n)
        .map(|i| o + i * 56)
        .find(|&p| u32::from_le_bytes(b[p..p + 4].try_into().unwrap()) == PT_LOAD && b[p + 4] & 1 != 0)
        .unwrap();
    b[p + 4] |= 2;
    assert_eq!(parse(&b, None), Err(Error::BadSegment("writable and executable")));
    // Executable stack.
    let mut b = good.clone();
    let p = find_phdr(&b, PT_GNU_STACK).unwrap();
    b[p + 4] |= 1;
    assert_eq!(parse(&b, None), Err(Error::BadSegment("executable stack requested")));
    // ET_EXEC.
    let mut b = good.clone();
    b[16] = 2;
    assert_eq!(parse(&b, None), Err(Error::NotSharedObject));
    // 32-bit / big-endian.
    let mut b = good.clone();
    b[4] = 1;
    assert_eq!(parse(&b, None), Err(Error::NotElf64Le));
    let mut b = good.clone();
    b[0] = 0;
    assert_eq!(parse(&b, None), Err(Error::BadMagic));
}

#[test]
fn refuses_textrel_and_rel() {
    let Some(good) = build("x86_64-linux-gnu", &[]) else { return };
    let img = parse(&good, None).unwrap();
    let (dva, dsz) = img.dynamic;
    let doff = va2off(&img, dva, dsz).unwrap() as usize;
    for (tag, want) in [(DT_TEXTREL, Error::TextRel), (DT_REL, Error::NotRela)] {
        let mut b = good.clone();
        // Overwrite the first DT_NULL-free slot we can spare: replace DT_SONAME/DT_FLAGS_1 or the
        // first entry whose loss is harmless. Use the last real entry before DT_NULL.
        let mut last = 0;
        for i in 0..dsz as usize / 16 {
            let t = u64::from_le_bytes(b[doff + i * 16..doff + i * 16 + 8].try_into().unwrap());
            if t == DT_NULL {
                break;
            }
            last = i;
        }
        // Prefer appending into the DT_NULL padding if there is room.
        let slot = last + 1;
        let slot = if (slot + 1) * 16 <= dsz as usize { slot } else { last };
        b[doff + slot * 16..doff + slot * 16 + 8].copy_from_slice(&tag.to_le_bytes());
        assert_eq!(parse(&b, None), Err(want));
    }
}

// ---- robustness: truncation and deterministic mutation never panic --------------------

#[test]
fn truncation_never_panics() {
    let Some(good) = build("aarch64-linux-gnu", &[]) else { return };
    for n in 0..good.len() {
        let _ = parse(&good[..n], None);
    }
}

#[test]
fn mutation_never_panics() {
    let Some(good) = build("x86_64-linux-gnu", &["-Wl,-z,pack-relative-relocs"]) else { return };
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut rnd = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    // Bias mutations toward the headers and dynamic metadata, where the parser looks.
    let hot = good.len().min(8192);
    for _ in 0..20_000 {
        let mut b = good.clone();
        for _ in 0..1 + rnd() % 8 {
            let i = (rnd() as usize) % if rnd() % 4 == 0 { b.len() } else { hot };
            b[i] = match rnd() % 4 {
                0 => 0,
                1 => 0xff,
                _ => rnd() as u8,
            };
        }
        let _ = parse(&b, None);
    }
}

// ---- real libraries (optional) ---------------------------------------------------------

#[test]
fn real_libraries() {
    let Ok(dir) = std::env::var("HOSPES_REAL_LIBS") else { return };
    let dir = PathBuf::from(dir);
    let cdm_exports = ["InitializeCdmModule_4", "CreateCdmInstance", "GetCdmVersion", "DeinitializeCdmModule"];
    for (f, m) in [("wv-x64.so", Machine::X86_64), ("wv-arm64.so", Machine::Aarch64)] {
        let Ok(b) = std::fs::read(dir.join(f)) else { continue };
        let img = parse(&b, Some(m)).unwrap_or_else(|e| panic!("{f}: {e:?}"));
        for e in cdm_exports {
            assert!(img.export(e, None).is_some(), "{f}: missing {e}");
        }
        let n = img.imports().count();
        std::println!("{f}: {} segs, {} syms, {} imports, {} relocs, {} relr, needed {:?}, tls {:?}",
            img.segments.len(), img.symbols.len(), n, img.relocs.len(), img.relr.len(), img.needed, img.tls);
        assert!(n > 50);
    }
    for (f, m) in [("libopenh264-2.6.0-linux64.8.so", Machine::X86_64), ("libopenh264-2.6.0-linux-arm64.8.so", Machine::Aarch64)] {
        let Ok(b) = std::fs::read(dir.join(f)) else { continue };
        let img = parse(&b, Some(m)).unwrap_or_else(|e| panic!("{f}: {e:?}"));
        assert!(img.export("WelsCreateDecoder", None).is_some(), "{f}");
        std::println!("{f}: {} imports, {} relocs, needed {:?}", img.imports().count(), img.relocs.len(), img.needed);
    }
}
