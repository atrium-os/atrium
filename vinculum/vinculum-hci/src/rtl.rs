//! Realtek firmware loading (spec §5.2), ported from FreeBSD's `rtlbtfw`
//! (usr.sbin/bluetooth/rtlbtfw, BSD-2-Clause: Adrian Chadd, Vladimir
//! Kondratyev, Future Crew LLC). Realtek USB controllers (the TP-Link UB500's
//! RTL8761BU among them) boot from ROM and need a patch downloaded at every
//! power-up before they work as a normal HCI controller.
//!
//! The sequence: Read Local Version picks the chip; the patch file's
//! trailing extension names the project the file is for, which must match;
//! the vendor Read ROM Version picks the patch inside the file (v1 files) or
//! the subsections for it (v2 files); the chip's config file is appended;
//! the result goes down in 252-byte vendor Download commands.
//!
//! Sans-I/O apart from the [`Hci`] trait, so the whole load runs against a
//! fake controller in tests. Every length read from a file is checked before
//! it is used: the files come from the firmware directory, not the air, but
//! a short or corrupt file must fail the load, never panic.

use std::path::Path;

/// One command and its Command Complete, synchronously.
pub trait Hci {
    /// Sends `opcode` with `params` and returns the Command Complete's
    /// return parameters (status first). An error if the controller answers
    /// with a failing Command Status, or nothing within its timeout.
    fn command(&mut self, opcode: u16, params: &[u8]) -> Result<Vec<u8>, Error>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Io(String),
    /// The controller answered, but not as a Realtek chip we know does.
    Controller(String),
    /// The firmware or config file is missing, short or does not fit.
    File(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(s) => write!(f, "{s}"),
            Error::Controller(s) => write!(f, "controller: {s}"),
            Error::File(s) => write!(f, "firmware: {s}"),
        }
    }
}

impl std::error::Error for Error {}

const LMP_8703B: u16 = 0x8703;
const LMP_8723A: u16 = 0x1200;
const LMP_8723B: u16 = 0x8723;
const LMP_8821A: u16 = 0x8821;
const LMP_8761A: u16 = 0x8761;
const LMP_8822B: u16 = 0x8822;
const LMP_8852A: u16 = 0x8852;
const LMP_8851B: u16 = 0x8851;
const LMP_8922A: u16 = 0x8922;

/// A chip as it reports itself from ROM, before any patch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ic {
    pub lmp_subversion: u16,
    pub hci_revision: u16,
    pub hci_version: u8,
    /// The patch file can be skipped (the chip works from ROM).
    pub simple: bool,
    /// The config file is required, not optional.
    pub config_required: bool,
    pub fw_name: &'static str,
    pub fw_suffix: &'static str,
}

const fn ic(lmp_subversion: u16, hci_revision: u16, hci_version: u8, fw_name: &'static str) -> Ic {
    Ic { lmp_subversion, hci_revision, hci_version, simple: false, config_required: false, fw_name, fw_suffix: "_fw.bin" }
}

/// Chips by their ROM identity (rtlbt_ic_id_table).
pub const ICS: &[Ic] = &[
    Ic { simple: true, ..ic(LMP_8723A, 0xb, 0x6, "rtl8723a") },
    ic(LMP_8723B, 0xb, 0x6, "rtl8723b"),
    Ic { config_required: true, ..ic(LMP_8723B, 0xd, 0x8, "rtl8723d") },
    ic(LMP_8821A, 0xa, 0x6, "rtl8821a"),
    ic(LMP_8821A, 0xc, 0x8, "rtl8821c"),
    ic(LMP_8761A, 0xa, 0x6, "rtl8761a"),
    ic(LMP_8761A, 0xb, 0xa, "rtl8761bu"),
    ic(LMP_8822B, 0xc, 0xa, "rtl8822cu"),
    Ic { config_required: true, ..ic(LMP_8822B, 0xb, 0x7, "rtl8822b") },
    ic(LMP_8852A, 0xa, 0xb, "rtl8852au"),
    ic(LMP_8852A, 0xb, 0xb, "rtl8852bu"),
    Ic { fw_suffix: "_fw_v2.bin", ..ic(LMP_8852A, 0xc, 0xc, "rtl8852cu") },
    ic(LMP_8851B, 0xb, 0xc, "rtl8851bu"),
    ic(LMP_8922A, 0xa, 0xc, "rtl8922au"),
    ic(LMP_8852A, 0x87, 0xc, "rtl8852btu"),
];

/// USB ids of Realtek controllers that need a patch (rtlbtfw's list).
pub const USB_IDS: &[(u16, u16)] = &[
    // 8723AE
    (0x0930, 0x021d),
    (0x13d3, 0x3394),
    // 8723BE
    (0x0489, 0xe085),
    (0x0489, 0xe08b),
    (0x04f2, 0xb49f),
    (0x13d3, 0x3410),
    (0x13d3, 0x3416),
    (0x13d3, 0x3459),
    (0x13d3, 0x3494),
    // 8723BU
    (0x7392, 0xa611),
    // 8723DE
    (0x2ff8, 0xb011),
    // 8761BUV (0x2357:0x0604 is the TP-Link UB500)
    (0x2c4e, 0x0115),
    (0x2357, 0x0604),
    (0x0b05, 0x190e),
    (0x2550, 0x8761),
    (0x6655, 0x8771),
    (0x7392, 0xc611),
    (0x2b89, 0x8761),
    // 8821AE
    (0x0b05, 0x17dc),
    (0x13d3, 0x3414),
    (0x13d3, 0x3458),
    (0x13d3, 0x3461),
    (0x13d3, 0x3462),
    // 8822BE
    (0x13d3, 0x3526),
    (0x0b05, 0x185c),
    // 8822CE
    (0x04ca, 0x4005),
    (0x04c5, 0x161f),
    (0x0b05, 0x18ef),
    (0x13d3, 0x3548),
    (0x13d3, 0x3549),
    (0x13d3, 0x3553),
    (0x13d3, 0x3555),
    (0x2ff8, 0x3051),
    (0x1358, 0xc123),
    (0x0cb5, 0xc547),
];

pub fn is_realtek(vendor: u16, product: u16) -> bool {
    USB_IDS.contains(&(vendor, product))
}

pub fn find_ic(lmp_subversion: u16, hci_revision: u16, hci_version: u8) -> Option<&'static Ic> {
    ICS.iter().find(|i| i.lmp_subversion == lmp_subversion && i.hci_revision == hci_revision && i.hci_version == hci_version)
}

/// The patch file's project id, as its extension names it, to the ROM LMP
/// subversion of the chips it is for.
fn project(id: u8) -> Option<u16> {
    Some(match id {
        0 => LMP_8723A,
        1 | 9 | 19 => LMP_8723B,
        2 | 10 => LMP_8821A,
        3 | 14 => LMP_8761A,
        7 => LMP_8703B,
        8 | 13 | 33 => LMP_8822B,
        18 | 20 | 25 | 47 => LMP_8852A,
        36 => LMP_8851B,
        44 => LMP_8922A,
        _ => return None,
    })
}

const SIG_V1: &[u8; 8] = b"Realtech";
const SIG_V2: &[u8; 8] = b"RTBTCore";
const EXT_SIG: [u8; 4] = [0x51, 0x04, 0xFD, 0x77];
const HEADER_V1: usize = 8 + 4 + 2;
const HEADER_V2: usize = 8 + 8 + 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FwType {
    V1,
    V2,
}

fn le16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}
fn le32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// The file's format and the ROM LMP subversion it is for (from the
/// project-id instruction in its trailing extension, read backwards from the
/// extension signature at the end).
pub fn fw_type(fw: &[u8]) -> Result<(FwType, u16), Error> {
    let bad = |s: &str| Err(Error::File(s.to_string()));
    let (t, header) = if fw.starts_with(SIG_V1) {
        (FwType::V1, HEADER_V1)
    } else if fw.starts_with(SIG_V2) {
        (FwType::V2, HEADER_V2)
    } else {
        return bad("not a Realtek patch file (signature)");
    };
    if fw.len() < header + EXT_SIG.len() + 4 {
        return bad("file too small");
    }
    if fw[fw.len() - 4..] != EXT_SIG {
        return bad("no extension signature");
    }
    // Instructions run backwards from the signature: opcode, length, then
    // `length` bytes of data before them. Opcode 0xff ends the list.
    let mut p = fw.len() - 4;
    while p > header + 2 {
        let opcode = fw[p - 1];
        let len = fw[p - 2] as usize;
        if p - 2 < header + len {
            return bad("extension instruction runs into the header");
        }
        p = p - 2 - len;
        if opcode == 0x00 {
            if len != 1 {
                return bad("project id instruction has the wrong length");
            }
            let id = fw[p];
            return match project(id) {
                Some(lmp) => Ok((t, lmp)),
                None => Err(Error::File(format!("unknown project id {id}"))),
            };
        }
        if opcode == 0xff {
            break;
        }
    }
    bad("no project id instruction")
}

/// v1: the patch for `rom_version` out of the file, with the file's version
/// in its last 4 bytes.
pub fn parse_v1(fw: &[u8], rom_version: u8) -> Result<Vec<u8>, Error> {
    let short = || Error::File("v1 patch table runs past the end".to_string());
    let version = fw.get(8..12).ok_or_else(short)?;
    let n = le16(fw, 12).ok_or_else(short)? as usize;
    if fw.len() < HEADER_V1 + EXT_SIG.len() + 4 + 8 * n {
        return Err(short());
    }
    let base = HEADER_V1;
    let want = rom_version as u16 + 1;
    let i = (0..n)
        .find(|&i| le16(fw, base + 2 * i) == Some(want))
        .ok_or_else(|| Error::File(format!("no patch for ROM version {rom_version}")))?;
    let len = le16(fw, base + 2 * (n + i)).ok_or_else(short)? as usize;
    let off = le32(fw, base + 4 * (n + i)).ok_or_else(short)? as usize;
    if len < 4 {
        return Err(Error::File("v1 patch shorter than its version".to_string()));
    }
    let body = off.checked_add(len - 4).and_then(|end| fw.get(off..end)).ok_or_else(short)?;
    if off.checked_add(len).is_none_or(|end| end > fw.len()) {
        return Err(short());
    }
    let mut out = body.to_vec();
    out.extend_from_slice(version);
    Ok(out)
}

const PATCH_SNIPPETS: u32 = 0x01;
const PATCH_DUMMY_HEADER: u32 = 0x02;
const PATCH_SECURITY_HEADER: u32 = 0x03;

/// A cursor over a byte slice that only hands out what is there.
struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
}

/// v2: the subsections for `rom_version` (and, in security sections, the
/// chip's `key_id`) out of each section, joined in priority order.
pub fn parse_v2(fw: &[u8], rom_version: u8, key_id: u8) -> Result<Vec<u8>, Error> {
    // The last 7 bytes are the extension's tail, never section data.
    let end = fw.len().checked_sub(7).ok_or_else(|| Error::File("file too small".to_string()))?;
    let mut c = Cursor(&fw[..end]);
    let hdr = c.take(HEADER_V2).ok_or_else(|| Error::File("v2 header".to_string()))?;
    let num_sections = le32(hdr, 16).unwrap_or(0);
    let mut entries: Vec<(u8, &[u8])> = Vec::new();
    for _ in 0..num_sections {
        let Some(sh) = c.take(8) else { break };
        let opcode = le32(sh, 0).unwrap_or(0);
        let len = le32(sh, 4).unwrap_or(0) as usize;
        let Some(data) = c.take(len) else { break };
        match opcode {
            PATCH_SECURITY_HEADER if key_id == 0 => {}
            PATCH_SNIPPETS | PATCH_DUMMY_HEADER | PATCH_SECURITY_HEADER => section(&mut entries, opcode, data, rom_version, key_id),
            _ => {}
        }
    }
    if entries.is_empty() {
        return Err(Error::File(format!("no v2 subsections for ROM version {rom_version}")));
    }
    // rtlbtfw inserts each entry at the head of its list and then sorts it
    // stably by priority, so entries of equal priority go down in the
    // reverse of their order in the file. Kept, as that is what has worked
    // on real chips.
    entries.reverse();
    entries.sort_by_key(|e| e.0);
    Ok(entries.into_iter().flat_map(|e| e.1.iter().copied()).collect())
}

fn section<'a>(entries: &mut Vec<(u8, &'a [u8])>, opcode: u32, data: &'a [u8], rom_version: u8, key_id: u8) {
    let mut c = Cursor(data);
    let Some(h) = c.take(4) else { return };
    let n = le16(h, 0).unwrap_or(0);
    for _ in 0..n {
        let Some(sh) = c.take(8) else { break };
        let (eco, prio, key) = (sh[0], sh[1], sh[2]);
        let len = le32(sh, 4).unwrap_or(0) as usize;
        let Some(sub) = c.take(len) else { break };
        if eco as u16 != rom_version as u16 + 1 {
            continue;
        }
        if opcode == PATCH_SECURITY_HEADER {
            // A security section only says whether the chip's key matches;
            // rtlbtfw stops at the matching subsection and adds nothing.
            if key == key_id {
                break;
            }
            continue;
        }
        entries.push((prio, sub));
    }
}

/// Download command payload size (index byte excluded).
pub const FRAG: usize = 252;

/// The vendor Download commands' parameters for `patch`: index byte (1..=0x7f
/// wrapping to 1, top bit on the last) then up to 252 bytes. Like rtlbtfw
/// (and the chips' own vendor code), a patch whose length is a multiple of
/// 252 ends with an empty fragment.
pub fn fragments(patch: &[u8]) -> Vec<Vec<u8>> {
    let n = patch.len() / FRAG + 1;
    let mut out = Vec::with_capacity(n);
    let mut j: u8 = 0;
    for i in 0..n {
        if j > 0x7f {
            j = 1;
        }
        let mut idx = j;
        let start = i * FRAG;
        let end = if i == n - 1 {
            idx |= 0x80;
            start + patch.len() % FRAG
        } else {
            start + FRAG
        };
        let mut p = Vec::with_capacity(1 + end - start);
        p.push(idx);
        p.extend_from_slice(&patch[start..end]);
        out.push(p);
        j += 1;
    }
    out
}

const OP_READ_LOCAL_VERSION: u16 = 0x1001;
const OP_RTL_DOWNLOAD: u16 = 0xFC20;
const OP_RTL_READ_REG16: u16 = 0xFC61;
const OP_RTL_READ_ROM_VERSION: u16 = 0xFC6D;
/// Read Reg16 of the security project register: its first byte is the
/// chip's key id.
const SEC_PROJ: [u8; 5] = [0x10, 0xA4, 0x0D, 0x00, 0xB0];

/// Firmware files are read from here; bounded so a wrong file cannot make
/// the transport allocate without limit.
const MAX_FILE: u64 = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalVersion {
    pub hci_version: u8,
    pub hci_revision: u16,
    pub lmp_subversion: u16,
}

fn ok_params(r: Vec<u8>, need: usize, what: &str) -> Result<Vec<u8>, Error> {
    match r.first() {
        None => Err(Error::Controller(format!("{what}: empty reply"))),
        Some(0) if r.len() >= need => Ok(r),
        Some(0) => Err(Error::Controller(format!("{what}: reply of {} bytes, wanted {need}", r.len()))),
        Some(s) => Err(Error::Controller(format!("{what}: status 0x{s:02x}"))),
    }
}

pub fn read_local_version(h: &mut impl Hci) -> Result<LocalVersion, Error> {
    // status, hci_version, hci_revision u16, lmp_version, manufacturer u16, lmp_subversion u16
    let r = ok_params(h.command(OP_READ_LOCAL_VERSION, &[])?, 9, "Read Local Version")?;
    Ok(LocalVersion { hci_version: r[1], hci_revision: u16::from_le_bytes([r[2], r[3]]), lmp_subversion: u16::from_le_bytes([r[7], r[8]]) })
}

/// What [`load`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The chip did not report a ROM identity: already patched (a restart of
    /// vinculum-hci without a power cycle), so nothing was sent.
    AlreadyLoaded(LocalVersion),
    Loaded { fw: String, bytes: usize, after: LocalVersion },
}

fn read_file(p: &Path) -> Result<Vec<u8>, std::io::Error> {
    use std::io::Read;
    let f = std::fs::File::open(p)?;
    let mut v = Vec::new();
    f.take(MAX_FILE + 1).read_to_end(&mut v)?;
    if v.len() as u64 > MAX_FILE {
        return Err(std::io::Error::other("file over 1 MiB"));
    }
    Ok(v)
}

/// Loads the patch for the Realtek controller behind `h` from `dir`.
pub fn load(h: &mut impl Hci, dir: &Path) -> Result<Outcome, Error> {
    let ver = read_local_version(h)?;
    let Some(ic) = find_ic(ver.lmp_subversion, ver.hci_revision, ver.hci_version) else {
        return Ok(Outcome::AlreadyLoaded(ver));
    };
    let fw_path = dir.join(format!("{}{}", ic.fw_name, ic.fw_suffix));
    let fw = read_file(&fw_path).map_err(|e| Error::File(format!("{}: {e}", fw_path.display())))?;
    let mut patch = match fw_type(&fw) {
        // A chip that runs from ROM takes a plain image as it is.
        Err(_) if ic.simple => fw,
        Err(e) => return Err(e),
        Ok((t, lmp)) => {
            if lmp != ver.lmp_subversion {
                return Err(Error::File(format!("{} is for 0x{lmp:04x}, this chip is 0x{:04x}", fw_path.display(), ver.lmp_subversion)));
            }
            let r = ok_params(h.command(OP_RTL_READ_ROM_VERSION, &[])?, 2, "Read ROM Version")?;
            let rom = r[1];
            let mut patch = match t {
                FwType::V1 => parse_v1(&fw, rom)?,
                FwType::V2 => {
                    let r = ok_params(h.command(OP_RTL_READ_REG16, &SEC_PROJ)?, 3, "Read Reg16")?;
                    parse_v2(&fw, rom, r[1])?
                }
            };
            let cfg_path = dir.join(format!("{}_config.bin", ic.fw_name));
            match read_file(&cfg_path) {
                Ok(cfg) => patch.extend_from_slice(&cfg),
                Err(e) if ic.config_required => return Err(Error::File(format!("{}: {e}", cfg_path.display()))),
                Err(_) => {}
            }
            patch
        }
    };
    let bytes = patch.len();
    for (n, frag) in fragments(&patch).iter().enumerate() {
        ok_params(h.command(OP_RTL_DOWNLOAD, frag)?, 2, &format!("Download fragment {n}"))?;
    }
    patch.clear();
    let after = read_local_version(h)?;
    Ok(Outcome::Loaded { fw: fw_path.display().to_string(), bytes, after })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A v1 file for project 14 (8761BU) with two patches, for ROM versions
    /// 0 and 1, shaped like linux-firmware's rtl8761bu_fw.bin.
    fn v1_file(p0: &[u8], p1: &[u8]) -> Vec<u8> {
        let mut f = SIG_V1.to_vec();
        f.extend_from_slice(&0xdfc6_d922u32.to_le_bytes());
        f.extend_from_slice(&2u16.to_le_bytes());
        f.extend_from_slice(&1u16.to_le_bytes()); // chip ids: rom+1
        f.extend_from_slice(&2u16.to_le_bytes());
        f.extend_from_slice(&((p0.len() + 4) as u16).to_le_bytes());
        f.extend_from_slice(&((p1.len() + 4) as u16).to_le_bytes());
        let off0 = HEADER_V1 + 2 * 2 + 2 * 2 + 4 * 2;
        let off1 = off0 + p0.len() + 4;
        f.extend_from_slice(&(off0 as u32).to_le_bytes());
        f.extend_from_slice(&(off1 as u32).to_le_bytes());
        f.extend_from_slice(p0);
        f.extend_from_slice(&[0; 4]);
        f.extend_from_slice(p1);
        f.extend_from_slice(&[0; 4]);
        // extension, read backwards: project id instruction (data 14, length 1,
        // opcode 0), then the end marker 0xff, as in rtl8761bu_fw.bin
        f.extend_from_slice(&[0xff, 0x0e, 0x01, 0x00]);
        f.extend_from_slice(&EXT_SIG);
        f
    }

    #[test]
    fn v1_type_and_patch_choice() {
        let f = v1_file(&[0xA0; 10], &[0xB1; 7]);
        assert_eq!(fw_type(&f), Ok((FwType::V1, LMP_8761A)));
        let p = parse_v1(&f, 1).unwrap();
        assert_eq!(&p[..7], &[0xB1; 7]);
        assert_eq!(&p[7..], &0xdfc6_d922u32.to_le_bytes());
        assert_eq!(parse_v1(&f, 0).unwrap()[..10], [0xA0; 10]);
        assert!(parse_v1(&f, 5).is_err());
    }

    #[test]
    fn truncated_files_fail_without_panicking() {
        let f = v1_file(&[0xA0; 10], &[0xB1; 7]);
        for n in 0..f.len() {
            let _ = fw_type(&f[..n]);
            let _ = parse_v1(&f[..n], 1);
            let _ = parse_v2(&f[..n], 1, 0);
        }
        // a lying patch length
        let mut g = f.clone();
        g[HEADER_V1 + 4 + 2] = 0xff;
        g[HEADER_V1 + 4 + 3] = 0xff;
        assert!(parse_v1(&g, 1).is_err());
        // an extension instruction longer than the file
        let mut h = f.clone();
        let n = h.len();
        h[n - 6] = 0xff;
        assert!(fw_type(&h).is_err());
    }

    fn v2_file() -> Vec<u8> {
        let mut f = SIG_V2.to_vec();
        f.extend_from_slice(&[1; 8]);
        f.extend_from_slice(&1u32.to_le_bytes());
        // one snippets section: 3 subsections
        let mut sec = Vec::new();
        sec.extend_from_slice(&3u16.to_le_bytes());
        sec.extend_from_slice(&[0, 0]);
        for (eco, prio, byte) in [(2u8, 5u8, 0xAAu8), (3, 1, 0xCC), (2, 1, 0xBB)] {
            sec.extend_from_slice(&[eco, prio, 0, 0]);
            sec.extend_from_slice(&2u32.to_le_bytes());
            sec.extend_from_slice(&[byte, byte]);
        }
        f.extend_from_slice(&PATCH_SNIPPETS.to_le_bytes());
        f.extend_from_slice(&(sec.len() as u32).to_le_bytes());
        f.extend_from_slice(&sec);
        // 7 bytes of extension tail (excluded from sections) incl. the id
        f.extend_from_slice(&[0x00, 0x00, 0x00, 0xff, 0x0e, 0x01, 0x00]);
        f.extend_from_slice(&EXT_SIG);
        f
    }

    #[test]
    fn v2_picks_subsections_for_the_rom_in_priority_order() {
        let f = v2_file();
        assert_eq!(fw_type(&f).map(|t| t.0), Ok(FwType::V2));
        // ROM 1 (eco 2): prio 1 (0xBB) before prio 5 (0xAA); eco 3 skipped.
        assert_eq!(parse_v2(&f, 1, 0).unwrap(), vec![0xBB, 0xBB, 0xAA, 0xAA]);
        assert!(parse_v2(&f, 7, 0).is_err());
    }

    #[test]
    fn fragments_index_and_last_flag() {
        let p: Vec<u8> = (0..600u32).map(|i| i as u8).collect();
        let f = fragments(&p);
        assert_eq!(f.len(), 3);
        assert_eq!((f[0][0], f[0].len()), (0x00, 253));
        assert_eq!((f[1][0], f[1].len()), (0x01, 253));
        assert_eq!((f[2][0], f[2].len()), (0x82, 1 + 600 - 504));
        assert_eq!(f.iter().flat_map(|x| x[1..].iter().copied()).collect::<Vec<_>>(), p);
        // index wraps from 0x7f to 1
        let big = vec![0u8; FRAG * 130 + 1];
        let g = fragments(&big);
        assert_eq!(g[127][0], 0x7f);
        assert_eq!(g[128][0], 0x01);
        assert_eq!(g[130][0], 0x80 | 0x03);
        // a multiple of 252 ends with an empty last fragment, as rtlbtfw does
        assert_eq!(fragments(&[0; FRAG]).last().unwrap(), &vec![0x81]);
    }

    /// A fake RTL8761BU: ROM identity until the last fragment lands.
    struct Fake {
        rom: u8,
        got: Vec<u8>,
        done: bool,
        log: Vec<u16>,
    }

    impl Hci for Fake {
        fn command(&mut self, op: u16, p: &[u8]) -> Result<Vec<u8>, Error> {
            self.log.push(op);
            Ok(match op {
                OP_READ_LOCAL_VERSION => {
                    let sub: u16 = if self.done { 0xd922 } else { LMP_8761A };
                    let mut r = vec![0, 0x0a, 0x0b, 0x00, 0x0a, 0x5d, 0x00];
                    r.extend_from_slice(&sub.to_le_bytes());
                    r
                }
                OP_RTL_READ_ROM_VERSION => vec![0, self.rom],
                OP_RTL_DOWNLOAD => {
                    self.got.extend_from_slice(&p[1..]);
                    if p[0] & 0x80 != 0 {
                        self.done = true;
                    }
                    vec![0, p[0]]
                }
                _ => vec![0x01],
            })
        }
    }

    #[test]
    fn load_against_a_fake_8761bu() {
        let dir = std::env::temp_dir().join(format!("vinculum-rtl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p1: Vec<u8> = (0..700u32).map(|i| (i * 7) as u8).collect();
        std::fs::write(dir.join("rtl8761bu_fw.bin"), v1_file(&[0xA0; 10], &p1)).unwrap();
        std::fs::write(dir.join("rtl8761bu_config.bin"), [0x55, 0xab, 0x23, 0x87, 0x00, 0x00]).unwrap();
        let mut fake = Fake { rom: 1, got: Vec::new(), done: false, log: Vec::new() };
        let out = load(&mut fake, &dir).unwrap();
        let Outcome::Loaded { bytes, after, .. } = out else { panic!("{out:?}") };
        let mut want = p1.clone();
        want.extend_from_slice(&0xdfc6_d922u32.to_le_bytes());
        want.extend_from_slice(&[0x55, 0xab, 0x23, 0x87, 0x00, 0x00]);
        assert_eq!(fake.got, want);
        assert_eq!(bytes, want.len());
        assert_eq!(after.lmp_subversion, 0xd922);
        // A second load on the patched chip sends nothing but the version read.
        fake.log.clear();
        assert!(matches!(load(&mut fake, &dir), Ok(Outcome::AlreadyLoaded(_))));
        assert_eq!(fake.log, vec![OP_READ_LOCAL_VERSION]);
        // A file for another project is refused before anything is sent.
        let mut other = v1_file(&[0xA0; 10], &p1);
        let n = other.len();
        other[n - 7] = 0x08; // project 8 = 8822B
        std::fs::write(dir.join("rtl8761bu_fw.bin"), other).unwrap();
        let mut fresh = Fake { rom: 1, got: Vec::new(), done: false, log: Vec::new() };
        assert!(matches!(load(&mut fresh, &dir), Err(Error::File(_))));
        assert!(fresh.got.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The real rtl8761bu files from linux-firmware, when
    /// VINCULUM_RTL_FW_DIR names a directory holding them (not in the repo:
    /// Realtek's licence allows redistribution, but they are not ours).
    #[test]
    fn real_rtl8761bu_file() {
        let Some(dir) = std::env::var_os("VINCULUM_RTL_FW_DIR") else { return };
        let dir = std::path::PathBuf::from(dir);
        let f = std::fs::read(dir.join("rtl8761bu_fw.bin")).unwrap();
        assert_eq!(fw_type(&f), Ok((FwType::V1, LMP_8761A)));
        for rom in [0u8, 1] {
            let p = parse_v1(&f, rom).unwrap();
            assert!(p.len() > 10_000, "rom {rom}: {} bytes", p.len());
            assert_eq!(&p[p.len() - 4..], &f[8..12]);
        }
        let mut fake = Fake { rom: 1, got: Vec::new(), done: false, log: Vec::new() };
        assert!(matches!(load(&mut fake, &dir), Ok(Outcome::Loaded { .. })));
    }

    #[test]
    fn ub500_is_known() {
        assert!(is_realtek(0x2357, 0x0604));
        assert_eq!(find_ic(0x8761, 0xb, 0xa).unwrap().fw_name, "rtl8761bu");
    }
}
