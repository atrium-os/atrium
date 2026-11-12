//! BAR5 register access for the probe: Linux through sysfs `resource5`,
//! FreeBSD through `/dev/mem` at the BAR's physical address (from pciconf).

use std::fs::OpenOptions;
use std::os::fd::AsRawFd;

/// Byte offsets in BAR5 that are fixed across SOC15 parts (no discovery
/// needed): the indirect window and NBIO's memory-size register.
pub const MM_INDEX: usize = 0x0;
pub const MM_DATA: usize = 0x4;
pub const MM_INDEX_HI: usize = 0x18;
pub const RCC_CONFIG_MEMSIZE: usize = 0xde3 * 4;

pub struct Device {
    desc: String,
    map: *mut u32,
    len: usize,
    writable: bool,
}

impl Device {
    pub fn open(dev: &str) -> Result<Device, String> {
        #[cfg(target_os = "linux")]
        {
            let dir = if dev.starts_with('/') { dev.to_string() } else { format!("/sys/bus/pci/devices/{dev}") };
            let rd = |f: &str| std::fs::read_to_string(format!("{dir}/{f}")).map(|s| s.trim().to_string()).unwrap_or_default();
            let driver = std::fs::read_link(format!("{dir}/driver")).ok()
                .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
                .unwrap_or_else(|| "(none)".into());
            let desc = format!("{dev}: vendor {} device {} rev {} class {} driver {}\nresources:\n{}",
                rd("vendor"), rd("device"), rd("revision"), rd("class"), driver, rd("resource"));
            let path = format!("{dir}/resource5");
            Self::map_file(&path, 0, None, desc, driver == "(none)")
        }
        #[cfg(target_os = "freebsd")]
        {
            // pciconf only resolves its own selector form, pci<dom>:<bus>:<slot>:<func>.
            let dev = &freebsd_selector(dev);
            let out = std::process::Command::new("pciconf").args(["-lbv", dev]).output()
                .map_err(|e| format!("pciconf: {e}"))?;
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            let mut base = None;
            let mut size = None;
            for l in text.lines() {
                // "    bar   [24] = type Memory, range 32, base 0xfcc00000, size 524288, enabled"
                if l.contains("bar") && l.contains("[24]") {
                    for part in l.split(',') {
                        let p = part.trim();
                        if let Some(v) = p.strip_prefix("base ") { base = u64::from_str_radix(v.trim_start_matches("0x"), 16).ok(); }
                        if let Some(v) = p.strip_prefix("size ") { size = v.parse::<usize>().ok(); }
                    }
                }
            }
            let (b, s) = base.zip(size).ok_or("no BAR5 ([24]) in pciconf -lbv output")?;
            // vgapci is FreeBSD's generic PCI display driver: it never touches
            // an AMD part's registers. The device counts as owned only when a
            // real GPU driver (drm-kmod amdgpu or ours) is loaded.
            let first = text.lines().next().unwrap_or("");
            let gpu_kmod = ["amdgpu", "atrium_gpu_amd"].iter().any(|m| {
                std::process::Command::new("kldstat").args(["-q", "-n", m]).status().map(|s| s.success()).unwrap_or(false)
            });
            let unbound = first.starts_with("none") || (first.starts_with("vgapci") && !gpu_kmod);
            Self::map_file("/dev/mem", b, Some(s), text, unbound)
        }
    }

    fn map_file(path: &str, off: u64, len: Option<usize>, desc: String, unbound: bool) -> Result<Device, String> {
        // Writable only when no GPU driver owns the device; even then the probe
        // writes nothing but the MM_INDEX window, and only with --vram-top.
        let f = OpenOptions::new().read(true).write(unbound).open(path).map_err(|e| format!("{path}: {e}"))?;
        let len = match len { Some(l) => l, None => f.metadata().map_err(|e| e.to_string())?.len() as usize };
        let prot = if unbound { libc::PROT_READ | libc::PROT_WRITE } else { libc::PROT_READ };
        // SAFETY: mapping a device BAR; all accesses below are aligned, in
        // bounds (checked) and volatile.
        let p = unsafe { libc::mmap(std::ptr::null_mut(), len, prot, libc::MAP_SHARED, f.as_raw_fd(), off as libc::off_t) };
        if p == libc::MAP_FAILED {
            return Err(format!("mmap {path}: {}", std::io::Error::last_os_error()));
        }
        Ok(Device { desc, map: p as *mut u32, len, writable: unbound })
    }

    pub fn describe(&self) -> String {
        format!("{}\nBAR5 mapped {} bytes{}", self.desc, self.len, if self.writable { "" } else { " (read-only: a GPU driver owns the device)" })
    }

    pub fn len(&self) -> usize { self.len }

    pub fn read32(&self, off: usize) -> u32 {
        assert!(off % 4 == 0 && off + 4 <= self.len);
        // SAFETY: in bounds and aligned (asserted).
        unsafe { std::ptr::read_volatile(self.map.add(off / 4)) }
    }

    fn write32(&mut self, off: usize, v: u32) {
        assert!(self.writable && off % 4 == 0 && off + 4 <= self.len);
        // SAFETY: in bounds and aligned (asserted); mapping is writable.
        unsafe { std::ptr::write_volatile(self.map.add(off / 4), v) }
    }

    /// Read VRAM through the MM_INDEX/MM_DATA window (dword at a time).
    /// Refused, before any write, when a GPU driver owns the device.
    pub fn read_vram(&mut self, addr: u64, n: usize) -> Result<Vec<u8>, String> {
        if !self.writable {
            return Err("a GPU driver owns the device and the MM_INDEX window is shared with it; \
                        unload it (Linux: unbind amdgpu) and run again".into());
        }
        let mut out = Vec::with_capacity(n);
        let mut a = addr & !3;
        while out.len() < n {
            self.write32(MM_INDEX, ((a as u32) & 0x7fff_ffff) | 0x8000_0000);
            self.write32(MM_INDEX_HI, (a >> 31) as u32);
            out.extend_from_slice(&self.read32(MM_DATA).to_le_bytes());
            a += 4;
        }
        out.truncate(n);
        Ok(out)
    }
}

/// Normalise a PCI address to pciconf's selector: "3:0:0", "0:3:0:0",
/// "03:00.0", "0000:03:00.0" and "pci0:3:0:0" all give "pci0:3:0:0".
/// Anything else (e.g. a device name like vgapci0) passes through.
#[cfg_attr(not(target_os = "freebsd"), allow(dead_code))]
pub fn freebsd_selector(dev: &str) -> String {
    let t = dev.trim_start_matches("pci");
    let parts: Vec<&str> = t.split([':', '.']).collect();
    let nums: Option<Vec<u32>> = parts.iter().map(|p| {
        if p.is_empty() || p.len() > 4 { None }
        else if dev.contains('.') { u32::from_str_radix(p, 16).ok() } // Linux form is hex
        else { p.parse().ok() }
    }).collect();
    match nums.as_deref() {
        Some([b, s, f]) => format!("pci0:{b}:{s}:{f}"),
        Some([d, b, s, f]) => format!("pci{d}:{b}:{s}:{f}"),
        _ => dev.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::freebsd_selector as sel;
    #[test]
    fn selectors() {
        for s in ["3:0:0", "0:3:0:0", "pci0:3:0:0", "03:00.0", "0000:03:00.0"] {
            assert_eq!(sel(s), "pci0:3:0:0", "{s}");
        }
        assert_eq!(sel("0000:1a:00.1"), "pci0:26:0:1");
        assert_eq!(sel("vgapci0"), "vgapci0");
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: unmapping our own mapping.
        unsafe { libc::munmap(self.map as *mut libc::c_void, self.len) };
    }
}
