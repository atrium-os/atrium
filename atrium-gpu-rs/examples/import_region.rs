//! `examples/import_region` — two-process token-based BO sharing.
//!
//! Mode 1 (mint):
//!   atrium-gpu-rs import_region mint
//!
//! Allocates a 4 KiB BO, writes "atrium-import-test\n" + a sequence of
//! bytes into it, mints a token, prints the token (hex) on stdout, and
//! sleeps until SIGINT so the BO's minter-ref stays alive long enough
//! for the importer to map.
//!
//! Mode 2 (import):
//!   atrium-gpu-rs import_region import <hex-token>
//!
//! Resolves the token via `IOC_GPU_IMPORT_REGION`, mmaps the BO, and
//! verifies the payload bytes. Exits 0 if they match, 1 otherwise.
//!
//! Mode 3 (import-park):
//!   atrium-gpu-rs import_region import-park <hex-token>
//!
//! Like `import`, but parks after verifying — keeps the importer's
//! ref on the BO alive. Used to test the refcount-survives-minter
//! property: kill the minter, then verify a fresh `import` still
//! succeeds because the importer-park process is still holding a
//! ref.
//!
//! This is the canonical Atrium cross-process memory-sharing pattern:
//! a host endpoint mints, a jailed client imports, neither side has
//! to share an address space.

use atrium_gpu::{abi, Gpu};
use std::io::Write;

fn payload(buf: &mut [u8]) {
    let prefix = b"atrium-import-test\n";
    buf[..prefix.len()].copy_from_slice(prefix);
    for (i, b) in buf.iter_mut().enumerate().skip(prefix.len()) {
        *b = (i & 0xff) as u8;
    }
}

fn main() -> std::io::Result<()> {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let gpu = Gpu::open()?;

    match mode.as_str() {
        "mint" => {
            let mut bo = gpu.alloc(4096, 0)?;
            payload(bo.as_mut_slice());
            let token = gpu.mint_token(&bo)?;
            let hex: String = token.iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            println!("{hex}");
            std::io::stdout().flush()?;
            eprintln!("minter: holding BO; press Ctrl-C or kill to exit");
            // Park until killed so the BO survives for the importer.
            std::thread::park();
            Ok(())
        }
        "import" | "import-park" => {
            let park = mode == "import-park";
            let hex = std::env::args().nth(2)
                .expect("usage: import_region import <hex-token>");
            if hex.len() != 2 * abi::ATRIUM_GPU_TOKEN_LEN {
                eprintln!("token must be {} hex chars (got {})",
                    2 * abi::ATRIUM_GPU_TOKEN_LEN, hex.len());
                std::process::exit(2);
            }
            let mut token = [0u8; abi::ATRIUM_GPU_TOKEN_LEN];
            for i in 0..abi::ATRIUM_GPU_TOKEN_LEN {
                token[i] = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16)
                    .expect("bad hex");
            }
            let bo = gpu.import_region(&token)?;
            let actual = bo.as_slice();
            let mut expected = vec![0u8; actual.len()];
            payload(&mut expected);
            if actual == expected.as_slice() {
                println!("import OK: {} bytes match", actual.len());
                std::io::stdout().flush()?;
                if park {
                    eprintln!("importer: holding ref; press Ctrl-C or kill to exit");
                    std::thread::park();
                }
                Ok(())
            } else {
                eprintln!("import MISMATCH at offset {}",
                    actual.iter().zip(&expected).position(|(a, b)| a != b)
                        .unwrap_or(0));
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!("usage: import_region mint | import <hex-token> | import-park <hex-token>");
            std::process::exit(2);
        }
    }
}
