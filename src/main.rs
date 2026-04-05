mod cas;
mod scene;
mod command;
mod render;
mod input;
mod platform;

use platform::ivshmem::IvshmemLink;
use platform::ivshmem_server::IvshmemServer;
use platform::network::NetworkLink;
use command::frontend::CommandFrontend;
use scene::graph::SceneGraph;
use cas::store::CasStore;
use render::backend::GpuBackend;
use render::metrics::FrameMetrics;
use input::capture::InputCapture;

use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use std::time::{Duration, Instant};
use winit::window::{Window, WindowId};

use std::sync::{Arc, Mutex};
use std::path::PathBuf;

struct GpuServer<B: GpuBackend> {
    window: Option<Arc<Window>>,
    renderer: Option<B>,
    cas: Arc<Mutex<CasStore>>,
    scene: Arc<Mutex<SceneGraph>>,
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
}

impl<B: GpuBackend> GpuServer<B> {
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
        let frontend = CommandFrontend::new(cas.clone(), scene.clone());
        let input_capture = InputCapture::new();

        Self {
            window: None,
            renderer: None,
            cas,
            scene,
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
        }
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

        // accept QEMU doorbell connection
        if let Some(ref mut db) = self.doorbell {
            db.try_accept();
        }

        // shared memory transport
        let mut had_completions = false;
        loop {
            let cmd = {
                let mut ring = self.link.command_ring();
                ring.dequeue()
            };
            match cmd {
                Some(cmd) => {
                    self.metrics.record_cmd();
                    let completion = self.frontend.dispatch(&cmd);
                    if cmd.opcode == 0x0300 || cmd.opcode == 0x0304 { needs_render = true; }
                    if cmd.opcode == 0x0003 && self.frontend.last_upload_size > 0 {
                        self.metrics.record_upload(self.frontend.last_upload_size);
                        self.frontend.last_upload_size = 0;
                    }
                    if let Some(comp) = completion {
                        let mut ring = self.link.completion_ring();
                        ring.enqueue(&comp);
                        had_completions = true;
                    }
                }
                None => break,
            }
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
                        let completion = self.frontend.dispatch(&cmd);
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

    fn write_input_events(&mut self) {
        use input::capture::{INPUT_MOUSE_MOVE, INPUT_MOUSE_BUTTON, InputEvent};

        let events = self.input_capture.drain();
        // Coalesce: one mouse move per frame, one press+release per button per frame
        let mut last_mouse_move: Option<InputEvent> = None;
        let mut saw_press: [bool; 4] = [false; 4]; // per button (left, right, middle, other)
        let mut saw_release: [bool; 4] = [false; 4];
        let mut coalesced = Vec::new();
        for evt in &events {
            if evt.event_type == INPUT_MOUSE_MOVE {
                last_mouse_move = Some(*evt);
            } else if evt.event_type == INPUT_MOUSE_BUTTON {
                let btn = (evt.code as usize).min(3);
                if evt.value_a != 0 {
                    // press — only keep first press per button
                    if !saw_press[btn] {
                        if let Some(mm) = last_mouse_move.take() {
                            coalesced.push(mm);
                        }
                        coalesced.push(*evt);
                        saw_press[btn] = true;
                    }
                } else {
                    // release — only keep first release per button
                    if !saw_release[btn] {
                        coalesced.push(*evt);
                        saw_release[btn] = true;
                    }
                }
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

        for evt in &coalesced {
            let mut ring = self.link.input_ring();
            ring.enqueue(evt);
        }
        if let Some(ref mut net) = self.net_link {
            for evt in &coalesced {
                net.send_input_event(evt);
            }
        }
    }
}

impl<B: GpuBackend> ApplicationHandler for GpuServer<B> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            let attrs = Window::default_attributes()
                .with_title("KarythraGPU")
                .with_inner_size(winit::dpi::LogicalSize::new(1024u32, 768u32));

            let window = Arc::new(event_loop.create_window(attrs).unwrap());
            let phys = window.inner_size();
            let scale = window.scale_factor();
            let logical = phys.to_logical::<u32>(scale);

            let renderer = B::new(
                window.clone(),
                phys.width,
                phys.height,
            );

            self.link.set_display_info(logical.width, logical.height, 60);
            if let Some(ref mut net) = self.net_link {
                net.set_display_info(logical.width, logical.height, 60);
            }

            window.set_cursor_visible(false);
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

            // Load system font into CAS
            let font_path = std::path::Path::new("/System/Library/Fonts/Geneva.ttf");
            if font_path.exists() {
                let font_bytes = std::fs::read(font_path).unwrap();
                let font_hash = {
                    let mut cas = self.cas.lock().unwrap();
                    cas.store_pinned(&font_bytes)
                };
                self.link.set_system_font_hash(&font_hash);
                self.system_font = Some(font_bytes);
                log::info!("System font loaded: {:02x}{:02x}.. ({} bytes)",
                    font_hash[0], font_hash[1], self.system_font.as_ref().unwrap().len());
            } else {
                log::warn!("System font not found at {:?}", font_path);
            }

            log::info!("KarythraGPU server ready ({}x{} logical, {}x{} physical)", logical.width, logical.height, phys.width, phys.height);

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
                    self.link.set_display_info(size.width, size.height, 60);
                    log::info!("Resized: {}x{} scale={}", size.width, size.height, scale);
                }
            }

            WindowEvent::RedrawRequested => {
                self.metrics.begin_frame();
                let needs_render = self.process_commands();

                if needs_render {
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

                let cx = self.input_capture.cursor_x;
                let cy = self.input_capture.cursor_y;
                let cursor_moved = cx != self.last_cursor_x || cy != self.last_cursor_y;
                if cursor_moved {
                    self.last_cursor_x = cx;
                    self.last_cursor_y = cy;
                }

                let render_items = if needs_render || cursor_moved {
                    let cursor_pos = Some((cx, cy));
                    if let Some(renderer) = &mut self.renderer {
                        let scene = self.scene.lock().unwrap();
                        let cas = self.cas.lock().unwrap();
                        let items = scene.render_list().len() as u32;
                        renderer.render_frame(&scene, &cas, self.metrics.frame_count, cursor_pos);
                        items
                    } else { 0 }
                } else { 0 };

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

                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }

            ref evt => {
                self.input_capture.handle_winit_event(evt);
            }
        }
    }
}

fn main() {
    env_logger::init();

    let args: Vec<String> = std::env::args().collect();

    let shmem_path = args.get(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp/karythra-gpu-shmem"));

    let shmem_size: usize = 16 * 1024 * 1024;

    // --port 9090 enables network transport (for QEMU without ivshmem)
    let net_port: Option<u16> = args.iter()
        .position(|a| a == "--port")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok());

    // --qemu <args...> launches QEMU as child after window opens
    let qemu_cmd: Option<Vec<String>> = args.iter()
        .position(|a| a == "--qemu")
        .map(|i| args[i + 1..].to_vec());

    log::info!("KarythraGPU server starting");
    log::info!("  shmem: {:?}", shmem_path);
    if let Some(port) = net_port {
        log::info!("  network: TCP port {}", port);
    }
    if qemu_cmd.is_some() {
        log::info!("  will launch QEMU after window init");
    }

    let event_loop = EventLoop::new().unwrap();
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut server = GpuServer::<render::metal_backend::MetalRenderer>::new(shmem_path, shmem_size, net_port);
    server.qemu_cmd = qemu_cmd;
    event_loop.run_app(&mut server).unwrap();
}
