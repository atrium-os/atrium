//! Trusted launch configuration supplies one store, account owner, persona and publisher.
//! Installed instances accept on the listener inherited from the supervisor.
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
    let args: Vec<_> = env::args().skip(1).collect();
    let installed = !args.is_empty();
    let (user, name, hex, registry, store_dir, socket) = match args.as_slice() {
        [mode, user, name, signer] if mode == "--installed" => (
            user.clone(),
            name.clone(),
            signer.clone(),
            "/registry".into(),
            "/store".into(),
            "/endpoint/calendar.sock".into(),
        ),
        [] => (
            required("FASTI_USER")?,
            required("FASTI_PERSONA")?,
            required("FASTI_PUBLISHER")?,
            env::var("FASTI_REGISTRY").unwrap_or_else(|_| DEFAULT_REGISTRY.into()),
            required("FASTI_STORE")?,
            required("FASTI_SOCKET")?,
        ),
        _ => return Err(io::Error::other("Invalid Fasti launch arguments")),
    };
    let persona = match name.as_str() {
        "personal" => None,
        named if portcullis_peer::valid_person_persona(named) => Some(named),
        _ => return Err(io::Error::other("Invalid Fasti persona")),
    };
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(io::Error::other("Invalid Calendar publisher"));
    }
    let mut publisher = [0; 32];
    for (i, b) in publisher.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap();
    }
    let mut store = Store::open(Path::new(&store_dir))?;
    let listener = if installed {
        let (listener, origin) = atrium_listen::listener(&socket)?;
        if origin != atrium_listen::Origin::Inherited {
            return Err(io::Error::other("Fasti requires its supervisor's socket"));
        }
        listener
    } else {
        // A standalone test owns both this endpoint directory and the store lock.
        match fs::symlink_metadata(&socket) {
            Ok(m) if m.file_type().is_socket() => fs::remove_file(&socket)?,
            Ok(_) => return Err(io::Error::other("Expected a Calendar socket")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        UnixListener::bind(socket)?
    };
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
