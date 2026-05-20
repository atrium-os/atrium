//! atrium-tier2-fresco-bridge — adapt the tier-2 software
//! renderer's `Tier2Backend::PresentCallback` to a Fresco
//! compositor connection.
//!
//! The bridge takes a `fresco_client::Connection` and a
//! `(window_id, slot_id)` pair, and produces a `PresentCallback`
//! that on every `vkQueuePresentKHR` does:
//!
//!   1. `Connection::upload_blob(pixels)` — pushes the RGBA8
//!      bytes through aqueduct's CAS state machine.  The
//!      blob's SHA-256 hash drops out.
//!   2. `Connection::slot_set_texture(slot_id, hash, w, h,
//!      Rgba8UnormSrgb)` — binds the just-uploaded blob to
//!      the per-window slot the compositor's scene references.
//!   3. `Connection::window_present(window_id)` — kicks the
//!      compositor to re-render the window with the new
//!      texture in place.
//!
//! Scene setup (the textured rect that references `slot_id`)
//! is the consumer's responsibility — typically a single
//! `scene_node_texture` call at window creation.  The bridge
//! is intentionally narrow: it owns no scene state, just
//! forwards bytes.
//!
//! # Threading
//!
//! `Tier2Backend::set_present_callback` requires a
//! `Send + Sync + 'static` callback; `fresco_client::Connection`
//! is `!Sync` (and effectively `!Send` once wrapped in a
//! `BufReader`/`BufWriter`).  The bridge holds the Connection
//! behind an `Arc<Mutex<Connection>>` and clones the Arc into
//! each callback invocation.  Each present takes the mutex
//! for the duration of the upload + slot_set + window_present
//! triplet; if the tier-2 backend ever drives multiple
//! `vkQueuePresentKHR` calls in flight, they serialise here.
//! That's the right shape -- the Fresco wire is request/
//! response and can't tolerate interleaved uploads anyway.

#![deny(missing_docs)]

use std::sync::{Arc, Mutex};

use aqueduct_gpu_host::{PresentCallback, PresentedFrame};
use fresco_client::Connection;
use fresco_protocol::TextureFormat;

/// Adapter from `Tier2Backend::PresentCallback` to a Fresco
/// compositor connection. Holds the connection behind a
/// `Mutex` so concurrent presents serialise.
#[derive(Clone)]
pub struct Tier2FrescoBridge {
    conn: Arc<Mutex<Connection>>,
}

impl Tier2FrescoBridge {
    /// Wrap a fresh `fresco_client::Connection` for use as a
    /// tier-2 present sink.  The connection should be
    /// `connect`ed already (or `wrap`ped around an established
    /// `UnixStream`).
    pub fn new(conn: Connection) -> Self {
        Self { conn: Arc::new(Mutex::new(conn)) }
    }

    /// Borrow the inner connection for scene-setup calls
    /// (`scene_node_texture` to install a rect that references
    /// `slot_id`, `font_open`, etc.).  Mutex-protected; held
    /// only as briefly as the caller's setup needs.
    pub fn connection(&self) -> Arc<Mutex<Connection>> {
        self.conn.clone()
    }

    /// Produce a `PresentCallback` wired to forward each
    /// presented frame to the given `(window_id, slot_id)`.
    /// The Tier2Backend's incoming `surface_id` is ignored --
    /// the consumer chooses up front which Fresco window to
    /// drive.  Multi-window apps register multiple bridges.
    ///
    /// Errors at any step (upload, slot_set, present) are
    /// logged + swallowed; the bridge can't propagate them
    /// upstream past the `PresentCallback` boundary (which is
    /// fire-and-forget).
    pub fn present_callback(
        &self,
        window_id: u32,
        slot_id: u32,
    ) -> PresentCallback {
        let conn = self.conn.clone();
        Box::new(move |_surface_id: u64, frame: &PresentedFrame| {
            let mut c = match conn.lock() {
                Ok(g) => g,
                Err(_) => {
                    log::warn!("Tier2FrescoBridge: connection mutex poisoned");
                    return;
                }
            };
            let hash = match c.upload_blob(&frame.pixels) {
                Ok(h) => h,
                Err(e) => {
                    log::warn!("Tier2FrescoBridge: upload_blob failed: {e}");
                    return;
                }
            };
            if let Err(e) = c.slot_set_texture(
                slot_id, hash, frame.width, frame.height,
                TextureFormat::Rgba8UnormSrgb,
            ) {
                log::warn!("Tier2FrescoBridge: slot_set_texture failed: {e}");
                return;
            }
            if let Err(e) = c.window_present(window_id) {
                log::warn!("Tier2FrescoBridge: window_present failed: {e}");
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aqueduct::Connection as AqConn;
    use aqueduct::classes::CLASS_DISPLAY;
    use fresco_client::Connection;
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    /// Drive a Tier2FrescoBridge against a paired UnixStream
    /// where the server end runs a real `aqueduct::Conn` so
    /// CAS UPLOAD_BEGIN/DATA/FINISH are auto-handled.  Verify
    /// that one PresentedFrame produces exactly the SLOT_SET +
    /// WINDOW_PRESENT pair we expect on the wire.
    #[test]
    fn one_present_emits_slot_set_and_window_present() {
        let (client_s, server_s) = UnixStream::pair().unwrap();
        // Bounded waiting so a bug doesn't hang the test runner.
        server_s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        client_s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

        // Per-message tuple: (op, class, payload bytes).
        let (tx, rx) = mpsc::channel::<(u16, u8, Vec<u8>)>();
        // Channel to return the populated CAS bytes after the
        // server thread's drain loop finishes (the server has
        // to outlive the upload to keep the cache live).
        let (cas_tx, cas_rx) =
            mpsc::channel::<Option<(aqueduct::Hash, Vec<u8>)>>();
        let server_thread = thread::spawn(move || {
            let mut s = AqConn::wrap(server_s).expect("server Conn::wrap");
            // Drain exactly 2 visible (non-CORE) messages -- the
            // SLOT_SET + WINDOW_PRESENT pair the bridge emits per
            // present.  CAS UPLOAD_BEGIN/ACK get auto-handled
            // inside recv_message via handle_core; they don't
            // bubble up here.
            let mut slot_hash: Option<aqueduct::Hash> = None;
            for _ in 0..2 {
                match s.recv_message() {
                    Ok(m) => {
                        // Stash the SLOT_SET payload's hash so
                        // we can look it up in the server's
                        // CAS cache after the loop.
                        if m.op == fresco_protocol::control::OP_SLOT_SET
                           && m.opcode_class == CLASS_DISPLAY
                        {
                            if let Ok(p) = fresco_protocol::decode::<
                                fresco_protocol::SlotSetPayload>(&m.payload)
                            {
                                slot_hash = Some(p.hash);
                            }
                        }
                        let _ = tx.send((m.op, m.opcode_class, m.payload));
                    }
                    Err(_) => break,
                }
            }
            // Reach into the server's cache for the just-uploaded
            // blob so the test can verify pixel-byte integrity.
            let cas = slot_hash.and_then(|h| s.cache_get(&h).map(|b| (h, b)));
            let _ = cas_tx.send(cas);
        });

        let client = Connection::wrap(client_s).expect("client wrap");
        let bridge = Tier2FrescoBridge::new(client);
        let cb = bridge.present_callback(0xAAAA, 0x0001);

        let frame = PresentedFrame {
            width: 4, height: 4,
            pixels: vec![0xAB; 64],
            frame_id: 17,
        };
        cb(0xBEEF, &frame);

        // We expect SLOT_SET (DISPLAY) then WINDOW_PRESENT
        // (DISPLAY).
        let m1 = rx.recv_timeout(Duration::from_secs(2))
            .expect("first wire message");
        let m2 = rx.recv_timeout(Duration::from_secs(2))
            .expect("second wire message");
        assert_eq!(m1.1, CLASS_DISPLAY, "first message class");
        assert_eq!(m2.1, CLASS_DISPLAY, "second message class");
        assert_eq!(m1.0, fresco_protocol::control::OP_SLOT_SET);
        assert_eq!(m2.0, fresco_protocol::control::OP_WINDOW_PRESENT);

        // Decode SLOT_SET and verify TextureDesc matches the
        // input frame (4x4 Rgba8UnormSrgb) + slot_id is what
        // the bridge was configured with.
        let slot_set: fresco_protocol::SlotSetPayload =
            fresco_protocol::decode(&m1.2).expect("decode SLOT_SET");
        assert_eq!(slot_set.slot_id, 0x0001);
        match slot_set.kind {
            fresco_protocol::SlotKind::Texture(desc) => {
                assert_eq!(desc.width, 4);
                assert_eq!(desc.height, 4);
                assert!(matches!(desc.format,
                                  fresco_protocol::TextureFormat::Rgba8UnormSrgb));
            }
        }

        // Drop the bridge (and thus the client end) so the
        // server's recv loop exits cleanly.
        drop(bridge);

        // Pull the server-side CAS lookup result + verify the
        // uploaded blob bytes match the input frame.
        let cas = cas_rx.recv_timeout(Duration::from_secs(2))
            .expect("server thread should report cas lookup")
            .expect("SLOT_SET hash should land in the server's CAS cache");
        let (cas_hash, cas_bytes) = cas;
        assert_eq!(cas_bytes, frame.pixels,
            "CAS-cached bytes must match the PresentedFrame pixels");
        // And the hash should be the SLOT_SET payload's hash
        // (sanity: SlotSet refers to the just-uploaded blob).
        let slot_set: fresco_protocol::SlotSetPayload =
            fresco_protocol::decode(&m1.2).unwrap();
        assert_eq!(slot_set.hash, cas_hash,
            "SLOT_SET hash must reference the just-uploaded CAS blob");

        let _ = server_thread.join();
    }

    #[test]
    fn bridge_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Tier2FrescoBridge>();
    }
}
