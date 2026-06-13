//! Host side of the Lyra C node ABI (`include/lyra_node.h`).
//!
//! Loads a plugin shared object, validates its descriptor, and drives it one
//! buffer at a time — the Rust counterpart of the C contract. This is what both
//! lyrad (for trusted in-process DSP) and the jailed `lyra-host` shim (for
//! untrusted plugins, ambition 3) use to run a C node.
//!
//! Loading 3rd-party code is the dangerous part, so the trust boundary is
//! explicit: `HostedNode::load` is `unsafe` and the caller states where it runs.
//! In lyrad it runs only for signed/trusted plugins; everything else is dlopen'd
//! inside a Portcullis jail by a separate process, so a fault is contained.

use std::ffi::{c_char, c_void, CStr, CString};
use std::os::raw::c_uint;
use std::path::Path;

pub const LYRA_NODE_ABI_VERSION: u32 = 1;

/// Mirror of `lyra_run_ctx` — must match the C layout exactly.
#[repr(C)]
pub struct RunCtx {
    pub sample_rate: u32,
    pub nframes: u32,
    pub nchannels: u32,
    _reserved: u32,
    pub frame_pos: u64,
}

/// Mirror of `lyra_node_desc`. Function pointers are `Option<extern "C" fn>` so
/// a NULL optional (set_param) is representable.
#[repr(C)]
struct NodeDesc {
    abi_version: c_uint,
    name: *const c_char,
    nchannels: c_uint,
    latency_frames: c_uint,
    instantiate: Option<extern "C" fn(c_uint, c_uint) -> *mut c_void>,
    destroy: Option<extern "C" fn(*mut c_void)>,
    process: Option<extern "C" fn(*mut c_void, *const RunCtx, *const f32, *mut f32)>,
    set_param: Option<extern "C" fn(*mut c_void, c_uint, f32)>,
}

type DescriptorFn = extern "C" fn() -> *const NodeDesc;

/// A loaded, instantiated C node.
pub struct HostedNode {
    handle: *mut c_void,    // dlopen handle (kept open for the node's life)
    instance: *mut c_void,  // plugin instance state
    desc: *const NodeDesc,
    nchannels: u32,
    sample_rate: u32,
}

#[derive(Debug)]
pub enum LoadError {
    Dlopen(String),
    NoDescriptor,
    AbiMismatch { got: u32, want: u32 },
    MissingFn(&'static str),
    Instantiate,
}

impl HostedNode {
    /// Load `path`, validate the ABI, and instantiate one node.
    ///
    /// # Safety
    /// dlopen's and runs arbitrary native code from `path`. The caller asserts
    /// the plugin is trusted, OR that this call itself runs inside a jail whose
    /// failure is contained.
    pub unsafe fn load(
        path: &Path,
        sample_rate: u32,
        nchannels: u32,
    ) -> Result<HostedNode, LoadError> {
        let cpath = CString::new(path.as_os_str().to_string_lossy().as_bytes())
            .map_err(|_| LoadError::Dlopen("path has NUL".into()))?;
        let handle = libc::dlopen(cpath.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if handle.is_null() {
            let e = libc::dlerror();
            let msg = if e.is_null() {
                "dlopen failed".to_string()
            } else {
                CStr::from_ptr(e).to_string_lossy().into_owned()
            };
            return Err(LoadError::Dlopen(msg));
        }

        let sym = CString::new("lyra_node_descriptor").unwrap();
        let dptr = libc::dlsym(handle, sym.as_ptr());
        if dptr.is_null() {
            libc::dlclose(handle);
            return Err(LoadError::NoDescriptor);
        }
        let descriptor_fn: DescriptorFn = std::mem::transmute(dptr);
        let desc = descriptor_fn();
        if desc.is_null() {
            libc::dlclose(handle);
            return Err(LoadError::NoDescriptor);
        }

        let d = &*desc;
        if d.abi_version != LYRA_NODE_ABI_VERSION {
            libc::dlclose(handle);
            return Err(LoadError::AbiMismatch {
                got: d.abi_version,
                want: LYRA_NODE_ABI_VERSION,
            });
        }
        let instantiate = d.instantiate.ok_or_else(|| {
            libc::dlclose(handle);
            LoadError::MissingFn("instantiate")
        })?;
        if d.process.is_none() {
            libc::dlclose(handle);
            return Err(LoadError::MissingFn("process"));
        }
        if d.destroy.is_none() {
            libc::dlclose(handle);
            return Err(LoadError::MissingFn("destroy"));
        }

        let instance = instantiate(sample_rate, nchannels);
        if instance.is_null() {
            libc::dlclose(handle);
            return Err(LoadError::Instantiate);
        }

        Ok(HostedNode { handle, instance, desc, nchannels, sample_rate })
    }

    pub fn name(&self) -> String {
        unsafe { CStr::from_ptr((*self.desc).name).to_string_lossy().into_owned() }
    }

    pub fn latency_frames(&self) -> u32 {
        unsafe { (*self.desc).latency_frames }
    }

    /// Set a control parameter (no-op if the plugin doesn't provide set_param).
    pub fn set_param(&mut self, id: u32, value: f32) {
        unsafe {
            if let Some(f) = (*self.desc).set_param {
                f(self.instance, id, value);
            }
        }
    }

    /// Run one buffer of interleaved frames through the node (in place).
    /// `buf.len()` must be a multiple of the channel count.
    pub fn process(&mut self, frame_pos: u64, buf: &mut [f32]) {
        let nframes = (buf.len() / self.nchannels.max(1) as usize) as u32;
        let ctx = RunCtx {
            sample_rate: self.sample_rate,
            nframes,
            nchannels: self.nchannels,
            _reserved: 0,
            frame_pos,
        };
        unsafe {
            let process = (*self.desc).process.unwrap();
            // in-place: same pointer for in and out (the C ABI permits aliasing).
            let p = buf.as_mut_ptr();
            process(self.instance, &ctx, p as *const f32, p);
        }
    }
}

impl Drop for HostedNode {
    fn drop(&mut self) {
        unsafe {
            if let Some(destroy) = (*self.desc).destroy {
                destroy(self.instance);
            }
            libc::dlclose(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Compile the reference tremolo plugin to a shared object in a temp dir.
    fn build_tremolo() -> std::path::PathBuf {
        let crate_dir = env!("CARGO_MANIFEST_DIR");
        let out = std::env::temp_dir().join("lyra_tremolo_test.so");
        let status = Command::new("cc")
            .args([
                "-shared",
                "-fPIC",
                "-O2",
                &format!("-I{crate_dir}/include"),
                &format!("{crate_dir}/plugins/tremolo.c"),
                "-lm",
                "-o",
            ])
            .arg(&out)
            .status()
            .expect("cc available");
        assert!(status.success(), "tremolo.c must compile");
        out
    }

    #[test]
    fn loads_and_reports_metadata() {
        let so = build_tremolo();
        let node = unsafe { HostedNode::load(&so, 48_000, 2).expect("loads") };
        assert_eq!(node.name(), "tremolo");
        assert_eq!(node.latency_frames(), 0);
    }

    #[test]
    fn modulates_a_dc_signal() {
        let so = build_tremolo();
        let mut node = unsafe { HostedNode::load(&so, 48_000, 1).expect("loads") };
        // a DC input; tremolo should scale it by the 0..1 LFO -> varying output,
        // all within [0,1], not constant.
        let mut buf = vec![1.0f32; 48_000]; // 1 s, enough for several LFO cycles
        node.process(0, &mut buf);
        let max = buf.iter().cloned().fold(f32::MIN, f32::max);
        let min = buf.iter().cloned().fold(f32::MAX, f32::min);
        assert!(max <= 1.0001 && min >= -0.0001, "stays in gain range [{min},{max}]");
        assert!(max - min > 0.3, "actually modulates (depth): {}", max - min);
    }

    #[test]
    fn streaming_state_persists_across_buffers() {
        let so = build_tremolo();
        // whole vs split: the LFO phase must carry across process() calls.
        let mut whole = unsafe { HostedNode::load(&so, 48_000, 1).unwrap() };
        let mut a = vec![1.0f32; 1000];
        whole.process(0, &mut a);

        let mut split = unsafe { HostedNode::load(&so, 48_000, 1).unwrap() };
        let mut b = vec![1.0f32; 1000];
        let (l, r) = b.split_at_mut(384);
        split.process(0, l);
        split.process(384, r);

        for (x, y) in a.iter().zip(&b) {
            assert!((x - y).abs() < 1e-6, "split == whole (phase carried)");
        }
    }

    #[test]
    fn set_param_changes_behaviour() {
        let so = build_tremolo();
        let mut node = unsafe { HostedNode::load(&so, 48_000, 1).unwrap() };
        // depth 0 -> no modulation (passthrough at unity).
        node.set_param(1, 0.0); // PARAM_DEPTH = 0
        let mut buf = vec![1.0f32; 1000];
        node.process(0, &mut buf);
        assert!(buf.iter().all(|&s| (s - 1.0).abs() < 1e-5), "depth 0 = passthrough");
    }

    #[test]
    fn abi_mismatch_is_rejected() {
        // sanity: loading a non-plugin .so (no descriptor) fails cleanly.
        let bogus = std::env::temp_dir().join("lyra_not_a_plugin.so");
        let _ = Command::new("cc")
            .args(["-shared", "-fPIC", "-xc", "-", "-o"])
            .arg(&bogus)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut c| {
                use std::io::Write;
                c.stdin.take().unwrap().write_all(b"int unrelated(void){return 0;}")?;
                c.wait()
            });
        if bogus.exists() {
            let r = unsafe { HostedNode::load(&bogus, 48_000, 1) };
            assert!(matches!(r, Err(LoadError::NoDescriptor)), "no descriptor -> rejected");
        }
    }
}
