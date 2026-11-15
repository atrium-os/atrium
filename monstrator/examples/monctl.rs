//! Ask Monstrator to open a link (tests): `monctl <url> [token hex]`.
//! Prints `opened` or `not opened: <why>`; exit 0 only when opened.
fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let Some(url) = a.first() else {
        eprintln!("usage: monctl <url> [32 hex digits]");
        std::process::exit(2);
    };
    let mut t = [0u8; 16];
    if let Some(h) = a.get(1) {
        for (i, b) in t.iter_mut().enumerate() {
            *b = h.get(2 * i..2 * i + 2).and_then(|x| u8::from_str_radix(x, 16).ok()).unwrap_or(0);
        }
    }
    match monstrator::open_link(&monstrator::socket(), url, t, std::time::Duration::from_secs(15)) {
        Ok(Ok(())) => println!("opened"),
        Ok(Err(why)) => {
            println!("not opened: {why}");
            std::process::exit(1);
        }
        Err(e) => {
            println!("not opened: {e}");
            std::process::exit(1);
        }
    }
}
