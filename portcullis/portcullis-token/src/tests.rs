//! Against SoftHSM (a software PKCS#11 token), when it is installed:
//! `apt install softhsm2` / `brew install softhsm`.

use super::*;

const MODULES: [&str; 4] = ["/usr/lib/softhsm/libsofthsm2.so", "/usr/local/lib/softhsm/libsofthsm2.so",
                            "/opt/homebrew/lib/softhsm/libsofthsm2.so", "/usr/lib/x86_64-linux-gnu/softhsm/libsofthsm2.so"];

fn pin(_: &str) -> Result<String, String> { Ok("123456".into()) }

/// One token "atrium-test" with a P-256 key pair at id `01`.
fn soft_token() -> Option<(String, KeyRef)> {
    let module = MODULES.iter().find(|m| std::path::Path::new(m).exists())?.to_string();
    let dir = std::env::temp_dir().join(format!("portcullis-token-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("tokens")).unwrap();
    std::fs::write(dir.join("softhsm2.conf"),
                   format!("directories.tokendir = {}\nobjectstore.backend = file\nlog.level = ERROR\n",
                           dir.join("tokens").display())).unwrap();
    std::env::set_var("SOFTHSM2_CONF", dir.join("softhsm2.conf"));
    let p = Pkcs11::new(&module).unwrap();
    p.initialize(CInitializeArgs::new(CInitializeFlags::OS_LOCKING_OK)).unwrap();
    let slot = p.get_all_slots().unwrap()[0];
    p.init_token(slot, &AuthPin::from("so-pin-1".to_string()), "atrium-test").unwrap();
    let slot = p.get_slots_with_initialized_token().unwrap()[0];
    let s = p.open_rw_session(slot).unwrap();
    s.login(UserType::So, Some(&AuthPin::from("so-pin-1".to_string()))).unwrap();
    s.init_pin(&AuthPin::from("123456".to_string())).unwrap();
    s.logout().unwrap();
    s.login(UserType::User, Some(&AuthPin::from("123456".to_string()))).unwrap();
    s.generate_key_pair(&Mechanism::EccKeyPairGen,
        &[Attribute::Token(true), Attribute::EcParams(P256_PARAMS.to_vec()), Attribute::Id(vec![1]),
          Attribute::Label(b"root1".to_vec()), Attribute::Verify(true)],
        &[Attribute::Token(true), Attribute::Private(true), Attribute::Sensitive(true), Attribute::Sign(true),
          Attribute::Id(vec![1]), Attribute::Label(b"root1".to_vec())]).unwrap();
    let serial = p.get_token_info(slot).unwrap().serial_number().trim().to_string();
    drop(s);
    p.finalize().unwrap();
    Some((module.clone(), KeyRef { format: 1, module, serial, id: "01".into() }))
}

#[test]
fn refs_parse() {
    assert_eq!(KeyRef::parse("-----BEGIN PRIVATE KEY-----\n"), None);
    let r = KeyRef { format: 1, module: "/x/libykcs11.dylib".into(), serial: "31234567".into(), id: "01".into() };
    assert_eq!(KeyRef::parse(&r.text()), Some(Ok(r.clone())));
    assert!(KeyRef::parse("atrium-token-key = 2\nmodule = \"m\"\nserial = \"s\"\nid = \"01\"\n").unwrap().is_err());
    assert!(KeyRef::parse("atrium-token-key = 1\nmodule = \"m\"\nserial = \"s\"\nid = \"0x1\"\n").unwrap().is_err());
    assert!(KeyRef::parse("atrium-token-key = 1\nmodule = \"m\"\nserial = \"s\"\nid = \"01\"\npin = \"1\"\n").unwrap().is_err());
}

/// The whole path on a software token: listing, the public key, signing with
/// the right and a wrong PIN, and a catalog root signed through
/// `portcullis_sig` with the reference file's text as the key.
#[test]
fn signs_on_a_software_token() {
    let Some((module, r)) = soft_token() else {
        eprintln!("SKIP: no SoftHSM (apt install softhsm2)");
        return;
    };
    let listed = list(&module).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].label, "atrium-test");
    assert_eq!(listed[0].keys.len(), 1);
    let (id, label, pem) = listed[0].keys[0].clone();
    assert_eq!((id.as_str(), label.as_str()), ("01", "root1"));
    assert_eq!(public_pem(&r).unwrap(), pem);

    let sig = sign_with_pin(&r, b"root.toml text", "123456").unwrap();
    portcullis_sig::verify_one(b"root.toml text", &sig, &pem).unwrap();
    assert!(portcullis_sig::verify_one(b"other text", &sig, &pem).is_err());
    assert!(sign_with_pin(&r, b"x", "000000").unwrap_err().contains("wrong PIN"));
    let gone = KeyRef { serial: "nope".into(), ..r.clone() };
    assert!(sign_with_pin(&gone, b"x", "123456").unwrap_err().contains("not plugged in"));
    let no_key = KeyRef { id: "02".into(), ..r.clone() };
    assert!(sign_with_pin(&no_key, b"x", "123456").is_err());

    // Through portcullis_sig and the catalog's root signing, as `opifex catalog root-sign` does.
    assert!(portcullis_sig::sign(b"x", &r.text()).is_err(), "no signer installed yet");
    install_with_pins(pin);
    assert_eq!(portcullis_sig::public_pem(&r.text()).unwrap(), pem);
    let (_, cat) = portcullis_sig::new_key([5; 32]).unwrap();
    let text = format!("schema = \"atrium-catalog-root/1\"\nname = \"T\"\nversion = 1\nexpires = 4000000000\nthreshold = 1\n\n\
                        [[key]]\npem = \"\"\"{pem}\"\"\"\nroles = [\"root\"]\n\n[[key]]\npem = \"\"\"{cat}\"\"\"\nroles = [\"catalog\"]\n");
    let root = opifex_catalog::Root::parse(&text).unwrap();
    let line = opifex_catalog::sign_root(&text, &r.text(), &pem).unwrap();
    assert_eq!(root.count_signers(&text, &line), 1);
    assert_eq!(root.count_signers(&format!("{text}\n"), &line), 0);
    // A PEM key still signs as before, with the token signer installed.
    let (sk, pk) = portcullis_sig::new_key([3; 32]).unwrap();
    portcullis_sig::verify_one(b"y", &portcullis_sig::sign(b"y", &sk).unwrap(), &pk).unwrap();
}
