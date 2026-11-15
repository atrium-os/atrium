//! compile-time: how long our compiler takes on one SPIR-V shader, for the
//! compile column of the RADV comparison (bench-script runs it on the box,
//! next to vk_bench's `vkCreateComputePipelines` timing, on the same CPU).
//!
//!   compile-time <shader.spv> <max_vgprs> <max_sgprs> <repeat>
//!   compile-time --all [repeat]      every bench shader, here (host numbers)
//!
//! Prints, like vk_result.txt: compile_ns_first, compile_ns (per compile,
//! the first included), compile_ns_min, compile_ns_median, code_dwords.
use atrium_rdna4_oracle::bench;

fn report(prefix: &str, ns: &[u64], dwords: usize) {
    let mut s = ns.to_vec();
    s.sort_unstable();
    let list: Vec<String> = ns.iter().map(|n| n.to_string()).collect();
    println!("{prefix}compile_ns_first={}\n{prefix}compile_ns={}\n{prefix}compile_ns_min={}\n{prefix}compile_ns_median={}\n{prefix}code_dwords={dwords}", ns[0], list.join(","), s[0], s[s.len() / 2]);
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.first().map(String::as_str) == Some("--all") {
        let repeat = a.get(1).and_then(|r| r.parse().ok()).unwrap_or(20);
        let mut seen = std::collections::HashSet::new();
        for b in bench::all() {
            if !seen.insert((b.spv.clone(), b.caps)) {
                continue;
            }
            let (ns, dw) = bench::compile_ns(&b.spv, b.caps, repeat).expect("compile");
            let mut s = ns.clone();
            s.sort_unstable();
            let st = bench::compile_stages(&b.spv, b.caps);
            let ms = |n: u64| n as f64 / 1e6;
            println!("{:<28} caps {:>3}/{:<3} first {:>9.3} ms  min {:>8.3} ms  median {:>8.3} ms  {dw} dwords", b.name, b.caps.0, b.caps.1, ns[0] as f64 / 1e6, s[0] as f64 / 1e6, s[s.len() / 2] as f64 / 1e6);
            println!("{:<28} stages: frontend {:.3} gir {:.3} optimize {:.3} verify {:.3} backend {:.3} ms", "", ms(st[0]), ms(st[1]), ms(st[2]), ms(st[3]), ms(st[4]));
        }
        return;
    }
    if a.len() != 4 {
        eprintln!("usage: compile-time <shader.spv> <max_vgprs> <max_sgprs> <repeat> | --all [repeat]");
        std::process::exit(2);
    }
    let spv = std::fs::read(&a[0]).unwrap_or_else(|e| panic!("{}: {e}", a[0]));
    let caps = (a[1].parse().expect("max_vgprs"), a[2].parse().expect("max_sgprs"));
    match bench::compile_ns(&spv, caps, a[3].parse().expect("repeat")) {
        Ok((ns, dw)) => {
            println!("status=ok");
            report("", &ns, dw)
        }
        Err(e) => {
            println!("status=error\nerror={e:?}");
            std::process::exit(1);
        }
    }
}
