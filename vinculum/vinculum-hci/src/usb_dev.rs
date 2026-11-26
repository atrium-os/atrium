//! The USB controller through the libusb-1.0 API: FreeBSD's own libusb in
//! base (BSD-2, on `ugen`), or libusb-1.0 on macOS and Linux for testing a
//! dongle on a development host. Only the synchronous API is used, from three
//! threads; libusb allows synchronous transfers on one handle from several
//! threads at once.
//!
//! Before that, a controller that needs a firmware patch at power-up gets it
//! (Realtek: [`vinculum_hci::rtl`]).
//!
//! The main loop sees the controller as one end of a socketpair carrying H4,
//! exactly like the TCP and UART transports: the threads turn it into USB
//! transfers and back. A fatal USB error (the dongle pulled out) shuts the
//! socket, so the main loop sees the controller close and exits.

use std::ffi::{c_char, c_int, c_uint, CStr};
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};

use vinculum_hci::rtl;
use vinculum_hci::usb::{self, Endpoint, HciEndpoints, Route, Select};
use vinculum_hci_proto::h4::{Decoder, Kind};

#[repr(C)]
struct Context {
    _p: [u8; 0],
}
#[repr(C)]
struct Device {
    _p: [u8; 0],
}
#[repr(C)]
struct Handle {
    _p: [u8; 0],
}

#[repr(C)]
#[derive(Default)]
struct DeviceDescriptor {
    b_length: u8,
    b_descriptor_type: u8,
    bcd_usb: u16,
    b_device_class: u8,
    b_device_sub_class: u8,
    b_device_protocol: u8,
    b_max_packet_size0: u8,
    id_vendor: u16,
    id_product: u16,
    bcd_device: u16,
    i_manufacturer: u8,
    i_product: u8,
    i_serial_number: u8,
    b_num_configurations: u8,
}

#[repr(C)]
struct EndpointDescriptor {
    b_length: u8,
    b_descriptor_type: u8,
    b_endpoint_address: u8,
    bm_attributes: u8,
    w_max_packet_size: u16,
    b_interval: u8,
    b_refresh: u8,
    b_synch_address: u8,
    extra: *const u8,
    extra_length: c_int,
}

#[repr(C)]
struct InterfaceDescriptor {
    b_length: u8,
    b_descriptor_type: u8,
    b_interface_number: u8,
    b_alternate_setting: u8,
    b_num_endpoints: u8,
    b_interface_class: u8,
    b_interface_sub_class: u8,
    b_interface_protocol: u8,
    i_interface: u8,
    endpoint: *const EndpointDescriptor,
    extra: *const u8,
    extra_length: c_int,
}

#[repr(C)]
struct Interface {
    altsetting: *const InterfaceDescriptor,
    num_altsetting: c_int,
}

#[repr(C)]
struct ConfigDescriptor {
    b_length: u8,
    b_descriptor_type: u8,
    w_total_length: u16,
    b_num_interfaces: u8,
    b_configuration_value: u8,
    i_configuration: u8,
    bm_attributes: u8,
    max_power: u8,
    interface: *const Interface,
    extra: *const u8,
    extra_length: c_int,
}

const LIBUSB_ERROR_TIMEOUT: c_int = -7;
const LIBUSB_ERROR_INTERRUPTED: c_int = -10;
const LIBUSB_ERROR_NOT_SUPPORTED: c_int = -12;

#[cfg_attr(target_os = "freebsd", link(name = "usb"))]
#[cfg_attr(not(target_os = "freebsd"), link(name = "usb-1.0"))]
extern "C" {
    fn libusb_init(ctx: *mut *mut Context) -> c_int;
    fn libusb_get_device_list(ctx: *mut Context, list: *mut *mut *mut Device) -> isize;
    fn libusb_free_device_list(list: *mut *mut Device, unref: c_int);
    fn libusb_get_device_descriptor(dev: *mut Device, desc: *mut DeviceDescriptor) -> c_int;
    fn libusb_get_active_config_descriptor(dev: *mut Device, config: *mut *mut ConfigDescriptor) -> c_int;
    fn libusb_get_config_descriptor(dev: *mut Device, index: u8, config: *mut *mut ConfigDescriptor) -> c_int;
    fn libusb_get_configuration(handle: *mut Handle, config: *mut c_int) -> c_int;
    fn libusb_set_configuration(handle: *mut Handle, config: c_int) -> c_int;
    fn libusb_free_config_descriptor(config: *mut ConfigDescriptor);
    fn libusb_get_bus_number(dev: *mut Device) -> u8;
    fn libusb_get_device_address(dev: *mut Device) -> u8;
    fn libusb_open(dev: *mut Device, handle: *mut *mut Handle) -> c_int;
    fn libusb_kernel_driver_active(handle: *mut Handle, iface: c_int) -> c_int;
    fn libusb_detach_kernel_driver(handle: *mut Handle, iface: c_int) -> c_int;
    fn libusb_claim_interface(handle: *mut Handle, iface: c_int) -> c_int;
    fn libusb_control_transfer(
        handle: *mut Handle,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        data: *mut u8,
        length: u16,
        timeout: c_uint,
    ) -> c_int;
    fn libusb_bulk_transfer(handle: *mut Handle, ep: u8, data: *mut u8, length: c_int, transferred: *mut c_int, timeout: c_uint) -> c_int;
    fn libusb_interrupt_transfer(handle: *mut Handle, ep: u8, data: *mut u8, length: c_int, transferred: *mut c_int, timeout: c_uint) -> c_int;
    fn libusb_error_name(code: c_int) -> *const c_char;
    fn libusb_get_string_descriptor_ascii(handle: *mut Handle, index: u8, data: *mut u8, length: c_int) -> c_int;
    fn libusb_close(handle: *mut Handle);
    fn libusb_clear_halt(handle: *mut Handle, endpoint: u8) -> c_int;
}

fn err_name(code: c_int) -> String {
    // SAFETY: libusb_error_name returns a pointer to a static, NUL-terminated
    // string for every code, known or not.
    unsafe { CStr::from_ptr(libusb_error_name(code)) }.to_string_lossy().into_owned()
}

fn io_err(what: &str, code: c_int) -> std::io::Error {
    std::io::Error::other(format!("{what}: {}", err_name(code)))
}

/// An open, claimed controller.
struct Usb {
    handle: *mut Handle,
    iface: u8,
    /// bConfigurationValue of the configuration holding `iface`.
    config: u8,
    eps: HciEndpoints,
    vendor: u16,
    product: u16,
}

// SAFETY: a libusb device handle may be used for synchronous transfers from
// several threads at once (libusb serialises event handling internally, and
// FreeBSD's libusb takes its own per-device lock). The handle is never closed
// while the process runs, so no thread sees it freed.
unsafe impl Send for Usb {}
// SAFETY: as for Send; the struct's other fields are plain data.
unsafe impl Sync for Usb {}

const WRITE_TIMEOUT_MS: c_uint = 2000;

impl Usb {
    fn command(&self, packet: &[u8]) -> std::io::Result<()> {
        let len = u16::try_from(packet.len()).map_err(|_| std::io::Error::other("command too long"))?;
        let mut buf = packet.to_vec();
        // SAFETY: `buf` is a live, exclusively owned buffer of `len` bytes for
        // the whole synchronous call; the handle is valid (see `Usb`).
        let r = unsafe {
            libusb_control_transfer(self.handle, usb::COMMAND_REQUEST_TYPE, 0, 0, self.iface as u16, buf.as_mut_ptr(), len, WRITE_TIMEOUT_MS)
        };
        if r < 0 {
            return Err(io_err("command", r));
        }
        Ok(())
    }

    fn acl_out(&self, packet: &[u8]) -> std::io::Result<()> {
        let len = c_int::try_from(packet.len()).map_err(|_| std::io::Error::other("ACL packet too long"))?;
        let mut buf = packet.to_vec();
        let mut done: c_int = 0;
        // SAFETY: `buf` holds `len` bytes and `done` is a live c_int, both
        // for the whole synchronous call; the handle is valid (see `Usb`).
        let r = unsafe { libusb_bulk_transfer(self.handle, self.eps.acl_out.address, buf.as_mut_ptr(), len, &mut done, WRITE_TIMEOUT_MS) };
        if r < 0 {
            return Err(io_err("ACL out", r));
        }
        if done != len {
            return Err(std::io::Error::other(format!("ACL out: {done} of {len} bytes sent")));
        }
        Ok(())
    }

    /// One transfer in from `ep` (interrupt or bulk), no timeout. Returns the
    /// bytes received; `Ok(0)` for a zero-length packet.
    fn read_in(&self, ep: Endpoint, interrupt: bool, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            match self.read_in_for(ep, interrupt, buf, 0) {
                Ok(Some(n)) => return Ok(n),
                Ok(None) => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// One transfer in, waiting at most `timeout_ms` (0: forever). `None`
    /// on a timeout.
    fn read_in_for(&self, ep: Endpoint, interrupt: bool, buf: &mut [u8], timeout_ms: c_uint) -> std::io::Result<Option<usize>> {
        let len = c_int::try_from(buf.len()).map_err(|_| std::io::Error::other("buffer too long"))?;
        loop {
            let mut got: c_int = 0;
            // SAFETY: `buf` is a live, exclusively borrowed buffer of `len`
            // bytes and `got` a live c_int for the whole synchronous call;
            // the handle is valid (see `Usb`).
            let r = unsafe {
                if interrupt {
                    libusb_interrupt_transfer(self.handle, ep.address, buf.as_mut_ptr(), len, &mut got, timeout_ms)
                } else {
                    libusb_bulk_transfer(self.handle, ep.address, buf.as_mut_ptr(), len, &mut got, timeout_ms)
                }
            };
            match r {
                0 => return usize::try_from(got).map(Some).map_err(|_| std::io::Error::other("negative transfer length")),
                LIBUSB_ERROR_TIMEOUT => return Ok(None),
                LIBUSB_ERROR_INTERRUPTED => continue,
                e => return Err(io_err("transfer in", e)),
            }
        }
    }
}

/// How long a vendor command may take during firmware load.
const LOAD_COMMAND_MS: u64 = 2000;

/// Synchronous commands for firmware loaders, before the reader threads
/// start: the command on the control pipe, then events from the interrupt
/// pipe until its Command Complete (other events are dropped: nothing else
/// can be pending on a controller no host has used yet).
impl rtl::Hci for &Usb {
    fn command(&mut self, opcode: u16, params: &[u8]) -> Result<Vec<u8>, rtl::Error> {
        let plen = u8::try_from(params.len()).map_err(|_| rtl::Error::Io("command parameters over 255 bytes".into()))?;
        let mut pkt = opcode.to_le_bytes().to_vec();
        pkt.push(plen);
        pkt.extend_from_slice(params);
        Usb::command(self, &pkt).map_err(|e| rtl::Error::Io(e.to_string()))?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(LOAD_COMMAND_MS);
        let mut dec = Decoder::typed(Kind::Event);
        let mut buf = vec![0u8; self.eps.events.packet_size()];
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return Err(rtl::Error::Controller(format!("no Command Complete for 0x{opcode:04x} in {LOAD_COMMAND_MS} ms")));
            }
            let ms = left.as_millis().clamp(1, LOAD_COMMAND_MS as u128) as c_uint;
            let n = match self.read_in_for(self.eps.events, true, &mut buf, ms) {
                Ok(Some(n)) => n,
                Ok(None) => continue,
                Err(e) => return Err(rtl::Error::Io(e.to_string())),
            };
            let mut events = Vec::new();
            dec.push(&buf[..n], |f| events.push(f.packet)).map_err(|e| rtl::Error::Controller(format!("bad event: {e}")))?;
            for ev in events {
                // Command Complete: code 0x0E, len, credits, opcode, return params
                if ev.len() >= 5 && ev[0] == 0x0E && u16::from_le_bytes([ev[3], ev[4]]) == opcode {
                    return Ok(ev[5..].to_vec());
                }
                // Command Status: code 0x0F, len, status, credits, opcode
                if ev.len() >= 6 && ev[0] == 0x0F && u16::from_le_bytes([ev[4], ev[5]]) == opcode && ev[2] != 0 {
                    return Err(rtl::Error::Controller(format!("0x{opcode:04x} refused with status 0x{:02x}", ev[2])));
                }
            }
        }
    }
}

/// Runs the vendor firmware loader the controller needs, if any.
fn load_firmware(usb: &Usb, name: &str, firmware_dir: &Path) -> std::io::Result<()> {
    if !rtl::is_realtek(usb.vendor, usb.product) {
        return Ok(());
    }
    match rtl::load(&mut &*usb, firmware_dir) {
        Ok(rtl::Outcome::AlreadyLoaded(v)) => {
            eprintln!("vinculum-hci: {name}: Realtek firmware already running (LMP subversion 0x{:04x})", v.lmp_subversion);
            Ok(())
        }
        Ok(rtl::Outcome::Loaded { fw, bytes, after }) => {
            eprintln!("vinculum-hci: {name}: loaded {fw} ({bytes} bytes); LMP subversion now 0x{:04x}", after.lmp_subversion);
            Ok(())
        }
        Err(e) => Err(std::io::Error::other(format!("{name}: Realtek firmware: {e}"))),
    }
}

/// Finds, opens and claims the controller `sel` names.
fn open(sel: Select) -> std::io::Result<(Usb, String)> {
    let mut ctx: *mut Context = std::ptr::null_mut();
    // SAFETY: `ctx` is a live out-pointer; the context is kept for the life
    // of the process.
    let r = unsafe { libusb_init(&mut ctx) };
    if r < 0 {
        return Err(io_err("libusb_init", r));
    }
    let mut list: *mut *mut Device = std::ptr::null_mut();
    // SAFETY: `ctx` was initialised above; `list` is a live out-pointer.
    let n = unsafe { libusb_get_device_list(ctx, &mut list) };
    if n < 0 {
        return Err(io_err("device list", n as c_int));
    }
    // SAFETY: libusb returned a list of `n` valid device pointers; it stays
    // valid until libusb_free_device_list below.
    let devices = unsafe { std::slice::from_raw_parts(list, n as usize) };
    let mut found = None;
    for &dev in devices {
        let mut dd = DeviceDescriptor::default();
        // SAFETY: `dev` is from the live list; `dd` is a live out-struct
        // laid out as struct libusb_device_descriptor.
        if unsafe { libusb_get_device_descriptor(dev, &mut dd) } < 0 {
            continue;
        }
        if let Select::Ids { vendor, product } = sel {
            if dd.id_vendor != vendor || dd.id_product != product {
                continue;
            }
        }
        if let Some((iface, eps, config)) = hci_interface(dev) {
            // SAFETY: `dev` is from the live list.
            let (bus, addr) = unsafe { (libusb_get_bus_number(dev), libusb_get_device_address(dev)) };
            let name = format!("{:04x}:{:04x} on bus {bus} address {addr}", dd.id_vendor, dd.id_product);
            let mut h: *mut Handle = std::ptr::null_mut();
            // SAFETY: `dev` is from the live list; `h` is a live out-pointer.
            // libusb_open takes its own reference on the device, so the
            // handle outlives the list.
            let r = unsafe { libusb_open(dev, &mut h) };
            if r < 0 {
                // SAFETY: frees the list this function got; `dev` is not used after.
                unsafe { libusb_free_device_list(list, 1) };
                return Err(io_err(&format!("open {name}"), r));
            }
            found = Some((Usb { handle: h, iface, config, eps, vendor: dd.id_vendor, product: dd.id_product }, name));
            break;
        }
    }
    // SAFETY: frees the list got above (dropping our references to devices
    // we did not open); no pointer from it is used after.
    unsafe { libusb_free_device_list(list, 1) };
    let Some((usb, name)) = found else {
        return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no Bluetooth controller on USB"));
    };
    claim(&usb, &name)?;
    Ok((usb, name))
}

/// Makes the kernel let go of `usb`, configures it if it is not, and claims
/// its HCI interface. Returns the configuration it was in when found.
fn claim(usb: &Usb, name: &str) -> std::io::Result<u8> {
    // The kernel's own driver (ng_ubt on stock FreeBSD) must let go. Ask
    // first and detach only when one is attached: under dispensatord the
    // kernel driver is already detached, and FreeBSD's detach ioctl needs
    // PRIV_DRIVER, which the jailed driver uid lacks (libusb's auto-detach
    // would call it unconditionally and fail the claim). macOS answers
    // not-supported: nothing there holds a third-party controller.
    // SAFETY: the handle was just opened.
    let active = unsafe { libusb_kernel_driver_active(usb.handle, usb.iface as c_int) };
    if active == 1 {
        // SAFETY: the handle was just opened; no interface is claimed yet.
        let r = unsafe { libusb_detach_kernel_driver(usb.handle, usb.iface as c_int) };
        if r < 0 {
            return Err(io_err(&format!("detach the kernel driver from interface {} of {name}", usb.iface), r));
        }
        eprintln!("vinculum-hci: {name}: detached the kernel driver from interface {}", usb.iface);
    } else if active < 0 && active != LIBUSB_ERROR_NOT_SUPPORTED {
        // Not an authority check: if a kernel driver does hold the
        // interface, the claim below fails and says so.
        eprintln!("vinculum-hci: {name}: ask for a kernel driver on interface {}: {}", usb.iface, err_name(active));
    }
    // A controller whose device descriptor says class 0xE0 (the UB500) is
    // left unconfigured by macOS, which configures only composite (class 0)
    // devices itself; FreeBSD's usb stack always sets one, so there this
    // finds it set and does nothing.
    let mut cur: c_int = 0;
    // SAFETY: the handle was just opened; `cur` is a live out-int.
    let r = unsafe { libusb_get_configuration(usb.handle, &mut cur) };
    if r < 0 {
        return Err(io_err(&format!("get configuration of {name}"), r));
    }
    if cur != usb.config as c_int {
        // SAFETY: the handle was just opened; no interface is claimed yet.
        let r = unsafe { libusb_set_configuration(usb.handle, usb.config as c_int) };
        if r < 0 {
            return Err(io_err(&format!("set configuration {} of {name} (was {cur})", usb.config), r));
        }
        eprintln!("vinculum-hci: {name}: set configuration {} (was {cur})", usb.config);
    }
    // SAFETY: the handle was just opened.
    let r = unsafe { libusb_claim_interface(usb.handle, usb.iface as c_int) };
    if r < 0 {
        return Err(io_err(&format!("claim interface {} of {name}", usb.iface), r));
    }
    // Start every pipe from a clean data toggle and no halt: whoever had the
    // device before us (the kernel's driver, a driver that died, the host
    // OS before a passthrough) may have left an endpoint stalled, and a
    // stalled interrupt IN fails every event read with a pipe error.
    for ep in [usb.eps.events.address, usb.eps.acl_in.address, usb.eps.acl_out.address] {
        // SAFETY: the handle is open and the interface holding `ep` is claimed.
        let r = unsafe { libusb_clear_halt(usb.handle, ep) };
        if r < 0 {
            eprintln!("vinculum-hci: {name}: clear halt on endpoint 0x{ep:02x}: {}", err_name(r));
        }
    }
    Ok(cur.clamp(0, 255) as u8)
}

/// The HCI interface of `dev`'s active configuration (or, for a device not
/// configured yet, its first): alternate setting 0 of the first interface
/// whose class is Bluetooth, its endpoints, and the configuration's value.
fn hci_interface(dev: *mut Device) -> Option<(u8, HciEndpoints, u8)> {
    let mut cfg: *mut ConfigDescriptor = std::ptr::null_mut();
    // SAFETY: `dev` is a valid device from the caller's live list; `cfg` is
    // a live out-pointer.
    if unsafe { libusb_get_active_config_descriptor(dev, &mut cfg) } < 0 || cfg.is_null() {
        cfg = std::ptr::null_mut();
        // SAFETY: as above; index 0 is the first configuration descriptor.
        if unsafe { libusb_get_config_descriptor(dev, 0, &mut cfg) } < 0 || cfg.is_null() {
            return None;
        }
    }
    let mut result = None;
    // SAFETY: libusb filled `cfg` with `b_num_interfaces` interfaces, each
    // with `num_altsetting` alternate settings, each with `b_num_endpoints`
    // endpoints; all stay valid until libusb_free_config_descriptor. Every
    // count is checked against a null pointer before it is used.
    unsafe {
        let c = &*cfg;
        let ifaces: &[Interface] = if c.interface.is_null() { &[] } else { std::slice::from_raw_parts(c.interface, c.b_num_interfaces as usize) };
        for i in ifaces {
            if i.altsetting.is_null() || i.num_altsetting < 1 {
                continue;
            }
            let alt = &*i.altsetting;
            if !usb::is_bluetooth(alt.b_interface_class, alt.b_interface_sub_class, alt.b_interface_protocol) {
                continue;
            }
            let eds: &[EndpointDescriptor] = if alt.endpoint.is_null() { &[] } else { std::slice::from_raw_parts(alt.endpoint, alt.b_num_endpoints as usize) };
            let eps: Vec<Endpoint> = eds
                .iter()
                .map(|e| Endpoint { address: e.b_endpoint_address, attributes: e.bm_attributes, max_packet: e.w_max_packet_size })
                .collect();
            if let Some(e) = usb::pick_endpoints(&eps) {
                result = Some((alt.b_interface_number, e, c.b_configuration_value));
                break;
            }
        }
        libusb_free_config_descriptor(cfg);
    }
    result
}

/// Opens the controller and starts the threads. Returns the main loop's end
/// of the H4 socketpair and the controller's name.
pub fn start(sel: Select, firmware_dir: &Path) -> std::io::Result<(UnixStream, String)> {
    let (usb, name) = open(sel)?;
    load_firmware(&usb, &name, firmware_dir)?;
    let usb = Arc::new(usb);
    let (mine, theirs) = UnixStream::pair()?;
    let up = Arc::new(Mutex::new(theirs.try_clone()?));

    for (ep, kind, interrupt) in [(usb.eps.events, Kind::Event, true), (usb.eps.acl_in, Kind::Acl, false)] {
        let usb = usb.clone();
        let up = up.clone();
        std::thread::spawn(move || {
            let e = reader(&usb, ep, kind, interrupt, &up);
            eprintln!("vinculum-hci: USB {kind:?} in: {e}");
            if let Ok(s) = up.lock() {
                let _ = s.shutdown(Shutdown::Both);
            }
        });
    }
    {
        let usb = usb.clone();
        let mut down = theirs;
        std::thread::spawn(move || {
            let e = writer(&usb, &mut down);
            eprintln!("vinculum-hci: USB out: {e}");
            let _ = down.shutdown(Shutdown::Both);
        });
    }
    Ok((mine, name))
}

/// A string descriptor as printable ASCII, at most 126 characters; None if
/// the device has none or will not say.
fn string(handle: *mut Handle, index: u8) -> Option<String> {
    if index == 0 {
        return None;
    }
    let mut buf = [0u8; 128];
    // SAFETY: the handle is open; `buf` is live for `buf.len()` bytes and
    // libusb writes at most that many.
    let n = unsafe { libusb_get_string_descriptor_ascii(handle, index, buf.as_mut_ptr(), buf.len() as c_int) };
    let n = usize::try_from(n).ok()?.min(buf.len());
    Some(buf[..n].iter().map(|&b| if (0x20..0x7F).contains(&b) { b as char } else { '?' }).collect())
}

/// `--report`: every Bluetooth controller `sel` matches, one by one, as
/// [`vinculum_hci::report`] describes it. A controller that cannot be opened
/// is listed with why.
pub fn report(sel: Select) -> std::io::Result<String> {
    let mut out = String::new();
    let mut ctx: *mut Context = std::ptr::null_mut();
    // SAFETY: `ctx` is a live out-pointer; the context is kept for the life
    // of the process.
    let r = unsafe { libusb_init(&mut ctx) };
    if r < 0 {
        return Err(io_err("libusb_init", r));
    }
    let mut list: *mut *mut Device = std::ptr::null_mut();
    // SAFETY: `ctx` was initialised above; `list` is a live out-pointer.
    let n = unsafe { libusb_get_device_list(ctx, &mut list) };
    if n < 0 {
        return Err(io_err("device list", n as c_int));
    }
    // SAFETY: libusb returned a list of `n` valid device pointers; it stays
    // valid until libusb_free_device_list below.
    let devices = unsafe { std::slice::from_raw_parts(list, n as usize) };
    let mut count = 0;
    for &dev in devices {
        let mut dd = DeviceDescriptor::default();
        // SAFETY: `dev` is from the live list; `dd` is a live out-struct.
        if unsafe { libusb_get_device_descriptor(dev, &mut dd) } < 0 {
            continue;
        }
        if let Select::Ids { vendor, product } = sel {
            if dd.id_vendor != vendor || dd.id_product != product {
                continue;
            }
        }
        let Some((iface, eps, config)) = hci_interface(dev) else { continue };
        count += 1;
        // SAFETY: `dev` is from the live list.
        let (bus, addr) = unsafe { (libusb_get_bus_number(dev), libusb_get_device_address(dev)) };
        let name = format!("{:04x}:{:04x} on bus {bus} address {addr}", dd.id_vendor, dd.id_product);
        let mut h: *mut Handle = std::ptr::null_mut();
        // SAFETY: `dev` is from the live list; `h` is a live out-pointer.
        let r = unsafe { libusb_open(dev, &mut h) };
        if r < 0 {
            out.push_str(&format!("controller {name}: cannot open: {} (run as root?)\n", err_name(r)));
            continue;
        }
        let usb = Usb { handle: h, iface, config, eps, vendor: dd.id_vendor, product: dd.id_product };
        match claim(&usb, &name) {
            Ok(was) => {
                let info = vinculum_hci::report::UsbInfo {
                    vendor: dd.id_vendor,
                    product: dd.id_product,
                    bcd_device: dd.bcd_device,
                    bcd_usb: dd.bcd_usb,
                    manufacturer: string(h, dd.i_manufacturer),
                    product_name: string(h, dd.i_product),
                    bus,
                    address: addr,
                    configured: was,
                    config,
                    interface: iface,
                    endpoints: eps,
                };
                out.push_str(&vinculum_hci::report::controller(&mut &usb, &info));
            }
            Err(e) => out.push_str(&format!("controller {name}: cannot claim: {e}\n")),
        }
        // SAFETY: the handle was opened above and nothing uses it after;
        // closing releases the interface and gives the kernel its driver back.
        unsafe { libusb_close(h) };
    }
    // SAFETY: frees the list got above; no pointer from it is used after.
    unsafe { libusb_free_device_list(list, 1) };
    if count == 0 {
        out.push_str("no USB Bluetooth controller (interface class E0/01/01) found\n");
    }
    Ok(out)
}

/// Endpoint `ep` to the main loop: reassembles `kind` packets (they may span
/// several USB packets, and one transfer may end mid-packet) and writes each
/// as one H4 frame. Returns only on an error.
fn reader(usb: &Usb, ep: Endpoint, kind: Kind, interrupt: bool, up: &Mutex<UnixStream>) -> std::io::Error {
    // One USB packet per transfer, so every transfer completes as soon as the
    // controller sends something, without relying on short or zero-length
    // packets to end a longer read.
    let mut buf = vec![0u8; ep.packet_size()];
    let mut dec = Decoder::typed(kind);
    loop {
        let n = match usb.read_in(ep, interrupt, &mut buf) {
            Ok(n) => n,
            Err(e) => return e,
        };
        let mut frames = Vec::new();
        if let Err(e) = dec.push(&buf[..n], |f| frames.push(f)) {
            return std::io::Error::other(format!("controller sent a bad packet: {e}"));
        }
        for f in frames {
            let Ok(mut s) = up.lock() else {
                return std::io::Error::other("socket lock poisoned");
            };
            if let Err(e) = s.write_all(&f.to_bytes()) {
                return e;
            }
        }
    }
}

/// The main loop's H4 stream to the bus. Returns only on an error or when the
/// main loop's end closes.
fn writer(usb: &Usb, down: &mut UnixStream) -> std::io::Error {
    let mut dec = Decoder::new();
    let mut buf = vec![0u8; 8192];
    loop {
        let n = match down.read(&mut buf) {
            Ok(0) => return std::io::Error::other("main loop closed"),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return e,
        };
        let mut frames = Vec::new();
        if let Err(e) = dec.push(&buf[..n], |f| frames.push(f)) {
            return std::io::Error::other(format!("bad H4 from the relay: {e}"));
        }
        for f in frames {
            let r = match usb::route(f) {
                Route::Control(p) => usb.command(&p),
                Route::Bulk(p) => usb.acl_out(&p),
                Route::Unsupported(k) => {
                    eprintln!("vinculum-hci: {k:?} packet dropped: not carried over USB yet");
                    Ok(())
                }
            };
            if let Err(e) = r {
                return e;
            }
        }
    }
}
