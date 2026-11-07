#[path = "common/vectors.rs"]
mod vectors;
use vectors::VECTORS;
use vinculum_smp::crypto::*;

fn a<const N: usize>(b: &[u8]) -> [u8; N] {
    b.try_into().unwrap()
}

#[test]
fn matches_bumble() {
    let mut counts = std::collections::BTreeMap::new();
    for v in VECTORS {
        let i = v.inputs;
        let got: Vec<u8> = match v.name {
            "aes_cmac" => {
                // Bumble's aes_cmac takes spec-order bytes, as ours does.
                aes_cmac(&a(i[0]), i[1]).to_vec()
            }
            "f4" => f4(&a(i[0]), &a(i[1]), &a(i[2]), i[3][0]).to_vec(),
            "f5" => {
                let (mk, ltk) = f5(&a(i[0]), &a(i[1]), &a(i[2]), &a(i[3]), &a(i[4]));
                [mk, ltk].concat()
            }
            "f6" => f6(&a(i[0]), &a(i[1]), &a(i[2]), &a(i[3]), &a(i[4]), &a(i[5]), &a(i[6])).to_vec(),
            "g2" => g2(&a(i[0]), &a(i[1]), &a(i[2]), &a(i[3])).to_be_bytes().to_vec(),
            "ah" => ah(&a(i[0]), &a(i[1])).to_vec(),
            "ecdh" => test_dh(&a(i[0]), &a(i[1])).expect("valid point").1.to_vec(),
            "pubkey" => {
                // pubkey vectors: check by DH with ourselves being well-formed
                let (pk, _) = test_dh(&a(i[0]), &a(v.out)).expect("own key on curve");
                pk.to_vec()
            }
            other => panic!("unknown vector {other}"),
        };
        assert_eq!(got, v.out, "{} vector", v.name);
        *counts.entry(v.name).or_insert(0) += 1;
    }
    assert_eq!(counts.len(), 8, "{counts:?}");
}

#[test]
fn ecdh_round_trip_and_attacks() {
    let mut x = KeyPair::generate();
    let mut y = KeyPair::generate();
    let (xp, yp) = (x.public, y.public);
    assert_eq!(x.dhkey(&yp).unwrap(), y.dhkey(&xp).unwrap());
    let mut z = KeyPair::generate();
    let zp = z.public;
    assert_eq!(z.dhkey(&zp), Err(EcdhError::Reflected));
    // a point off the curve: flip one bit of y
    let mut bad = yp;
    bad[40] ^= 1;
    assert_eq!(z.dhkey(&bad), Err(EcdhError::InvalidPoint));
    // the secret is single-use
    assert_eq!(x.dhkey(&yp), Err(EcdhError::Used));
}

/// The f4 sample data from Core 6.0 Vol 3 Part H §D.2 (spec order there,
/// reversed here to on-air order), as an anchor independent of Bumble.
#[test]
fn spec_sample_f4() {
    fn le<const N: usize>(spec_hex: &str) -> [u8; N] {
        let mut v: Vec<u8> = (0..spec_hex.len()).step_by(2).map(|i| u8::from_str_radix(&spec_hex[i..i + 2], 16).unwrap()).collect();
        v.reverse();
        v.try_into().unwrap()
    }
    let u = le::<32>("20b003d2f297be2c5e2c83a7e9f9a5b9eff49111acf4fddbcc0301480e359de6");
    let v = le::<32>("55188b3d32f6bb9a900afcfbeed4e72a59cb9ac2f19d7cfb6b4fdd49f47fc5fd");
    let x = le::<16>("d5cb8454d177733effffb2ec712baeab");
    assert_eq!(f4(&u, &v, &x, 0), le::<16>("f2c916f107a9bd1cf1eda1bea974872d"));
}
