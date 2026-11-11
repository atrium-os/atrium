//! OpenH264's C interface (codec_api.h, v2.6.0), called through its vtable.
use hospes_rt::load::Library;

#[repr(C)]
struct VideoProperty {
    size: u32,
    bs_type: u32,
}
#[repr(C)]
struct DecodingParam {
    file_name_restructed: *const u8,
    cpu_load: u32,
    target_dq_layer: u8,
    ec_active_idc: u32,
    parse_only: bool,
    video_property: VideoProperty,
}
const _: () = assert!(core::mem::size_of::<DecodingParam>() == 32);

#[repr(C)]
#[derive(Default)]
struct SysMemBuffer {
    width: i32,
    height: i32,
    format: i32,
    stride: [i32; 2],
}
#[repr(C)]
struct BufferInfo {
    status: i32,
    in_ts: u64,
    out_ts: u64,
    sys: SysMemBuffer,
    dst: [*mut u8; 3],
}
const _: () = assert!(core::mem::size_of::<BufferInfo>() == 72);
const _: () = assert!(core::mem::offset_of!(BufferInfo, sys) == 24);

type This = *mut *const Vtbl;
#[repr(C)]
struct Vtbl {
    initialize: extern "C" fn(This, *const DecodingParam) -> i64,
    uninitialize: extern "C" fn(This) -> i64,
    decode_frame: usize,
    decode_frame_no_delay: extern "C" fn(This, *const u8, i32, *mut *mut u8, *mut BufferInfo) -> u32,
    decode_frame2: usize,
    flush_frame: extern "C" fn(This, *mut *mut u8, *mut BufferInfo) -> u32,
    decode_parser: usize,
    decode_frame_ex: usize,
    set_option: extern "C" fn(This, u32, *mut core::ffi::c_void) -> i64,
    get_option: usize,
}

const OPT_END_OF_STREAM: u32 = 1;
const OPT_TRACE_LEVEL: u32 = 9;
const OPT_NUM_OF_THREADS: u32 = 19;
const WELS_LOG_ERROR: i32 = 1;
const VIDEO_BITSTREAM_DEFAULT: u32 = 1;
/// Pictures a decoder can hold for reordering; draining past this means it is empty.
const MAX_HELD: usize = 16;

pub struct Frame {
    pub pts: i64,
    pub width: u32,
    pub height: u32,
    planes: [*const u8; 3],
    strides: [usize; 3],
}

impl Frame {
    /// Hand the picture out row by row as packed I420.
    pub fn write_i420(&self, mut put: impl FnMut(&[u8])) {
        let (w, h) = (self.width as usize, self.height as usize);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        for (pl, (pw, ph)) in [(0, (w, h)), (1, (cw, ch)), (2, (cw, ch))] {
            for y in 0..ph {
                put(unsafe { core::slice::from_raw_parts(self.planes[pl].add(y * self.strides[pl]), pw) });
            }
        }
    }
}

pub struct Decoder {
    d: This,
    destroy: extern "C" fn(This),
}

impl Decoder {
    fn vt(&self) -> &Vtbl {
        unsafe { &**self.d }
    }

    pub fn new(lib: &Library) -> Option<Decoder> {
        let create: extern "C" fn(*mut This) -> i64 = unsafe { core::mem::transmute(lib.sym("WelsCreateDecoder")?) };
        let destroy: extern "C" fn(This) = unsafe { core::mem::transmute(lib.sym("WelsDestroyDecoder")?) };
        let mut d: This = core::ptr::null_mut();
        if create(&mut d) != 0 || d.is_null() {
            return None;
        }
        let dec = Decoder { d, destroy };
        if !dec.init() {
            return None;
        }
        Some(dec)
    }

    fn init(&self) -> bool {
        let vt = self.vt();
        // One thread: OpenH264's threaded mode does not decode identically (H0 §4).
        let mut threads: i32 = 1;
        (vt.set_option)(self.d, OPT_NUM_OF_THREADS, &mut threads as *mut i32 as *mut _);
        let mut level = WELS_LOG_ERROR;
        (vt.set_option)(self.d, OPT_TRACE_LEVEL, &mut level as *mut i32 as *mut _);
        let p = DecodingParam {
            file_name_restructed: core::ptr::null(),
            cpu_load: 0,
            target_dq_layer: 0,
            ec_active_idc: 0,
            parse_only: false,
            video_property: VideoProperty { size: 0, bs_type: VIDEO_BITSTREAM_DEFAULT },
        };
        (vt.initialize)(self.d, &p) == 0
    }

    fn frame(bi: &BufferInfo) -> Option<Frame> {
        if bi.status != 1 || bi.sys.width <= 0 || bi.sys.height <= 0 || bi.dst.iter().any(|p| p.is_null()) {
            return None;
        }
        let s = [bi.sys.stride[0] as usize, bi.sys.stride[1] as usize, bi.sys.stride[1] as usize];
        Some(Frame {
            pts: bi.out_ts as i64,
            width: bi.sys.width as u32,
            height: bi.sys.height as u32,
            planes: [bi.dst[0], bi.dst[1], bi.dst[2]],
            strides: s,
        })
    }

    fn blank() -> BufferInfo {
        BufferInfo { status: 0, in_ts: 0, out_ts: 0, sys: SysMemBuffer::default(), dst: [core::ptr::null_mut(); 3] }
    }

    /// Decode one access unit; returns OpenH264's DECODING_STATE bits (0 = no error).
    pub fn decode(&mut self, au: &[u8], pts: i64, out: &mut dyn FnMut(&Frame)) -> u32 {
        let mut bi = Self::blank();
        bi.in_ts = pts as u64;
        let mut dst: [*mut u8; 3] = [core::ptr::null_mut(); 3];
        let st = (self.vt().decode_frame_no_delay)(self.d, au.as_ptr(), au.len() as i32, dst.as_mut_ptr(), &mut bi);
        bi.dst = dst;
        if let Some(f) = Self::frame(&bi) {
            out(&f);
        }
        st
    }

    /// End of stream: release every held picture.
    pub fn drain(&mut self, out: &mut dyn FnMut(&Frame)) -> u32 {
        let mut eos: i32 = 1;
        (self.vt().set_option)(self.d, OPT_END_OF_STREAM, &mut eos as *mut i32 as *mut _);
        for _ in 0..MAX_HELD {
            let mut bi = Self::blank();
            let mut dst: [*mut u8; 3] = [core::ptr::null_mut(); 3];
            (self.vt().flush_frame)(self.d, dst.as_mut_ptr(), &mut bi);
            bi.dst = dst;
            match Self::frame(&bi) {
                Some(f) => out(&f),
                None => break,
            }
        }
        0
    }

    /// Start over (a seek): uninitialise and initialise again.
    pub fn reset(&mut self) -> bool {
        (self.vt().uninitialize)(self.d);
        self.init()
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        (self.vt().uninitialize)(self.d);
        (self.destroy)(self.d);
    }
}
