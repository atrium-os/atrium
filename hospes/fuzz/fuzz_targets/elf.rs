//! Hostile ELF input to hospes-elf (docs/spec/hospes.md §11.6): parse must return, never
//! panic, and whatever it returns must be self-consistent.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(img) = hospes_elf::parse(data, None) {
        let (lo, hi) = img.span;
        assert!(lo <= hi && hi - lo <= hospes_elf::MAX_SPAN);
        for s in &img.segments {
            assert!(s.filesz <= s.memsz);
            assert!(!(s.flags.w && s.flags.x));
            assert!(s.vaddr >= lo && s.vaddr + s.memsz <= hi);
        }
        for r in &img.relocs {
            assert!((r.sym as usize) < img.symbols.len().max(1));
        }
        let _ = img.imports().count();
        let _ = img.export("fixture_main", None);
    }
});
