//! Word 97–2003's binary structures (MS-DOC): the FIB, the piece table and
//! the text it points at, formatted disk pages of paragraph and character
//! properties, property modifiers (sprms), string tables, and RC4
//! decryption of the streams.
//!
//! Every offset and length is checked against the stream it points into; a
//! bad one is an error or an empty result, never a panic.

use crate::crypt::CryptError;
use crate::xls::biff::{self, Cipher, Cur, Rc4};

/// The parts of the FIB this reader uses.
#[derive(Debug, Clone, Default)]
pub struct Fib {
    pub n_fib: u16,
    pub complex: bool,
    pub encrypted: bool,
    pub obfuscated: bool,
    /// `1Table` rather than `0Table`.
    pub table1: bool,
    pub l_key: u32,
    /// Character counts of the main text, footnotes, headers, comments,
    /// endnotes, text boxes and header text boxes.
    pub ccp_text: u32,
    pub ccp_ftn: u32,
    pub ccp_hdd: u32,
    pub ccp_atn: u32,
    pub ccp_edn: u32,
    pub ccp_txbx: u32,
    pub ccp_hdr_txbx: u32,
    /// (fc, lcb) pairs of FibRgFcLcb, by index.
    pub fclcb: Vec<(u32, u32)>,
}

/// Indices into FibRgFcLcb97 (MS-DOC §2.5.6).
pub mod fc {
    pub const STSHF: usize = 1;
    pub const PLCFFND_REF: usize = 2;
    pub const PLCFFND_TXT: usize = 3;
    pub const PLCFAND_REF: usize = 4;
    pub const PLCFAND_TXT: usize = 5;
    pub const PLCF_SED: usize = 6;
    pub const PLCF_HDD: usize = 11;
    pub const PLCF_BTE_CHPX: usize = 12;
    pub const PLCF_BTE_PAPX: usize = 13;
    pub const STTBF_FFN: usize = 15;
    pub const STTBF_BKMK: usize = 21;
    pub const PLCF_BKF: usize = 22;
    pub const PLCF_BKL: usize = 23;
    pub const DOP: usize = 31;
    pub const CLX: usize = 33;
    pub const GRP_XST_ATN_OWNERS: usize = 36;
    pub const STTBF_ATN_BKMK: usize = 37;
    pub const PLC_SPA_MOM: usize = 40;
    pub const PLCF_ATN_BKF: usize = 42;
    pub const PLCF_ATN_BKL: usize = 43;
    pub const PLCFEND_REF: usize = 46;
    pub const PLCFEND_TXT: usize = 47;
    pub const STTBF_RMARK: usize = 51;
    pub const PLCFTXBX_TXT: usize = 56;
    pub const ATRD_EXTRA: usize = 112;
    pub const PLF_LST: usize = 73;
    pub const PLF_LFO: usize = 74;
}

#[derive(Debug)]
pub struct Bad(pub String);

fn bad<T>(s: &str) -> Result<T, Bad> {
    Err(Bad(s.into()))
}

impl Fib {
    pub fn read(w: &[u8]) -> Result<Fib, Bad> {
        let mut c = Cur::of(w);
        let short = |_| Bad("the FIB is cut short".into());
        let ident = c.u16().map_err(short)?;
        let n_fib = c.u16().map_err(short)?;
        if ident == 0xA5DC || ident == 0xA5DB || (ident == 0xA5EC && n_fib < 0x00C1) {
            return bad("Word 95 and older documents aren't supported; save it from Word 97 or later");
        }
        if ident != 0xA5EC {
            return bad("not a Word document");
        }
        c.skip(6).map_err(short)?;
        let flags = c.u16().map_err(short)?;
        c.skip(2).map_err(short)?;
        let l_key = c.u32().map_err(short)?;
        c.p = 32;
        let csw = c.u16().map_err(short)? as usize;
        c.skip(csw * 2).map_err(short)?;
        let cslw = c.u16().map_err(short)? as usize;
        let lw_at = c.p;
        c.skip(cslw * 4).map_err(short)?;
        let lw = |i: usize| -> u32 {
            let o = lw_at + i * 4;
            if i < cslw && o + 4 <= w.len() {
                u32::from_le_bytes([w[o], w[o + 1], w[o + 2], w[o + 3]])
            } else {
                0
            }
        };
        let n = c.u16().map_err(short)? as usize;
        let mut fclcb = Vec::with_capacity(n);
        for _ in 0..n {
            fclcb.push((c.u32().map_err(short)?, c.u32().map_err(short)?));
        }
        Ok(Fib {
            n_fib,
            complex: flags & 0x0004 != 0,
            encrypted: flags & 0x0100 != 0,
            obfuscated: flags & 0x8000 != 0,
            table1: flags & 0x0200 != 0,
            l_key,
            ccp_text: lw(3),
            ccp_ftn: lw(4),
            ccp_hdd: lw(5),
            ccp_atn: lw(7),
            ccp_edn: lw(8),
            ccp_txbx: lw(9),
            ccp_hdr_txbx: lw(10),
            fclcb,
        })
    }

    /// The bytes `fclcb[i]` names in `table`, or nothing.
    pub fn part<'a>(&self, i: usize, table: &'a [u8]) -> &'a [u8] {
        match self.fclcb.get(i) {
            Some(&(fc, lcb)) if lcb > 0 => {
                let (a, n) = (fc as usize, lcb as usize);
                if a <= table.len() && n <= table.len() - a {
                    &table[a..a + n]
                } else {
                    &[]
                }
            }
            _ => &[],
        }
    }
}

/// RC4 over a whole stream in 512-byte blocks, keyed per block, keeping
/// the first `clear` bytes as they are (MS-DOC §2.2.6.2).
pub fn rc4_stream(data: &mut [u8], cipher: &Cipher, clear: usize) {
    for (b, chunk) in data.chunks_mut(512).enumerate() {
        let start = b * 512;
        if start + chunk.len() <= clear {
            continue;
        }
        let mut ks = vec![0u8; chunk.len()];
        Rc4::new(&cipher.block_key(b as u32)).apply(&mut ks);
        for (i, x) in chunk.iter_mut().enumerate() {
            if start + i >= clear {
                *x ^= ks[i];
            }
        }
    }
}

/// The FIB's first fields, which encryption leaves in the clear: whether
/// the document is encrypted, obfuscated, which table stream it uses, and
/// the size of the encryption header.
pub fn fib_base(w: &[u8]) -> Option<(bool, bool, bool, u32)> {
    let flags = u16::from_le_bytes(w.get(10..12)?.try_into().ok()?);
    let l_key = u32::from_le_bytes(w.get(14..18)?.try_into().ok()?);
    Some((flags & 0x0100 != 0, flags & 0x8000 != 0, flags & 0x0200 != 0, l_key))
}

/// The cipher a protected document's table stream starts with.
pub fn cipher(table: &[u8], obfuscated: bool, l_key: u32, password: &str) -> Result<Cipher, CryptError> {
    if obfuscated {
        return Err(CryptError::Unsupported("XOR obfuscation".into()));
    }
    let n = (l_key as usize).min(table.len());
    biff::rc4_info(&mut Cur::of(&table[..n]), password)
}

/// One piece of the text (MS-DOC §2.9.177): character positions
/// `cp0..cp1` at byte `fc`, one byte a character when `compressed`.
#[derive(Debug, Clone, Copy)]
pub struct Piece {
    pub cp0: u32,
    pub cp1: u32,
    pub fc: u32,
    pub compressed: bool,
    pub prm: u16,
}

/// The piece table and the property lists (`Prc`) pieces may name.
#[derive(Debug, Default)]
pub struct Clx {
    pub pieces: Vec<Piece>,
    pub prcs: Vec<Vec<u8>>,
}

impl Clx {
    pub fn read(b: &[u8]) -> Result<Clx, Bad> {
        let mut c = Cur::of(b);
        let short = |_| Bad("the piece table is cut short".into());
        let mut out = Clx::default();
        while c.left() > 0 {
            match c.u8().map_err(short)? {
                1 => {
                    let n = c.u16().map_err(short)? as usize;
                    if out.prcs.len() < 1 << 16 {
                        out.prcs.push(c.bytes(n).map_err(short)?.to_vec());
                    } else {
                        c.skip(n).map_err(short)?;
                    }
                }
                2 => {
                    let lcb = c.u32().map_err(short)? as usize;
                    let plc = c.bytes(lcb).map_err(short)?;
                    if lcb < 4 || (lcb - 4) % 12 != 0 {
                        return bad("a damaged piece table");
                    }
                    let n = (lcb - 4) / 12;
                    let cp = |i: usize| u32::from_le_bytes(plc[i * 4..i * 4 + 4].try_into().unwrap());
                    let pcd = &plc[(n + 1) * 4..];
                    for i in 0..n {
                        let d = &pcd[i * 8..i * 8 + 8];
                        let raw = u32::from_le_bytes([d[2], d[3], d[4], d[5]]);
                        let compressed = raw & 0x4000_0000 != 0;
                        let fc = raw & 0x3FFF_FFFF;
                        let (cp0, cp1) = (cp(i), cp(i + 1));
                        if cp1 < cp0 {
                            return bad("a damaged piece table");
                        }
                        out.pieces.push(Piece { cp0, cp1, fc: if compressed { fc / 2 } else { fc }, compressed, prm: u16::from_le_bytes([d[6], d[7]]) });
                    }
                    return Ok(out);
                }
                _ => return bad("a damaged piece table"),
            }
        }
        bad("no piece table")
    }

    /// The piece holding `cp`.
    pub fn piece(&self, cp: u32) -> Option<usize> {
        let i = self.pieces.partition_point(|p| p.cp0 <= cp);
        (i > 0 && cp < self.pieces[i - 1].cp1).then(|| i - 1)
    }

    /// The byte offset of `cp` in the WordDocument stream.
    pub fn fc(&self, cp: u32) -> Option<u32> {
        let p = &self.pieces[self.piece(cp)?];
        Some(p.fc + (cp - p.cp0) * if p.compressed { 1 } else { 2 })
    }
}

/// Windows-1252's 0x80–0x9F, the rest of a compressed piece being Latin-1.
const CP1252: [u16; 32] = [
    0x20AC, 0x0081, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039, 0x0152, 0x008D, 0x017D, 0x008F, 0x0090, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0x009D, 0x017E, 0x0178,
];

/// The document's text, one UTF-16 unit a character position, up to `end`.
/// Text a piece names past the stream's end comes back as spaces.
pub fn text(w: &[u8], clx: &Clx, end: u32) -> Vec<u16> {
    let mut out: Vec<u16> = Vec::with_capacity((end as usize).min(w.len()));
    for p in &clx.pieces {
        if p.cp0 >= end {
            break;
        }
        if p.cp0 as usize != out.len() {
            // Pieces must tile the text; fill a gap.
            out.resize(p.cp0 as usize, b' ' as u16);
        }
        let n = (p.cp1.min(end) - p.cp0) as usize;
        let at = p.fc as usize;
        if p.compressed {
            for i in 0..n {
                let b = w.get(at + i).copied().unwrap_or(b' ');
                out.push(if (0x80..0xA0).contains(&b) { CP1252[(b - 0x80) as usize] } else { b as u16 });
            }
        } else {
            for i in 0..n {
                let o = at + 2 * i;
                out.push(if o + 2 <= w.len() { u16::from_le_bytes([w[o], w[o + 1]]) } else { b' ' as u16 });
            }
        }
    }
    out.resize(end as usize, b' ' as u16);
    out
}

/// A run of the WordDocument stream's bytes `fc0..fc1` with the same
/// properties: the paragraph's style and sprms, or the characters' sprms.
#[derive(Debug, Clone)]
pub struct FkpRun {
    pub fc0: u32,
    pub fc1: u32,
    pub istd: u16,
    pub grpprl: Vec<u8>,
}

/// Runs from formatted disk pages (MS-DOC §2.9.62–2.9.63) a bin table
/// (`PlcBteChpx` or `PlcBtePapx`) points at, in order.
pub fn fkp_runs(w: &[u8], plc: &[u8], papx: bool) -> Vec<FkpRun> {
    let mut out: Vec<FkpRun> = Vec::new();
    if plc.len() < 4 || (plc.len() - 4) % 8 != 0 {
        return out;
    }
    let n = (plc.len() - 4) / 8;
    let pns = &plc[(n + 1) * 4..];
    let mut seen = std::collections::HashSet::new();
    for i in 0..n {
        let pn = u32::from_le_bytes(pns[i * 4..i * 4 + 4].try_into().unwrap()) & 0x003F_FFFF;
        if !seen.insert(pn) {
            continue;
        }
        let at = pn as usize * 512;
        let Some(page) = w.get(at..at + 512) else { continue };
        let crun = page[511] as usize;
        if 4 * (crun + 1) + crun * if papx { 13 } else { 1 } > 511 {
            continue;
        }
        let rgfc = |k: usize| u32::from_le_bytes(page[k * 4..k * 4 + 4].try_into().unwrap());
        for k in 0..crun {
            let (fc0, fc1) = (rgfc(k), rgfc(k + 1));
            if fc1 <= fc0 {
                continue;
            }
            let (istd, grpprl) = if papx {
                let b = page[4 * (crun + 1) + k * 13] as usize * 2;
                if b == 0 || b >= 511 {
                    (0, Vec::new())
                } else {
                    let cb = page[b] as usize;
                    let (from, len) = if cb != 0 { (b + 1, 2 * cb - 1) } else { (b + 2, 2 * *page.get(b + 1).unwrap_or(&0) as usize) };
                    let d = page.get(from..(from + len).min(511)).unwrap_or(&[]);
                    if d.len() < 2 {
                        (0, Vec::new())
                    } else {
                        (u16::from_le_bytes([d[0], d[1]]), d[2..].to_vec())
                    }
                }
            } else {
                let b = page[4 * (crun + 1) + k] as usize * 2;
                if b == 0 || b >= 511 {
                    (0, Vec::new())
                } else {
                    let cb = page[b] as usize;
                    (0, page.get(b + 1..(b + 1 + cb).min(511)).unwrap_or(&[]).to_vec())
                }
            };
            out.push(FkpRun { fc0, fc1, istd, grpprl });
        }
    }
    out.sort_by_key(|r| r.fc0);
    out
}

/// The run holding byte `fc`.
pub fn run_at(runs: &[FkpRun], fc: u32) -> Option<usize> {
    let i = runs.partition_point(|r| r.fc0 <= fc);
    (i > 0 && fc < runs[i - 1].fc1).then(|| i - 1)
}

/// One property modifier: its code and operand.
#[derive(Debug, Clone, Copy)]
pub struct Sprm<'a> {
    pub op: u16,
    pub arg: &'a [u8],
}

impl Sprm<'_> {
    pub fn u8(&self) -> u8 {
        self.arg.first().copied().unwrap_or(0)
    }
    pub fn u16(&self) -> u16 {
        match self.arg {
            [a, b, ..] => u16::from_le_bytes([*a, *b]),
            [a] => *a as u16,
            _ => 0,
        }
    }
    pub fn i16(&self) -> i16 {
        self.u16() as i16
    }
    pub fn u32(&self) -> u32 {
        match self.arg {
            [a, b, c, d, ..] => u32::from_le_bytes([*a, *b, *c, *d]),
            _ => self.u16() as u32,
        }
    }
}

/// The sprms of a `grpprl`, in order; a cut-short one ends the list.
pub fn sprms(g: &[u8]) -> Vec<Sprm<'_>> {
    let mut out = Vec::new();
    let mut p = 0;
    while p + 2 <= g.len() {
        let op = u16::from_le_bytes([g[p], g[p + 1]]);
        p += 2;
        let (skip, n) = match op >> 13 {
            0 | 1 => (0, 1),
            2 | 4 | 5 => (0, 2),
            3 => (0, 4),
            7 => (0, 3),
            _ => {
                if op == 0xD608 || op == 0xD606 {
                    // sprmTDefTable: a 2-byte size, one more than the rest.
                    match g.get(p..p + 2) {
                        Some(b) => (2, (u16::from_le_bytes([b[0], b[1]]) as usize).saturating_sub(1)),
                        None => break,
                    }
                } else if op == 0xC615 && g.get(p) == Some(&255) {
                    // sprmPChgTabs with its long form: deletions with
                    // close distances, then additions.
                    let dels = *g.get(p + 1).unwrap_or(&0) as usize;
                    let adds = *g.get(p + 2 + dels * 4).unwrap_or(&0) as usize;
                    (1, 1 + dels * 4 + 1 + adds * 3)
                } else {
                    match g.get(p) {
                        Some(&b) => (1, b as usize),
                        None => break,
                    }
                }
            }
        };
        p += skip;
        if p + n > g.len() {
            break;
        }
        out.push(Sprm { op, arg: &g[p..p + n] });
        p += n;
    }
    out
}

/// A string table (MS-DOC §2.2.4): its strings, extra data skipped.
pub fn sttb(b: &[u8]) -> Vec<String> {
    sttb_extra(b).into_iter().map(|e| e.0).collect()
}

/// A string table with each string's extra data.
pub fn sttb_extra(b: &[u8]) -> Vec<(String, &[u8])> {
    let mut c = Cur::of(b);
    let mut out = Vec::new();
    let Ok(first) = c.u16() else { return out };
    let wide = first == 0xFFFF;
    let n = if wide { c.u16().unwrap_or(0) as usize } else { first as usize };
    let extra = c.u16().unwrap_or(0) as usize;
    for _ in 0..n.min(1 << 16) {
        let s = if wide {
            let Ok(cch) = c.u16() else { break };
            match c.bytes(cch as usize * 2) {
                Ok(b) => String::from_utf16_lossy(&b.chunks_exact(2).map(|x| u16::from_le_bytes([x[0], x[1]])).collect::<Vec<_>>()),
                Err(_) => break,
            }
        } else {
            let Ok(cch) = c.u8() else { break };
            match c.bytes(cch as usize) {
                Ok(b) => b.iter().map(|&x| x as char).collect(),
                Err(_) => break,
            }
        };
        let Ok(x) = c.bytes(extra) else { break };
        out.push((s, x));
    }
    out
}

/// Strings one after another, each its length then UTF-16 (`Xst`s, as in
/// `GrpXstAtnOwners`).
pub fn xsts(b: &[u8]) -> Vec<String> {
    let mut c = Cur::of(b);
    let mut out = Vec::new();
    while out.len() < 1 << 16 {
        let Ok(cch) = c.u16() else { break };
        let Ok(b) = c.bytes(cch as usize * 2) else { break };
        out.push(String::from_utf16_lossy(&b.chunks_exact(2).map(|x| u16::from_le_bytes([x[0], x[1]])).collect::<Vec<_>>()));
    }
    out
}

/// A `DTTM` as an ISO 8601 time, when it is one.
pub fn dttm(v: u32) -> Option<String> {
    let (min, hr, dom, mon, yr) = (v & 0x3F, (v >> 6) & 0x1F, (v >> 11) & 0x1F, (v >> 16) & 0xF, (v >> 20) & 0x1FF);
    (v != 0 && (1..=12).contains(&mon) && (1..=31).contains(&dom) && hr < 24 && min < 60).then(|| format!("{:04}-{:02}-{:02}T{:02}:{:02}:00Z", 1900 + yr, mon, dom, hr, min))
}

/// The font names of `SttbfFfn`, by index.
pub fn fonts(b: &[u8]) -> Vec<String> {
    let mut c = Cur::of(b);
    let mut out = Vec::new();
    let n = c.u16().unwrap_or(0) as usize;
    let _ = c.u16();
    for _ in 0..n.min(4096) {
        let Ok(cb) = c.u8() else { break };
        let Ok(ffn) = c.bytes(cb as usize) else { break };
        let name: Vec<u16> = ffn.get(39..).unwrap_or(&[]).chunks_exact(2).map(|x| u16::from_le_bytes([x[0], x[1]])).take_while(|&u| u != 0).collect();
        out.push(String::from_utf16_lossy(&name));
    }
    out
}

/// A PLC's character positions and its data elements of `size` bytes.
pub fn plc(b: &[u8], size: usize) -> (Vec<u32>, Vec<&[u8]>) {
    if b.len() < 4 {
        return (Vec::new(), Vec::new());
    }
    let n = (b.len() - 4) / (4 + size);
    let cps = (0..=n).map(|i| u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap())).collect();
    let data = (0..n).map(|i| &b[(n + 1) * 4 + i * size..(n + 1) * 4 + (i + 1) * size]).collect();
    (cps, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sprm_sizes() {
        // sprmCFBold (1 byte), sprmCHps (2), sprmCCv (4), a variable one,
        // sprmTDefTable with its 2-byte size.
        let g = [0x35, 0x08, 1, 0x43, 0x4A, 24, 0, 0x70, 0x68, 1, 2, 3, 0, 0x15, 0xC6, 2, 9, 9, 0x08, 0xD6, 3, 0, 7, 7];
        let s = sprms(&g);
        assert_eq!(s.len(), 5);
        assert_eq!((s[0].op, s[0].u8()), (0x0835, 1));
        assert_eq!(s[1].u16(), 24);
        assert_eq!(s[2].u32(), 0x0003_0201);
        assert_eq!(s[3].arg, &[9, 9]);
        assert_eq!(s[4].arg, &[7, 7]);
        // Cut short: what fits.
        assert_eq!(sprms(&g[..5]).len(), 1);
        for n in 0..g.len() {
            let _ = sprms(&g[n..]);
        }
    }

    #[test]
    fn pieces_and_text() {
        // Two pieces: "Hi" compressed at byte 100, "é!" as UTF-16 at 200.
        let mut plc = Vec::new();
        for cp in [0u32, 2, 4] {
            plc.extend(cp.to_le_bytes());
        }
        for (fc, comp) in [(100u32, true), (200, false)] {
            plc.extend([0, 0]);
            plc.extend((if comp { fc * 2 | 0x4000_0000 } else { fc }).to_le_bytes());
            plc.extend([0, 0]);
        }
        let mut clx = vec![2u8];
        clx.extend((plc.len() as u32).to_le_bytes());
        clx.extend(&plc);
        let c = Clx::read(&clx).unwrap();
        let mut w = vec![0u8; 300];
        w[100..102].copy_from_slice(b"Hi");
        w[200..204].copy_from_slice(&[0xE9, 0, b'!', 0]);
        assert_eq!(String::from_utf16_lossy(&text(&w, &c, 4)), "Hié!");
        assert_eq!(c.fc(1), Some(101));
        assert_eq!(c.fc(3), Some(202));
        assert_eq!(c.fc(4), None);
        for n in 0..clx.len() {
            let _ = Clx::read(&clx[..n]);
        }
    }
}
