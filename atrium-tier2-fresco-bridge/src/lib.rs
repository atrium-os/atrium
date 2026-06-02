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
use fresco_protocol::{TextureFormat, DamageRect};

/// Per-surface routing entry. Tells the bridge which Fresco
/// window + texture slot a `Tier2Backend::present` for
/// `surface_id` should land on.
#[derive(Debug, Clone, Copy)]
pub struct SurfaceRoute {
    /// Target Fresco window-id.
    pub window_id: u32,
    /// Target per-connection texture slot id.
    pub slot_id: u32,
}

/// Adapter from `Tier2Backend::PresentCallback` to a Fresco
/// compositor connection. Holds the connection behind a
/// `Mutex` so concurrent presents serialise. A surface->route
/// table dispatches each incoming present to the right
/// (window_id, slot_id) pair, so multi-window apps drive a
/// single bridge instead of one per window.
#[derive(Clone)]
pub struct Tier2FrescoBridge {
    conn: Arc<Mutex<Connection>>,
    routes: Arc<Mutex<std::collections::HashMap<u64, SurfaceRoute>>>,
}

impl Tier2FrescoBridge {
    /// Wrap a fresh `fresco_client::Connection` for use as a
    /// tier-2 present sink.  The connection should be
    /// `connect`ed already (or `wrap`ped around an established
    /// `UnixStream`).
    pub fn new(conn: Connection) -> Self {
        Self {
            conn: Arc::new(Mutex::new(conn)),
            routes: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Borrow the inner connection for scene-setup calls
    /// (`scene_node_texture` to install a rect that references
    /// `slot_id`, `font_open`, etc.).  Mutex-protected; held
    /// only as briefly as the caller's setup needs.
    pub fn connection(&self) -> Arc<Mutex<Connection>> {
        self.conn.clone()
    }

    /// Register (or replace) the route for a Fresco surface.
    /// The `surface_id` is the value the Tier2Backend receives
    /// in its `present` call -- typically the Fresco window-id
    /// the guest's `VkSurfaceKHR` resolves to. The route's
    /// `window_id` is the Fresco window the bridge will
    /// `window_present` against, and `slot_id` is where the
    /// texture lives.
    ///
    /// Most apps register exactly once at window-create time;
    /// re-registering with the same `surface_id` is allowed
    /// and silently replaces the prior route (useful for
    /// swapchain recreation).
    pub fn register_surface(&self, surface_id: u64, route: SurfaceRoute) {
        if let Ok(mut r) = self.routes.lock() {
            r.insert(surface_id, route);
        }
    }

    /// Drop a previously-registered surface route. Presents
    /// for an unregistered surface get a warn-and-skip; the
    /// connection itself isn't touched.
    pub fn unregister_surface(&self, surface_id: u64) -> Option<SurfaceRoute> {
        self.routes.lock().ok().and_then(|mut r| r.remove(&surface_id))
    }

    /// Look up a surface's current route, if any.
    pub fn route_for(&self, surface_id: u64) -> Option<SurfaceRoute> {
        self.routes.lock().ok().and_then(|r| r.get(&surface_id).copied())
    }

    /// Produce a `PresentCallback` that dispatches by
    /// `surface_id` through the bridge's registered routes.
    /// Presents for unknown surfaces log a warn and return
    /// without touching the connection.
    ///
    /// Errors at any wire step (upload, slot_set, present)
    /// are logged + swallowed; the bridge can't propagate
    /// them upstream past the `PresentCallback` boundary
    /// (which is fire-and-forget).
    pub fn present_callback(&self) -> PresentCallback {
        let conn = self.conn.clone();
        let routes = self.routes.clone();
        Box::new(move |surface_id: u64, frame: &PresentedFrame| {
            let route = match routes.lock() {
                Ok(r) => match r.get(&surface_id) {
                    Some(rt) => *rt,
                    None => {
                        log::warn!("Tier2FrescoBridge: present on \
                                    unregistered surface {surface_id}");
                        return;
                    }
                },
                Err(_) => {
                    log::warn!("Tier2FrescoBridge: routes mutex poisoned");
                    return;
                }
            };

            let mut c = match conn.lock() {
                Ok(g) => g,
                Err(_) => {
                    log::warn!("Tier2FrescoBridge: connection mutex poisoned");
                    return;
                }
            };
            // PT fast path: a damage rect (in bounds, non-empty) means
            // only a sub-region changed — ship just those pixels via
            // OP_SLOT_UPDATE_REGION + a damaged present, instead of
            // re-uploading + re-hashing the whole surface. Requires the
            // slot to already be bound (a prior whole-surface present
            // does that); the scene-server drops region updates to
            // unbound slots, so a damaged first frame falls back safely.
            if let Some([dx, dy, dw, dh]) = frame.damage {
                let in_bounds = dw > 0 && dh > 0
                    && dx.saturating_add(dw) <= frame.width
                    && dy.saturating_add(dh) <= frame.height
                    && frame.pixels.len() >= (frame.width * frame.height * 4) as usize;
                if in_bounds {
                    // Gather the w×h sub-rect (RGBA8) row by row from the
                    // width-strided surface.
                    let mut sub = Vec::with_capacity((dw * dh * 4) as usize);
                    for row in 0..dh {
                        let src_y = dy + row;
                        let start = ((src_y * frame.width + dx) * 4) as usize;
                        let end = start + (dw * 4) as usize;
                        sub.extend_from_slice(&frame.pixels[start..end]);
                    }
                    if let Err(e) = c.slot_update_region(
                        route.slot_id, dx, dy, dw, dh, sub) {
                        log::warn!("Tier2FrescoBridge: slot_update_region \
                                    failed: {e}");
                        return;
                    }
                    if let Err(e) = c.window_present_with_damage(
                        route.window_id, DamageRect { x: dx, y: dy, w: dw, h: dh }) {
                        log::warn!("Tier2FrescoBridge: \
                                    window_present_with_damage failed: {e}");
                    }
                    return;
                }
                log::debug!("Tier2FrescoBridge: damage {:?} out of bounds for \
                             {}x{}; whole-surface present", frame.damage,
                            frame.width, frame.height);
            }

            // Whole-surface path: upload + (re)bind the slot + present.
            let hash = match c.upload_blob(&frame.pixels) {
                Ok(h) => h,
                Err(e) => {
                    log::warn!("Tier2FrescoBridge: upload_blob failed: {e}");
                    return;
                }
            };
            if let Err(e) = c.slot_set_texture(
                route.slot_id, hash, frame.width, frame.height,
                TextureFormat::Rgba8UnormSrgb,
            ) {
                log::warn!("Tier2FrescoBridge: slot_set_texture failed: {e}");
                return;
            }
            if let Err(e) = c.window_present(route.window_id) {
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
        bridge.register_surface(0xBEEF, SurfaceRoute {
            window_id: 0xAAAA, slot_id: 0x0001,
        });
        let cb = bridge.present_callback();

        let frame = PresentedFrame {
            width: 4, height: 4,
            pixels: vec![0xAB; 64],
            frame_id: 17,
            damage: None,
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

    #[test]
    fn two_surfaces_route_to_distinct_windows() {
        let (client_s, server_s) = UnixStream::pair().unwrap();
        server_s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        client_s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

        // Drain 4 visible messages (2 presents x {SLOT_SET,
        // WINDOW_PRESENT}). For each, capture (op, decoded
        // window_id for WINDOW_PRESENT, decoded slot_id for
        // SLOT_SET).
        let (tx, rx) = mpsc::channel::<(u16, u32)>();
        let server_thread = thread::spawn(move || {
            let mut s = AqConn::wrap(server_s).expect("server wrap");
            for _ in 0..4 {
                match s.recv_message() {
                    Ok(m) => {
                        let id = if m.op == fresco_protocol::control::OP_SLOT_SET {
                            fresco_protocol::decode::<
                                fresco_protocol::SlotSetPayload>(&m.payload)
                                .map(|p| p.slot_id).unwrap_or(u32::MAX)
                        } else if m.op == fresco_protocol::control::OP_WINDOW_PRESENT {
                            fresco_protocol::decode::<
                                fresco_protocol::WindowPresentPayload>(&m.payload)
                                .map(|p| p.window_id).unwrap_or(u32::MAX)
                        } else { u32::MAX };
                        let _ = tx.send((m.op, id));
                    }
                    Err(_) => break,
                }
            }
        });

        let client = Connection::wrap(client_s).expect("client wrap");
        let bridge = Tier2FrescoBridge::new(client);
        bridge.register_surface(100, SurfaceRoute {
            window_id: 0xAA01, slot_id: 1,
        });
        bridge.register_surface(200, SurfaceRoute {
            window_id: 0xAA02, slot_id: 2,
        });
        assert_eq!(bridge.route_for(100).unwrap().window_id, 0xAA01);

        let cb = bridge.present_callback();
        let f1 = PresentedFrame { width: 2, height: 2,
            pixels: vec![1; 16], frame_id: 1, damage: None };
        let f2 = PresentedFrame { width: 2, height: 2,
            pixels: vec![2; 16], frame_id: 2, damage: None };
        cb(100, &f1);
        cb(200, &f2);

        // Order: SLOT_SET(slot=1), WINDOW_PRESENT(win=0xAA01),
        //        SLOT_SET(slot=2), WINDOW_PRESENT(win=0xAA02).
        let msgs: Vec<(u16, u32)> = (0..4)
            .map(|_| rx.recv_timeout(Duration::from_secs(2))
                       .expect("wire msg"))
            .collect();
        assert_eq!(msgs[0].0, fresco_protocol::control::OP_SLOT_SET);
        assert_eq!(msgs[0].1, 1);
        assert_eq!(msgs[1].0, fresco_protocol::control::OP_WINDOW_PRESENT);
        assert_eq!(msgs[1].1, 0xAA01);
        assert_eq!(msgs[2].0, fresco_protocol::control::OP_SLOT_SET);
        assert_eq!(msgs[2].1, 2);
        assert_eq!(msgs[3].0, fresco_protocol::control::OP_WINDOW_PRESENT);
        assert_eq!(msgs[3].1, 0xAA02);

        drop(bridge);
        let _ = server_thread.join();
    }

    #[test]
    fn damaged_frame_ships_region_update_and_damaged_present() {
        let (client_s, server_s) = UnixStream::pair().unwrap();
        server_s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        client_s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

        // Server reports (op, id, region_bytes). For SLOT_UPDATE_REGION
        // id = slot_id and region_bytes = the sub-rect payload; for the
        // damaged present id = window_id.
        let (tx, rx) = mpsc::channel::<(u16, u32, Vec<u8>)>();
        let server_thread = thread::spawn(move || {
            let mut s = AqConn::wrap(server_s).expect("server wrap");
            // Whole present (SLOT_SET + WINDOW_PRESENT) then damaged
            // present (SLOT_UPDATE_REGION + WINDOW_PRESENT_DAMAGE) = 4.
            for _ in 0..4 {
                match s.recv_message() {
                    Ok(m) => {
                        use fresco_protocol::control as ctl;
                        let (id, extra) = if m.op == ctl::OP_SLOT_UPDATE_REGION {
                            fresco_protocol::decode::<
                                fresco_protocol::SlotUpdateRegionPayload>(&m.payload)
                                .map(|p| (p.slot_id, p.bytes))
                                .unwrap_or((u32::MAX, Vec::new()))
                        } else if m.op == ctl::OP_WINDOW_PRESENT_DAMAGE {
                            fresco_protocol::decode::<
                                fresco_protocol::WindowPresentDamagePayload>(&m.payload)
                                .map(|p| (p.window_id, Vec::new()))
                                .unwrap_or((u32::MAX, Vec::new()))
                        } else if m.op == ctl::OP_SLOT_SET {
                            fresco_protocol::decode::<
                                fresco_protocol::SlotSetPayload>(&m.payload)
                                .map(|p| (p.slot_id, Vec::new()))
                                .unwrap_or((u32::MAX, Vec::new()))
                        } else if m.op == ctl::OP_WINDOW_PRESENT {
                            fresco_protocol::decode::<
                                fresco_protocol::WindowPresentPayload>(&m.payload)
                                .map(|p| (p.window_id, Vec::new()))
                                .unwrap_or((u32::MAX, Vec::new()))
                        } else { (u32::MAX, Vec::new()) };
                        let _ = tx.send((m.op, id, extra));
                    }
                    Err(_) => break,
                }
            }
        });

        let client = Connection::wrap(client_s).expect("client wrap");
        let bridge = Tier2FrescoBridge::new(client);
        bridge.register_surface(100, SurfaceRoute { window_id: 0xAA01, slot_id: 1 });
        let cb = bridge.present_callback();

        // 4×4 RGBA surface; pixel (x,y) = [x, y, 0, 255] so a gathered
        // sub-rect is byte-verifiable.
        let mut pixels = Vec::with_capacity(4 * 4 * 4);
        for y in 0..4u8 { for x in 0..4u8 { pixels.extend_from_slice(&[x, y, 0, 255]); } }

        // Frame 1: whole-surface (binds the slot).
        cb(100, &PresentedFrame {
            width: 4, height: 4, pixels: pixels.clone(), frame_id: 1, damage: None });
        // Frame 2: damaged 2×2 region at (1,1).
        cb(100, &PresentedFrame {
            width: 4, height: 4, pixels: pixels.clone(), frame_id: 2,
            damage: Some([1, 1, 2, 2]) });

        let msgs: Vec<(u16, u32, Vec<u8>)> = (0..4)
            .map(|_| rx.recv_timeout(Duration::from_secs(2)).expect("wire msg"))
            .collect();
        use fresco_protocol::control as ctl;
        // Frame 1: whole-surface present.
        assert_eq!(msgs[0].0, ctl::OP_SLOT_SET);
        assert_eq!(msgs[1].0, ctl::OP_WINDOW_PRESENT);
        // Frame 2: region update + damaged present (NOT a whole upload).
        assert_eq!(msgs[2].0, ctl::OP_SLOT_UPDATE_REGION);
        assert_eq!(msgs[2].1, 1, "region update targets the bound slot");
        // The gathered sub-rect: rows y=1,2 × cols x=1,2.
        assert_eq!(msgs[2].2, vec![
            1,1,0,255,  2,1,0,255,   // y=1: (1,1),(2,1)
            1,2,0,255,  2,2,0,255,   // y=2: (1,2),(2,2)
        ]);
        assert_eq!(msgs[3].0, ctl::OP_WINDOW_PRESENT_DAMAGE);
        assert_eq!(msgs[3].1, 0xAA01);

        drop(bridge);
        let _ = server_thread.join();
    }

    #[test]
    fn unregistered_surface_skips_silently() {
        let (client_s, server_s) = UnixStream::pair().unwrap();
        server_s.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        client_s.set_read_timeout(Some(Duration::from_millis(200))).unwrap();

        let (tx, rx) = mpsc::channel::<u16>();
        let server_thread = thread::spawn(move || {
            let mut s = AqConn::wrap(server_s).expect("server wrap");
            // Drain anything that comes (should be nothing).
            while let Ok(m) = s.recv_message() {
                let _ = tx.send(m.op);
            }
        });

        let client = Connection::wrap(client_s).expect("client wrap");
        let bridge = Tier2FrescoBridge::new(client);
        // Do NOT register surface 999.
        let cb = bridge.present_callback();
        let frame = PresentedFrame { width: 1, height: 1,
            pixels: vec![9, 9, 9, 9], frame_id: 1, damage: None };
        cb(999, &frame);

        // Unregistered surface must not emit anything. Wait the
        // server's read timeout to be sure.
        let got = rx.recv_timeout(Duration::from_millis(400));
        assert!(got.is_err(),
            "unregistered surface should not produce wire traffic, \
             got op {:?}", got);

        drop(bridge);
        let _ = server_thread.join();
    }

    #[test]
    fn unregister_removes_route() {
        let bridge = {
            // Just need a Connection, even a dead-ended one --
            // we never fire a present in this test.
            let (a, _b) = UnixStream::pair().unwrap();
            Tier2FrescoBridge::new(Connection::wrap(a).unwrap())
        };
        bridge.register_surface(7, SurfaceRoute { window_id: 1, slot_id: 1 });
        assert!(bridge.route_for(7).is_some());
        let dropped = bridge.unregister_surface(7);
        assert!(dropped.is_some());
        assert!(bridge.route_for(7).is_none());
        // Unregistering again is a no-op (returns None).
        assert!(bridge.unregister_surface(7).is_none());
    }
}
