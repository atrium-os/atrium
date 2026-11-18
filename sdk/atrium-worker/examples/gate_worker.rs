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
        "fail" => return Err("asked to fail".into()),
        "crash" => std::process::exit(3),
        "sleep" => { std::thread::sleep(std::time::Duration::from_secs(60)); String::new() }
        w => return Err(format!("unknown {w:?}")),
    };
    Ok(Answer { text })
}

atrium_worker::main!(handle);
