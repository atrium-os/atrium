//! The worker of scripts/vm-workers-check.sh.
use std::io::Read;
use atrium_worker::Message;

#[derive(Message, Debug)]
#[message(version = 1)]
pub struct Ask {
    #[message(max_len = 32)]
    pub what: String,
}

#[derive(Message, Debug)]
#[message(version = 1)]
pub struct Answer {
    #[message(max_len = 4096)]
    pub text: String,
}

fn handle(a: Ask, f: Option<std::fs::File>) -> Result<Answer, String> {
    let text = match a.what.as_str() {
        "uid" => format!("{}", unsafe { libc::getuid() }),
        "read" => {
            let mut s = String::new();
            f.ok_or("no file")?.read_to_string(&mut s).map_err(|e| e.to_string())?;
            s.lines().next().unwrap_or("").to_string()
        }
        "net" => match std::net::TcpStream::connect_timeout(&"1.1.1.1:80".parse().unwrap(), std::time::Duration::from_secs(3)) {
            Ok(_) => "connected".into(),
            Err(e) => format!("refused: {e}"),
        },
        "nest" => match atrium_worker::Workers::get().and_then(|w| w.call::<Ask, Answer>("echo", &Ask { what: "uid".into() }, None)) {
            Ok(a) => format!("called: {}", a.text),
            Err(e) => format!("refused: {e}"),
        },
        // Past its 128M: rctl ends the jail.
        "hog" => {
            let mut v: Vec<Vec<u8>> = Vec::new();
            for _ in 0..64 { v.push(vec![1u8; 16 << 20]); }
            format!("held {} MiB", v.iter().map(|b| b.len()).sum::<usize>() >> 20)
        }
        "ifs" => net_view(),
        // The UTC offset localtime(3) gives here, as date +%z would print it.
        "tz" => {
            extern "C" { fn tzset(); }
            unsafe { tzset() };
            let now = unsafe { libc::time(std::ptr::null_mut()) };
            let mut tm: libc::tm = unsafe { std::mem::zeroed() };
            unsafe { libc::localtime_r(&now, &mut tm) };
            let off = tm.tm_gmtoff / 60;
            let z = format!("{}{:02}{:02}", if off < 0 { '-' } else { '+' }, off.abs() / 60, off.abs() % 60);
            match std::fs::metadata("/etc/localtime") {
                Ok(m) if m.len() > 0 => z,
                Ok(m) => format!("{z} (/etc/localtime is {} bytes)", m.len()),
                Err(e) => format!("{z} (/etc/localtime: {e})"),
            }
        }
        "fail" => return Err("asked to fail".into()),
        "crash" => std::process::exit(3),
        "sleep" => { std::thread::sleep(std::time::Duration::from_secs(60)); String::new() }
        w => return Err(format!("unknown {w:?}")),
    };
    Ok(Answer { text })
}

/// What of the host's network the worker can see: the interfaces
/// getifaddrs lists (names, and link-layer addresses that aren't zero), the
/// interface MIB sysctl, SIOCGIFCONF on a local socket, and which socket
/// families open. A network-less worker sees at most its own lo0, no
/// address, and only AF_UNIX sockets.
#[cfg(target_os = "freebsd")]
fn net_view() -> String {
    use std::ffi::CStr;
    let mut names: Vec<String> = Vec::new();
    let mut macs: Vec<String> = Vec::new();
    let mut ifa: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills ifa with a list freed by freeifaddrs below;
    // each entry and its address are read only while the list lives.
    let gia = unsafe { libc::getifaddrs(&mut ifa) };
    if gia == 0 {
        let mut p = ifa;
        while !p.is_null() {
            let e = unsafe { &*p };
            let n = unsafe { CStr::from_ptr(e.ifa_name) }.to_string_lossy().into_owned();
            if !names.contains(&n) { names.push(n.clone()) }
            if !e.ifa_addr.is_null() && unsafe { (*e.ifa_addr).sa_family } as i32 == libc::AF_LINK {
                let dl = unsafe { &*(e.ifa_addr as *const libc::sockaddr_dl) };
                let (nl, al) = (dl.sdl_nlen as usize, dl.sdl_alen as usize);
                let a: Vec<u8> = dl.sdl_data.iter().skip(nl).take(al).map(|c| *c as u8).collect();
                if a.iter().any(|b| *b != 0) {
                    macs.push(format!("{n}={}", a.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")));
                }
            }
            p = e.ifa_next;
        }
        unsafe { libc::freeifaddrs(ifa) };
    }
    // net.link.generic.ifdata.1.general: interface 1's MIB data.
    let mib = [libc::CTL_NET, 18 /* AF_LINK */, 0 /* NETLINK_GENERIC */, 2 /* IFMIB_IFDATA */, 1, 1 /* IFDATA_GENERAL */];
    let mut buf = [0u8; 1024];
    let mut len = buf.len();
    // SAFETY: buf and len describe a writable buffer.
    let mibr = unsafe { libc::sysctl(mib.as_ptr(), mib.len() as u32, buf.as_mut_ptr().cast(), &mut len, std::ptr::null(), 0) };
    let ifmib = if mibr == 0 { format!("read {len}") } else { "refused".into() };
    // SIOCGIFCONF on an AF_UNIX socket (the one family the worker has).
    const SIOCGIFCONF: libc::c_ulong = 0xc010_6924;
    #[repr(C)] struct IfConf { len: libc::c_int, buf: *mut libc::c_char }
    let mut ifbuf = [0u8; 4096];
    let mut ifc = IfConf { len: ifbuf.len() as libc::c_int, buf: ifbuf.as_mut_ptr().cast() };
    let ifconf = unsafe {
        let s = libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM, 0);
        let r = if s < 0 { -1 } else { libc::ioctl(s, SIOCGIFCONF, &mut ifc as *mut IfConf) };
        if s >= 0 { libc::close(s); }
        if r == 0 { format!("{} bytes", ifc.len) } else { "refused".into() }
    };
    // The host's network counters and settings under net. (here the IP
    // statistics): an empty vnet showed its own, a netblind jail none.
    let mut st = [0u8; 4096];
    let mut stlen = st.len();
    // SAFETY: st and stlen describe a writable buffer.
    let ipstats = if unsafe { libc::sysctlbyname(c"net.inet.ip.stats".as_ptr(), st.as_mut_ptr().cast(), &mut stlen, std::ptr::null(), 0) } == 0 {
        format!("read {stlen}") } else { "refused".into() };
    let mut open = Vec::new();
    for (name, af) in [("inet", libc::AF_INET), ("inet6", libc::AF_INET6), ("route", libc::AF_ROUTE), ("netlink", 38)] {
        // SAFETY: plain socket(2); closed at once.
        let s = unsafe { libc::socket(af, if af == 38 { libc::SOCK_RAW } else { libc::SOCK_DGRAM }, 0) };
        if s >= 0 { open.push(name); unsafe { libc::close(s) }; }
    }
    format!("ifs=[{}] macs=[{}] getifaddrs={} ifmib={ifmib} ifconf={ifconf} ipstats={ipstats} sockets=[{}]",
        names.join(","), macs.join(","), if gia == 0 { "ok" } else { "refused" }, open.join(","))
}

#[cfg(not(target_os = "freebsd"))]
fn net_view() -> String { "not freebsd".into() }

atrium_worker::main!(handle);
