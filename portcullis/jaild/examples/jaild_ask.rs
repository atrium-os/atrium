//! Ask jaild one request, for check scripts: `jaild_ask 'JSON'` prints the
//! answer as JSON. Root only (jaild refuses anyone else).

fn main() {
    let Some(text) = std::env::args().nth(1) else {
        eprintln!("usage: jaild_ask '{{\"kind\":\"ping\"}}'");
        std::process::exit(2);
    };
    let req: jaild::protocol::Request = match serde_json::from_str(&text) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("jaild_ask: {e}");
            std::process::exit(2);
        }
    };
    let mut c = match jaild::client::Client::connect("/var/run/atrium/jaild.sock") {
        Ok(c) => c,
        Err(e) => {
            eprintln!("jaild_ask: connect: {e}");
            std::process::exit(1);
        }
    };
    match c.send(&req) {
        Ok((r, _)) => println!("{}", serde_json::to_string(&r).unwrap_or_default()),
        Err(e) => {
            eprintln!("jaild_ask: {e}");
            std::process::exit(1);
        }
    }
}
