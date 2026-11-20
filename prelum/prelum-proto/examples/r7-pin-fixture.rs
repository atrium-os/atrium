//! Synthetic PIN submission for the isolated Prelum gate. Handles travel over stdin/stdout.
use prelum_proto::print::{client::Client, Print};
use std::{fs::File, io::Read, os::unix::net::UnixStream, path::Path, time::Duration};
const ARCA: &str = "/atrium/sockets/arca/arcad.sock";
fn request(op: u16, body: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut s = UnixStream::connect(ARCA).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(&((2 + body.len()) as u32).to_le_bytes())
        .unwrap();
    s.write_all(&op.to_le_bytes()).unwrap();
    s.write_all(body).unwrap();
    let mut n = [0u8; 4];
    s.read_exact(&mut n).unwrap();
    let n = u32::from_le_bytes(n) as usize;
    assert!((1..=256).contains(&n));
    let mut value = vec![0; n];
    s.read_exact(&mut value).unwrap();
    assert_eq!(value[0], 0, "Arca status for {op:#x}");
    value.split_off(1)
}
fn bytes(body: &mut Vec<u8>, data: &[u8]) {
    body.extend((data.len() as u32).to_le_bytes());
    body.extend(data);
}
fn main() {
    use std::io::Write;
    let a: Vec<String> = std::env::args().collect();
    let mut body = Vec::new();
    bytes(&mut body, b"root");
    match a[1].as_str() {
        "unlock" => {
            body.extend([31; 32]);
            request(0x40, &body);
            request(0x41, &body);
        }
        "stage" => {
            bytes(&mut body, b"personal");
            let key = &a[2];
            assert_eq!(key.len(), 64);
            for i in (0..64).step_by(2) {
                body.push(u8::from_str_radix(&key[i..i + 2], 16).unwrap());
            }
            bytes(&mut body, a[3].as_bytes());
            bytes(&mut body, b"0427");
            std::io::stdout().write_all(&request(0x68, &body)).unwrap();
        }
        "submit" => {
            let mut handle = [0; 16];
            std::io::stdin().read_exact(&mut handle).unwrap();
            let pdf = File::open(&a[3]).unwrap();
            let mut c = Client::connect(Path::new(&a[2])).unwrap();
            println!(
                "{}",
                c.print_pin(&Print::new(1, &a[4]), handle, &pdf).unwrap()
            );
        }
        _ => panic!("unknown command"),
    }
}
