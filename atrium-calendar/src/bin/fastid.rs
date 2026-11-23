//! Trusted launch configuration supplies one store, account owner, persona and publisher.
//! The socket directory and store must be separate, service-owned mounts.
use atrium_calendar::{store::Store, wire};
use portcullis_peer::{AppRegistry, DEFAULT_REGISTRY};
use std::{
    env, fs, io,
    os::{
        fd::AsRawFd,
        unix::{fs::FileTypeExt, net::UnixListener},
    },
    path::Path,
};

fn required(key: &str) -> io::Result<String> {
    env::var(key)
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| io::Error::other(format!("missing {key}")))
}

fn main() -> io::Result<()> {
    let user = required("FASTI_USER")?;
    let name = required("FASTI_PERSONA")?;
    let persona = match name.as_str() {
        "personal" => None,
        named if portcullis_peer::valid_person_persona(named) => Some(named),
        _ => return Err(io::Error::other("Invalid Fasti persona")),
    };
    let hex = required("FASTI_PUBLISHER")?;
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(io::Error::other("Invalid Calendar publisher"));
    }
    let mut publisher = [0; 32];
    for (i, b) in publisher.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap();
    }
    let registry = env::var("FASTI_REGISTRY").unwrap_or_else(|_| DEFAULT_REGISTRY.into());
    let mut store = Store::open(Path::new(&required("FASTI_STORE")?))?;
    let socket = required("FASTI_SOCKET")?;
    // One launcher-owned endpoint per store. Holding the store lock excludes a live predecessor.
    match fs::symlink_metadata(&socket) {
        Ok(m) if m.file_type().is_socket() => fs::remove_file(&socket)?,
        Ok(_) => return Err(io::Error::other("Expected a Calendar socket")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(socket)?;
    for conn in listener.incoming() {
        let mut conn = conn?;
        let Ok((uid, _)) = portcullis_peer::uid_gid(conn.as_raw_fd()) else {
            continue;
        };
        let Ok(bindings) = AppRegistry::load(&registry) else {
            eprintln!("fastid: Calendar launch registry unavailable");
            continue;
        };
        if bindings.uid_for_instance(&user, "org.atrium.calendar", Some(&publisher), persona)
            == Some(uid)
        {
            if let Err(error) = wire::serve(&mut conn, &mut store) {
                eprintln!("fastid: reply failed ({:?})", error.kind());
            }
        }
    }
    Ok(())
}
