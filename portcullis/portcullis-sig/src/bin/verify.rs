//! atrium-verify-manifest — the manifest-trust gate as a tool.
//!
//! Exit 0 = a trusted publisher signed this manifest (launch may proceed);
//! non-zero = unsigned / tampered / untrusted (Portcullis must refuse). This is
//! the check Portcullis runs before honouring a manifest's capabilities.
//!
//! usage: atrium-verify-manifest <manifest> <sig> <trusted-pubkey.pem>...
//!   <sig> is the DER signature (openssl/cosign) — or base64 text, auto-detected.

use std::process::exit;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: {} <manifest> <sig> <trusted-pubkey.pem>...", args[0]);
        exit(2);
    }
    let manifest = match std::fs::read(&args[1]) {
        Ok(b) => b,
        Err(e) => { eprintln!("verify: read manifest {}: {e}", args[1]); exit(2); }
    };
    let sig_raw = match std::fs::read(&args[2]) {
        Ok(b) => b,
        Err(e) => { eprintln!("verify: read sig {}: {e}", args[2]); exit(2); }
    };
    // accept a DER signature directly, or base64 text.
    let sig_der = if let Ok(s) = std::str::from_utf8(&sig_raw) {
        portcullis_sig::sig_from_base64(s).unwrap_or_else(|_| sig_raw.clone())
    } else {
        sig_raw.clone()
    };
    let mut trusted = Vec::new();
    for p in &args[3..] {
        match std::fs::read_to_string(p) {
            Ok(pem) => trusted.push(pem),
            Err(e) => { eprintln!("verify: read pubkey {p}: {e}"); exit(2); }
        }
    }

    match portcullis_sig::verify_trusted(&manifest, &sig_der, &trusted) {
        Ok(()) => {
            println!("verify: OK — manifest {} signed by a trusted publisher", args[1]);
            exit(0);
        }
        Err(e) => {
            eprintln!("verify: REFUSED — manifest {} is {:?}", args[1], e);
            exit(1);
        }
    }
}
