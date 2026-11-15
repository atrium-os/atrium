//! `crypt_tool dec IN PASSWORD OUT` / `crypt_tool enc IN PASSWORD OUT`:
//! decrypt or encrypt an OOXML package (MS-OFFCRYPTO) or an ODF one, for gates.
fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() != 5 {
        eprintln!("usage: crypt_tool dec|enc IN PASSWORD OUT");
        std::process::exit(2);
    }
    let input = std::fs::read(&a[2]).expect("read");
    let out = match a[1].as_str() {
        "dec" if officina_convert::cfb::is_cfb(&input) => officina_convert::crypt::decrypt(&input, &a[3]),
        "dec" => officina_convert::odfcrypt::decrypt(&input, &a[3]),
        "enc" if a[2].ends_with(".odt") || a[2].ends_with(".ods") || a[2].ends_with(".odp") => officina_convert::odfcrypt::encrypt(&input, &a[3]),
        "enc" => Ok(officina_convert::crypt::encrypt(&input, &a[3])),
        _ => panic!("dec or enc"),
    };
    match out {
        Ok(b) => std::fs::write(&a[4], b).expect("write"),
        Err(e) => {
            eprintln!("{e:?}");
            std::process::exit(1);
        }
    }
}
