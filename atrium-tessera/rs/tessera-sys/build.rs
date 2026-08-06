// Build script for tessera-sys: locate libtessera_core, emit link flags, and
// GENERATE the reserve-tree table from the C header that defines it.
//
// Phase 0 only links if TESSERA_CORE_LIB is set in the environment;
// otherwise the crate compiles as a pure-rust stub so `cargo check` works
// before the C library is built.

use std::collections::HashMap;
use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=TESSERA_CORE_LIB");
    println!("cargo:rerun-if-env-changed=TESSERA_CORE_INCLUDE");

    // ── linking the C core ──────────────────────────────────────────────
    //
    // Cross (the FreeBSD target) links the prebuilt archive the bootstrap
    // produces. The HOST cannot: the archive sitting in the tree is that same
    // CROSS-built ELF one, so linking it into a macOS binary dies with
    //
    //     ld: archive member 'tessera_reader.o' not a mach-o file
    //
    // which is why tessera-fsck could not be built on the host at all — and an
    // offline fsck of vm/*.img is exactly what you want when a guest is too
    // wedged to run one itself. `cargo clean -p tessera-sys` does not help;
    // the archive it picks up is wrong for the target, not stale.
    //
    // So for a non-FreeBSD target, compile the core from source right here.
    // No archive to mismatch, no build ordering to get right.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("freebsd") {
        if let Ok(libdir) = env::var("TESSERA_CORE_LIB") {
            println!("cargo:rustc-link-search=native={libdir}");
            println!("cargo:rustc-link-lib=static=tessera_core");
        }
    } else {
        build_core_for_host();
    }
    // libmd provides SHA-256 on FreeBSD. CARGO_CFG_TARGET_OS reflects
    // the cross-target (set by cargo); cfg!(target_os = ...) in
    // build.rs evaluates the build host, which is wrong for our
    // macOS → FreeBSD flow.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("freebsd") {
        println!("cargo:rustc-link-lib=md");
    }

    gen_reserve_trees();
}

/// Compile the C core for the build target (the non-FreeBSD case).
///
/// The file list comes from the core's own Makefile, never a copy: 17 of the 20
/// .c files in core/src belong to the library, so globbing would pull in ones
/// that don't, and hand-copying the list is how a file silently stops being
/// built. (scripts/core-host-tests.sh carries its own hand-copied SRCS for the
/// same job — worth collapsing onto this parser.)
fn build_core_for_host() {
    let core = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../core");
    let mk = core.join("Makefile");
    println!("cargo:rerun-if-changed={}", mk.display());
    let Ok(text) = std::fs::read_to_string(&mk) else {
        println!("cargo:warning=tessera-sys: {} unreadable — core not linked", mk.display());
        return;
    };
    let srcs = parse_make_srcs(&text);
    if srcs.is_empty() {
        println!("cargo:warning=tessera-sys: no SRCS in the core Makefile — core not linked");
        return;
    }
    let mut b = cc::Build::new();
    b.include(core.join("include")).include(core.join("src"))
        .flag_if_supported("-fno-strict-aliasing")
        .warnings(false);
    for f in &srcs {
        let p = core.join("src").join(f);
        println!("cargo:rerun-if-changed={}", p.display());
        b.file(p);
    }
    b.compile("tessera_core");
}

/// Pull `SRCS= a.c b.c \` (backslash-continued) out of a BSD makefile.
fn parse_make_srcs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_srcs = false;
    for line in text.lines() {
        let l = line.trim();
        if !in_srcs {
            if !l.starts_with("SRCS") { continue; }
            in_srcs = true;
        }
        let cont = l.ends_with('\\');
        let body = l.trim_end_matches('\\');
        let body = body.split_once('=').map(|(_, v)| v).unwrap_or(body);
        out.extend(body.split_whitespace()
                       .filter(|t| t.ends_with(".c"))
                       .map(str::to_string));
        if !cont { break; }
    }
    out
}

/// Turn `tessera/reserve_trees.h`'s X-macro table into a Rust slice.
///
/// The C side expands that macro directly; Rust cannot, so it is parsed here
/// — from the SAME file, on every build, so the two cannot drift. That is the
/// whole point: three trees were added to the metadata reserve and each was
/// forgotten by at least one consumer keeping its own copy of the list (see
/// docs/spec/tessera-reserve-trees.md). Now there is one list.
fn gen_reserve_trees() {
    let inc = env::var("TESSERA_CORE_INCLUDE").map(PathBuf::from).unwrap_or_else(|_| {
        // Default to the in-tree core beside this crate: rs/tessera-sys → ../../core
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../core/include")
    });
    let hdr = inc.join("tessera/reserve_trees.h");
    let fmt = inc.join("tessera/format.h");
    println!("cargo:rerun-if-changed={}", hdr.display());
    println!("cargo:rerun-if-changed={}", fmt.display());

    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("reserve_trees.rs");

    let (Ok(hsrc), Ok(fsrc)) = (std::fs::read_to_string(&hdr), std::fs::read_to_string(&fmt))
    else {
        // Header not reachable (crate vendored alone). Emit an EMPTY table,
        // never a stale hand-written one: an empty sweep checks nothing and
        // says so loudly, a stale sweep quietly checks the wrong trees.
        std::fs::write(&out,
            "pub const RESERVE_TREES: &[ReserveTree] = &[]; // header unavailable\n\
             /// # Safety\n\
             /// `v` must be a live volume handle.\n\
             pub unsafe fn reserve_tree_root(_v: *const tessera_volume_t, \
             _t: &ReserveTree) -> u64 { 0 }\n").unwrap();
        return;
    };

    // `#define NAME <integer>[uU]` — the only form the kind/size columns may
    // use. Anything else must fail loudly rather than resolve to garbage.
    let mut consts: HashMap<String, u64> = HashMap::new();
    for line in fsrc.lines() {
        let Some(rest) = line.trim().strip_prefix("#define ") else { continue };
        let mut it = rest.split_whitespace();
        let (Some(name), Some(val)) = (it.next(), it.next()) else { continue };
        if let Ok(n) = val.trim_end_matches(['u', 'U']).parse::<u64>() {
            consts.insert(name.to_string(), n);
        }
    }
    let resolve = |tok: &str| -> u64 {
        let tok = tok.trim();
        tok.trim_end_matches(['u', 'U']).parse::<u64>().ok()
            .or_else(|| consts.get(tok).copied())
            .unwrap_or_else(|| panic!(
                "reserve_trees.h: cannot resolve `{tok}` — kind and size columns must be \
                 integer literals or plain #define'd constants in format.h; this generator \
                 deliberately does not evaluate expressions"))
    };

    // Fold line continuations, then take the macro body.
    let flat = hsrc.replace("\\\n", " ");
    let body = flat.split_once("#define TESSERA_RESERVE_TREES(X)")
        .expect("reserve_trees.h: TESSERA_RESERVE_TREES(X) not found").1;
    let body = body.split("/* clang-format on */").next().unwrap();

    let (mut rows, mut n, mut rest) = (String::new(), 0usize, body);
    let mut fields: Vec<String> = Vec::new();
    while let Some(i) = rest.find("X(") {
        let (args, tail) = split_row(&rest[i + 2..]);
        rest = tail;
        let p = split_args(&args);
        assert_eq!(p.len(), 6, "reserve_trees.h: row {n} has {} columns, expected 6", p.len());
        let tier = match p[4].trim() {
            "REBUILD" => "StaleTier::Rebuild",
            "CLEAR"   => "StaleTier::Clear",
            "REFUSE"  => "StaleTier::Refuse",
            other => panic!("reserve_trees.h: unknown tier `{other}` for {}", p[0]),
        };
        rows.push_str(&format!(
            "    ReserveTree {{ field: \"{}\", kind: {}, ksz: {}, vsz: {}, \
             tier: {tier}, consequence: {} }},\n",
            p[0].trim(), resolve(&p[1]), resolve(&p[2]), resolve(&p[3]), p[5].trim()));
        fields.push(p[0].trim().to_string());
        n += 1;
    }
    assert!(n > 0, "reserve_trees.h: parsed zero rows");

    // Also generate the accessor dispatch. Without it every consumer would
    // hand-map field name -> tessera_volume_*() and could omit a tree there
    // instead — moving the bug rather than removing it. Arms come from the
    // same rows, so the mapping cannot be short.
    let mut arms = String::new();
    for f in &fields {
        arms.push_str(&format!(
            "        \"{f}\" => tessera_volume_{f}(v),\n"));
    }

    std::fs::write(&out, format!(
        "// @generated by tessera-sys/build.rs from tessera/reserve_trees.h — do not edit.\n\
         pub const RESERVE_TREES: &[ReserveTree] = &[\n{rows}];\n\
         \n\
         /// Read this tree's root out of the superblock.\n\
         ///\n\
         /// # Safety\n\
         /// `v` must be a live volume handle from tessera_volume_open.\n\
         pub unsafe fn reserve_tree_root(v: *const tessera_volume_t, t: &ReserveTree) -> u64 {{\n\
         \x20   match t.field {{\n{arms}\
         \x20       // Unreachable: arms and rows are generated together.\n\
         \x20       _ => 0,\n\
         \x20   }}\n\
         }}\n")).unwrap();
}

/// Return (row arguments, remainder after the row's closing paren).
fn split_row(s: &str) -> (String, &str) {
    let (mut depth, mut in_str) = (1i32, false);
    for (i, c) in s.char_indices() {
        match c {
            '"' => in_str = !in_str,
            '(' if !in_str => depth += 1,
            ')' if !in_str => {
                depth -= 1;
                if depth == 0 { return (s[..i].to_string(), &s[i + 1..]); }
            }
            _ => {}
        }
    }
    panic!("reserve_trees.h: unterminated X( row");
}

/// Split on commas outside strings and nested parens — the consequence column
/// is prose and may contain both.
fn split_args(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut depth, mut in_str) = (Vec::new(), String::new(), 0i32, false);
    for c in s.chars() {
        match c {
            '"' => { in_str = !in_str; cur.push(c); }
            '(' if !in_str => { depth += 1; cur.push(c); }
            ')' if !in_str => { depth -= 1; cur.push(c); }
            ',' if !in_str && depth == 0 => { out.push(cur.trim().to_string()); cur.clear(); }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() { out.push(cur.trim().to_string()); }
    out
}
