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

/// Golden: the R9700's (Navi48) discovery table, read from the top of VRAM by
/// atrium-gpu-probe under Atrium on bare metal, 2026-11-12. Real bytes, so it
/// catches constants that a synthetic fixture would share with the parser
/// (the "IPIP"/"IPDS" signature bug).
#[test]
fn r9700_golden_discovery() {
    let b = include_bytes!("../data/r9700-discovery.bin");
    let d = Discovery::parse(b).unwrap();
    assert_eq!((d.version_major, d.version_minor, d.ip_version, d.num_dies), (1, 3, 3, 1));
    let v = |id, inst| {
        let ip = d.find(id, inst).unwrap().unwrap_or_else(|| panic!("hw {id} inst {inst} missing"));
        (ip.major, ip.minor, ip.revision)
    };
    assert_eq!(v(hwid::GC, 0), (12, 0, 1));
    assert_eq!(v(hwid::MP0, 0), (14, 0, 3));
    assert_eq!(v(hwid::MP1, 0), (14, 0, 3));
    assert_eq!(v(hwid::SDMA0, 0), (7, 0, 1));
    assert_eq!(v(hwid::SDMA1, 0), (7, 0, 1));
    assert_eq!(v(hwid::MMHUB, 0), (4, 1, 0));
    assert_eq!(v(hwid::OSSSYS, 0), (7, 0, 0));
    assert_eq!(v(hwid::HDP, 0), (7, 0, 0));
    assert_eq!(v(hwid::NBIF, 0), (6, 3, 1));
    assert_eq!(v(hwid::VCN, 0), (5, 0, 0));
    assert_eq!(v(hwid::DMU, 0), (4, 1, 0));
    assert_eq!(d.find(hwid::GC, 0).unwrap().unwrap().bases(), &[0x1260, 0xa000, 0x1c000, 0x2402c00]);
    assert_eq!(d.find(hwid::MP0, 0).unwrap().unwrap().bases()[0], 0x16000);
    let mut n = 0;
    d.for_each_ip(|_| n += 1).unwrap();
    assert_eq!(n, 46);
    let mut umc = 0;
    d.for_each_ip(|ip| if ip.hw_id == hwid::UMC { umc += 1 }).unwrap();
    assert_eq!(umc, 8);
}

/// The driver's view of the real R9700 table (what identify.c gets).
#[test]
fn r9700_ipinfo() {
    use crate::ipinfo::*;
    let b = include_bytes!("../data/r9700-discovery.bin");
    let mut i = IpInfo::default();
    assert_eq!(fill(b, &mut i), 0);
    assert_eq!(i.nips, 46);
    assert_eq!((i.gc.present, i.gc.major, i.gc.minor, i.gc.rev), (1, 12, 0, 1));
    assert_eq!(&i.gc.base[..i.gc.nbase as usize], &[0x1260, 0xa000, 0x1c000, 0x2402c00]);
    assert_eq!((i.mp0.major, i.mp0.minor, i.mp0.rev), (14, 0, 3));
    assert_eq!(i.mp0.base[0], 0x16000);
    assert_eq!(i.mp1.base[1], 0x16200);
    assert_eq!((i.dmu.major, i.dmu.minor, i.dmu.rev), (4, 1, 0));
    assert_eq!(i.dmu.base[2] * 4, 0x34c0 * 4);
    assert_eq!((i.vcn.major, i.osssys.major, i.nbif.minor), (5, 7, 3));
    // Corruption is an error code with a zeroed result, never a panic.
    let mut c = b.to_vec();
    c[0x40] ^= 0xff;
    assert!(fill(&c, &mut i) < 0);
    assert_eq!(i, IpInfo::default());
    for n in 0..b.len() {
        let _ = fill(&b[..n], &mut i);
    }
}

/// A PSP v2.0 file whose sub-binaries are `types`, each 8 bytes, in that order.
fn psp_file(types: &[u32]) -> Vec<u8> {
    let mut extra = std::vec![types.len() as u32];
    for (i, t) in types.iter().enumerate() {
        extra.extend_from_slice(&[*t, 0x100 + i as u32, (i * 8) as u32, 8]);
    }
    let mut f = fw_file(2, 0, &extra, types.len() * 8);
    put32(&mut f, 4, 36); // real layout: header covers common + count only
    f
}

#[test]
fn psp_plan_orders_and_checks() {
    use crate::psp_plan::*;
    let mut s = [Step::default(); MAX_STEPS];
    // File order (SOS first, as in psp_14_0_3_sos.bin) is not load order.
    let f = psp_file(&[ty::SOS, ty::SYS_DRV, ty::KDB, ty::TOC, ty::SPL, ty::SOC_DRV, ty::INTF_DRV,
        ty::DBG_DRV, ty::RAS_DRV, ty::IPKEYMGR_DRV]);
    let n = plan(&f, &mut s).unwrap();
    let cmds: Vec<u32> = s[..n].iter().map(|x| x.cmd).collect();
    assert_eq!(cmds, [bl::LOAD_KEY_DATABASE, bl::LOAD_TOS_SPL_TABLE, bl::LOAD_SYSDRV, bl::LOAD_SOCDRV,
        bl::LOAD_INTFDRV, bl::LOAD_DBGDRV, bl::LOAD_RASDRV, bl::LOAD_IPKEYMGRDRV, bl::LOAD_SOSDRV]);
    // Regions point at each piece's own bytes.
    let base = 32 + 4 + 16 * 10; // ucode array follows the table
    assert_eq!((s[0].fw_type, s[0].offset, s[0].size), (ty::KDB, base as u32 + 16, 8));
    assert_eq!((s[8].fw_type, s[8].offset), (ty::SOS, base as u32));
    // SYS_DRV and SOS are required; duplicates are refused.
    assert_eq!(plan(&psp_file(&[ty::SOS, ty::KDB]), &mut s), Err(PlanError::Missing(ty::SYS_DRV)));
    assert_eq!(plan(&psp_file(&[ty::SYS_DRV]), &mut s), Err(PlanError::Missing(ty::SOS)));
    assert_eq!(plan(&psp_file(&[ty::SOS, ty::SYS_DRV, ty::SOS]), &mut s), Err(PlanError::Duplicate(ty::SOS)));
    // Every truncation is an error, never a panic.
    for n in 0..f.len() {
        let _ = plan(&f[..n], &mut s);
    }
}
