//! Safe, `no_std` ELF64 parser for Hospes (docs/spec/hospes.md §5.1).
//!
//! It reads a shared object from its file bytes and returns a [`Image`]: what to map, what
//! to relocate, what it imports (with symbol versions) and what it exports. It never maps or
//! writes memory; `hospes-rt` does that from the plan. Everything is bounds-checked, and
//! anything outside the accepted subset is an error, never a guess:
//!
//! - ELF64, little-endian, `ET_DYN`, machine `EM_X86_64` or `EM_AARCH64`;
//! - no `PT_INTERP` (we load libraries, not programs), no text relocations;
//! - `PT_LOAD` alignment a multiple of the page size, offsets congruent to addresses;
//! - RELA relocations of the types in [`RelocKind`], plus `DT_RELR`.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;

/// Page size Hospes maps with (FreeBSD arm64 and amd64 both use 4 KiB).
pub const PAGE: u64 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Machine {
    X86_64,
    Aarch64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Truncated(&'static str),
    BadMagic,
    NotElf64Le,
    NotSharedObject,
    WrongMachine { found: u16 },
    HasInterp,
    TextRel,
    BadSegment(&'static str),
    BadDynamic(&'static str),
    UnsupportedReloc { machine: Machine, rtype: u32 },
    NotRela,
    BadSymbol(&'static str),
    BadVersion(&'static str),
    TooLarge(&'static str),
}

/// Limits on what we accept. A real CDM is ~21 MiB with ~5,000 relocations and ~1,700
/// symbols; these are generous multiples.
pub const MAX_PHDRS: usize = 64;
pub const MAX_SYMBOLS: usize = 200_000;
pub const MAX_RELOCS: usize = 2_000_000;
pub const MAX_SPAN: u64 = 1 << 32; // 4 GiB of address space for one image
pub const MAX_NEEDED: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flags {
    pub r: bool,
    pub w: bool,
    pub x: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub vaddr: u64,
    pub memsz: u64,
    pub offset: u64,
    pub filesz: u64,
    pub flags: Flags,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tls {
    /// Template address (relative to load base), initialised bytes, total size, alignment.
    pub vaddr: u64,
    pub filesz: u64,
    pub memsz: u64,
    pub align: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymKind {
    NoType,
    Object,
    Func,
    Section,
    File,
    Common,
    Tls,
    Ifunc,
    Other(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bind {
    Local,
    Global,
    Weak,
    Unique,
    Other(u8),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub value: u64,
    pub size: u64,
    pub kind: SymKind,
    pub bind: Bind,
    /// `false` for an import (`SHN_UNDEF`).
    pub defined: bool,
    /// Version name: for an import, from `DT_VERNEED`; for a definition, from `DT_VERDEF`.
    pub version: Option<String>,
    /// For an import, the file that should provide it (`DT_VERNEED` entry's file name).
    pub version_file: Option<String>,
    /// `VERSYM_HIDDEN`: a non-default version of a definition (`name@V` rather than `name@@V`).
    pub hidden: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelocKind {
    None,
    /// B + A
    Relative,
    /// S + A (`R_X86_64_64`, `R_AARCH64_ABS64`)
    Abs64,
    /// S (+A on aarch64)
    GlobDat,
    /// S (+A on aarch64)
    JumpSlot,
    /// module id of S's TLS module
    DtpMod,
    /// S + A relative to the module's TLS block
    DtpOff,
    /// S + A relative to the thread pointer (static TLS)
    TpOff,
    /// TLS descriptor: two words, resolver + argument
    TlsDesc,
    /// call (B + A)(hwcap...) and store the result
    IRelative,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reloc {
    pub offset: u64,
    pub kind: RelocKind,
    /// Index into [`Image::symbols`], 0 = none.
    pub sym: u32,
    pub addend: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Image {
    pub machine: Option<Machine>,
    pub segments: Vec<Segment>,
    /// Lowest and highest (exclusive) virtual address over all `PT_LOAD`, page-rounded.
    pub span: (u64, u64),
    pub tls: Option<Tls>,
    pub relro: Option<(u64, u64)>,
    pub eh_frame_hdr: Option<(u64, u64)>,
    pub stack_exec: bool,
    pub soname: Option<String>,
    pub needed: Vec<String>,
    pub bind_now: bool,
    pub init: Option<u64>,
    pub fini: Option<u64>,
    pub init_array: Option<(u64, u64)>,
    pub fini_array: Option<(u64, u64)>,
    pub preinit_array: bool,
    /// Every dynamic symbol, index 0 is the null symbol.
    pub symbols: Vec<Symbol>,
    /// RELA relocations: `DT_RELA` then `DT_JMPREL`.
    pub relocs: Vec<Reloc>,
    /// Addresses (relative to base) to which `DT_RELR` applies a `Relative` fixup.
    pub relr: Vec<u64>,
    /// Where the dynamic section itself lives (relative address and size).
    pub dynamic: (u64, u64),
}

impl Image {
    /// Symbols this image imports (undefined, named, global or weak).
    pub fn imports(&self) -> impl Iterator<Item = (usize, &Symbol)> {
        self.symbols.iter().enumerate().filter(|(i, s)| *i != 0 && !s.defined && !s.name.is_empty())
    }
    /// Symbols this image exports (defined, global/weak/unique, not hidden versions).
    pub fn exports(&self) -> impl Iterator<Item = (usize, &Symbol)> {
        self.symbols.iter().enumerate().filter(|(i, s)| {
            *i != 0 && s.defined && !s.name.is_empty() && matches!(s.bind, Bind::Global | Bind::Weak | Bind::Unique)
        })
    }
    /// Find an exported symbol by name (and, if given, version).
    pub fn export(&self, name: &str, version: Option<&str>) -> Option<&Symbol> {
        let mut fallback = None;
        for (_, s) in self.exports() {
            if s.name != name {
                continue;
            }
            match (version, &s.version) {
                (Some(v), Some(sv)) if v == sv => return Some(s),
                (Some(_), _) => {}
                (None, _) if !s.hidden => return Some(s),
                (None, _) => fallback = fallback.or(Some(s)),
            }
        }
        if version.is_none() { fallback } else { None }
    }
}

// ---------------------------------------------------------------------------------------
// Byte reading

struct R<'a> {
    d: &'a [u8],
}

impl<'a> R<'a> {
    fn bytes(&self, off: u64, len: u64, what: &'static str) -> Result<&'a [u8], Error> {
        let off = usize::try_from(off).map_err(|_| Error::Truncated(what))?;
        let len = usize::try_from(len).map_err(|_| Error::Truncated(what))?;
        let end = off.checked_add(len).ok_or(Error::Truncated(what))?;
        self.d.get(off..end).ok_or(Error::Truncated(what))
    }
    fn u8(&self, off: u64, what: &'static str) -> Result<u8, Error> {
        Ok(self.bytes(off, 1, what)?[0])
    }
    fn u16(&self, off: u64, what: &'static str) -> Result<u16, Error> {
        let b = self.bytes(off, 2, what)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&self, off: u64, what: &'static str) -> Result<u32, Error> {
        let b = self.bytes(off, 4, what)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&self, off: u64, what: &'static str) -> Result<u64, Error> {
        let b = self.bytes(off, 8, what)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }
    fn cstr(&self, off: u64, what: &'static str) -> Result<String, Error> {
        let start = usize::try_from(off).map_err(|_| Error::Truncated(what))?;
        let rest = self.d.get(start..).ok_or(Error::Truncated(what))?;
        let n = rest.iter().take(4096).position(|&c| c == 0).ok_or(Error::Truncated(what))?;
        core::str::from_utf8(&rest[..n]).map(String::from).map_err(|_| Error::BadSymbol("name not UTF-8"))
    }
}

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
const PT_TLS: u32 = 7;
const PT_GNU_EH_FRAME: u32 = 0x6474_e550;
const PT_GNU_STACK: u32 = 0x6474_e551;
const PT_GNU_RELRO: u32 = 0x6474_e552;

const DT_NULL: u64 = 0;
const DT_NEEDED: u64 = 1;
const DT_PLTRELSZ: u64 = 2;
const DT_HASH: u64 = 4;
const DT_STRTAB: u64 = 5;
const DT_SYMTAB: u64 = 6;
const DT_RELA: u64 = 7;
const DT_RELASZ: u64 = 8;
const DT_RELAENT: u64 = 9;
const DT_STRSZ: u64 = 10;
const DT_SYMENT: u64 = 11;
const DT_INIT: u64 = 12;
const DT_FINI: u64 = 13;
const DT_SONAME: u64 = 14;
const DT_REL: u64 = 17;
const DT_PLTREL: u64 = 20;
const DT_TEXTREL: u64 = 22;
const DT_JMPREL: u64 = 23;
const DT_BIND_NOW: u64 = 24;
const DT_INIT_ARRAY: u64 = 25;
const DT_FINI_ARRAY: u64 = 26;
const DT_INIT_ARRAYSZ: u64 = 27;
const DT_FINI_ARRAYSZ: u64 = 28;
const DT_FLAGS: u64 = 30;
const DT_PREINIT_ARRAY: u64 = 32;
const DT_RELRSZ: u64 = 35;
const DT_RELR: u64 = 36;
const DT_RELRENT: u64 = 37;
const DT_GNU_HASH: u64 = 0x6fff_fef5;
const DT_VERSYM: u64 = 0x6fff_fff0;
const DT_FLAGS_1: u64 = 0x6fff_fffb;
const DT_VERDEF: u64 = 0x6fff_fffc;
const DT_VERDEFNUM: u64 = 0x6fff_fffd;
const DT_VERNEED: u64 = 0x6fff_fffe;
const DT_VERNEEDNUM: u64 = 0x6fff_ffff;

const DF_TEXTREL: u64 = 0x4;
const DF_BIND_NOW: u64 = 0x8;
const DF_1_NOW: u64 = 0x1;

/// Map a relocation type to its meaning for this machine.
pub fn reloc_kind(m: Machine, t: u32) -> Result<RelocKind, Error> {
    use RelocKind::*;
    Ok(match (m, t) {
        (_, 0) => None,
        (Machine::X86_64, 1) => Abs64,
        (Machine::X86_64, 6) => GlobDat,
        (Machine::X86_64, 7) => JumpSlot,
        (Machine::X86_64, 8) => Relative,
        (Machine::X86_64, 16) => DtpMod,
        (Machine::X86_64, 17) => DtpOff,
        (Machine::X86_64, 18) => TpOff,
        (Machine::X86_64, 36) => TlsDesc,
        (Machine::X86_64, 37) => IRelative,
        (Machine::Aarch64, 257) => Abs64,
        (Machine::Aarch64, 1025) => GlobDat,
        (Machine::Aarch64, 1026) => JumpSlot,
        (Machine::Aarch64, 1027) => Relative,
        (Machine::Aarch64, 1028) => DtpMod,
        (Machine::Aarch64, 1029) => DtpOff,
        (Machine::Aarch64, 1030) => TpOff,
        (Machine::Aarch64, 1031) => TlsDesc,
        (Machine::Aarch64, 1032) => IRelative,
        (machine, rtype) => return Err(Error::UnsupportedReloc { machine, rtype }),
    })
}

/// Parse a shared object's file bytes.
pub fn parse(data: &[u8], expect: Option<Machine>) -> Result<Image, Error> {
    let r = R { d: data };
    if r.bytes(0, 4, "ident")? != b"\x7fELF" {
        return Err(Error::BadMagic);
    }
    if r.u8(4, "class")? != 2 || r.u8(5, "data")? != 1 || r.u8(6, "version")? != 1 {
        return Err(Error::NotElf64Le);
    }
    if r.u16(16, "e_type")? != 3 {
        return Err(Error::NotSharedObject);
    }
    let em = r.u16(18, "e_machine")?;
    let machine = match em {
        62 => Machine::X86_64,
        183 => Machine::Aarch64,
        found => return Err(Error::WrongMachine { found }),
    };
    if let Some(e) = expect {
        if e != machine {
            return Err(Error::WrongMachine { found: em });
        }
    }
    let phoff = r.u64(32, "e_phoff")?;
    let phentsize = r.u16(54, "e_phentsize")? as u64;
    let phnum = r.u16(56, "e_phnum")? as usize;
    if phentsize != 56 || phnum == 0 || phnum > MAX_PHDRS {
        return Err(Error::BadSegment("program header table"));
    }

    let mut img = Image { machine: Some(machine), ..Default::default() };
    let mut dyn_ph: Option<(u64, u64)> = None;
    let (mut lo, mut hi) = (u64::MAX, 0u64);
    for i in 0..phnum as u64 {
        let p = phoff.checked_add(i * 56).ok_or(Error::Truncated("phdr"))?;
        let p_type = r.u32(p, "p_type")?;
        let p_flags = r.u32(p + 4, "p_flags")?;
        let p_offset = r.u64(p + 8, "p_offset")?;
        let p_vaddr = r.u64(p + 16, "p_vaddr")?;
        let p_filesz = r.u64(p + 32, "p_filesz")?;
        let p_memsz = r.u64(p + 40, "p_memsz")?;
        let p_align = r.u64(p + 48, "p_align")?;
        let flags = Flags { r: p_flags & 4 != 0, w: p_flags & 2 != 0, x: p_flags & 1 != 0 };
        match p_type {
            PT_LOAD => {
                if p_filesz > p_memsz {
                    return Err(Error::BadSegment("filesz > memsz"));
                }
                if p_align < PAGE || p_align % PAGE != 0 {
                    return Err(Error::BadSegment("alignment not a multiple of the page size"));
                }
                if p_offset % PAGE != p_vaddr % PAGE {
                    return Err(Error::BadSegment("offset and address not congruent"));
                }
                if flags.w && flags.x {
                    return Err(Error::BadSegment("writable and executable"));
                }
                r.bytes(p_offset, p_filesz, "segment contents")?;
                let end = p_vaddr.checked_add(p_memsz).ok_or(Error::BadSegment("overflow"))?;
                lo = lo.min(p_vaddr & !(PAGE - 1));
                hi = hi.max(end.checked_add(PAGE - 1).ok_or(Error::BadSegment("overflow"))? & !(PAGE - 1));
                img.segments.push(Segment { vaddr: p_vaddr, memsz: p_memsz, offset: p_offset, filesz: p_filesz, flags });
            }
            PT_DYNAMIC => dyn_ph = Some((p_vaddr, p_filesz)),
            PT_INTERP => return Err(Error::HasInterp),
            PT_TLS => {
                if p_filesz > p_memsz || p_align > PAGE {
                    return Err(Error::BadSegment("TLS"));
                }
                img.tls = Some(Tls { vaddr: p_vaddr, filesz: p_filesz, memsz: p_memsz, align: p_align.max(1) });
            }
            PT_GNU_RELRO => img.relro = Some((p_vaddr, p_memsz)),
            PT_GNU_EH_FRAME => img.eh_frame_hdr = Some((p_vaddr, p_memsz)),
            PT_GNU_STACK => img.stack_exec = flags.x,
            _ => {}
        }
    }
    if img.segments.is_empty() {
        return Err(Error::BadSegment("no PT_LOAD"));
    }
    if hi - lo > MAX_SPAN {
        return Err(Error::TooLarge("image span"));
    }
    // Segments must be in address order and not overlap.
    for w in img.segments.windows(2) {
        if w[0].vaddr + w[0].memsz > w[1].vaddr {
            return Err(Error::BadSegment("PT_LOAD out of order or overlapping"));
        }
    }
    if img.stack_exec {
        return Err(Error::BadSegment("executable stack requested"));
    }
    img.span = (lo, hi);
    if let Some(t) = &img.tls {
        t.vaddr.checked_add(t.memsz).ok_or(Error::BadSegment("TLS"))?;
        let inside = img.segments.iter().any(|s| t.vaddr >= s.vaddr && t.vaddr + t.filesz <= s.vaddr + s.filesz);
        if !inside {
            return Err(Error::BadSegment("TLS template outside a loaded segment"));
        }
    }
    let (dyn_va, dyn_sz) = dyn_ph.ok_or(Error::BadDynamic("no PT_DYNAMIC"))?;
    img.dynamic = (dyn_va, dyn_sz);
    parse_dynamic(&r, &mut img, machine, dyn_va, dyn_sz)?;
    Ok(img)
}

/// Translate a virtual address (relative to base) to a file offset, for `len` bytes that
/// must be backed by file contents.
fn va2off(img: &Image, va: u64, len: u64) -> Result<u64, Error> {
    for s in &img.segments {
        if va >= s.vaddr && va.checked_add(len).map_or(false, |e| e <= s.vaddr + s.filesz) {
            return Ok(s.offset + (va - s.vaddr));
        }
    }
    Err(Error::BadDynamic("address not backed by file"))
}

fn parse_dynamic(r: &R, img: &mut Image, m: Machine, dyn_va: u64, dyn_sz: u64) -> Result<(), Error> {
    let base = va2off(img, dyn_va, dyn_sz)?;
    let n = dyn_sz / 16;
    let mut entries: Vec<(u64, u64)> = Vec::new();
    for i in 0..n {
        let tag = r.u64(base + i * 16, "d_tag")?;
        let val = r.u64(base + i * 16 + 8, "d_val")?;
        if tag == DT_NULL {
            break;
        }
        entries.push((tag, val));
    }
    let get = |t: u64| entries.iter().find(|e| e.0 == t).map(|e| e.1);
    if get(DT_TEXTREL).is_some() || get(DT_FLAGS).map_or(false, |f| f & DF_TEXTREL != 0) {
        return Err(Error::TextRel);
    }
    if get(DT_REL).is_some() {
        return Err(Error::NotRela);
    }
    img.bind_now = get(DT_BIND_NOW).is_some()
        || get(DT_FLAGS).map_or(false, |f| f & DF_BIND_NOW != 0)
        || get(DT_FLAGS_1).map_or(false, |f| f & DF_1_NOW != 0);

    let strtab = get(DT_STRTAB).ok_or(Error::BadDynamic("no DT_STRTAB"))?;
    let strsz = get(DT_STRSZ).ok_or(Error::BadDynamic("no DT_STRSZ"))?;
    let stroff = va2off(img, strtab, strsz)?;
    let s = |o: u64| -> Result<String, Error> {
        if o >= strsz {
            return Err(Error::BadDynamic("string offset outside DT_STRTAB"));
        }
        r.cstr(stroff + o, "string")
    };
    for (t, v) in &entries {
        match *t {
            DT_NEEDED => {
                if img.needed.len() >= MAX_NEEDED {
                    return Err(Error::TooLarge("DT_NEEDED"));
                }
                img.needed.push(s(*v)?)
            }
            DT_SONAME => img.soname = Some(s(*v)?),
            DT_INIT => img.init = Some(*v),
            DT_FINI => img.fini = Some(*v),
            DT_PREINIT_ARRAY => img.preinit_array = true,
            _ => {}
        }
    }
    if let (Some(a), Some(sz)) = (get(DT_INIT_ARRAY), get(DT_INIT_ARRAYSZ)) {
        va2off(img, a, sz).or_else(|_| in_image(img, a, sz))?;
        img.init_array = Some((a, sz / 8));
    }
    if let (Some(a), Some(sz)) = (get(DT_FINI_ARRAY), get(DT_FINI_ARRAYSZ)) {
        va2off(img, a, sz).or_else(|_| in_image(img, a, sz))?;
        img.fini_array = Some((a, sz / 8));
    }

    // Symbols.
    let symtab = get(DT_SYMTAB).ok_or(Error::BadDynamic("no DT_SYMTAB"))?;
    if get(DT_SYMENT).map_or(false, |e| e != 24) {
        return Err(Error::BadDynamic("DT_SYMENT"));
    }
    let nsyms = symbol_count(r, img, get(DT_HASH), get(DT_GNU_HASH))?;
    if nsyms > MAX_SYMBOLS {
        return Err(Error::TooLarge("symbols"));
    }
    let symoff = va2off(img, symtab, nsyms as u64 * 24)?;
    let versym = match get(DT_VERSYM) {
        Some(v) => Some(va2off(img, v, nsyms as u64 * 2)?),
        None => None,
    };
    let need = parse_verneed(r, img, &s, get(DT_VERNEED), get(DT_VERNEEDNUM))?;
    let defs = parse_verdef(r, img, &s, get(DT_VERDEF), get(DT_VERDEFNUM))?;
    for i in 0..nsyms as u64 {
        let o = symoff + i * 24;
        let name_off = r.u32(o, "st_name")? as u64;
        let info = r.u8(o + 4, "st_info")?;
        let shndx = r.u16(o + 6, "st_shndx")?;
        let value = r.u64(o + 8, "st_value")?;
        let size = r.u64(o + 16, "st_size")?;
        let name = if i == 0 { String::new() } else { s(name_off)? };
        let kind = match info & 0xf {
            0 => SymKind::NoType,
            1 => SymKind::Object,
            2 => SymKind::Func,
            3 => SymKind::Section,
            4 => SymKind::File,
            5 => SymKind::Common,
            6 => SymKind::Tls,
            10 => SymKind::Ifunc,
            k => SymKind::Other(k),
        };
        let bind = match info >> 4 {
            0 => Bind::Local,
            1 => Bind::Global,
            2 => Bind::Weak,
            10 => Bind::Unique,
            b => Bind::Other(b),
        };
        let defined = shndx != 0;
        let (mut version, mut version_file, mut hidden) = (None, None, false);
        if let Some(vs) = versym {
            let v = r.u16(vs + i * 2, "versym")?;
            hidden = v & 0x8000 != 0;
            let idx = v & 0x7fff;
            if idx >= 2 {
                if defined {
                    version = defs.iter().find(|d| d.0 == idx).map(|d| d.1.clone());
                } else if let Some(n) = need.iter().find(|n| n.0 == idx) {
                    version = Some(n.1.clone());
                    version_file = Some(n.2.clone());
                } else {
                    return Err(Error::BadVersion("versym index without a definition"));
                }
            }
        }
        img.symbols.push(Symbol { name, value, size, kind, bind, defined, version, version_file, hidden });
    }

    // Relocations.
    if let Some(a) = get(DT_RELA) {
        let sz = get(DT_RELASZ).ok_or(Error::BadDynamic("DT_RELASZ"))?;
        if get(DT_RELAENT).map_or(false, |e| e != 24) {
            return Err(Error::BadDynamic("DT_RELAENT"));
        }
        read_rela(r, img, m, a, sz, nsyms)?;
    }
    if let Some(a) = get(DT_JMPREL) {
        if get(DT_PLTREL).map_or(false, |t| t != DT_RELA) {
            return Err(Error::NotRela);
        }
        let sz = get(DT_PLTRELSZ).ok_or(Error::BadDynamic("DT_PLTRELSZ"))?;
        read_rela(r, img, m, a, sz, nsyms)?;
    }
    if let Some(a) = get(DT_RELR) {
        let sz = get(DT_RELRSZ).ok_or(Error::BadDynamic("DT_RELRSZ"))?;
        if get(DT_RELRENT).map_or(false, |e| e != 8) {
            return Err(Error::BadDynamic("DT_RELRENT"));
        }
        let off = va2off(img, a, sz)?;
        let mut where_: u64 = 0;
        for i in 0..sz / 8 {
            let e = r.u64(off + i * 8, "relr")?;
            if e & 1 == 0 {
                img.relr.push(e);
                where_ = e.wrapping_add(8);
            } else {
                let mut bits = e >> 1;
                let mut p = where_;
                while bits != 0 {
                    if bits & 1 != 0 {
                        img.relr.push(p);
                    }
                    bits >>= 1;
                    p = p.wrapping_add(8);
                }
                where_ = where_.wrapping_add(63 * 8);
            }
            if img.relr.len() > MAX_RELOCS {
                return Err(Error::TooLarge("relr"));
            }
        }
    }
    // Every relocation target must be inside a writable-at-load segment's span.
    for rl in &img.relocs {
        in_image(img, rl.offset, 8)?;
    }
    for &a in &img.relr {
        in_image(img, a, 8)?;
    }
    Ok(())
}

fn in_image(img: &Image, va: u64, len: u64) -> Result<u64, Error> {
    for s in &img.segments {
        if va >= s.vaddr && va.checked_add(len).map_or(false, |e| e <= s.vaddr + s.memsz) {
            return Ok(va);
        }
    }
    Err(Error::BadDynamic("address outside every PT_LOAD"))
}

fn read_rela(r: &R, img: &mut Image, m: Machine, a: u64, sz: u64, nsyms: usize) -> Result<(), Error> {
    let off = va2off(img, a, sz)?;
    for i in 0..sz / 24 {
        let o = off + i * 24;
        let offset = r.u64(o, "r_offset")?;
        let info = r.u64(o + 8, "r_info")?;
        let addend = r.u64(o + 16, "r_addend")? as i64;
        let sym = (info >> 32) as u32;
        if sym as usize >= nsyms.max(1) {
            return Err(Error::BadSymbol("relocation symbol index"));
        }
        let kind = reloc_kind(m, info as u32)?;
        if kind != RelocKind::None {
            img.relocs.push(Reloc { offset, kind, sym, addend });
        }
        if img.relocs.len() > MAX_RELOCS {
            return Err(Error::TooLarge("relocations"));
        }
    }
    Ok(())
}

fn symbol_count(r: &R, img: &Image, hash: Option<u64>, gnu: Option<u64>) -> Result<usize, Error> {
    if let Some(h) = hash {
        let o = va2off(img, h, 8)?;
        return Ok(r.u32(o + 4, "nchain")? as usize);
    }
    let g = gnu.ok_or(Error::BadDynamic("no DT_HASH or DT_GNU_HASH"))?;
    let o = va2off(img, g, 16)?;
    let nbuckets = r.u32(o, "gnu nbuckets")? as u64;
    let symoffset = r.u32(o + 4, "gnu symoffset")? as u64;
    let bloom = r.u32(o + 8, "gnu bloom")? as u64;
    if nbuckets > MAX_SYMBOLS as u64 || bloom > MAX_SYMBOLS as u64 {
        return Err(Error::TooLarge("gnu hash"));
    }
    let buckets = o + 16 + bloom * 8;
    let mut max = 0u64;
    for b in 0..nbuckets {
        max = max.max(r.u32(buckets + b * 4, "gnu bucket")? as u64);
    }
    if max < symoffset {
        return Ok(symoffset as usize);
    }
    let chains = buckets + nbuckets * 4;
    let mut i = max;
    loop {
        let h = r.u32(chains + (i - symoffset) * 4, "gnu chain")?;
        i += 1;
        if h & 1 != 0 {
            break;
        }
        if i - symoffset > MAX_SYMBOLS as u64 {
            return Err(Error::TooLarge("gnu chain"));
        }
    }
    Ok(i as usize)
}

/// (version index, version name, file)
fn parse_verneed(
    r: &R,
    img: &Image,
    s: &dyn Fn(u64) -> Result<String, Error>,
    at: Option<u64>,
    num: Option<u64>,
) -> Result<Vec<(u16, String, String)>, Error> {
    let mut out = Vec::new();
    let (Some(a), Some(n)) = (at, num) else { return Ok(out) };
    if n > 1024 {
        return Err(Error::TooLarge("verneed"));
    }
    let mut o = va2off(img, a, 16)?;
    for _ in 0..n {
        let cnt = r.u16(o + 2, "vn_cnt")?;
        let file = s(r.u32(o + 4, "vn_file")? as u64)?;
        let aux = r.u32(o + 8, "vn_aux")? as u64;
        let next = r.u32(o + 12, "vn_next")? as u64;
        let mut ao = o + aux;
        for _ in 0..cnt {
            let other = r.u16(ao + 6, "vna_other")?;
            let name = s(r.u32(ao + 8, "vna_name")? as u64)?;
            out.push((other & 0x7fff, name, file.clone()));
            let anext = r.u32(ao + 12, "vna_next")? as u64;
            if anext == 0 {
                break;
            }
            ao += anext;
        }
        if next == 0 {
            break;
        }
        o += next;
    }
    Ok(out)
}

/// (version index, version name) for VERDEF entries other than the file's base version.
fn parse_verdef(
    r: &R,
    img: &Image,
    s: &dyn Fn(u64) -> Result<String, Error>,
    at: Option<u64>,
    num: Option<u64>,
) -> Result<Vec<(u16, String)>, Error> {
    let mut out = Vec::new();
    let (Some(a), Some(n)) = (at, num) else { return Ok(out) };
    if n > 4096 {
        return Err(Error::TooLarge("verdef"));
    }
    let mut o = va2off(img, a, 20)?;
    for _ in 0..n {
        let flags = r.u16(o + 2, "vd_flags")?;
        let ndx = r.u16(o + 4, "vd_ndx")?;
        let aux = r.u32(o + 12, "vd_aux")? as u64;
        let next = r.u32(o + 16, "vd_next")? as u64;
        if flags & 1 == 0 {
            // not VER_FLG_BASE
            let name = s(r.u32(o + aux, "vda_name")? as u64)?;
            out.push((ndx & 0x7fff, name));
        }
        if next == 0 {
            break;
        }
        o += next;
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
