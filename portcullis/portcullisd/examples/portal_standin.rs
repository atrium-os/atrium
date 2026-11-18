//! portal_standin: atrium-net's `network-portal` socket for
//! scripts/navigator-confined-vm.sh's sign-in window section (atrium-net.md
//! §8.1 S3), where no real portal LAN is plugged in. It answers
//! `PORTAL_INFO` with a Wi-Fi network called "Hotel Guest", the start page
//! and DNS server given on the command line, and online once the file
//! given exists; `PORTAL_WATCH` is logged. Everything else is refused.
//!
//!   portal_standin <dir> <start url> <dns server> <online file>
//!
//! portcullisd is pointed at `<dir>` with PORTCULLISD_NETCONF_DIR.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::mpsc;

use atrium_netconf::server::{serve, Backend};
use atrium_netconf::{status, Fail, Grant, Iface, Join, Nearby, Online, PortalInfo, Profile, Res, Routes, SetStatic, Status};

struct Standin {
    start: String,
    dns: String,
    online: PathBuf,
}

fn no<T>() -> Res<T> {
    Err(Fail(status::FAILED, "a stand-in".into()))
}

impl Backend for Standin {
    fn status(&self) -> Res<Status> { no() }
    fn subscribe(&self) -> Res<mpsc::Receiver<Status>> { no() }
    fn interfaces(&self) -> Res<Vec<Iface>> { no() }
    fn routes(&self) -> Res<Routes> { no() }
    fn profiles(&self) -> Res<Vec<Profile>> { no() }
    fn join(&self, _j: Join) -> Res<()> { no() }
    fn leave(&self) -> Res<()> { no() }
    fn forget(&self, _ssid: Vec<u8>) -> Res<()> { no() }
    fn set_profile(&self, _p: Profile) -> Res<()> { no() }
    fn nearby(&self) -> Res<Vec<Nearby>> { no() }
    fn set_country(&self, _cc: String) -> Res<()> { no() }
    fn set_static(&self, _s: SetStatic) -> Res<()> { no() }
    fn set_dns(&self, _servers: Vec<std::net::IpAddr>) -> Res<()> { no() }
    fn portal_info(&self) -> Res<PortalInfo> {
        let online = if self.online.exists() { Online::Yes } else { Online::Captive };
        eprintln!("portal_standin: PORTAL_INFO ({online:?})");
        Ok(PortalInfo { iface: "vtnet0".into(), ssid: Some(b"Hotel Guest".to_vec()), online, portal: Some(self.start.clone()),
                        router: None, dns: vec![self.dns.clone()], watching: false })
    }
    fn portal_watch(&self, on: bool) -> Res<()> {
        eprintln!("portal_standin: PORTAL_WATCH {}", if on { "on" } else { "off" });
        Ok(())
    }
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let [dir, start, dns, online] = &a[..] else {
        eprintln!("portal_standin <dir> <start url> <dns server> <online file>");
        std::process::exit(2)
    };
    let path = atrium_netconf::client::Client::path(std::path::Path::new(dir), Grant::Portal);
    let parent = path.parent().expect("a directory");
    std::fs::create_dir_all(parent).expect("socket directory");
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).expect("0700");
    let _ = std::fs::remove_file(&path);
    let l = UnixListener::bind(&path).expect("bind");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("0600");
    eprintln!("portal_standin: on {}", path.display());
    let b: &'static Standin = Box::leak(Box::new(Standin { start: start.clone(), dns: dns.clone(), online: PathBuf::from(online) }));
    for s in l.incoming().flatten() {
        std::thread::spawn(move || {
            if let Ok(c) = aqueduct::Connection::wrap(s) {
                let _ = serve(c, Grant::Portal, b);
            }
        });
    }
}
