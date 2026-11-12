/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * discovery.rs — read the IP discovery table an AMD GPU keeps at the top of
 * VRAM (or in its VBIOS), for real-silicon bring-up.
 *
 * Since Navi1x the register base of every IP block (GC, MMHUB, OSSSYS, MP0 =
 * PSP, MP1 = SMU, NBIF, DMU, ...) is not fixed per chip: the device describes
 * itself in this table, and the driver reads block versions and register
 * bases from it instead of hardcoding them. That is what lets one driver
 * pick its per-IP code by version (Shankar 2026-11-05: never hardcode RDNA4).
 *
 * Layout: a binary header (signature 0x28211407, checksum over the binary,
 * a table list), then the IP discovery table (signature "IPIP") with one
 * die header per die and a variable-length entry per IP instance. These are
 * the documented on-device structs (Linux's MIT-licensed discovery.h has the
 * same ones); all little-endian.
 *
 * The bytes come from the device, so they are untrusted: every offset,
 * count and length is bounds-checked, both checksums are verified, and a bad
 * table is an error, never a panic or an out-of-bounds read. Pure, no_std,
 * no unsafe: host-tested by atrium-gpu-amd/rs-host-tests.
 */

pub const BINARY_SIGNATURE: u32 = 0x2821_1407;
pub const IP_DISCOVERY_SIGNATURE: u32 = 0x5049_5049; /* "IPIP" */
/// Where the table sits, counted back from the end of VRAM.
pub const DISCOVERY_TMR_OFFSET_FROM_TOP: u64 = 64 << 10;
pub const DISCOVERY_TMR_SIZE: usize = 10 << 10;

const BINARY_HEADER_LEN: usize = 12 + 6 * 8;
const IP_HEADER_LEN: usize = 80;
const MAX_DIES: usize = 16;

/// Table indices in the binary header's table list.
pub const TABLE_IP_DISCOVERY: usize = 0;
pub const TABLE_GC: usize = 1;
pub const TABLE_HARVEST_INFO: usize = 2;

/// Hardware ids of the blocks bring-up cares about (soc15_hw_ip.h).
pub mod hwid {
    pub const MP1: u16 = 1; /* SMU */
    pub const THM: u16 = 3;
    pub const SMUIO: u16 = 4;
    pub const GC: u16 = 11;
    pub const MMHUB: u16 = 34;
    pub const ATHUB: u16 = 35;
    pub const OSSSYS: u16 = 40; /* IH */
    pub const HDP: u16 = 41;
    pub const SDMA0: u16 = 42;
    pub const VCN: u16 = 44;
    pub const DF: u16 = 46;
    pub const NBIF: u16 = 108;
    pub const UMC: u16 = 150;
    pub const MP0: u16 = 255; /* PSP */
    pub const DMU: u16 = 271; /* display (DCN) */
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryError {
    Truncated,
    BadBinarySignature(u32),
    BadBinaryChecksum { want: u16, got: u16 },
    BadIpSignature(u32),
    BadIpChecksum { want: u16, got: u16 },
    TooManyDies(u16),
    OutOfBounds,
}

type R<T> = Result<T, DiscoveryError>;

#[inline]
fn le16(b: &[u8], off: usize) -> R<u16> {
    let s = b.get(off..off.checked_add(2).ok_or(DiscoveryError::OutOfBounds)?)
        .ok_or(DiscoveryError::Truncated)?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}

#[inline]
fn le32(b: &[u8], off: usize) -> R<u32> {
    let s = b.get(off..off.checked_add(4).ok_or(DiscoveryError::OutOfBounds)?)
        .ok_or(DiscoveryError::Truncated)?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// The table's checksum: the byte sum, truncated to 16 bits.
pub fn checksum(bytes: &[u8]) -> u16 {
    bytes.iter().fold(0u16, |a, &b| a.wrapping_add(b as u16))
}

/// One table in the binary header's list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableInfo {
    pub offset: u16,
    pub checksum: u16,
    pub size: u16,
}

/// A parsed, checksum-verified discovery binary.
#[derive(Debug, Clone, Copy)]
pub struct Discovery<'a> {
    bin: &'a [u8],
    pub version_major: u16,
    pub version_minor: u16,
    pub tables: [TableInfo; 6],
    /// Offset of the IP discovery header in `bin`.
    ip_off: usize,
    pub ip_version: u16,
    pub num_dies: u16,
    die_offsets: [u16; MAX_DIES],
    base_addr_64: bool,
}

/// One IP block instance as the table describes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IpInstance {
    pub die: u16,
    pub hw_id: u16,
    pub instance: u8,
    pub major: u8,
    pub minor: u8,
    pub revision: u8,
    /// The byte after `revision`: harvest/sub-revision in the low nibble,
    /// variant in the high nibble.
    pub variant_byte: u8,
    pub num_base: u8,
    /// Register segment bases (dword addresses); only `num_base` are valid.
    pub base: [u64; 8],
}

impl IpInstance {
    pub fn bases(&self) -> &[u64] {
        &self.base[..(self.num_base as usize).min(8)]
    }
}

impl<'a> Discovery<'a> {
    pub fn parse(bin: &'a [u8]) -> R<Discovery<'a>> {
        if bin.len() < BINARY_HEADER_LEN {
            return Err(DiscoveryError::Truncated);
        }
        let sig = le32(bin, 0)?;
        if sig != BINARY_SIGNATURE {
            return Err(DiscoveryError::BadBinarySignature(sig));
        }
        let version_major = le16(bin, 4)?;
        let version_minor = le16(bin, 6)?;
        let want = le16(bin, 8)?;
        let size = le16(bin, 10)? as usize;
        // The checksum covers the binary from the size field on.
        let covered = bin.get(10..size).ok_or(DiscoveryError::Truncated)?;
        let got = checksum(covered);
        if got != want {
            return Err(DiscoveryError::BadBinaryChecksum { want, got });
        }
        let bin = &bin[..size];
        let mut tables = [TableInfo { offset: 0, checksum: 0, size: 0 }; 6];
        for (i, t) in tables.iter_mut().enumerate() {
            let o = 12 + i * 8;
            *t = TableInfo { offset: le16(bin, o)?, checksum: le16(bin, o + 2)?, size: le16(bin, o + 4)? };
        }

        let ip_off = tables[TABLE_IP_DISCOVERY].offset as usize;
        let isig = le32(bin, ip_off)?;
        if isig != IP_DISCOVERY_SIGNATURE {
            return Err(DiscoveryError::BadIpSignature(isig));
        }
        let ip_version = le16(bin, ip_off + 4)?;
        let ip_size = le16(bin, ip_off + 6)? as usize;
        let ip_bytes = bin
            .get(ip_off..ip_off.checked_add(ip_size).ok_or(DiscoveryError::OutOfBounds)?)
            .ok_or(DiscoveryError::Truncated)?;
        let want = tables[TABLE_IP_DISCOVERY].checksum;
        let got = checksum(ip_bytes);
        if got != want {
            return Err(DiscoveryError::BadIpChecksum { want, got });
        }
        if ip_size < IP_HEADER_LEN {
            return Err(DiscoveryError::Truncated);
        }
        let num_dies = le16(bin, ip_off + 12)?;
        if num_dies as usize > MAX_DIES {
            return Err(DiscoveryError::TooManyDies(num_dies));
        }
        let mut die_offsets = [0u16; MAX_DIES];
        for (i, d) in die_offsets.iter_mut().enumerate().take(num_dies as usize) {
            *d = le16(bin, ip_off + 14 + i * 4 + 2)?;
        }
        let flags = le16(bin, ip_off + 14 + MAX_DIES * 4)?;
        Ok(Discovery {
            bin,
            version_major,
            version_minor,
            tables,
            ip_off,
            ip_version,
            num_dies,
            die_offsets,
            base_addr_64: ip_version >= 4 && (flags & 1) != 0,
        })
    }

    /// Every IP instance on every die, in table order. A malformed entry
    /// stops the walk with an error.
    pub fn for_each_ip(&self, mut f: impl FnMut(&IpInstance)) -> R<()> {
        let ip_end = self.ip_off + le16(self.bin, self.ip_off + 6)? as usize;
        for d in 0..self.num_dies as usize {
            let mut o = self.die_offsets[d] as usize;
            if o < self.ip_off + IP_HEADER_LEN || o + 4 > ip_end {
                return Err(DiscoveryError::OutOfBounds);
            }
            let die_id = le16(self.bin, o)?;
            let num_ips = le16(self.bin, o + 2)?;
            o += 4;
            for _ in 0..num_ips {
                if o + 8 > ip_end {
                    return Err(DiscoveryError::OutOfBounds);
                }
                let num_base = *self.bin.get(o + 3).ok_or(DiscoveryError::Truncated)?;
                let mut ip = IpInstance {
                    die: die_id,
                    hw_id: le16(self.bin, o)?,
                    instance: self.bin[o + 2],
                    num_base,
                    major: *self.bin.get(o + 4).ok_or(DiscoveryError::Truncated)?,
                    minor: *self.bin.get(o + 5).ok_or(DiscoveryError::Truncated)?,
                    revision: *self.bin.get(o + 6).ok_or(DiscoveryError::Truncated)?,
                    variant_byte: *self.bin.get(o + 7).ok_or(DiscoveryError::Truncated)?,
                    base: [0; 8],
                };
                o += 8;
                let step = if self.base_addr_64 { 8 } else { 4 };
                for i in 0..num_base as usize {
                    if o + step > ip_end {
                        return Err(DiscoveryError::OutOfBounds);
                    }
                    let v = if self.base_addr_64 {
                        (le32(self.bin, o)? as u64) | ((le32(self.bin, o + 4)? as u64) << 32)
                    } else {
                        le32(self.bin, o)? as u64
                    };
                    if i < 8 {
                        ip.base[i] = v;
                    }
                    o += step;
                }
                f(&ip);
            }
        }
        Ok(())
    }

    /// First instance of `hw_id` with the given instance number.
    pub fn find(&self, hw_id: u16, instance: u8) -> R<Option<IpInstance>> {
        let mut found = None;
        self.for_each_ip(|ip| {
            if found.is_none() && ip.hw_id == hw_id && ip.instance == instance {
                found = Some(*ip);
            }
        })?;
        Ok(found)
    }
}
