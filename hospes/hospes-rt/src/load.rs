//! Map, relocate and initialise foreign shared objects (docs/spec/hospes.md §5.2): one
//! library, or a library with the libraries it needs ([`load_set`]).
//!
//! Order: parse (in `hospes-elf`) → reserve the image span → map each `PT_LOAD` with its own
//! permissions (never write+execute) → register the TLS template and give the main thread
//! its control block → apply `DT_RELR`, then RELA relocations, then `IRELATIVE` and
//! ifunc-valued symbols last (their resolvers may read the GOT and the canary) → make
//! `PT_GNU_RELRO` read-only. Initialisers run separately ([`Library::run_init`]) so the host
//! can finish its own setup first.
use crate::sys::{self, page_down, page_up};
use crate::tls;
use alloc::string::String;
use alloc::vec::Vec;
use hospes_elf::{Bind, Image, Machine, RelocKind, SymKind};

#[derive(Debug)]
pub enum LoadError {
    Io(&'static str, sys::Errno),
    Elf(hospes_elf::Error),
    /// Imports with no definition (strong references only), as `name@version`.
    Unresolved(Vec<String>),
    Reloc(&'static str, u64),
    Tls(&'static str),
}

/// Where imports come from: the shim table in production, a test table in fixtures.
pub trait Resolver {
    /// Address of `name` at exactly `version` (None: an unversioned import).
    fn resolve(&self, name: &str, version: Option<&str>) -> Option<usize>;
}

pub struct Options {
    /// Linux-shaped `(AT_HWCAP, AT_HWCAP2)` for ifunc resolvers.
    pub hwcap: (usize, usize),
    /// Development only: bind unknown strong imports to a stub that aborts when called,
    /// instead of refusing the load (spec §6.4).
    pub allow_unknown: bool,
    /// Trap system calls made from the library's code (K1, [`crate::trap`]): register its
    /// executable segments and lock the table before any of its code runs. Needs
    /// [`crate::trap::init`] first. On the Linux development backend a missing facility
    /// (qemu-user has no syscall user dispatch) is a warning, on FreeBSD an error.
    pub trap: bool,
}

pub struct Library {
    pub base: usize,
    pub map_len: usize,
    pub img: Image,
    pub hwcap: (usize, usize),
    /// Unknown imports bound to the abort stub (only with `allow_unknown`).
    pub stubbed: Vec<String>,
    /// The library's TLS module (0-based), if it has `PT_TLS`.
    pub tls_module: Option<usize>,
}

#[cfg(target_arch = "x86_64")]
const MACHINE: Machine = Machine::X86_64;
#[cfg(target_arch = "aarch64")]
const MACHINE: Machine = Machine::Aarch64;

extern "C" fn unknown_import_called() -> ! {
    crate::hlog!("hospes: the library called an import that has no implementation");
    sys::exit(134)
}

/// aarch64 glibc passes `(hwcap | _IFUNC_ARG_HWCAP, &__ifunc_arg_t)` to resolvers.
#[repr(C)]
#[cfg(target_arch = "aarch64")]
struct IfuncArg {
    size: u64,
    hwcap: u64,
    hwcap2: u64,
}

unsafe fn call_resolver(addr: usize, hwcap: (usize, usize)) -> usize {
    #[cfg(target_arch = "aarch64")]
    {
        let arg = IfuncArg { size: 24, hwcap: hwcap.0 as u64, hwcap2: hwcap.1 as u64 };
        let f: extern "C" fn(u64, *const IfuncArg) -> usize = core::mem::transmute(addr);
        f(hwcap.0 as u64 | (1u64 << 62), &arg)
    }
    #[cfg(target_arch = "x86_64")]
    {
        // x86_64 resolvers take no arguments (they read CPUID); pass hwcap anyway as gcc's
        // __builtin_cpu_supports-style resolvers ignore it.
        let f: extern "C" fn(u64) -> usize = core::mem::transmute(addr);
        f(hwcap.0 as u64)
    }
}

fn file_size(fd: i32) -> Result<usize, LoadError> {
    let n = sys::lseek(fd, 0, 2).map_err(|e| LoadError::Io("lseek", e))?;
    usize::try_from(n).map_err(|_| LoadError::Io("size", sys::Errno(27)))
}

fn prot(f: hospes_elf::Flags) -> usize {
    (if f.r { sys::PROT_READ } else { 0 }) | (if f.w { sys::PROT_WRITE } else { 0 }) | (if f.x { sys::PROT_EXEC } else { 0 })
}

/// Load the shared object open at `fd` (read-only is enough).
pub fn load(fd: i32, opts: &Options, res: &dyn Resolver) -> Result<Library, LoadError> {
    let mut v = load_set(&[fd], opts, res)?;
    Ok(v.pop().expect("one library"))
}

/// Load several shared objects into one scope, dependencies first (`[libstdc++, module]`).
/// Every object is mapped and its TLS registered before the main thread gets its control
/// block; then each is relocated in order, its imports bound to its own exports, then to
/// the exports of the objects before it, then to `res`. Only once per process: TLS
/// modules can't be added after the first thread control block exists.
pub fn load_set(fds: &[i32], opts: &Options, res: &dyn Resolver) -> Result<Vec<Library>, LoadError> {
    let mut libs: Vec<Library> = Vec::with_capacity(fds.len());
    let mut r = Ok(());
    for &fd in fds {
        match map_one(fd, opts) {
            Ok(l) => libs.push(l),
            Err(e) => {
                r = Err(e);
                break;
            }
        }
    }
    if r.is_ok() {
        r = link_set(&mut libs, opts, res);
    }
    if let Err(e) = r {
        for l in &libs {
            let _ = sys::munmap(l.base + l.img.span.0 as usize, l.map_len);
        }
        return Err(e);
    }
    Ok(libs)
}

/// Parse, reserve and map one object, and register its TLS.
fn map_one(fd: i32, opts: &Options) -> Result<Library, LoadError> {
    let size = file_size(fd)?;
    if size == 0 {
        return Err(LoadError::Elf(hospes_elf::Error::Truncated("empty file")));
    }
    let file = sys::mmap(0, size, sys::PROT_READ, sys::MAP_PRIVATE, fd, 0).map_err(|e| LoadError::Io("mmap file", e))?;
    let bytes = unsafe { core::slice::from_raw_parts(file as *const u8, size) };
    let parsed = hospes_elf::parse(bytes, Some(MACHINE));
    let _ = sys::munmap(file, size);
    let img = parsed.map_err(LoadError::Elf)?;

    // Reserve the span, aligned to the largest PT_LOAD alignment we honour (64 KiB on arm64).
    let (lo, hi) = (img.span.0 as usize, img.span.1 as usize);
    let len = hi - lo;
    let resv = sys::mmap_aligned(len, 64 * 1024, sys::PROT_NONE).map_err(|e| LoadError::Io("reserve", e))?;
    let base = resv - lo;
    let mut lib = Library { base, map_len: len, img, hwcap: opts.hwcap, stubbed: Vec::new(), tls_module: None };
    let r = map_segments(fd, &mut lib);
    if r.is_err() {
        let _ = sys::munmap(resv, len);
    }
    r.map(|_| lib)
}

fn map_segments(fd: i32, lib: &mut Library) -> Result<(), LoadError> {
    let base = lib.base;
    for s in &lib.img.segments {
        let va = base + s.vaddr as usize;
        let map_start = page_down(va);
        let file_end = va + s.filesz as usize;
        let mem_end = va + s.memsz as usize;
        let p = prot(s.flags);
        if s.filesz > 0 {
            let off = page_down(s.offset as usize) as u64;
            sys::mmap(map_start, page_up(file_end) - map_start, p, sys::MAP_PRIVATE | sys::MAP_FIXED, fd, off)
                .map_err(|e| LoadError::Io("map segment", e))?;
        }
        if mem_end > file_end {
            // Zero the tail of the last file page, then map anonymous pages for the rest.
            let tail_end = page_up(file_end).min(mem_end);
            if s.filesz > 0 && tail_end > file_end {
                if !s.flags.w {
                    sys::mprotect(page_down(file_end), sys::PAGE, sys::PROT_READ | sys::PROT_WRITE)
                        .map_err(|e| LoadError::Io("mprotect bss", e))?;
                }
                unsafe { core::ptr::write_bytes(file_end as *mut u8, 0, page_up(file_end) - file_end) };
                if !s.flags.w {
                    sys::mprotect(page_down(file_end), sys::PAGE, p).map_err(|e| LoadError::Io("mprotect bss", e))?;
                }
            }
            let anon = if s.filesz > 0 { page_up(file_end) } else { map_start };
            if page_up(mem_end) > anon {
                sys::mmap(anon, page_up(mem_end) - anon, p, sys::MAP_PRIVATE | sys::MAP_FIXED | sys::MAP_ANON, -1, 0)
                    .map_err(|e| LoadError::Io("map bss", e))?;
            }
        }
    }
    // TLS: each module's block sits in one static block per thread.
    if let Some(t) = &lib.img.tls {
        let m = tls::add_module(tls::Template {
            image: base + t.vaddr as usize,
            filesz: t.filesz as usize,
            memsz: t.memsz as usize,
            align: t.align as usize,
        })
        .ok_or(LoadError::Tls("too many TLS modules, or one after the first thread"))?;
        lib.tls_module = Some(m);
    }
    Ok(())
}

fn link_set(libs: &mut [Library], opts: &Options, res: &dyn Resolver) -> Result<(), LoadError> {
    // Trap the libraries' own system calls before anything of them runs: relocation runs
    // IRELATIVE resolvers, which are library code.
    if opts.trap {
        if let Err(e) = trap_segments(libs) {
            if cfg!(target_os = "freebsd") {
                return Err(LoadError::Io("trap", e));
            }
            crate::hlog!("hospes: no system call trap on this host ({:?}); the library's raw calls reach the kernel", e);
        }
    }
    tls::init_process_secrets();
    tls::install_main().map_err(|e| LoadError::Io("main thread TCB", e))?;

    for i in 0..libs.len() {
        let (deps, rest) = libs.split_at_mut(i);
        let lib = &mut rest[0];
        relocate(lib, deps, opts, res)?;
        if let Some((a, l)) = lib.img.relro {
            let start = page_down(lib.base + a as usize);
            let end = page_down(lib.base + (a + l) as usize);
            if end > start {
                sys::mprotect(start, end - start, sys::PROT_READ).map_err(|e| LoadError::Io("relro", e))?;
            }
        }
    }
    Ok(())
}

fn trap_segments(libs: &[Library]) -> sys::R<()> {
    for lib in libs {
        for (start, end) in lib.text() {
            crate::trap::add(start, end - start)?;
        }
    }
    crate::trap::lock()
}

/// What a symbol index resolves to.
enum Val {
    Addr(usize),
    /// Defined ifunc: call the resolver at this address (deferred to the last pass).
    Ifunc(usize),
    /// Offset in a TLS module (0-based module, offset).
    Tls(usize, u64),
}

fn resolve_all(lib: &mut Library, deps: &[Library], opts: &Options, res: &dyn Resolver) -> Result<Vec<Option<Val>>, LoadError> {
    let base = lib.base;
    let img = &lib.img;
    let mut out = Vec::with_capacity(img.symbols.len());
    let mut missing = Vec::new();
    let mut stubbed = Vec::new();
    // Only symbols a relocation names need resolving.
    let mut used = alloc::vec![false; img.symbols.len()];
    for r in &img.relocs {
        if let Some(u) = used.get_mut(r.sym as usize) {
            *u = true;
        }
    }
    for (i, s) in img.symbols.iter().enumerate() {
        if i == 0 || !used[i] {
            out.push(None);
            continue;
        }
        let own_tls = || lib.tls_module.ok_or(LoadError::Tls("TLS symbol without PT_TLS"));
        let v = if s.defined {
            match s.kind {
                SymKind::Tls => Val::Tls(own_tls()?, s.value),
                SymKind::Ifunc => Val::Ifunc(base + s.value as usize),
                _ => Val::Addr(base + s.value as usize),
            }
        } else if let Some(d) = img.export(&s.name, s.version.as_deref()) {
            // The library's own definition first (glibc's global scope order).
            match d.kind {
                SymKind::Tls => Val::Tls(own_tls()?, d.value),
                SymKind::Ifunc => Val::Ifunc(base + d.value as usize),
                _ => Val::Addr(base + d.value as usize),
            }
        } else if let Some(v) = from_deps(deps, &s.name, s.version.as_deref())? {
            // Then the libraries loaded before it, in order.
            v
        } else if s.kind == SymKind::Tls {
            return Err(LoadError::Tls("TLS import with no defining module"));
        } else if let Some(a) = res.resolve(&s.name, s.version.as_deref()) {
            Val::Addr(a)
        } else if s.bind == Bind::Weak {
            Val::Addr(0)
        } else {
            let mut n = s.name.clone();
            if let Some(v) = &s.version {
                n.push('@');
                n.push_str(v);
            }
            if opts.allow_unknown {
                stubbed.push(n);
                Val::Addr(unknown_import_called as *const () as usize)
            } else {
                missing.push(n);
                Val::Addr(0)
            }
        };
        out.push(Some(v));
    }
    if !missing.is_empty() {
        return Err(LoadError::Unresolved(missing));
    }
    lib.stubbed = stubbed;
    Ok(out)
}

/// An export of an already relocated library (its ifuncs can run).
fn from_deps(deps: &[Library], name: &str, version: Option<&str>) -> Result<Option<Val>, LoadError> {
    for d in deps {
        let Some(e) = d.img.export(name, version) else { continue };
        let a = d.base + e.value as usize;
        return Ok(Some(match e.kind {
            SymKind::Tls => Val::Tls(d.tls_module.ok_or(LoadError::Tls("TLS export without PT_TLS"))?, e.value),
            SymKind::Ifunc => Val::Addr(unsafe { call_resolver(a, d.hwcap) }),
            _ => Val::Addr(a),
        }));
    }
    Ok(None)
}

fn writable(img: &Image, off: u64) -> bool {
    img.segments.iter().any(|s| s.flags.w && off >= s.vaddr && off + 8 <= s.vaddr + s.memsz)
}

unsafe fn put(base: usize, off: u64, v: u64) {
    core::ptr::write_unaligned((base + off as usize) as *mut u64, v);
}

fn relocate(lib: &mut Library, deps: &[Library], opts: &Options, res: &dyn Resolver) -> Result<(), LoadError> {
    let vals = resolve_all(lib, deps, opts, res)?;
    let base = lib.base;
    let img = &lib.img;
    let hw = lib.hwcap;
    let own = lib.tls_module;
    for &a in &img.relr {
        if !writable(img, a) {
            return Err(LoadError::Reloc("RELR target not writable", a));
        }
        unsafe {
            let p = (base + a as usize) as *mut u64;
            core::ptr::write_unaligned(p, core::ptr::read_unaligned(p).wrapping_add(base as u64));
        }
    }
    let mut deferred = Vec::new();
    for (i, r) in img.relocs.iter().enumerate() {
        if !writable(img, r.offset) {
            return Err(LoadError::Reloc("relocation target not writable (text relocation)", r.offset));
        }
        let a = r.addend as u64;
        let sym = vals.get(r.sym as usize).and_then(|v| v.as_ref());
        let s_addr = |what| -> Result<Option<u64>, LoadError> {
            match sym {
                None if r.sym == 0 => Ok(Some(0)),
                None => Err(LoadError::Reloc(what, r.offset)),
                Some(Val::Addr(x)) => Ok(Some(*x as u64)),
                Some(Val::Ifunc(_)) => Ok(None),
                Some(Val::Tls(..)) => Err(LoadError::Reloc("TLS symbol in a non-TLS relocation", r.offset)),
            }
        };
        // (module, offset): symbol 0 means the library's own module (local-dynamic).
        let s_tls = || -> Result<(usize, u64), LoadError> {
            match sym {
                None if r.sym == 0 => Ok((own.ok_or(LoadError::Tls("TLS relocation without PT_TLS"))?, 0)),
                Some(Val::Tls(m, v)) => Ok((*m, *v)),
                _ => Err(LoadError::Reloc("non-TLS symbol in a TLS relocation", r.offset)),
            }
        };
        unsafe {
            match r.kind {
                RelocKind::None => {}
                RelocKind::Relative => put(base, r.offset, (base as u64).wrapping_add(a)),
                RelocKind::Abs64 => match s_addr("ABS64 symbol")? {
                    Some(s) => put(base, r.offset, s.wrapping_add(a)),
                    None => deferred.push(i),
                },
                RelocKind::GlobDat | RelocKind::JumpSlot => match s_addr("GOT symbol")? {
                    // x86_64 ignores the addend for these; aarch64 adds it.
                    Some(s) => put(base, r.offset, if MACHINE == Machine::Aarch64 { s.wrapping_add(a) } else { s }),
                    None => deferred.push(i),
                },
                RelocKind::DtpMod => put(base, r.offset, s_tls()?.0 as u64 + 1),
                RelocKind::DtpOff => put(base, r.offset, s_tls()?.1.wrapping_add(a)),
                RelocKind::TpOff => {
                    let (m, v) = s_tls()?;
                    put(base, r.offset, tls::tpoff_in(m, v.wrapping_add(a)))
                }
                RelocKind::TlsDesc => {
                    let (m, v) = s_tls()?;
                    if !writable(img, r.offset + 8) {
                        return Err(LoadError::Reloc("TLSDESC second word not writable", r.offset));
                    }
                    put(base, r.offset, tls::hospes_tlsdesc_static as *const () as usize as u64);
                    put(base, r.offset + 8, tls::tpoff_in(m, v.wrapping_add(a)));
                }
                RelocKind::IRelative => deferred.push(i),
            }
        }
    }
    // Last pass: ifuncs. Every other relocation is in place, as is the canary.
    for i in deferred {
        let r = &img.relocs[i];
        let a = r.addend as u64;
        unsafe {
            let v = match r.kind {
                RelocKind::IRelative => call_resolver(base + a as usize, hw) as u64,
                _ => {
                    let Some(Some(Val::Ifunc(res_addr))) = vals.get(r.sym as usize) else {
                        return Err(LoadError::Reloc("deferred relocation without ifunc", r.offset));
                    };
                    let s = call_resolver(*res_addr, hw) as u64;
                    match r.kind {
                        RelocKind::Abs64 => s.wrapping_add(a),
                        _ if MACHINE == Machine::Aarch64 => s.wrapping_add(a),
                        _ => s,
                    }
                }
            };
            put(base, r.offset, v);
        }
    }
    Ok(())
}

impl Library {
    /// The library's executable segments as page-aligned `[start, end)` ranges.
    pub fn text(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.img.segments.iter().filter(|s| s.flags.x).map(|s| {
            let va = self.base + s.vaddr as usize;
            (page_down(va), page_up(va + s.memsz as usize))
        })
    }

    /// Address of an exported symbol (ifuncs resolved).
    pub fn sym(&self, name: &str) -> Option<usize> {
        let s = self.img.export(name, None)?;
        let a = self.base + s.value as usize;
        Some(if s.kind == SymKind::Ifunc { unsafe { call_resolver(a, self.hwcap) } } else { a })
    }

    /// Run `DT_INIT`, then `DT_INIT_ARRAY` in order, with glibc's `(argc, argv, envp)`.
    pub unsafe fn run_init(&self, argc: usize, argv: *const *const u8, envp: *const *const u8) {
        type Init = extern "C" fn(i32, *const *const u8, *const *const u8);
        if let Some(i) = self.img.init {
            let f: Init = core::mem::transmute(self.base + i as usize);
            f(argc as i32, argv, envp);
        }
        if let Some((a, n)) = self.img.init_array {
            for k in 0..n as usize {
                let e = core::ptr::read((self.base + a as usize + k * 8) as *const usize);
                if e == 0 || e == usize::MAX {
                    continue;
                }
                let f: Init = core::mem::transmute(e);
                f(argc as i32, argv, envp);
            }
        }
    }

    /// Run `DT_FINI_ARRAY` in reverse, then `DT_FINI` (orderly shutdown only).
    pub unsafe fn run_fini(&self) {
        if let Some((a, n)) = self.img.fini_array {
            for k in (0..n as usize).rev() {
                let e = core::ptr::read((self.base + a as usize + k * 8) as *const usize);
                if e == 0 || e == usize::MAX {
                    continue;
                }
                let f: extern "C" fn() = core::mem::transmute(e);
                f();
            }
        }
        if let Some(i) = self.img.fini {
            let f: extern "C" fn() = core::mem::transmute(self.base + i as usize);
            f();
        }
    }
}
