//! Compiles wireless-regdb's db.txt into a Rust table. Only the bands Aura
//! radios use (2.4, 5 and 6 GHz) are kept. Flags map to aura-proto's
//! `reg_flags`; any flag not known here fails the build.

use std::fmt::Write as _;

fn mbm(power: &str) -> i32 {
    let p = power.trim();
    if let Some(mw) = p.strip_suffix("mW") {
        let mw: f64 = mw.trim().parse().expect("mW value");
        // Round down: never more than the rule allows.
        (1000.0 * mw.log10()).floor() as i32
    } else {
        let dbm: f64 = p.parse().expect("dBm value");
        (dbm * 100.0).round() as i32
    }
}

fn main() {
    println!("cargo:rerun-if-changed=data/db.txt");
    let db = std::fs::read_to_string("data/db.txt").expect("data/db.txt");
    let mut out = String::from("pub static COUNTRIES: &[Country] = &[\n");
    let mut cur: Option<(String, Vec<String>)> = None;
    let flush = |c: Option<(String, Vec<String>)>, out: &mut String| {
        if let Some((cc, rules)) = c {
            let b = cc.as_bytes();
            writeln!(out, "    Country {{ alpha2: [{}, {}], rules: &[{}] }},", b[0], b[1], rules.join(", ")).unwrap();
        }
    };
    for (n, raw) in db.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() || line.starts_with("wmmrule") || line.contains("cw_min") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("country ") {
            flush(cur.take(), &mut out);
            let cc = rest.split(':').next().unwrap().trim().to_string();
            assert!(cc.len() == 2, "line {}: country {cc}", n + 1);
            cur = Some((cc, Vec::new()));
            continue;
        }
        assert!(line.starts_with('('), "line {}: {line}", n + 1);
        // (start - end @ bw), (power), FLAG, ...
        let close = line.find(')').unwrap();
        let range = &line[1..close];
        let (se, bw) = range.split_once('@').unwrap();
        let (s, e) = se.split_once('-').unwrap();
        let mhz = |x: &str| -> f64 { x.trim().parse().unwrap() };
        let (start, end, bw) = (mhz(s), mhz(e), mhz(bw));
        let rest = &line[close + 1..];
        let p0 = rest.find('(').unwrap();
        let p1 = rest.find(')').unwrap();
        let power = mbm(&rest[p0 + 1..p1]);
        let mut flags = Vec::new();
        for f in rest[p1 + 1..].split(',').map(str::trim).filter(|f| !f.is_empty()) {
            match f {
                "NO-IR" => flags.push("reg_flags::NO_IR"),
                "DFS" => flags.push("reg_flags::DFS"),
                "NO-OUTDOOR" => flags.push("reg_flags::NO_OUTDOOR"),
                // Outdoor only, or 802.11b only: a station must not
                // initiate there.
                "NO-INDOOR" | "NO-OFDM" => flags.push("reg_flags::NO_IR"),
                "AUTO-BW" => {}
                f if f.starts_with("wmmrule=") => {}
                other => panic!("line {}: unknown regdb flag {other}", n + 1),
            }
        }
        if !(2400.0..=7125.0).contains(&start) {
            continue;
        }
        // aura-proto's limits: bandwidth within the range, power 0..36 dBm.
        let bw = bw.min(end - start);
        let power = power.clamp(0, 3600);
        let flags = if flags.is_empty() { "0".to_string() } else { flags.join(" | ") };
        let r = format!(
            "RegRule {{ start_khz: {}, end_khz: {}, max_bw_khz: {}, max_eirp_mbm: {power}, flags: {flags} }}",
            (start * 1000.0) as u32,
            (end * 1000.0) as u32,
            (bw * 1000.0) as u32
        );
        cur.as_mut().expect("rule before any country").1.push(r);
    }
    flush(cur.take(), &mut out);
    out.push_str("];\n");
    let dst = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("db.rs");
    std::fs::write(dst, out).unwrap();
}
