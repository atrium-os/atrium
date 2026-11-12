use crate::discovery::*;
use crate::fw::*;
use std::vec::Vec;

fn put16(v: &mut Vec<u8>, o: usize, x: u16) { v[o..o + 2].copy_from_slice(&x.to_le_bytes()); }
fn put32(v: &mut Vec<u8>, o: usize, x: u32) { v[o..o + 4].copy_from_slice(&x.to_le_bytes()); }

/// A firmware file: common header (major, minor), `extra` header bytes, payload.
fn fw_file(major: u16, minor: u16, extra: &[u32], payload: usize) -> Vec<u8> {
    let hlen = 32 + extra.len() * 4;
    let mut v = std::vec![0u8; hlen + payload];
    let n = v.len() as u32;
    put32(&mut v, 0, n);
    put32(&mut v, 4, hlen as u32);
    put16(&mut v, 8, major);
    put16(&mut v, 10, minor);
    put16(&mut v, 12, 12);
    put32(&mut v, 16, 0x1234);
    put32(&mut v, 20, payload as u32);
    put32(&mut v, 24, hlen as u32);
    for (i, x) in extra.iter().enumerate() { put32(&mut v, 32 + i * 4, *x); }
    v
}

#[test]
fn common_header_ok_and_ucode_region() {
    let f = fw_file(1, 0, &[], 64);
    let h = CommonHeader::parse(&f).unwrap();
    assert_eq!(h.ucode_version, 0x1234);
    let r = h.ucode(&f).unwrap();
    assert_eq!((r.offset, r.size), (32, 64));
    assert_eq!(r.bytes(&f).unwrap().len(), 64);
}

#[test]
fn common_header_rejects_bad_files() {
    assert_eq!(CommonHeader::parse(&[0u8; 10]), Err(FwError::Truncated));
    let mut f = fw_file(1, 0, &[], 16);
    put32(&mut f, 0, 999);
    assert_eq!(CommonHeader::parse(&f), Err(FwError::SizeMismatch));
    let mut f = fw_file(1, 0, &[], 16);
    put32(&mut f, 24, u32::MAX);
    assert_eq!(CommonHeader::parse(&f).unwrap().ucode(&f), Err(FwError::RegionOutOfBounds));
}

#[test]
fn rs64_parses_and_bounds() {
    // feature, ucode_size, ucode_off, data_size, data_off, start lo, start hi
    let f = fw_file(2, 0, &[7, 16, 60, 8, 76, 0x1000, 0x1], 32);
    let p = Rs64Fw::parse(&f).unwrap();
    assert_eq!(p.ucode, Region { offset: 60, size: 16 });
    assert_eq!(p.data, Region { offset: 76, size: 8 });
    assert_eq!(p.ucode_start_addr, 0x1_0000_1000);
    let bad = fw_file(2, 0, &[7, 16, 60, 8, 90, 0, 0], 32);
    assert_eq!(Rs64Fw::parse(&bad), Err(FwError::RegionOutOfBounds));
    assert!(matches!(Rs64Fw::parse(&fw_file(1, 0, &[0; 7], 8)), Err(FwError::UnknownVersion { .. })));
    // Header claims v2 but is too short for the v2 fields.
    assert_eq!(Rs64Fw::parse(&fw_file(2, 0, &[], 8)), Err(FwError::Truncated));
}

#[test]
fn mes_imu_sdma_parse() {
    let f = fw_file(1, 0, &[3, 8, 72, 4, 8, 80, 0, 0, 0, 0], 16);
    let m = MesFw::parse(&f).unwrap();
    assert_eq!((m.ucode.offset, m.data.offset), (72, 80));
    let f = fw_file(1, 0, &[8, 0, 8, 0], 16);
    let i = ImuFw::parse(&f).unwrap();
    assert_eq!((i.iram.offset, i.dram.offset), (48, 56));
    let f = fw_file(3, 0, &[1, 44, 4], 8);
    assert_eq!(SdmaFw::parse(&f).unwrap().ucode, Region { offset: 44, size: 4 });
}

#[test]
fn psp_v2_tables() {
    // v2.0: count=2, then (type, version, offset, size) x2, relative to the array.
    let f = fw_file(2, 0, &[2, 1, 0x10, 0, 8, 2, 0x20, 8, 8], 16);
    let p = PspFw::parse(&f).unwrap();
    assert_eq!(p.bins().len(), 2);
    assert_eq!(p.bins()[1].region, Region { offset: 68 + 8, size: 8 });
    // v2.1 with aux index.
    let f = fw_file(2, 1, &[1, 0, 5, 1, 0, 4], 4);
    let p = PspFw::parse(&f).unwrap();
    assert_eq!(p.aux_index, Some(0));
    // Count larger than the table.
    let f = fw_file(2, 0, &[1000], 4);
    assert_eq!(PspFw::parse(&f), Err(FwError::TooManyBins));
    let f = fw_file(2, 0, &[3, 1, 0, 0, 4], 4);
    assert_eq!(PspFw::parse(&f), Err(FwError::Truncated));
    // Real files: header_size covers only common + count; the table follows.
    let mut f = fw_file(2, 0, &[1, 7, 9, 0, 4], 4);
    put32(&mut f, 4, 36);
    let p = PspFw::parse(&f).unwrap();
    assert_eq!(p.bins()[0].fw_type, 7);
}

/// A discovery binary with one die and the given IPs (hw_id, inst, ver, bases).
fn disc(ips: &[(u16, u8, (u8, u8, u8), &[u32])]) -> Vec<u8> {
    let ip_off = 60usize;
    let die_off = ip_off + 80;
    let mut v = std::vec![0u8; die_off + 4];
    put16(&mut v, die_off, 0);
    put16(&mut v, die_off + 2, ips.len() as u16);
    for (hw, inst, (ma, mi, rv), bases) in ips {
        let o = v.len();
        v.resize(o + 8 + bases.len() * 4, 0);
        put16(&mut v, o, *hw);
        v[o + 2] = *inst;
        v[o + 3] = bases.len() as u8;
        v[o + 4] = *ma; v[o + 5] = *mi; v[o + 6] = *rv;
        for (i, b) in bases.iter().enumerate() { put32(&mut v, o + 8 + i * 4, *b); }
    }
    let ip_size = v.len() - ip_off;
    put32(&mut v, ip_off, IP_DISCOVERY_SIGNATURE);
    put16(&mut v, ip_off + 4, 3);
    put16(&mut v, ip_off + 6, ip_size as u16);
    put16(&mut v, ip_off + 12, 1);
    put16(&mut v, ip_off + 14 + 2, die_off as u16);
    let ipck = checksum(&v[ip_off..]);
    put32(&mut v, 0, BINARY_SIGNATURE);
    put16(&mut v, 4, 1);
    let n = v.len() as u16;
    put16(&mut v, 10, n);
    put16(&mut v, 12, ip_off as u16);
    put16(&mut v, 14, ipck);
    put16(&mut v, 16, ip_size as u16);
    let bck = checksum(&v[10..]);
    put16(&mut v, 8, bck);
    v
}

#[test]
fn discovery_walks_ips() {
    let b = disc(&[(hwid::GC, 0, (12, 0, 1), &[0x1260, 0xa000]), (hwid::MP0, 0, (14, 0, 3), &[0x16000])]);
    let d = Discovery::parse(&b).unwrap();
    let gc = d.find(hwid::GC, 0).unwrap().unwrap();
    assert_eq!((gc.major, gc.minor, gc.revision), (12, 0, 1));
    assert_eq!(gc.bases(), &[0x1260, 0xa000]);
    let psp = d.find(hwid::MP0, 0).unwrap().unwrap();
    assert_eq!(psp.bases(), &[0x16000]);
    assert_eq!(d.find(hwid::DMU, 0).unwrap(), None);
}

#[test]
fn discovery_rejects_corruption() {
    let b = disc(&[(hwid::GC, 0, (12, 0, 1), &[0x1260])]);
    let mut c = b.clone(); c[0] ^= 1;
    assert!(matches!(Discovery::parse(&c), Err(DiscoveryError::BadBinarySignature(_))));
    let mut c = b.clone(); let n = c.len(); c[n - 1] ^= 1;
    assert!(matches!(Discovery::parse(&c), Err(DiscoveryError::BadBinaryChecksum { .. })));
    // Every truncation is an error, never a panic.
    for n in 0..b.len() {
        let _ = Discovery::parse(&b[..n]).and_then(|d| d.for_each_ip(|_| {}));
    }
}

#[test]
fn no_panic_on_any_single_byte_corruption() {
    let b = disc(&[(hwid::GC, 0, (12, 0, 1), &[0x1260, 1, 2]), (hwid::OSSSYS, 0, (7, 0, 0), &[0x40])]);
    for i in 0..b.len() {
        for x in [0x00u8, 0xff, 0x7f, 0x80] {
            let mut c = b.clone(); c[i] = x;
            // Fix up the binary checksum so the walk itself is exercised.
            let ck = checksum(&c[10..]); c[8..10].copy_from_slice(&ck.to_le_bytes());
            let _ = Discovery::parse(&c).and_then(|d| d.for_each_ip(|_| {}));
        }
    }
    let f = fw_file(2, 0, &[2, 1, 0x10, 0, 8, 2, 0x20, 8, 8], 16);
    for i in 0..f.len() {
        for x in [0x00u8, 0xff, 0x80] {
            let mut c = f.clone(); c[i] = x;
            let _ = PspFw::parse(&c);
            let _ = Rs64Fw::parse(&c);
            let _ = MesFw::parse(&c);
            let _ = ImuFw::parse(&c);
            let _ = SdmaFw::parse(&c);
        }
    }
}
