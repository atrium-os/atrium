//! `hospes-install`: put Widevine where Hospes loads it from (docs/spec/hospes.md §12).
//!
//!   hospes-install widevine-index INRELEASE PACKAGES [amd64|arm64]
//!       verify Google's signed index; print the package to fetch: path, size, sha256, version
//!       (another machine's index if named: the installing machine verifies again)
//!   hospes-install widevine-deb INRELEASE PACKAGES DEB DEST
//!       verify the chain to DEB, take out the CDM and its manifest, install into DEST
//!   hospes-install widevine-file LIB [MANIFEST] DEST
//!       the user's own libwidevinecdm.so (and manifest.json), checked and installed
//!   hospes-install widevine-check INRELEASE PACKAGES DEB
//!       as widevine-deb, but install nothing: print the CDM's version and import count
//!   hospes-install check-imports LIB
//!       any library Hospes hosts (OpenH264 too): every strong import provided by the shim
//!
//! The last two are the version watch (docs/spec/hospes.md §11.4, .github/workflows/
//! hospes-version-watch.yml).
//!
//! The files come from the caller (the system fetches them through Nuntius). Nothing from
//! the package or the library is run.
use hospes_install::{apt, check_library, extract_cdm, install, Error};
use std::path::Path;
use std::process::ExitCode;

const PACKAGE: &str = "google-chrome-stable";

fn read(p: &str) -> Result<Vec<u8>, Error> {
    std::fs::read(p).map_err(|e| Error::Io(format!("{p}: {e}")))
}

fn run(args: &[String]) -> Result<(), Error> {
    let text = |p: &str| -> Result<String, Error> { String::from_utf8(read(p)?).map_err(|_| Error::Io(format!("{p}: not text"))) };
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["widevine-index", inrelease, packages] => {
            let p = apt::verify_apt(&text(inrelease)?, &read(packages)?, PACKAGE)?;
            println!("{} {} {} {}", p.filename, p.size, p.sha256, p.version);
            Ok(())
        }
        ["widevine-index", inrelease, packages, arch] => {
            let p = apt::verify_apt_for(&text(inrelease)?, &read(packages)?, PACKAGE, arch)?;
            println!("{} {} {} {}", p.filename, p.size, p.sha256, p.version);
            Ok(())
        }
        ["widevine-deb", inrelease, packages, deb, dest] => {
            let p = apt::verify_apt(&text(inrelease)?, &read(packages)?, PACKAGE)?;
            let deb = read(deb)?;
            apt::check_deb(&p, &deb)?;
            let x = extract_cdm(&deb)?;
            let dir = install(Path::new(dest), &x.library, Some(&x.manifest), false)?;
            println!("installed Widevine from {} {} into {}", p.name, p.version, dir.display());
            Ok(())
        }
        ["widevine-check", inrelease, packages, deb] => {
            let p = apt::verify_apt(&text(inrelease)?, &read(packages)?, PACKAGE)?;
            let deb = read(deb)?;
            apt::check_deb(&p, &deb)?;
            let x = extract_cdm(&deb)?;
            let info = check_library(&x.library)?;
            let v = hospes_install::manifest_version(&x.manifest).unwrap_or_else(|| "unknown".into());
            println!("{} {}: CDM {v}, {} imports, all provided, sha256 {}", p.name, p.version, info.imports, info.sha256);
            Ok(())
        }
        ["check-imports", lib] => {
            let info = hospes_install::check_imports(&read(lib)?)?;
            println!("{lib}: {} imports, all provided, sha256 {}", info.imports, info.sha256);
            Ok(())
        }
        ["widevine-file", lib, dest] | ["widevine-file", lib, _, dest] => {
            let manifest = if args.len() == 4 { Some(read(&args[2])?) } else { None };
            let lib = read(lib)?;
            let info = check_library(&lib)?;
            let dir = install(Path::new(dest), &lib, manifest.as_deref(), false)?;
            println!("installed {} ({} imports, all provided) into {}", info.sha256, info.imports, dir.display());
            Ok(())
        }
        _ => {
            eprintln!("usage: hospes-install widevine-index INRELEASE PACKAGES [ARCH] | widevine-deb INRELEASE PACKAGES DEB DEST | widevine-file LIB [MANIFEST] DEST | widevine-check INRELEASE PACKAGES DEB | check-imports LIB");
            Err(Error::Io("usage".into()))
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("hospes-install: {e}");
            ExitCode::FAILURE
        }
    }
}
