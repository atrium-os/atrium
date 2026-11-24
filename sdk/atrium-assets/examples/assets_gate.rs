//! The app of scripts/vm-image-decode-check.sh: decodes its bundle's
//! pictures through the system's decoder and prints `assets-gate <check>
//! <result>` lines.
use std::io::Write;
use std::time::Instant;

fn say(check: &str, r: String) {
    println!("assets-gate {check} {r}");
    let _ = std::io::stdout().flush();
}

fn show(r: Result<atrium_assets::Picture, atrium_assets::Error>, at: Option<(u32, u32)>) -> String {
    match r {
        Ok(p) => {
            let px = at.map(|(x, y)| {
                let i = ((y * p.width + x) * 4) as usize;
                format!(" {:02x}{:02x}{:02x}{:02x}", p.rgba[i], p.rgba[i + 1], p.rgba[i + 2], p.rgba[i + 3])
            }).unwrap_or_default();
            format!("ok {}x{}{px}", p.width, p.height)
        }
        Err(atrium_assets::Error::Decoder(atrium_worker::Error::Worker(why))) => format!("refused {why}"),
        Err(e) => format!("error {e}"),
    }
}

fn main() {
    // SAFETY: getuid takes nothing and cannot fail.
    let uid = unsafe { libc_getuid() };
    say("core-uid", uid.to_string());
    say("icon", show(atrium_assets::image("icons/app.svg", 48, 48), Some((24, 24))));
    say("photo", show(atrium_assets::image("photo.png", 64, 64), Some((10, 10))));
    say("junk", show(atrium_assets::image("junk.png", 16, 16), None));
    say("script", show(atrium_assets::image("script.svg", 16, 16), None));
    say("escape", show(atrium_assets::image("../atrium.toml", 16, 16), None));
    let mut ms = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        let _ = atrium_assets::image("icons/app.svg", 48, 48);
        ms.push(t.elapsed().as_millis());
    }
    ms.sort();
    say("timing", format!("median {} ms of 5", ms[2]));
    // One decoder for many pictures: one jail.
    let t = Instant::now();
    let shown = atrium_assets::Decoder::new().map_err(|e| e.to_string()).and_then(|mut d| {
        (0..5).map(|_| d.image("icons/app.svg", 48, 48).map(|_| ()).map_err(|e| e.to_string())).collect::<Result<Vec<_>, _>>()
    });
    say("session", match shown {
        Ok(v) => format!("ok {} pictures in {} ms", v.len(), t.elapsed().as_millis()),
        Err(e) => format!("error {e}"),
    });
    // A few decoders per app at once (portcullisd's per-caller cap, 4):
    // the fifth is refused while four are held, and each held one works.
    let held: Vec<_> = (0..5).map(|_| atrium_assets::Decoder::new()).collect();
    let mut kept = 0;
    let mut refused = String::new();
    for d in held {
        match d {
            Ok(mut d) => if d.image("icons/app.svg", 16, 16).is_ok() { kept += 1 },
            Err(e) => refused = e.to_string(),
        }
    }
    say("per-caller", format!("held {kept} refused {refused}"));
    say("done", String::new());
}

extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}
