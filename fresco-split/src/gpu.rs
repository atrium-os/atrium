//! The app GPU channel (F6 G2, docs/spec/gpu-app-path-f6.md): how an app
//! that brings its own shaders gets them run on the GPU without holding a
//! GPU fd.
//!
//! An app connects to the broker's GPU socket. Reaching that socket is the
//! grant: the TCB mounts it only into jails whose manifest has `gpu.access`,
//! and the broker caps contexts per peer uid. Each connection is one GPU
//! context: the broker makes a GPU VM for it on its own device fd, and drops
//! the VM when the connection ends. The app's records never touch the
//! renderer channel, so a busy GPU app cannot crowd its windows' records.
//!
//! The broker writes every PM4 packet (`atrium_gpu_app::build`). What the
//! app sends is the code header and machine code (untrusted: option A, the
//! GPU MMU confines it), buffer sizes, and work records from a closed set.
//!
//! Same framing as the renderer records ([`crate::chan`], 96 bytes): word 0
//! kind, word 1 the id the kind names, words 2..8 arguments, bytes 32..96
//! zero. The bulk data rides in shared memory the broker created:
//!
//! - the **upload** slot ([`GpuMsg::Hello`], first fd): a code header and
//!   machine code for [`GpuMsg::Code`];
//! - the **work** slot (second fd): records for [`GpuMsg::Submit`];
//! - one **staging** slot per buffer ([`GpuMsg::BufferOk`]'s fd), copied to
//!   and from the buffer by [`GpuMsg::Write`] and [`GpuMsg::Read`] (a copy
//!   for now; F4e's zero-copy import replaces it).
//!
//! The broker copies a slot out before it checks it. A record it refuses
//! gets [`GpuMsg::Refused`] and the connection is closed. Work that hangs or
//! faults gets [`GpuMsg::Lost`] and the connection is closed too: the
//! context is gone, and the app connects again for a fresh VM.

use crate::{Rec, Refused, MSG_BYTES};

/// Most code objects one context may hold.
pub const MAX_CODES: u32 = 64;
/// Most buffers one context may hold.
pub const MAX_BUFFERS: u32 = 256;
/// Largest single buffer.
pub const MAX_BUFFER_BYTES: u32 = 64 << 20;
/// Most buffer bytes one context may hold in all.
pub const MAX_CONTEXT_BYTES: u64 = 256 << 20;

/// Why the broker refused, in [`GpuMsg::Refused`]. `detail` carries the
/// record index for work refusals and is zero otherwise.
pub mod reason {
    /// A record that does not parse, or one only the broker sends.
    pub const MALFORMED: u32 = 1;
    /// The code header or code size.
    pub const CODE: u32 = 2;
    /// The work records (`detail`: the record number).
    pub const WORK: u32 = 3;
    /// Over a per-context limit (codes, buffers, bytes, slot size).
    pub const LIMIT: u32 = 4;
    /// An id that is not one of this context's, or one already in use.
    pub const ID: u32 = 5;
    /// An offset or length outside the buffer or slot.
    pub const RANGE: u32 = 6;
    /// The broker could not do it (no GPU, out of memory).
    pub const DEVICE: u32 = 7;
    /// Records faster than the broker serves them.
    pub const RATE: u32 = 8;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GpuMsg {
    // ── app → broker ──────────────────────────────────────────────
    /// Load the first `bytes` of the upload slot (code header + machine
    /// code) as code object `id`. Answered by `CodeOk`.
    Code { id: u32, bytes: u32 },
    /// Make buffer `id` of `bytes` in this context's VM. Answered by
    /// `BufferOk` with the staging slot as its fd.
    Buffer { id: u32, bytes: u32 },
    /// Free buffer `id`. No answer.
    Free { id: u32 },
    /// Copy `bytes` at `offset` of buffer `id`'s staging slot into the
    /// buffer. No answer: later work sees it.
    Write { id: u32, offset: u32, bytes: u32 },
    /// Copy `bytes` at `offset` of buffer `id` into its staging slot.
    /// Answered by `ReadDone`.
    Read { id: u32, offset: u32, bytes: u32 },
    /// Run the first `bytes` of the work slot (whole records) and answer
    /// `Done` or `Lost` with `fence`.
    Submit { fence: u32, bytes: u32 },

    // ── broker → app ──────────────────────────────────────────────
    /// The first record: the broker's GPU architecture (the id code
    /// headers must carry; `atrium_gpu_app::ARCH_RDNA4` is 1), and the
    /// upload and work slots, which follow as fds.
    Hello { arch: u32, upload_bytes: u32, work_bytes: u32 },
    CodeOk { id: u32 },
    /// Buffer `id` is at `va` in this context's VM; its staging slot of
    /// `bytes` is the fd.
    BufferOk { id: u32, va: u64, bytes: u32 },
    ReadDone { id: u32 },
    Done { fence: u32 },
    /// The work behind `fence` hung or faulted (`errno`, usually EIO). The
    /// context is gone and the broker closes the connection.
    Lost { fence: u32, errno: u32 },
    /// The broker refused the last record and closes the connection.
    Refused { reason: u32, detail: u32 },
}

mod kind {
    pub const CODE: u32 = 0x301;
    pub const BUFFER: u32 = 0x302;
    pub const FREE: u32 = 0x303;
    pub const WRITE: u32 = 0x304;
    pub const READ: u32 = 0x305;
    pub const SUBMIT: u32 = 0x306;
    pub const HELLO: u32 = 0x380;
    pub const CODE_OK: u32 = 0x381;
    pub const BUFFER_OK: u32 = 0x382;
    pub const READ_DONE: u32 = 0x383;
    pub const DONE: u32 = 0x384;
    pub const LOST: u32 = 0x385;
    pub const REFUSED: u32 = 0x386;
}

impl GpuMsg {
    pub fn encode(&self) -> [u8; MSG_BYTES] {
        use kind::*;
        let (k, id, args): (u32, u32, &[u32]) = match *self {
            GpuMsg::Code { id, bytes } => (CODE, id, &[bytes]),
            GpuMsg::Buffer { id, bytes } => (BUFFER, id, &[bytes]),
            GpuMsg::Free { id } => (FREE, id, &[]),
            GpuMsg::Write { id, offset, bytes } => (WRITE, id, &[offset, bytes]),
            GpuMsg::Read { id, offset, bytes } => (READ, id, &[offset, bytes]),
            GpuMsg::Submit { fence, bytes } => (SUBMIT, fence, &[bytes]),
            GpuMsg::Hello { arch, upload_bytes, work_bytes } => (HELLO, 0, &[arch, upload_bytes, work_bytes]),
            GpuMsg::CodeOk { id } => (CODE_OK, id, &[]),
            GpuMsg::BufferOk { id, va, bytes } => {
                return Self::rec(BUFFER_OK, id, &[va as u32, (va >> 32) as u32, bytes]);
            }
            GpuMsg::ReadDone { id } => (READ_DONE, id, &[]),
            GpuMsg::Done { fence } => (DONE, fence, &[]),
            GpuMsg::Lost { fence, errno } => (LOST, fence, &[errno]),
            GpuMsg::Refused { reason, detail } => (REFUSED, 0, &[reason, detail]),
        };
        Self::rec(k, id, args)
    }

    fn rec(k: u32, id: u32, args: &[u32]) -> [u8; MSG_BYTES] {
        let mut r = Rec::new(k, id);
        for (i, &a) in args.iter().enumerate() {
            r.put(2 + i, a);
        }
        r.0
    }

    /// Parse a record: known kind, ids and sizes in range, unused bytes
    /// zero.
    pub fn decode(bytes: &[u8]) -> Result<GpuMsg, Refused> {
        use kind::*;
        let b: [u8; MSG_BYTES] = bytes.try_into().map_err(|_| Refused("record size"))?;
        let r = Rec(b);
        let (k, id) = (r.get(0), r.get(1));
        let args = match k {
            FREE | CODE_OK | READ_DONE | DONE => 0,
            CODE | BUFFER | SUBMIT | LOST => 1,
            WRITE | READ | REFUSED => 2,
            HELLO | BUFFER_OK => 3,
            _ => return Err(Refused("unknown kind")),
        };
        if (2 + args..8).any(|i| r.get(i) != 0) || r.0[32..].iter().any(|&c| c != 0) {
            return Err(Refused("unused field not zero"));
        }
        if matches!(k, HELLO | REFUSED) && id != 0 {
            return Err(Refused("id on a record that names none"));
        }
        let code_id = |v: u32| if v < MAX_CODES { Ok(v) } else { Err(Refused("code id out of range")) };
        let buf_id = |v: u32| if v < MAX_BUFFERS { Ok(v) } else { Err(Refused("buffer id out of range")) };
        let size = |v: u32| if (1..=MAX_BUFFER_BYTES).contains(&v) { Ok(v) } else { Err(Refused("size out of range")) };
        Ok(match k {
            CODE => GpuMsg::Code { id: code_id(id)?, bytes: size(r.get(2))? },
            BUFFER => GpuMsg::Buffer { id: buf_id(id)?, bytes: size(r.get(2))? },
            FREE => GpuMsg::Free { id: buf_id(id)? },
            WRITE => GpuMsg::Write { id: buf_id(id)?, offset: r.get(2), bytes: size(r.get(3))? },
            READ => GpuMsg::Read { id: buf_id(id)?, offset: r.get(2), bytes: size(r.get(3))? },
            SUBMIT => GpuMsg::Submit { fence: id, bytes: size(r.get(2))? },
            HELLO => GpuMsg::Hello { arch: r.get(2), upload_bytes: r.get(3), work_bytes: r.get(4) },
            CODE_OK => GpuMsg::CodeOk { id: code_id(id)? },
            BUFFER_OK => GpuMsg::BufferOk {
                id: buf_id(id)?,
                va: r.get(2) as u64 | (r.get(3) as u64) << 32,
                bytes: size(r.get(4))?,
            },
            READ_DONE => GpuMsg::ReadDone { id: buf_id(id)? },
            DONE => GpuMsg::Done { fence: id },
            LOST => GpuMsg::Lost { fence: id, errno: r.get(2) },
            _ => GpuMsg::Refused { reason: r.get(2), detail: r.get(3) },
        })
    }

    /// Records an app may send. The broker refuses any other kind from an
    /// app.
    pub fn from_app(&self) -> bool {
        matches!(
            self,
            GpuMsg::Code { .. }
                | GpuMsg::Buffer { .. }
                | GpuMsg::Free { .. }
                | GpuMsg::Write { .. }
                | GpuMsg::Read { .. }
                | GpuMsg::Submit { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let all = [
            GpuMsg::Code { id: 3, bytes: 100 },
            GpuMsg::Buffer { id: 255, bytes: MAX_BUFFER_BYTES },
            GpuMsg::Free { id: 1 },
            GpuMsg::Write { id: 2, offset: 4096, bytes: 64 },
            GpuMsg::Read { id: 2, offset: 0, bytes: 64 },
            GpuMsg::Submit { fence: u32::MAX, bytes: 240 },
            GpuMsg::Hello { arch: 1, upload_bytes: 2 << 20, work_bytes: 32768 },
            GpuMsg::CodeOk { id: 63 },
            GpuMsg::BufferOk { id: 7, va: 0x1_2345_6000, bytes: 4096 },
            GpuMsg::ReadDone { id: 7 },
            GpuMsg::Done { fence: 9 },
            GpuMsg::Lost { fence: 9, errno: 5 },
            GpuMsg::Refused { reason: reason::WORK, detail: 4 },
        ];
        for m in all {
            assert_eq!(GpuMsg::decode(&m.encode()), Ok(m.clone()), "{m:?}");
        }
    }

    #[test]
    fn refusals() {
        let ok = GpuMsg::Write { id: 2, offset: 0, bytes: 64 }.encode();
        let mut bad = ok;
        bad[16] = 1; // word 4: unused by WRITE
        assert!(GpuMsg::decode(&bad).is_err());
        let mut bad = ok;
        bad[95] = 1;
        assert!(GpuMsg::decode(&bad).is_err());
        let mut bad = ok;
        bad[0] = 0x99;
        assert!(GpuMsg::decode(&bad).is_err());
        assert!(GpuMsg::decode(&ok[..95]).is_err());
        assert!(GpuMsg::decode(&GpuMsg::Code { id: MAX_CODES, bytes: 8 }.encode()).is_err());
        assert!(GpuMsg::decode(&GpuMsg::Buffer { id: MAX_BUFFERS, bytes: 8 }.encode()).is_err());
        assert!(GpuMsg::decode(&GpuMsg::Buffer { id: 0, bytes: 0 }.encode()).is_err());
        assert!(GpuMsg::decode(&GpuMsg::Buffer { id: 0, bytes: MAX_BUFFER_BYTES + 1 }.encode()).is_err());
        let mut hello = GpuMsg::Hello { arch: 1, upload_bytes: 1, work_bytes: 1 }.encode();
        hello[4] = 1;
        assert!(GpuMsg::decode(&hello).is_err());
        // The renderer protocol's kinds are not GPU records.
        assert!(GpuMsg::decode(&crate::Msg::WindowDestroy { window: 1 }.encode()).is_err());
    }

    #[test]
    fn only_requests_come_from_apps() {
        assert!(GpuMsg::Submit { fence: 0, bytes: 80 }.from_app());
        assert!(!GpuMsg::Done { fence: 0 }.from_app());
        assert!(!GpuMsg::Hello { arch: 1, upload_bytes: 0, work_bytes: 0 }.from_app());
    }
}
