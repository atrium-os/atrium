//! An exec-only Navigator worker. Its installed bundle restricts the caller
//! to the broker. One Start opens the entry, followed by bounded anonymous
//! requests for scripts, styles and the Play icon on the same private socket.
use navigator_fetch::{addr::Policy, tls_config, youtube, Conversation};
use std::io::Write;

fn run() -> Result<(), String> {
    #[cfg(not(target_os = "freebsd"))]
    if std::env::args().nth(1).as_deref() != Some("--unconfined-for-testing") {
        return Err("the host control requires --unconfined-for-testing".into());
    }
    #[cfg(target_os = "freebsd")]
    let mut out = {
        use std::os::fd::AsFd;
        use std::os::unix::net::UnixStream;
        let fd = std::io::stdin()
            .as_fd()
            .try_clone_to_owned()
            .map_err(|e| e.to_string())?;
        let channel = UnixStream::from(fd);
        let timeout = Some(std::time::Duration::from_secs(10));
        channel
            .set_read_timeout(Some(navigator_control::youtube::PLAYER_WINDOW))
            .and_then(|_| channel.set_write_timeout(timeout))
            .map_err(|e| e.to_string())?;
        channel
    };
    #[cfg(not(target_os = "freebsd"))]
    let (mut out, url) = (
        std::io::stdout().lock(),
        std::env::args()
            .nth(2)
            .unwrap_or_else(|| navigator_control::YOUTUBE_ENTRY.into()),
    );
    let pem = std::fs::read("/etc/ssl/cert.pem").map_err(|e| e.to_string())?;
    let tls = tls_config(&pem)?;
    #[cfg(target_os = "freebsd")]
    let net = {
        use navigator_fetch::{sandbox, Locked};
        let net = sandbox::enter().map_err(|e| e.to_string())?;
        if !sandbox::in_capability_mode() {
            return Err("Capsicum entry failed".into());
        }
        Locked(std::sync::Mutex::new(net))
    };
    #[cfg(not(target_os = "freebsd"))]
    let net = navigator_fetch::Direct;
    let mut conversation = Conversation::new(&net, tls, Policy::default());
    #[cfg(not(target_os = "freebsd"))]
    return send(&mut out, &youtube::resource(&url, &mut conversation)?);
    #[cfg(target_os = "freebsd")]
    {
        use std::io::Read;
        let mut remaining = navigator_control::youtube::ASSET_BUDGET;
        use navigator_control::youtube_media as media;
        let mut issued = None::<String>;
        let mut media_posts = 0;
        let until = std::time::Instant::now() + navigator_control::youtube::PLAYER_WINDOW;
        let mut posts = 0;
        for step in 0..=navigator_control::youtube::ASSET_COUNT + media::REQUESTS {
            let mut kind = [0];
            if out.read(&mut kind).map_err(|e| e.to_string())? == 0 {
                break;
            }
            if kind[0] == 4 {
                if media_posts >= media::REQUESTS || std::time::Instant::now() >= until {
                    return Err("delivery window exhausted".into());
                }
                let mut read_part = |max: usize| -> Result<Vec<u8>, String> {
                    let mut len = [0; 2];
                    out.read_exact(&mut len).map_err(|e| e.to_string())?;
                    let n = u16::from_be_bytes(len) as usize;
                    if !(1..=max).contains(&n) {
                        return Err("invalid media field length".into());
                    }
                    let mut data = vec![0; n];
                    out.read_exact(&mut data).map_err(|e| e.to_string())?;
                    Ok(data)
                };
                let url = String::from_utf8(read_part(8192)?).map_err(|_| "invalid media URL")?;
                if !issued
                    .as_deref()
                    .is_some_and(|grant| media::permits(grant, &url))
                {
                    return Err("unissued media destination".into());
                }
                let body = read_part(media::BODY)?;
                let reply =
                    youtube::delivery(&url, &body, &mut conversation, remaining.min(media::REPLY))?;
                remaining -= reply.len();
                media_posts += 1;
                send(&mut out, &reply)?;
                continue;
            }
            if step - media_posts > navigator_control::youtube::ASSET_COUNT {
                return Err("startup request count exhausted".into());
            }
            if step > 0 && kind[0] == 3 {
                use navigator_control::youtube::{PLAYER_BODY_LIMIT, PLAYER_REQUESTS};
                if posts >= PLAYER_REQUESTS || remaining == 0 {
                    return Err("player budget exhausted".into());
                }
                let mut header = [0; 2];
                out.read_exact(&mut header).map_err(|e| e.to_string())?;
                let len = usize::from(u16::from_be_bytes(header));
                if !(1..=PLAYER_BODY_LIMIT).contains(&len) {
                    return Err("invalid player request size".into());
                }
                let mut bytes = vec![0; len];
                out.read_exact(&mut bytes).map_err(|e| e.to_string())?;
                let reply = youtube::player(&bytes, &mut conversation)?;
                remaining = remaining
                    .checked_sub(reply.len())
                    .ok_or("startup byte budget")?;
                issued = media::issued(&reply);
                posts += 1;
                send(&mut out, &reply)?;
                continue;
            }
            let url = match (step, kind[0]) {
                (0, 1) => navigator_control::YOUTUBE_ENTRY.to_owned(),
                (1.., 2) => {
                    let mut size = [0; 2];
                    out.read_exact(&mut size).map_err(|e| e.to_string())?;
                    let size = u16::from_be_bytes(size) as usize;
                    if size > 2200 || remaining == 0 {
                        return Err("startup grant exhausted".into());
                    }
                    let mut bytes = vec![0; size];
                    out.read_exact(&mut bytes).map_err(|e| e.to_string())?;
                    let url = String::from_utf8(bytes).map_err(|e| e.to_string())?;
                    navigator_control::youtube::asset_mime(&url).ok_or("invalid startup asset")?;
                    url
                }
                _ => return Err("invalid bootstrap sequence".into()),
            };
            let body = youtube::resource(&url, &mut conversation)?;
            if step > 0 {
                remaining = remaining
                    .checked_sub(body.len())
                    .ok_or("startup byte budget")?;
            }
            send(&mut out, &body)?;
        }
        Ok(())
    }
}

fn send(out: &mut impl Write, doc: &[u8]) -> Result<(), String> {
    out.write_all(&(doc.len() as u32).to_be_bytes())
        .and_then(|_| out.write_all(doc))
        .and_then(|_| out.flush())
        .map_err(|e| e.to_string())
}

fn main() {
    if let Err(reason) = run() {
        eprintln!("navigator-youtube-entry: {reason}");
        std::process::exit(1);
    }
}
