//! tessera-binsplit — function-level dedup analysis for ELF binaries.
//!
//! Phase 1: research tool. Walks an ELF binary, identifies functions,
//! normalises each (zeroes the bytes covered by relocations), hashes
//! the normalised form, and prints stats. Optionally compares two
//! binaries to see how many function blobs they share.
//!
//! Goal: validate the function-level-dedup hypothesis on real Rust
//! binaries before investing in extract/reconstitute (Phases 2-3).
//!
//! USAGE:
//!     tessera-binsplit --analyze <BINARY>
//!     tessera-binsplit --compare <BINARY_A> <BINARY_B>
//!
//! See docs/spec/tessera-binsplit.md for the full design.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::process::ExitCode;

use object::{
    Object, ObjectSection, ObjectSymbol, RelocationTarget, SectionIndex, SymbolKind,
};
use sha2::{Digest, Sha256};

/// One extracted function's analysis.
struct FuncInfo {
    name:        String,
    /// Blob hash = sha256(bytes with relocation-affected ranges zeroed).
    blob_hash:   [u8; 32],
    /// Original byte length (.text bytes for this function).
    size:        u64,
    /// Number of relocations within this function's range.
    reloc_count: usize,
    /// Total bytes affected by relocations (sum of reloc widths).
    reloc_bytes: usize,
}

struct AnalysisResult {
    binary_path:      String,
    text_bytes:       u64,
    functions:        Vec<FuncInfo>,
    /// Bytes in .text that did NOT belong to any extracted function
    /// (alignment padding, embedded data, code without symbols).
    unaccounted:      u64,
}

/// Mask the PC-relative offset fields in aarch64 instructions to
/// zero. This normalises functions across binaries: two binaries
/// that contain the same logical function but with different
/// neighbours produce the same blob hash.
///
/// Handles the high-leverage cases:
///   - B / BL (`bl <target>`, `b <target>`)
///   - B.cond / CBZ / CBNZ / TBZ / TBNZ (conditional branches)
///   - ADRP (PC-relative page address — every global access uses it)
///   - LDR (literal) (PC-relative load, less common but not rare)
///
/// Conservative: a non-PC-rel instruction that happens to match
/// an encoding pattern gets masked too. False-positive-mask only
/// hurts dedup precision (the function won't match its real twin
/// any worse than already), never correctness — we never produce
/// wrong bytes, only fewer dedup hits.
fn mask_pc_rel_aarch64(bytes: &mut [u8]) {
    /* aarch64 instructions are 4-byte aligned. */
    let mut i = 0;
    while i + 4 <= bytes.len() {
        let w = u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());

        /* B  (unconditional branch): top 6 bits = 000101 → 0x14000000 */
        /* BL (branch + link):        top 6 bits = 100101 → 0x94000000 */
        if (w & 0x7C00_0000) == 0x1400_0000 {
            /* zero bits 0..25 (signed offset / 4) */
            let masked = w & !0x03FF_FFFF;
            bytes[i..i + 4].copy_from_slice(&masked.to_le_bytes());
            i += 4; continue;
        }

        /* B.cond:    01010100 iiiii iiiii iiii iiii iccc cccc → 0x54000000 mask 0xFF000010 */
        if (w & 0xFF00_0010) == 0x5400_0000 {
            /* zero bits 5..23 (signed offset / 4, 19 bits) */
            let masked = w & !0x00FF_FFE0;
            bytes[i..i + 4].copy_from_slice(&masked.to_le_bytes());
            i += 4; continue;
        }

        /* CBZ / CBNZ:  Z011_010Z iiiii iiiii iiii iiii iRRR RRRR
         *   sf=0,1; opc bit 24 = 0 (CBZ) or 1 (CBNZ)
         *   pattern: 0x34000000 (CBZ 32), 0x35000000 (CBNZ 32),
         *            0xB4000000 (CBZ 64), 0xB5000000 (CBNZ 64)
         *   common mask: 0x7E000000 == 0x34000000 */
        if (w & 0x7E00_0000) == 0x3400_0000 {
            /* zero bits 5..23 (offset / 4) */
            let masked = w & !0x00FF_FFE0;
            bytes[i..i + 4].copy_from_slice(&masked.to_le_bytes());
            i += 4; continue;
        }

        /* TBZ / TBNZ:  b011_011o bbbbb iiii iiii iiii iRRR RRRR
         *   pattern (mask 0x7E000000): 0x36000000 (TBZ) / 0x37000000 (TBNZ) */
        if (w & 0x7E00_0000) == 0x3600_0000 {
            /* zero bits 5..18 (offset / 4, 14 bits) */
            let masked = w & !0x0007_FFE0;
            bytes[i..i + 4].copy_from_slice(&masked.to_le_bytes());
            i += 4; continue;
        }

        /* ADR / ADRP:  Pii1 0000 iiiii iiiii iiii iiii iRRR RR
         *   ADR  pattern 0x10000000 (P=0, top byte 0x10..0x1F)
         *   ADRP pattern 0x90000000 (P=1, top byte 0x90..0x9F)
         *   common mask: 0x1F000000 == 0x10000000 */
        if (w & 0x1F00_0000) == 0x1000_0000 {
            /* zero bits 5..23 (immhi) and bits 29..30 (immlo) */
            let masked = w & !0x60FF_FFE0;
            bytes[i..i + 4].copy_from_slice(&masked.to_le_bytes());
            i += 4; continue;
        }

        /* LDR (literal):  oo01_1000 iiiii iiiii iiii iiii iTTT TT
         *   patterns: 0x18000000 (LDR W), 0x58000000 (LDR X),
         *             0x1C000000 (LDR S), 0x5C000000 (LDR D),
         *             0x9C000000 (LDR Q)
         *   common high mask 0x3B000000 == 0x18000000 */
        if (w & 0x3B00_0000) == 0x1800_0000 {
            /* zero bits 5..23 (offset / 4) */
            let masked = w & !0x00FF_FFE0;
            bytes[i..i + 4].copy_from_slice(&masked.to_le_bytes());
            i += 4; continue;
        }

        i += 4;
    }
}

/// Width in bytes of the field a relocation type rewrites. Conservative
/// per-arch table; unknown types fall back to 8 bytes (the safe upper
/// bound for any reasonable reloc).
fn reloc_width(arch: object::Architecture, ty: u32) -> usize {
    use object::elf::*;
    match arch {
        object::Architecture::Aarch64 => match ty {
            R_AARCH64_ABS64 | R_AARCH64_PREL64 => 8,
            R_AARCH64_ABS32 | R_AARCH64_PREL32 => 4,
            R_AARCH64_CALL26 | R_AARCH64_JUMP26 => 4,
            R_AARCH64_ADR_PREL_PG_HI21 | R_AARCH64_ADR_PREL_LO21 => 4,
            R_AARCH64_ADD_ABS_LO12_NC => 4,
            R_AARCH64_LDST8_ABS_LO12_NC | R_AARCH64_LDST16_ABS_LO12_NC
            | R_AARCH64_LDST32_ABS_LO12_NC | R_AARCH64_LDST64_ABS_LO12_NC
            | R_AARCH64_LDST128_ABS_LO12_NC => 4,
            R_AARCH64_TSTBR14 | R_AARCH64_CONDBR19 => 4,
            R_AARCH64_MOVW_UABS_G0_NC | R_AARCH64_MOVW_UABS_G1_NC
            | R_AARCH64_MOVW_UABS_G2_NC | R_AARCH64_MOVW_UABS_G3 => 4,
            _ => 8, /* conservative — over-zero is a precision hit
                     * not a correctness one */
        },
        object::Architecture::X86_64 => match ty {
            R_X86_64_64 => 8,
            R_X86_64_32 | R_X86_64_32S | R_X86_64_PC32
            | R_X86_64_PLT32 | R_X86_64_GOTPCREL => 4,
            R_X86_64_16 | R_X86_64_PC16 => 2,
            R_X86_64_8 | R_X86_64_PC8 => 1,
            _ => 8,
        },
        _ => 8,
    }
}

fn analyze(path: &str) -> Result<AnalysisResult, String> {
    let bytes = fs::read(path).map_err(|e| format!("read {path}: {e}"))?;
    let obj = object::File::parse(&*bytes)
        .map_err(|e| format!("parse {path}: {e}"))?;
    let arch = obj.architecture();

    /* Find .text — that's where executable code lives in a normal
     * Rust ELF release build. */
    let text = obj.section_by_name(".text")
        .ok_or_else(|| format!("{path}: no .text section"))?;
    let text_index = text.index();
    let text_addr  = text.address();
    let text_size  = text.size();
    let text_data  = text.data().map_err(|e| format!("text data: {e}"))?;

    /* Index relocations by section + offset for quick lookup. ELF
     * relocations target a section + offset; for .text we want a map
     * from offset-within-text → reloc info. */
    let text_relocs: Vec<(u64, u32)> = collect_text_relocs(&obj, text_index)?;

    /* Collect function symbols within .text. */
    let mut funcs: Vec<(String, u64, u64)> = Vec::new();
    for sym in obj.symbols() {
        if sym.kind() != SymbolKind::Text { continue; }
        let Some(SectionIndex(idx)) = sym.section_index() else { continue };
        if idx != text_index.0 { continue; }
        let size = sym.size();
        if size == 0 { continue; } /* aliases / weak fwd-decls */
        let name = sym.name().unwrap_or("<anon>").to_string();
        funcs.push((name, sym.address(), size));
    }
    /* Sort by virtual address; helps unaccounted-bytes calc. */
    funcs.sort_by_key(|t| t.1);

    /* Extract + hash each function. */
    let mut accounted: u64 = 0;
    let mut out = Vec::with_capacity(funcs.len());
    for (name, vaddr, size) in funcs {
        let off = (vaddr - text_addr) as usize;
        let end = off + size as usize;
        if end > text_data.len() {
            /* Symbol table claims a function past .text end —
             * skip with a note. */
            continue;
        }
        let mut bytes = text_data[off..end].to_vec();

        /* Zero bytes covered by any link-time relocation. (For
         * fully-linked executables this is usually empty.) */
        let mut reloc_count = 0;
        let mut reloc_bytes = 0;
        for &(roff, rtype) in &text_relocs {
            if roff >= vaddr && roff < vaddr + size {
                let rstart = (roff - vaddr) as usize;
                let w = reloc_width(arch, rtype);
                let rend = (rstart + w).min(bytes.len());
                for b in &mut bytes[rstart..rend] {
                    *b = 0;
                }
                reloc_count += 1;
                reloc_bytes += rend - rstart;
            }
        }

        /* Per-arch PC-relative offset masking. This is what lets
         * us dedup the same logical function across binaries that
         * placed it next to different neighbours. */
        match arch {
            object::Architecture::Aarch64 => mask_pc_rel_aarch64(&mut bytes),
            _ => {} /* x86_64 PC-rel masking deferred — see binsplit.md §5 */
        }

        let mut h = Sha256::new();
        h.update(&bytes);
        let hash_arr: [u8; 32] = h.finalize().into();

        out.push(FuncInfo {
            name,
            blob_hash:   hash_arr,
            size,
            reloc_count,
            reloc_bytes,
        });
        accounted += size;
    }

    Ok(AnalysisResult {
        binary_path: path.to_string(),
        text_bytes:  text_size,
        functions:   out,
        unaccounted: text_size.saturating_sub(accounted),
    })
}

/// Walk all relocation sections that target the .text section and
/// return (relocation_offset_virtual, relocation_type). Object's API
/// gives us per-section reloc iterators that already reference the
/// target section.
fn collect_text_relocs(
    obj: &object::File,
    text_index: SectionIndex,
) -> Result<Vec<(u64, u32)>, String> {
    let text_addr = obj.section_by_index(text_index)
        .map_err(|e| format!("text section: {e}"))?
        .address();
    let mut out = Vec::new();
    /* `Section::relocations()` yields all relocations applied to this
     * section. The offset returned is the offset within the section. */
    let text = obj.section_by_index(text_index)
        .map_err(|e| format!("text section: {e}"))?;
    for (off, rel) in text.relocations() {
        let _ = rel.target(); /* target lookup unused in Phase 1; we
                               * only need type+location to normalize */
        let rtype = match rel.flags() {
            object::RelocationFlags::Elf { r_type } => r_type,
            _ => 0,
        };
        out.push((text_addr + off, rtype));
    }
    /* Some relocations live in dynamic sections (.rela.dyn / .rela.plt)
     * for executables — those describe runtime fixups. Object's
     * section.relocations() iterates link-time relocs; for stripped
     * dynamic-linked binaries we'd also need dynamic relocs. Keep this
     * Phase 1 simple — note any binary where reloc count seems too
     * low. */
    Ok(out)
}

fn print_analysis(a: &AnalysisResult) {
    println!("=== {} ===", a.binary_path);
    println!("  .text bytes        : {} ({})", a.text_bytes, human(a.text_bytes));
    println!("  functions found    : {}", a.functions.len());
    let accounted: u64 = a.functions.iter().map(|f| f.size).sum();
    println!("  function bytes     : {} ({})", accounted, human(accounted));
    println!("  unaccounted bytes  : {} ({:.1}%)",
        a.unaccounted,
        100.0 * a.unaccounted as f64 / a.text_bytes.max(1) as f64);
    let total_relocs: usize = a.functions.iter().map(|f| f.reloc_count).sum();
    let total_reloc_bytes: usize = a.functions.iter().map(|f| f.reloc_bytes).sum();
    println!("  relocations        : {} ({} bytes zeroed for normalisation)",
        total_relocs, total_reloc_bytes);

    /* Unique blobs vs total. */
    let mut by_hash: HashMap<[u8; 32], (u32, u64)> = HashMap::new();
    for f in &a.functions {
        let e = by_hash.entry(f.blob_hash).or_insert((0, 0));
        e.0 += 1;
        e.1 = f.size;
    }
    let unique = by_hash.len();
    let dup_funcs = a.functions.len() - unique;
    let dup_bytes: u64 = by_hash.values()
        .filter(|(c, _)| *c > 1)
        .map(|(c, sz)| (*c as u64 - 1) * *sz)
        .sum();
    println!("  unique blobs       : {} (within-binary collapse: {} dup funcs, {} bytes)",
        unique, dup_funcs, human(dup_bytes));

    /* Function size distribution. */
    let mut sizes: Vec<u64> = a.functions.iter().map(|f| f.size).collect();
    sizes.sort_unstable();
    if !sizes.is_empty() {
        let med = sizes[sizes.len() / 2];
        let p90 = sizes[sizes.len() * 9 / 10];
        let p99 = sizes[(sizes.len() * 99 / 100).min(sizes.len() - 1)];
        let max = *sizes.last().unwrap();
        println!("  size distribution  : median {}  p90 {}  p99 {}  max {}",
            human(med), human(p90), human(p99), human(max));
    }

    /* Top-5 largest. */
    let mut top: Vec<&FuncInfo> = a.functions.iter().collect();
    top.sort_by_key(|f| std::cmp::Reverse(f.size));
    println!("  top 5 largest:");
    for f in top.iter().take(5) {
        println!("    {:>10}  {}",
            human(f.size),
            short_name(&f.name));
    }
}

fn compare(a: &AnalysisResult, b: &AnalysisResult) {
    let a_blobs: HashMap<[u8; 32], u64> = a.functions.iter()
        .map(|f| (f.blob_hash, f.size))
        .collect();
    let b_blobs: HashMap<[u8; 32], u64> = b.functions.iter()
        .map(|f| (f.blob_hash, f.size))
        .collect();
    let a_set: HashSet<[u8; 32]> = a_blobs.keys().copied().collect();
    let b_set: HashSet<[u8; 32]> = b_blobs.keys().copied().collect();
    let shared: HashSet<[u8; 32]> = a_set.intersection(&b_set).copied().collect();

    let shared_bytes_a: u64 = shared.iter().filter_map(|h| a_blobs.get(h)).sum();
    let shared_bytes_b: u64 = shared.iter().filter_map(|h| b_blobs.get(h)).sum();
    let unique_a: u64 = a_set.difference(&b_set)
        .filter_map(|h| a_blobs.get(h)).sum();
    let unique_b: u64 = b_set.difference(&a_set)
        .filter_map(|h| b_blobs.get(h)).sum();

    let total_a: u64 = a_blobs.values().sum();
    let total_b: u64 = b_blobs.values().sum();

    println!();
    println!("=== Cross-binary comparison ===");
    println!("  A: {}", a.binary_path);
    println!("  B: {}", b.binary_path);
    println!("  shared blobs       : {} ({} unique-blob hashes)",
        shared.len(), shared.len());
    println!("  shared bytes (A)   : {} / {} = {:.1}% of A",
        human(shared_bytes_a), human(total_a),
        100.0 * shared_bytes_a as f64 / total_a.max(1) as f64);
    println!("  shared bytes (B)   : {} / {} = {:.1}% of B",
        human(shared_bytes_b), human(total_b),
        100.0 * shared_bytes_b as f64 / total_b.max(1) as f64);
    println!("  unique-to-A bytes  : {}", human(unique_a));
    println!("  unique-to-B bytes  : {}", human(unique_b));

    /* If both binaries were stored function-deduped, total disk =
     * union of all unique blobs. */
    let union: HashSet<[u8; 32]> = a_set.union(&b_set).copied().collect();
    let union_bytes: u64 = union.iter().filter_map(|h| {
        a_blobs.get(h).or_else(|| b_blobs.get(h))
    }).sum();
    println!();
    println!("  flat sum (A+B)     : {} ({})",
        human(total_a + total_b), total_a + total_b);
    println!("  function-deduped   : {} ({})",
        human(union_bytes), union_bytes);
    let saved = (total_a + total_b).saturating_sub(union_bytes);
    let ratio = (total_a + total_b) as f64 / union_bytes.max(1) as f64;
    println!("  saved              : {} ({:.2}× compression vs flat)",
        human(saved), ratio);
}

fn human(b: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    if b >= MIB { format!("{:.2} MiB", b as f64 / MIB as f64) }
    else if b >= KIB { format!("{:.2} KiB", b as f64 / KIB as f64) }
    else { format!("{b} B") }
}

fn short_name(s: &str) -> &str {
    /* Rust mangled names get long; show last component of the path. */
    s.rsplit("::").next().unwrap_or(s)
}

fn multi(paths: &[String]) -> Result<(), String> {
    let mut analyses: Vec<AnalysisResult> = Vec::with_capacity(paths.len());
    for p in paths {
        let a = analyze(p)?;
        analyses.push(a);
    }

    /* For aggregate: treat each unique blob hash as one entry,
     * sized by max(any binary's view of its size — should always
     * match since the bytes hash determines size, but defensively). */
    let mut union: HashMap<[u8; 32], u64> = HashMap::new();
    let mut per_bin: Vec<HashSet<[u8; 32]>> = Vec::with_capacity(analyses.len());
    let mut flat_total: u64 = 0;
    for a in &analyses {
        let mut s: HashSet<[u8; 32]> = HashSet::new();
        let mut bin_total: u64 = 0;
        for f in &a.functions {
            s.insert(f.blob_hash);
            bin_total += f.size;
            union.entry(f.blob_hash).or_insert(f.size);
        }
        per_bin.push(s);
        flat_total += bin_total;
    }
    let union_bytes: u64 = union.values().sum();

    println!("\n=== N-way aggregate (n={}) ===", analyses.len());
    println!("  per-binary function bytes:");
    for (a, set) in analyses.iter().zip(per_bin.iter()) {
        let bytes: u64 = a.functions.iter().map(|f| f.size).sum();
        println!("    {:>30}  {:>10}  ({} blobs)",
            short_path(&a.binary_path),
            human(bytes),
            set.len());
    }
    println!();
    println!("  flat sum (Σ binaries) : {} ({})",
        human(flat_total), flat_total);
    println!("  function-deduped union: {} ({})",
        human(union_bytes), union_bytes);
    let saved = flat_total.saturating_sub(union_bytes);
    let ratio = flat_total as f64 / union_bytes.max(1) as f64;
    println!("  saved                 : {} ({:.2}× compression vs flat)",
        human(saved), ratio);

    /* Marginal cost: if I install binary i+1 having already
     * installed binaries 0..=i, how many NEW bytes does it add? */
    println!();
    println!("  marginal install cost (bytes added per binary, in argv order):");
    let mut seen: HashSet<[u8; 32]> = HashSet::new();
    let mut cumulative: u64 = 0;
    for a in &analyses {
        let mut new_bytes: u64 = 0;
        for f in &a.functions {
            if seen.insert(f.blob_hash) {
                new_bytes += f.size;
            }
        }
        cumulative += new_bytes;
        let bin_total: u64 = a.functions.iter().map(|f| f.size).sum();
        let savings = if bin_total > 0 {
            100.0 * (1.0 - new_bytes as f64 / bin_total as f64)
        } else { 0.0 };
        println!("    {:>30}  flat {:>10}  marginal {:>10}  ({:.0}% saved)  cum {}",
            short_path(&a.binary_path),
            human(bin_total),
            human(new_bytes),
            savings,
            human(cumulative));
    }

    Ok(())
}

fn short_path(p: &str) -> String {
    p.rsplit('/').next().unwrap_or(p).to_string()
}

fn usage() -> ! {
    eprintln!(
        "usage:\n  \
         tessera-binsplit --analyze <BINARY>\n  \
         tessera-binsplit --compare <BINARY_A> <BINARY_B>\n  \
         tessera-binsplit --multi <BIN1> [<BIN2> ...]"
    );
    std::process::exit(2);
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("--analyze") if args.len() == 3 => {
            match analyze(&args[2]) {
                Ok(a) => { print_analysis(&a); ExitCode::SUCCESS }
                Err(e) => { eprintln!("error: {e}"); ExitCode::from(1) }
            }
        }
        Some("--compare") if args.len() == 4 => {
            match (analyze(&args[2]), analyze(&args[3])) {
                (Ok(a), Ok(b)) => {
                    print_analysis(&a);
                    print_analysis(&b);
                    compare(&a, &b);
                    let _ = (BTreeMap::<u8, u8>::new(),);
                    ExitCode::SUCCESS
                }
                (Err(e), _) | (_, Err(e)) => {
                    eprintln!("error: {e}");
                    ExitCode::from(1)
                }
            }
        }
        Some("--multi") if args.len() >= 3 => {
            match multi(&args[2..]) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => { eprintln!("error: {e}"); ExitCode::from(1) }
            }
        }
        _ => usage(),
    }
}
