//! Run under Mail's signed fixture identity against the callback overlay.
//! Exercise the same persisted configuration and connection path as sync.
use atrium_mail::accounts::{courierd_socket, Couriers};
use mail_engine::accounts::Connect;
use mail_store::{Protocol, Store};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| ".".into());
    let dir = std::env::var_os("MAIL_STORE").map(PathBuf::from).unwrap_or_else(|| home.join(".local/share/atrium-mail"));
    let store = Store::open(&dir)?;
    let accounts = store.accounts()?;
    assert_eq!(accounts.len(), 1);
    let cfg = store.account_config(accounts[0].0)?.ok_or("missing account")?;
    assert_eq!(cfg.address, "ada@outlook.com");
    assert_eq!(cfg.oauth.as_deref(), Some("microsoft"));
    let Protocol::Imap { imap, smtp } = &cfg.protocol else { return Err("expected IMAP".into()) };
    // The provider is up throughout this gate and reachable by its courier.
    let token_fixture = "10.0.2.100:443".parse()?;
    if std::net::TcpStream::connect_timeout(&token_fixture, std::time::Duration::from_secs(1)).is_ok() {
        return Err("Mail has a direct route to the fixture".into());
    }
    println!("Direct Mail connection to the local token endpoint denied");
    let net = Couriers { socket: courierd_socket() };
    for (server, item) in [(imap, cfg.incoming_item), (smtp, cfg.outgoing_item)] {
        let mut s = net.account_mail(&cfg, server, &item.ok_or("missing reference")?)?;
        let mut r = BufReader::new(s.try_clone()?);
        let mut line = String::new();
        r.read_line(&mut line)?;
        if server == imap {
            assert!(line.starts_with("* PREAUTH"), "IMAP login failed");
            s.write_all(b"r1 NOOP\r\n")?;
            line.clear();
            r.read_line(&mut line)?;
            assert!(line.starts_with("r1 OK"));
        } else {
            for _ in 0..32 {
                if !line.starts_with("250-") { break; }
                line.clear();
                r.read_line(&mut line)?;
            }
            assert!(line.starts_with("250 "), "submission login failed");
            s.write_all(b"NOOP\r\n")?;
            line.clear();
            r.read_line(&mut line)?;
            assert!(line.starts_with("250 "));
        }
        println!("{} authenticated and answered NOOP", server.scheme);
    }
    Ok(())
}
