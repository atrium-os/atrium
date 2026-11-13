//! A stand-in USB driver for scripts/vm-usb-check.sh: proves that a jail
//! dispensatord started for a `usb` rule can use its device and nothing else.
//!
//! `atrium-usbprobe /dev/ugenB.A /dev/usb/B.A.0 vvvv:pppp`
//!
//! Opens both nodes read-write, asks the device who it is
//! (`USB_GET_DEVICEINFO`, which the `device` sysfil profile allows), and
//! checks that `/dev` shows no other USB node. Then it waits for ever, so the
//! jail stays up; on any failure it exits, and the jail (persist=0) goes,
//! which is what the gate looks for. Exit codes: 2 open, 3 ioctl, 4 wrong
//! device, 5 other USB nodes visible.

#[cfg(target_os = "freebsd")]
fn main() {
    use std::os::fd::AsRawFd;

    /// `struct usb_device_info` (sys/dev/usb/usb_ioctl.h), 368 bytes.
    #[repr(C)]
    struct UsbDeviceInfo {
        product_no: u16,
        vendor_no: u16,
        rest: [u8; 364],
    }
    const _: () = assert!(std::mem::size_of::<UsbDeviceInfo>() == 368);
    const USB_GET_DEVICEINFO: libc::c_ulong = 0x4170_5570;

    let a: Vec<String> = std::env::args().collect();
    if a.len() != 4 {
        std::process::exit(64);
    }
    let open = |p: &str| std::fs::OpenOptions::new().read(true).write(true).open(p).unwrap_or_else(|_| std::process::exit(2));
    let ugen = open(&a[1]);
    let _ctl = open(&a[2]);
    let mut i: UsbDeviceInfo = unsafe { std::mem::zeroed() };
    // SAFETY: the kernel fills a struct usb_device_info, which this mirrors.
    if unsafe { libc::ioctl(ugen.as_raw_fd(), USB_GET_DEVICEINFO as _, &mut i as *mut UsbDeviceInfo) } < 0 {
        std::process::exit(3);
    }
    if format!("{:04x}:{:04x}", i.vendor_no, i.product_no) != a[3] {
        std::process::exit(4);
    }
    let mine = a[2].trim_start_matches("/dev/usb/");
    let others = std::fs::read_dir("/dev/usb").map(|d| d.filter_map(|e| e.ok()).any(|e| e.file_name() != mine)).unwrap_or(false)
        || std::fs::read_dir("/dev").map(|d| d.filter_map(|e| e.ok()).any(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            n.starts_with("ugen") && format!("/dev/{n}") != a[1]
        })).unwrap_or(false);
    if others {
        std::process::exit(5);
    }
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

#[cfg(not(target_os = "freebsd"))]
fn main() {
    eprintln!("atrium-usbprobe runs on FreeBSD only");
    std::process::exit(1);
}
