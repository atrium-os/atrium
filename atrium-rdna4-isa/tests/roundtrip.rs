//! Every base-encoding form of every instruction must encode and decode back
//! to itself, with arbitrary values in its free fields.

use atrium_rdna4_isa::{decode, encode, is_base_condition, ENCODINGS, INSTRUCTIONS};

fn lcg(s: &mut u64) -> u64 {
    *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    *s >> 11
}

#[test]
fn tables_are_populated() {
    assert!(ENCODINGS.len() >= 40, "encodings: {}", ENCODINGS.len());
    assert!(INSTRUCTIONS.len() >= 1200, "instructions: {}", INSTRUCTIONS.len());
}

#[test]
fn base_encodings_round_trip() {
    let mut seed = 0x5eed_u64;
    let (mut ok, mut skipped) = (0, Vec::new());
    for inst in INSTRUCTIONS {
        for ie in inst.encodings.iter().filter(|e| is_base_condition(e.condition)) {
            let enc = &ENCODINGS[ie.encoding];
            if !enc.name.starts_with("ENC_") && enc.name != "VOP3_SDST_ENC" {
                continue;
            }
            for _ in 0..4 {
                let fields: Vec<(&str, u64)> = enc
                    .fields
                    .iter()
                    .filter(|f| f.name != "ENCODING" && f.name != "OP")
                    .filter(|f| {
                        let m = f.ranges.iter().fold(0u128, |m, &(o, c)| m | (((1u128 << c) - 1) << o));
                        m & enc.id_mask == 0
                    })
                    .map(|f| (f.name, lcg(&mut seed) & ((1u64 << f.width().min(63)) - 1)))
                    .collect();
                let words = match encode(inst.name, enc.name, "default", &fields) {
                    Ok(w) => w,
                    Err(e) => {
                        skipped.push(format!("{} {}: {:?}", inst.name, enc.name, e));
                        break;
                    }
                };
                let d = decode(&words).unwrap_or_else(|e| panic!("{} {}: decode {:?} of {:08x?}", inst.name, enc.name, e, words));
                assert_eq!(d.encoding, enc.name, "{}: {:08x?}", inst.name, words);
                // Aliased opcodes decode to the first table entry with that opcode.
                let same_opcode = INSTRUCTIONS.iter().any(|i| i.name == d.inst
                    && i.encodings.iter().any(|e| e.encoding == ie.encoding && e.opcode == ie.opcode));
                assert!(same_opcode, "{} decoded as {}", inst.name, d.inst);
                for (n, v) in &fields {
                    let got = d.fields.iter().find(|(m, _)| m == n).map(|x| x.1);
                    assert_eq!(got, Some(*v), "{} {} field {}", inst.name, enc.name, n);
                }
                ok += 1;
            }
        }
    }
    eprintln!("round-tripped {ok} encodings; {} not encodable", skipped.len());
    assert!(skipped.is_empty(), "not encodable: {:#?}", &skipped[..skipped.len().min(20)]);
    assert!(ok > 3000);
}

#[test]
fn known_encodings() {
    // s_endpgm: SOPP opcode 48 -> 0xbfb00000.
    assert_eq!(encode("S_ENDPGM", "ENC_SOPP", "default", &[]).unwrap(), vec![0xbfb0_0000]);
    // v_mov_b32 v0, v1 (VOP1, src0 = 256 + 1 for v1) -> 0x7e000301, as LLVM's
    // AMDGPU assembler emits it.
    assert_eq!(
        encode("V_MOV_B32", "ENC_VOP1", "default", &[("VDST", 0), ("SRC0", 257)]).unwrap(),
        vec![0x7e00_0301]
    );
    // s_mov_b32 s0, s1 -> 0xbe800001 (SOP1 opcode 0).
    assert_eq!(encode("S_MOV_B32", "ENC_SOP1", "default", &[("SDST", 0), ("SSRC0", 1)]).unwrap(), vec![0xbe80_0001]);
    let d = decode(&[0xbfb0_0000]).unwrap();
    assert_eq!(d.inst, "S_ENDPGM");
}
