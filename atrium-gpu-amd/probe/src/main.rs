//! atrium-gpu-probe — the first, read-only look at an AMD GPU on silicon.
//!
//! Stage 2 of the R9700 bring-up (docs/spec/r9700-bare-metal.md). Before
//! atrium-gpu-amd touches real hardware we want ground truth: which IP
//! blocks the chip has, at which register bases and versions, and what the
//! firmware files we will load say about themselves. The same tool runs on
//! Ubuntu (amdgpu bound, for comparison) and on Atrium (no driver bound).
//!
//!   atrium-gpu-probe fw <dir>               parse every firmware header in <dir>
//!   atrium-gpu-probe discovery <file>       parse a saved discovery binary
//!   atrium-gpu-probe pci <bdf> [--vram-top] identity, BARs, MMIO reads
//!
//! `pci` maps BAR5 (registers) read-only, except with --vram-top, which
//! writes the MM_INDEX/MM_INDEX_HI window registers to read the discovery
//! table from the top of VRAM. Never use --vram-top while amdgpu (or
//! atrium-gpu-amd) owns the device: the window is shared with the driver.
//!
//! The parsers are the kmod's own (../rs/fw.rs, ../rs/discovery.rs).

#[allow(dead_code)]
#[path = "../../rs/fw.rs"]
mod fw;
#[allow(dead_code)]
#[path = "../../rs/discovery.rs"]
mod discovery;
mod mmio;

use std::path::Path;
use std::process::exit;

fn usage() -> ! {
    eprintln!("usage: atrium-gpu-probe fw <dir> | discovery <file> | pci <bdf|sysfs-dir> [--vram-top] [--save <file>]");
    exit(2)
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    match a.first().map(String::as_str) {
        Some("fw") if a.len() == 2 => fw_dir(Path::new(&a[1])),
        Some("discovery") if a.len() == 2 => {
            let b = std::fs::read(&a[1]).unwrap_or_else(|e| die(&format!("{}: {e}", a[1])));
            print_discovery(&b);
        }
        Some("pci") if a.len() >= 2 => {
            let vram_top = a.iter().any(|x| x == "--vram-top");
            let save = a.iter().position(|x| x == "--save").and_then(|i| a.get(i + 1)).cloned();
            pci(&a[1], vram_top, save.as_deref());
        }
        _ => usage(),
    }
}

fn die(m: &str) -> ! {
    eprintln!("atrium-gpu-probe: {m}");
    exit(1)
}

fn kind_of(name: &str) -> &'static str {
    let n = name.trim_end_matches(".bin").trim_end_matches("_kicker");
    if n.starts_with("psp_") && (n.ends_with("_sos") || n.ends_with("_ta") || n.ends_with("_toc")) {
        "psp"
    } else if n.ends_with("_pfp") || n.ends_with("_me") || n.ends_with("_mec") {
        "rs64"
    } else if n.contains("_mes") {
        "mes"
    } else if n.ends_with("_imu") {
        "imu"
    } else if n.starts_with("sdma_") {
        "sdma"
    } else {
        "common"
    }
}

fn fw_dir(dir: &Path) {
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| die(&format!("{}: {e}", dir.display())))
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".bin"))
        .collect();
    names.sort();
    let mut bad = 0;
    for n in names {
        let f = match std::fs::read(dir.join(&n)) {
            Ok(f) => f,
            Err(e) => { println!("{n}: read error {e}"); bad += 1; continue; }
        };
        let c = match fw::CommonHeader::parse(&f) {
            Ok(c) => c,
            Err(e) => { println!("{n}: REFUSED {e:?}"); bad += 1; continue; }
        };
        print!(
            "{n}: {} bytes, header v{}.{} ({} bytes), ip {}.{}, ucode ver 0x{:x}",
            f.len(), c.header_version_major, c.header_version_minor, c.header_size_bytes,
            c.ip_version_major, c.ip_version_minor, c.ucode_version
        );
        let detail = match kind_of(&n) {
            "psp" => fw::PspFw::parse(&f).map(|p| {
                let mut s = format!(" psp bins={}", p.count);
                if let Some(a) = p.aux_index { s += &format!(" aux={a}"); }
                for b in p.bins() {
                    s += &format!(" [type {} ver 0x{:x} @{:#x}+{:#x}]", b.fw_type, b.fw_version, b.region.offset, b.region.size);
                }
                s
            }),
            "rs64" => fw::Rs64Fw::parse(&f).map(|p| format!(
                " rs64 feat {} ucode @{:#x}+{:#x} data @{:#x}+{:#x} start {:#x}",
                p.feature_version, p.ucode.offset, p.ucode.size, p.data.offset, p.data.size, p.ucode_start_addr)),
            "mes" => fw::MesFw::parse(&f).map(|p| format!(
                " mes ucode @{:#x}+{:#x} data @{:#x}+{:#x} start {:#x}/{:#x}",
                p.ucode.offset, p.ucode.size, p.data.offset, p.data.size, p.ucode_start_addr, p.data_start_addr)),
            "imu" => fw::ImuFw::parse(&f).map(|p| format!(
                " imu iram @{:#x}+{:#x} dram @{:#x}+{:#x}", p.iram.offset, p.iram.size, p.dram.offset, p.dram.size)),
            "sdma" => fw::SdmaFw::parse(&f).map(|p| format!(
                " sdma feat {} ucode @{:#x}+{:#x}", p.feature_version, p.ucode.offset, p.ucode.size)),
            _ => c.ucode(&f).map(|r| format!(" ucode @{:#x}+{:#x}", r.offset, r.size)),
        };
        match detail {
            Ok(d) => println!("{d}"),
            Err(e) => { println!(" DETAIL REFUSED {e:?}"); bad += 1; }
        }
    }
    if bad > 0 { exit(1); }
}

fn hw_name(id: u16) -> &'static str {
    use discovery::hwid::*;
    match id {
        MP1 => "MP1(SMU)", THM => "THM", SMUIO => "SMUIO", GC => "GC", MMHUB => "MMHUB",
        ATHUB => "ATHUB", OSSSYS => "OSSSYS(IH)", HDP => "HDP", SDMA0 => "SDMA", VCN => "VCN",
        DF => "DF", NBIF => "NBIF", UMC => "UMC", MP0 => "MP0(PSP)", DMU => "DMU(DCN)",
        _ => "?",
    }
}

fn print_discovery(b: &[u8]) {
    let d = discovery::Discovery::parse(b).unwrap_or_else(|e| die(&format!("discovery: {e:?}")));
    println!("discovery binary v{}.{}, ip table v{}, {} die(s)", d.version_major, d.version_minor, d.ip_version, d.num_dies);
    for (i, t) in d.tables.iter().enumerate() {
        if t.size != 0 { println!("  table {i}: offset {:#x} size {:#x}", t.offset, t.size); }
    }
    d.for_each_ip(|ip| {
        let bases: Vec<String> = ip.bases().iter().map(|b| format!("{b:#x}")).collect();
        println!(
            "  die {} hw {:>3} {:<11} inst {} v{}.{}.{} var {:#04x} bases [{}]",
            ip.die, ip.hw_id, hw_name(ip.hw_id), ip.instance, ip.major, ip.minor, ip.revision,
            ip.variant_byte, bases.join(", ")
        );
    })
    .unwrap_or_else(|e| die(&format!("discovery walk: {e:?}")));
}

fn pci(dev: &str, vram_top: bool, save: Option<&str>) {
    let mut m = mmio::Device::open(dev).unwrap_or_else(|e| die(&e));
    println!("{}", m.describe());
    // Fixed-offset registers that need no discovery (NBIO/BIF early window).
    let memsize = m.read32(mmio::RCC_CONFIG_MEMSIZE);
    println!("RCC_CONFIG_MEMSIZE = {memsize:#x} ({} MiB VRAM)", memsize);
    if !vram_top {
        println!("(pass --vram-top, with no driver bound, to read the discovery table)");
        return;
    }
    if memsize == 0 || memsize == 0xffff_ffff {
        die("VRAM size unreadable; not touching the MM window");
    }
    let top = (memsize as u64) << 20;
    let base = top - discovery::DISCOVERY_TMR_OFFSET_FROM_TOP;
    let bin = m.read_vram(base, discovery::DISCOVERY_TMR_SIZE);
    if let Some(p) = save {
        std::fs::write(p, &bin).unwrap_or_else(|e| die(&format!("{p}: {e}")));
        println!("saved {} bytes to {p}", bin.len());
    }
    print_discovery(&bin);
}
