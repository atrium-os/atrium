use fresco_server::platform::ivshmem::IvshmemLink;
use fresco_server::platform::ivshmem_server::IvshmemServer;
use fresco_server::platform::network::NetworkLink;
use fresco_server::command::frontend::CommandFrontend;
use fresco_server::scene::graph::SceneGraph;
use fresco_server::cas::store::CasStore;
use fresco_server::render::backend::GpuBackend;
use fresco_server::render::metrics::FrameMetrics;
use fresco_server::input::capture::InputCapture;
use fresco_server::{cas, command, input, platform, render, scene, window};

use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use std::time::{Duration, Instant};
use winit::window::{Window, WindowId};

use std::sync::{Arc, Mutex};
use std::path::PathBuf;

extern "C" {
    /// Direct libc read — used by the doorbell-pipe wakeup thread.
    /// Std's File::read would also work but goes through buffered I/O
    /// machinery we don't need here.
    #[link_name = "read"]
    fn libc_read(fd: i32, buf: *mut u8, count: usize) -> isize;
}

/// Wake the winit event loop from outside (the doorbell-pipe reader
/// thread). One variant suffices — any wake means "check command
/// ring and redraw if needed".
#[derive(Debug, Clone, Copy)]
enum WakeEvent {
    Doorbell,
}

struct GpuServer {
    window: Option<Arc<Window>>,
    renderer: Option<render::metal_backend::MetalRenderer>,
    cas: Arc<Mutex<CasStore>>,
    scene: Arc<Mutex<SceneGraph>>,
    compositor: Arc<Mutex<window::Compositor>>,
    frontend: CommandFrontend,
    input_capture: InputCapture,
    link: IvshmemLink,
    doorbell: Option<IvshmemServer>,
    net_link: Option<NetworkLink>,
    metrics: FrameMetrics,
    qemu_cmd: Option<Vec<String>>,
    qemu_launched: bool,
    system_font: Option<Vec<u8>>,
    last_cursor_x: f32,
    last_cursor_y: f32,
    /// Set when the WM intercept changes window state (drag, raise,
    /// ...) so the next redraw recomposes even if no client sent a
    /// frame and the input cursor didn't move.
    wm_dirty: bool,
    /// Snapshot of the kmod's slot-alive bitmap from the previous
    /// poll. Any bit that goes 1→0 is a client disconnect; the
    /// server sweeps that slot's owned windows.
    last_alive_mask: u32,
    proxy: Option<EventLoopProxy<WakeEvent>>,
    wakeup_thread_started: bool,
}

impl GpuServer {
    fn new(shmem_path: PathBuf, shmem_size: usize, net_port: Option<u16>) -> Self {
        let link = IvshmemLink::open(&shmem_path, shmem_size)
            .expect("failed to open ivshmem region");

        // Start doorbell server and wait for QEMU to connect.
        // QEMU's -chardev socket needs the connection accepted and init
        // messages sent before the machine can boot.
        let sock_path = shmem_path.with_extension("sock");
        let doorbell = match IvshmemServer::new(&sock_path, &shmem_path, shmem_size) {
            Ok(mut s) => {
                log::info!("Waiting for QEMU to connect to {:?}...", sock_path);
                // Block until QEMU connects (up to 60s)
                for _ in 0..600 {
                    if s.try_accept() { break; }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                if s.has_peer() {
                    log::info!("QEMU connected to ivshmem-server");
                } else {
                    log::warn!("QEMU did not connect within timeout");
                }
                Some(s)
            }
            Err(e) => {
                log::warn!("doorbell server failed: {}, falling back to polling", e);
                None
            }
        };

        let net_link = net_port.and_then(|port| {
            NetworkLink::bind(port).ok()
        });

        let cas = Arc::new(Mutex::new(CasStore::new()));
        let scene = Arc::new(Mutex::new(SceneGraph::new()));
        let slot_table = Arc::new(Mutex::new(scene::slots::SlotTable::new()));
        // Compositor's window 0 (the "screen") shares its scene +
        // slot_table Arcs with GpuServer.scene/slot_table so the
        // renderer can read window 0 directly.
        let compositor = Arc::new(Mutex::new(window::Compositor::new_with_window0(
            scene.clone(),
            slot_table.clone(),
        )));
        compositor.lock().unwrap().init_decorations(&mut cas.lock().unwrap());
        let frontend = CommandFrontend::new(cas.clone(), scene.clone(), slot_table, compositor.clone());
        let input_capture = InputCapture::new();

        Self {
            window: None,
            renderer: None,
            cas,
            scene,
            compositor,
            frontend,
            input_capture,
            link,
            doorbell,
            net_link,
            metrics: FrameMetrics::new(),
            qemu_cmd: None,
            qemu_launched: false,
            system_font: None,
            last_cursor_x: 0.0,
            last_cursor_y: 0.0,
            wm_dirty: false,
            last_alive_mask: 0,
            proxy: None,
            wakeup_thread_started: false,
        }
    }

    /// Spawn a thread that blocks on the doorbell pipe and wakes the
    /// winit event loop when the guest fires the doorbell. Lets the
    /// loop run on `ControlFlow::Wait` instead of vsync polling, so
    /// the GPU can drop to idle DVFS state when the scene is static.
    fn start_wakeup_thread(&mut self) {
        if self.wakeup_thread_started { return; }
        let (Some(db), Some(proxy)) = (self.doorbell.as_ref(), self.proxy.clone())
        else { return; };
        let fd = db.doorbell_read_fd();
        std::thread::Builder::new()
            .name("fresco-doorbell".into())
            .spawn(move || {
                let mut buf = [0u8; 64];
                loop {
                    let n = unsafe {
                        libc_read(fd, buf.as_mut_ptr(), buf.len())
                    };
                    if n <= 0 {
                        // pipe closed or error — retry briefly then bail
                        std::thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                    if proxy.send_event(WakeEvent::Doorbell).is_err() {
                        return;  // event loop gone
                    }
                }
            })
            .expect("spawn wakeup thread");
        self.wakeup_thread_started = true;
        log::info!("doorbell wakeup thread started (fd={})", fd);
    }

    fn check_guest_reset(&mut self) {
        if self.link.status() != 0 { return; }

        log::info!("Guest reset detected — reinitializing");
        self.link.reset_rings();

        // clear scene and CAS
        {
            let mut scene = self.scene.lock().unwrap();
            scene.clear();
        }
        {
            let mut cas = self.cas.lock().unwrap();
            cas.clear();
        }
        self.frontend.reset();
        self.metrics = FrameMetrics::new();

        // re-load system font into fresh CAS
        if let Some(ref font_bytes) = self.system_font {
            let mut cas = self.cas.lock().unwrap();
            let font_hash = cas.store_pinned(font_bytes);
            self.link.set_system_font_hash(&font_hash);
        }

        // re-publish display info (logical size, not physical) and set READY
        if let Some(window) = &self.window {
            let scale = window.scale_factor();
            let size = window.inner_size().to_logical::<u32>(scale);
            self.link.set_display_info(size.width, size.height, 60);
        }
        self.link.set_status(1);
        if let Some(ref mut db) = self.doorbell {
            db.reset();
        }
        log::info!("Guest reconnected — ready");
    }

    fn process_commands(&mut self) -> bool {
        let mut needs_render = false;
        // check for guest reboot
        self.check_guest_reset();

        // Detect client disconnects via the kmod-maintained alive
        // bitmap. A 1→0 transition means a process holding that slot
        // exited (or its fd was closed); sweep its windows so they
        // don't linger on screen.
        let alive = self.link.slots_alive_mask();
        let disconnected = self.last_alive_mask & !alive;
        if disconnected != 0 {
            self.cleanup_disconnected_clients(disconnected);
            needs_render = true;
        }
        self.last_alive_mask = alive;

        // accept QEMU doorbell connection
        if let Some(ref mut db) = self.doorbell {
            db.try_accept();
        }

        // Shared memory transport — fan-in across all client slots.
        // Each open() of /dev/fresco0 in the guest gets a slot index;
        // we drain each slot's cmd ring and route completions back to
        // the originating slot.
        let mut had_completions = false;
        // Round-robin across slots with a per-slot drain cap to
        // prevent any one client from monopolizing the server. We
        // keep cycling until every slot is empty within its cap.
        // With DMA upload in place this is mostly defensive — a
        // misbehaving client can no longer flood the cmd ring with
        // 36 000 chunked atlas-upload entries — but the cap keeps
        // future high-rate clients from starving their neighbors.
        const MAX_DRAIN_PER_ROUND: u32 = 64;
        'rounds: loop {
            let mut any_drained = false;
            for slot in 0..fresco_server::platform::ivshmem::NUM_CLIENT_SLOTS {
            let mut drained_this_round = 0u32;
            loop {
                if drained_this_round >= MAX_DRAIN_PER_ROUND { break; }
                let cmd = self.link.command_ring(slot).dequeue();
                let Some(cmd) = cmd else { break; };
                drained_this_round += 1;
                any_drained = true;
                self.metrics.record_cmd();

                // CMD_UPLOAD_DMA fast path: client memcpy'd the blob
                // into its slot's staging window; we read straight
                // from there. One command instead of len/116 inline
                // chunks. Stays inside main.rs because it needs
                // direct ivshmem access.
                if cmd.opcode == command::protocol::CMD_UPLOAD_DMA {
                    let len = cmd.u32_at(8) as usize;
                    let staging = self.link.read_slot_staging(slot, 0, len);
                    let comp = match staging {
                        Some(data) => {
                            let mut cas = self.cas.lock().unwrap();
                            let hash = cas.store(data);
                            self.metrics.record_upload(len as u64);
                            command::protocol::Completion::upload_complete(
                                cmd.sequence_id, hash)
                        }
                        None => {
                            log::warn!("UPLOAD_DMA: slot {} length {} exceeds staging window",
                                slot, len);
                            command::protocol::Completion::error(cmd.sequence_id,
                                command::protocol::STATUS_INVALID_HASH)
                        }
                    };
                    self.link.completion_ring(slot).enqueue(&comp);
                    had_completions = true;
                    continue;
                }

                let completion = self.frontend.dispatch(&cmd, slot as u8);
                if cmd.opcode == 0x0300
                    || cmd.opcode == 0x0304
                    || (cmd.opcode & 0xff00) == 0x0500
                {
                    needs_render = true;
                }
                if cmd.opcode == 0x0003 && self.frontend.last_upload_size > 0 {
                    self.metrics.record_upload(self.frontend.last_upload_size);
                    self.frontend.last_upload_size = 0;
                }
                if let Some(comp) = completion {
                    self.link.completion_ring(slot).enqueue(&comp);
                    had_completions = true;
                }
                if !self.frontend.pending_completions.is_empty() {
                    let pending: Vec<_> = self.frontend.pending_completions.drain(..).collect();
                    let mut ring = self.link.completion_ring(slot);
                    for c in pending {
                        ring.enqueue(&c);
                        had_completions = true;
                    }
                }
            }
            }
            if !any_drained { break 'rounds; }
        }

        // signal guest via doorbell interrupt (MSI-X)
        if had_completions {
            if let Some(ref db) = self.doorbell {
                if db.has_peer() {
                    db.notify_peer();
                }
            }
        }
        // One-time log for first completion
        {
            static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if had_completions && !LOGGED.load(std::sync::atomic::Ordering::Relaxed) {
                LOGGED.store(true, std::sync::atomic::Ordering::Relaxed);
                log::info!("First completion batch processed, doorbell fired");
            }
        }

        // network transport
        if let Some(ref mut net) = self.net_link {
            for _ in 0..256 {
                match net.recv_command() {
                    Some(cmd) => {
                        self.metrics.record_cmd();
                        // Network transport currently single-client (slot 0).
                        let completion = self.frontend.dispatch(&cmd, 0);
                        if cmd.opcode == 0x0003 && self.frontend.last_upload_size > 0 {
                            self.metrics.record_upload(self.frontend.last_upload_size);
                            self.frontend.last_upload_size = 0;
                        }
                        if let Some(comp) = completion {
                            net.send_completion(&comp);
                        }
                    }
                    None => break,
                }
            }
        }
        needs_render
    }

    /// Decide which window an input event should be routed to. The
    /// guest receives the event tagged with this id so apps can
    /// filter input by window. 0 = no specific window (screen).
    ///
    /// - Pointer events: hit-test cursor against window content rects
    ///   in z-order; falls back to 0 if cursor is over the screen.
    /// - Keyboard events: routed to the focused window.
    /// - Other events (resize, scroll without hover, ...): 0.
    fn resolve_input_target(&self, event: &WindowEvent) -> u32 {
        let comp = self.compositor.lock().unwrap();
        match event {
            WindowEvent::CursorMoved { .. }
            | WindowEvent::MouseInput { .. }
            | WindowEvent::MouseWheel { .. } => {
                let (cx, cy) = comp.cursor;
                comp.hit_content(cx, cy).map(|id| id as u32).unwrap_or(0)
            }
            WindowEvent::KeyboardInput { .. } => {
                comp.focus.map(|id| id as u32).unwrap_or(0)
            }
            _ => 0,
        }
    }

    /// Tear down every window owned by any client whose alive bit
    /// just dropped. `disconnected_mask` has one bit set per slot
    /// that transitioned 1→0 since the last poll. Emits FOCUS
    /// shifts to surviving clients, frees per-window FBOs via the
    /// next `sync_fbos` pass, and marks the screen dirty so the
    /// recompose drops the dead decorations.
    fn cleanup_disconnected_clients(&mut self, disconnected_mask: u32) {
        let mut comp = self.compositor.lock().unwrap();
        let mut all_shifts = Vec::new();
        for slot in 0..fresco_server::platform::ivshmem::NUM_CLIENT_SLOTS {
            if disconnected_mask & (1u32 << slot) == 0 { continue; }
            let owner = slot as u32;
            let owned: Vec<u16> = comp.windows.iter()
                .filter(|(&id, w)| id != 0 && w.owner == owner)
                .map(|(&id, _)| id)
                .collect();
            if owned.is_empty() {
                log::info!("client slot {} disconnected (no windows)", slot);
                continue;
            }
            log::info!("client slot {} disconnected — tearing down {} window(s): {:?}",
                slot, owned.len(), owned);
            for id in owned {
                let (_, shift) = comp.destroy_with_focus_shift(id, owner);
                if let Some(fc) = shift {
                    all_shifts.push(fc);
                }
            }
        }
        drop(comp);
        // Emit accumulated focus shifts (each dispatches to the
        // owners' completion rings).
        for fc in all_shifts {
            self.emit_focus_change(Some(fc));
        }
        self.wm_dirty = true;
    }

    /// Push FOCUS completions for a focus shift onto the originating
    /// owners' completion rings and ring the doorbell so guests learn
    /// immediately. Blurred goes to `prev`'s owner, focused goes to
    /// `new`'s owner — they may be different clients.
    fn emit_focus_change(&mut self, shift: Option<window::FocusChange>) {
        let Some(fc) = shift else { return; };
        let comp = self.compositor.lock().unwrap();
        let prev_owner = fc.prev
            .and_then(|id| comp.windows.get(&id))
            .map(|w| w.owner as usize);
        let new_owner = comp.windows.get(&fc.new)
            .map(|w| w.owner as usize)
            .unwrap_or(0);
        drop(comp);
        if let (Some(prev), Some(slot)) = (fc.prev, prev_owner) {
            self.link.completion_ring(slot).enqueue(&command::protocol::Completion {
                comp_type:   command::protocol::COMP_WINDOW_FOCUS,
                status:      0,
                id:          prev as u32,
                result_hash: [0u8; 32],
                _pad:        [0; 22],
            });
        }
        self.link.completion_ring(new_owner).enqueue(&command::protocol::Completion {
            comp_type:   command::protocol::COMP_WINDOW_FOCUS,
            status:      1,
            id:          fc.new as u32,
            result_hash: [0u8; 32],
            _pad:        [0; 22],
        });
        if let Some(ref db) = self.doorbell {
            if db.has_peer() { db.notify_peer(); }
        }
    }

    /// Server-side WM intercept for mouse events. Returns `true` if
    /// the event was handled by the compositor and should NOT be
    /// forwarded to the guest input ring.
    ///
    /// Behavior:
    /// - Press on a titlebar: start dragging, raise window. Consumed.
    /// - Press on content: raise window. NOT consumed (guest sees click).
    /// - Move while dragging: update window.pos. Consumed.
    /// - Release while dragging: end drag. Consumed.
    fn handle_wm_event(&mut self, event: &WindowEvent) -> bool {
        use winit::event::{ElementState, MouseButton};
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                let scale = self.input_capture.scale;
                let lx = (position.x / scale) as f32;
                let ly = (position.y / scale) as f32;
                let mut comp = self.compositor.lock().unwrap();
                comp.cursor = (lx, ly);
                if let Some((id, ox, oy)) = comp.dragging {
                    if let Some(win) = comp.windows.get_mut(&id) {
                        win.pos = (lx + ox, ly + oy);
                    }
                    drop(comp);
                    self.input_capture.cursor_x = position.x as f32;
                    self.input_capture.cursor_y = position.y as f32;
                    self.wm_dirty = true;
                    return true;
                }
                if let Some(anchor) = comp.resizing {
                    let dx = lx - anchor.start_cursor.0;
                    let dy = ly - anchor.start_cursor.1;
                    let mut new_x = anchor.start_pos.0;
                    let mut new_y = anchor.start_pos.1;
                    let mut new_w = anchor.start_size.0;
                    let mut new_h = anchor.start_size.1;
                    if anchor.edges & window::RESIZE_EDGE_L != 0 {
                        let nw = (anchor.start_size.0 - dx).max(window::MIN_WINDOW_W);
                        new_x = anchor.start_pos.0 + (anchor.start_size.0 - nw);
                        new_w = nw;
                    }
                    if anchor.edges & window::RESIZE_EDGE_R != 0 {
                        new_w = (anchor.start_size.0 + dx).max(window::MIN_WINDOW_W);
                    }
                    if anchor.edges & window::RESIZE_EDGE_T != 0 {
                        let nh = (anchor.start_size.1 - dy).max(window::MIN_WINDOW_H);
                        new_y = anchor.start_pos.1 + (anchor.start_size.1 - nh);
                        new_h = nh;
                    }
                    if anchor.edges & window::RESIZE_EDGE_B != 0 {
                        new_h = (anchor.start_size.1 + dy).max(window::MIN_WINDOW_H);
                    }
                    if let Some(win) = comp.windows.get_mut(&anchor.id) {
                        win.pos = (new_x, new_y);
                        win.size = (new_w, new_h);
                    }
                    // Re-layout title to track the new width's ellipsis.
                    let mut cas = self.cas.lock().unwrap();
                    comp.rebuild_window_title(anchor.id, &mut cas);
                    drop(cas);
                    drop(comp);
                    self.input_capture.cursor_x = position.x as f32;
                    self.input_capture.cursor_y = position.y as f32;
                    self.wm_dirty = true;
                    return true;
                }
                false
            }
            WindowEvent::MouseInput { state: ElementState::Pressed,
                                       button: MouseButton::Left, .. } => {
                let mut comp = self.compositor.lock().unwrap();
                let (cx, cy) = comp.cursor;
                // Close button takes priority over titlebar drag.
                if let Some(id) = window::hit_close_button(&comp, cx, cy) {
                    let owner = comp.windows.get(&id)
                        .map(|w| w.owner as usize)
                        .unwrap_or(0);
                    drop(comp);
                    let comp_msg = command::protocol::Completion {
                        comp_type: command::protocol::COMP_WINDOW_CLOSE_REQUESTED,
                        status:    command::protocol::STATUS_OK,
                        id:        id as u32,
                        result_hash: [0u8; 32],
                        _pad:      [0; 22],
                    };
                    self.link.completion_ring(owner).enqueue(&comp_msg);
                    if let Some(ref db) = self.doorbell {
                        if db.has_peer() { db.notify_peer(); }
                    }
                    log::info!("WM: close-button click on window {} (owner slot {})", id, owner);
                    self.wm_dirty = true;
                    return true;
                }
                // Resize edge takes priority over titlebar drag — the
                // top edge band overlaps the titlebar's top.
                if let Some((id, edges)) = comp.hit_resize_edge(cx, cy) {
                    let win = &comp.windows[&id];
                    comp.resizing = Some(window::ResizeAnchor {
                        id,
                        edges,
                        start_cursor: (cx, cy),
                        start_pos:    win.pos,
                        start_size:   win.size,
                    });
                    let shift = comp.raise(id);
                    drop(comp);
                    self.emit_focus_change(shift);
                    self.wm_dirty = true;
                    log::info!("WM: resize start window {} edges=0x{:02x}", id, edges);
                    return true;
                }
                if let Some(id) = comp.hit_titlebar(cx, cy) {
                    let win = &comp.windows[&id];
                    let offset = (win.pos.0 - cx, win.pos.1 - cy);
                    comp.dragging = Some((id, offset.0, offset.1));
                    let shift = comp.raise(id);
                    drop(comp);
                    self.emit_focus_change(shift);
                    self.wm_dirty = true;
                    log::info!("WM: drag start window {} (cursor {},{})", id, cx, cy);
                    return true;
                }
                if let Some(id) = comp.hit_content(cx, cy) {
                    let shift = comp.raise(id);
                    drop(comp);
                    self.emit_focus_change(shift);
                    self.wm_dirty = true;
                    return false;
                }
                false
            }
            WindowEvent::MouseInput { state: ElementState::Released,
                                       button: MouseButton::Left, .. } => {
                let mut comp = self.compositor.lock().unwrap();
                if comp.dragging.take().is_some() {
                    drop(comp);
                    self.wm_dirty = true;
                    return true;
                }
                if let Some(anchor) = comp.resizing.take() {
                    let (final_w, final_h, owner) = comp.windows.get(&anchor.id)
                        .map(|w| (w.size.0 as u32, w.size.1 as u32, w.owner as usize))
                        .unwrap_or((0, 0, 0));
                    drop(comp);
                    if final_w > 0 && final_h > 0 {
                        // Emit RESIZED to the owning client so apps
                        // can re-layout their content for the new size.
                        let mut rh = [0u8; 32];
                        rh[0..4].copy_from_slice(&final_w.to_le_bytes());
                        rh[4..8].copy_from_slice(&final_h.to_le_bytes());
                        self.link.completion_ring(owner).enqueue(
                            &command::protocol::Completion {
                                comp_type: command::protocol::COMP_WINDOW_RESIZED,
                                status:    0,
                                id:        anchor.id as u32,
                                result_hash: rh,
                                _pad:      [0; 22],
                            });
                        if let Some(ref db) = self.doorbell {
                            if db.has_peer() { db.notify_peer(); }
                        }
                        log::info!("WM: resize end window {} → {}x{}",
                            anchor.id, final_w, final_h);
                    }
                    self.wm_dirty = true;
                    return true;
                }
                false
            }
            _ => false,
        }
    }

    fn write_input_events(&mut self) {
        use input::capture::{INPUT_MOUSE_MOVE, INPUT_MOUSE_BUTTON, InputEvent};

        let events = self.input_capture.drain();
        // Coalesce mouse moves (one per frame), pass all other events through
        let mut last_mouse_move: Option<InputEvent> = None;
        let mut coalesced = Vec::new();
        for evt in &events {
            if evt.event_type == INPUT_MOUSE_MOVE {
                last_mouse_move = Some(*evt);
            } else {
                if let Some(mm) = last_mouse_move.take() {
                    coalesced.push(mm);
                }
                coalesced.push(*evt);
            }
        }
        if let Some(mm) = last_mouse_move {
            coalesced.push(mm);
        }

        // Route each event to the owning client's per-slot input
        // ring. target_window=0 means "the screen / no specific
        // window" — we drop those for now (no client owns them).
        // Events targeting a window go only to that window's owner.
        for evt in &coalesced {
            if evt.target_window == 0 { continue; }
            let owner = {
                let comp = self.compositor.lock().unwrap();
                comp.windows.get(&(evt.target_window as u16))
                    .map(|w| w.owner as usize)
            };
            if let Some(slot) = owner {
                self.link.input_ring(slot).enqueue(evt);
            }
        }
        if let Some(ref mut net) = self.net_link {
            for evt in &coalesced {
                net.send_input_event(evt);
            }
        }
    }
}

impl ApplicationHandler<WakeEvent> for GpuServer {
    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _ev: WakeEvent) {
        // Doorbell fired — guest pushed something. Schedule a redraw;
        // RedrawRequested will process_commands and decide whether
        // GPU work is actually needed.
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            let attrs = Window::default_attributes()
                .with_title("Fresco")
                .with_inner_size(winit::dpi::LogicalSize::new(1024u32, 768u32));

            let window = Arc::new(event_loop.create_window(attrs).unwrap());
            let phys = window.inner_size();
            let scale = window.scale_factor();
            let logical = phys.to_logical::<u32>(scale);

            let renderer = render::metal_backend::MetalRenderer::new(
                window.clone(),
                phys.width,
                phys.height,
            );

            self.link.set_display_info(logical.width, logical.height, 60);
            if let Some(ref mut net) = self.net_link {
                net.set_display_info(logical.width, logical.height, 60);
            }

            window.set_cursor_visible(false);
            self.input_capture.scale = scale;
            window.request_redraw();

            // macOS: activate app so it receives input events when launched from terminal
            #[cfg(target_os = "macos")]
            {
                use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
                use objc2_foundation::MainThreadMarker;
                let mtm = MainThreadMarker::new().unwrap();
                let app = NSApplication::sharedApplication(mtm);
                app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
                #[allow(deprecated)]
                app.activateIgnoringOtherApps(true);
                log::info!("macOS: activated app (isActive={})", unsafe { app.isActive() });
            }

            self.window = Some(window);
            self.renderer = Some(renderer);

            // Start the doorbell wakeup thread now that the window
            // (and proxy) are alive.
            self.start_wakeup_thread();

            // Load system font into CAS
            let font_path = std::path::Path::new("/System/Library/Fonts/Geneva.ttf");
            if font_path.exists() {
                let font_bytes = std::fs::read(font_path).unwrap();
                let font_hash = {
                    let mut cas = self.cas.lock().unwrap();
                    cas.store_pinned(&font_bytes)
                };
                self.link.set_system_font_hash(&font_hash);
                // Install in compositor so server-side title text can
                // be laid out from the same font.
                self.compositor.lock().unwrap().set_font(&font_bytes);
                self.system_font = Some(font_bytes);

                // Load wood titlebar texture (CC0, Polyhaven). Set as
                // theme.titlebar_texture so decorations bake a
                // textured material with this albedo.
                let wood_path = std::path::Path::new(
                    "/Users/girivs/src/bsd/test-assets/themes/rosewood_veneer1_diff_1k.jpg");
                if wood_path.exists() {
                    let tex_hash = {
                        let mut cas = self.cas.lock().unwrap();
                        window::load_texture_from_path(&mut cas, wood_path)
                    };
                    if let Some(h) = tex_hash {
                        let mut comp = self.compositor.lock().unwrap();
                        let mut new_theme = window::Theme::default();
                        new_theme.titlebar_texture = Some(h);
                        // Use white tint so the wood appears in its
                        // natural color (any other color multiplies).
                        new_theme.titlebar_color   = 0xFFFFFFFF;
                        new_theme.titlebar_height  = 30.0;   // a bit
                                                              // taller for the textured look
                        let mut cas = self.cas.lock().unwrap();
                        comp.set_theme(new_theme, &mut cas);
                        log::info!("theme: wooden titlebar tex={:02x}{:02x}.. installed",
                            h[0], h[1]);
                    }
                }
                log::info!("System font loaded: {:02x}{:02x}.. ({} bytes)",
                    font_hash[0], font_hash[1], self.system_font.as_ref().unwrap().len());
            } else {
                log::warn!("System font not found at {:?}", font_path);
            }

            log::info!("Fresco server ready ({}x{} logical, {}x{} physical)", logical.width, logical.height, phys.width, phys.height);

            if !self.qemu_launched {
                if let Some(ref cmd) = self.qemu_cmd {
                    if !cmd.is_empty() {
                        log::info!("Launching QEMU: {} ...", &cmd[0]);
                        match std::process::Command::new(&cmd[0])
                            .args(&cmd[1..])
                            .stdin(std::process::Stdio::inherit())
                            .stdout(std::process::Stdio::inherit())
                            .stderr(std::process::Stdio::inherit())
                            .spawn()
                        {
                            Ok(child) => {
                                log::info!("QEMU launched (pid={})", child.id());
                            }
                            Err(e) => {
                                log::error!("Failed to launch QEMU: {}", e);
                            }
                        }
                        self.qemu_launched = true;
                    }
                }
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
            }

            WindowEvent::Resized(size) => {
                if let Some(renderer) = &mut self.renderer {
                    let scale = self.window.as_ref().map(|w| w.scale_factor()).unwrap_or(1.0);
                    renderer.resize(size.width, size.height);
                    renderer.set_scale(scale);
                    self.input_capture.scale = scale;
                    let logical = size.to_logical::<u32>(scale);
                    self.link.set_display_info(logical.width, logical.height, 60);
                    log::info!("Resized: {}x{} physical, {}x{} logical, scale={}",
                        size.width, size.height, logical.width, logical.height, scale);
                }
            }

            WindowEvent::RedrawRequested => {
                self.metrics.begin_frame();
                let needs_render = self.process_commands();

                let cx = self.input_capture.cursor_x;
                let cy = self.input_capture.cursor_y;
                let cursor_moved = cx != self.last_cursor_x || cy != self.last_cursor_y;
                if cursor_moved {
                    self.last_cursor_x = cx;
                    self.last_cursor_y = cy;
                }

                // Reconcile per-window FBOs against compositor windows.
                // Allocates GPU textures for new windows, frees them
                // for destroyed ones. Done before any render.
                if let Some(renderer) = &mut self.renderer {
                    let scale = self.input_capture.scale as f32;
                    // Snapshot windows: ids + size + scene Arc, then drop
                    // the compositor lock so per-window scene locks are
                    // free to acquire.
                    let snapshot: Vec<(u16, (u32, u32), Arc<Mutex<scene::graph::SceneGraph>>)> = {
                        let comp = self.compositor.lock().unwrap();
                        comp.windows.iter()
                            .filter(|(&id, _)| id != 0)
                            .map(|(&id, win)| {
                                let pw = (win.size.0 * scale).max(1.0) as u32;
                                let ph = (win.size.1 * scale).max(1.0) as u32;
                                (id, (pw, ph), win.scene.clone())
                            })
                            .collect()
                    };
                    let live: std::collections::HashMap<u16, (u32, u32)> =
                        snapshot.iter().map(|(id, sz, _)| (*id, *sz)).collect();
                    renderer.sync_fbos(&live);

                    // Render each non-screen window's scene into its FBO.
                    let cas = self.cas.lock().unwrap();
                    for (id, _, scene_arc) in &snapshot {
                        let scene = scene_arc.lock().unwrap();
                        renderer.render_window_to_fbo(*id, &scene, &cas);
                    }
                }

                // Compose overlay items (windows above the screen)
                // into the screen scene before tessellate+render.
                // The prior length is restored after, so the screen
                // scene retains only window 0's own items between
                // frames.
                let need_compose = needs_render || cursor_moved || self.wm_dirty;
                let overlay_prior_len = if need_compose {
                    let overlay = self.compositor.lock().unwrap().compose_overlay();
                    let mut scene = self.scene.lock().unwrap();
                    Some(scene.compose_append(overlay))
                } else { None };

                // Tessellate whenever we composed overlay items too —
                // otherwise on cursor-only redraws (no FRAME_END), the
                // freshly-composed glyph-path items have no
                // tessellated mesh and the renderer silently skips
                // them. The tess_cache makes repeat calls nearly free.
                if needs_render || overlay_prior_len.is_some() {
                    let (dw, dh) = self.window.as_ref()
                        .map(|w| { let s = w.inner_size(); (s.width, s.height) })
                        .unwrap_or((1024, 768));
                    let mut scene = self.scene.lock().unwrap();
                    let mut cas = self.cas.lock().unwrap();
                    if let Some(renderer) = &mut self.renderer {
                        let mut gpu_tess = |data: &[u8], tol: f32, fill: bool| {
                            renderer.tessellate_path(data, tol, fill)
                        };
                        scene.tessellate_paths(&mut cas, dw, dh, Some(&mut gpu_tess));
                    } else {
                        scene.tessellate_paths(&mut cas, dw, dh, None);
                    }
                }

                let render_items = if needs_render || cursor_moved || self.wm_dirty {
                    let cursor_pos = Some((cx, cy));
                    // Build overlay list (window FBOs to composite onto
                    // the screen) from the compositor in z-order.
                    let overlays: Vec<render::backend::WindowOverlay> = {
                        let comp = self.compositor.lock().unwrap();
                        comp.z_order.iter()
                            .filter_map(|&id| {
                                if id == 0 { return None; }
                                let w = comp.windows.get(&id)?;
                                Some(render::backend::WindowOverlay {
                                    id, x: w.pos.0, y: w.pos.1,
                                    w: w.size.0, h: w.size.1,
                                })
                            })
                            .collect()
                    };
                    if let Some(renderer) = &mut self.renderer {
                        let scene = self.scene.lock().unwrap();
                        let cas = self.cas.lock().unwrap();
                        let items = scene.render_list().len() as u32;
                        renderer.render_frame_with_overlays(&scene, &cas,
                            self.metrics.frame_count, cursor_pos, &overlays);
                        items
                    } else { 0 }
                } else { 0 };

                // Strip overlay items appended above so the screen
                // scene retains only window 0's own content for the
                // next frame's compose pass.
                if let Some(prior) = overlay_prior_len {
                    self.scene.lock().unwrap().truncate_render_list(prior);
                }
                self.wm_dirty = false;

                self.metrics.end_frame(render_items);

                // GC sweep after render — all reachable blobs are marked during
                // traverse + tessellate, and render has finished reading them
                self.frontend.maybe_sweep();

                if self.metrics.should_report() {
                    let cas = self.cas.lock().unwrap();
                    self.metrics.total_dedup_hits = cas.dedup_hits;
                    self.metrics.total_dedup_bytes_saved = cas.dedup_bytes_saved;
                    self.metrics.log_summary(
                        cas.blob_count(), cas.total_bytes(),
                        cas.gc_freed_blobs, cas.gc_freed_bytes,
                        cas.last_tree_size, cas.last_tree_shared,
                    );
                    self.metrics.mark_reported();
                }

                self.write_input_events();

                // No unconditional request_redraw here — we wake on
                // doorbell or macOS input events instead. GPU stays
                // idle when the scene is static.
            }

            ref evt => {
                let consumed = self.handle_wm_event(evt);
                if !consumed {
                    let target = self.resolve_input_target(evt);
                    self.input_capture.handle_winit_event(evt, target);
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
        }
    }
}

pub fn run() {
    env_logger::init();

    let args: Vec<String> = std::env::args().collect();

    let shmem_path = args.get(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp/karythra-gpu-shmem"));

    let shmem_size: usize = 32 * 1024 * 1024;

    // --port 9090 enables network transport (for QEMU without ivshmem)
    let net_port: Option<u16> = args.iter()
        .position(|a| a == "--port")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok());

    // --qemu <args...> launches QEMU as child after window opens
    let qemu_cmd: Option<Vec<String>> = args.iter()
        .position(|a| a == "--qemu")
        .map(|i| args[i + 1..].to_vec());

    log::info!("Fresco server starting");
    log::info!("  shmem: {:?}", shmem_path);
    if let Some(port) = net_port {
        log::info!("  network: TCP port {}", port);
    }
    if qemu_cmd.is_some() {
        log::info!("  will launch QEMU after window init");
    }

    let event_loop = EventLoop::<WakeEvent>::with_user_event().build().unwrap();
    // Sleep when there's nothing to do. Wake on:
    //   - winit window events (input, resize, focus, ...)
    //   - WakeEvent::Doorbell from the doorbell-pipe reader thread
    // GPU enters DVFS idle when the scene is static, dropping baseline
    // power use. Replaces the previous vsync-driven polling loop.
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut server = GpuServer::new(shmem_path, shmem_size, net_port);
    server.qemu_cmd = qemu_cmd;
    server.proxy = Some(event_loop.create_proxy());
    event_loop.run_app(&mut server).unwrap();
}
