mod cas;
mod scene;
mod command;
mod render;
mod input;
mod platform;

use platform::ivshmem::IvshmemLink;
use command::frontend::CommandFrontend;
use scene::graph::SceneGraph;
use cas::store::CasStore;
use render::metal_backend::MetalRenderer;
use render::metrics::FrameMetrics;
use input::capture::InputCapture;

use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

use std::sync::{Arc, Mutex};
use std::path::PathBuf;

struct GpuServer {
    window: Option<Arc<Window>>,
    renderer: Option<MetalRenderer>,
    cas: Arc<Mutex<CasStore>>,
    scene: Arc<Mutex<SceneGraph>>,
    frontend: CommandFrontend,
    input_capture: InputCapture,
    link: IvshmemLink,
    metrics: FrameMetrics,
}

impl GpuServer {
    fn new(shmem_path: PathBuf, shmem_size: usize) -> Self {
        let link = IvshmemLink::open(&shmem_path, shmem_size)
            .expect("failed to open ivshmem region");

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
            metrics: FrameMetrics::new(),
        }
    }

    fn process_commands(&mut self) {
        loop {
            let cmd = {
                let mut ring = self.link.command_ring();
                ring.dequeue()
            };
            match cmd {
                Some(cmd) => {
                    self.metrics.record_cmd();
                    let completion = self.frontend.dispatch(&cmd);
                    if cmd.opcode == 0x0003 && self.frontend.last_upload_size > 0 {
                        self.metrics.record_upload(self.frontend.last_upload_size);
                        self.frontend.last_upload_size = 0;
                    }
                    if let Some(comp) = completion {
                        let mut ring = self.link.completion_ring();
                        ring.enqueue(&comp);
                    }
                }
                None => break,
            }
        }
    }

    fn write_input_events(&mut self) {
        let events = self.input_capture.drain();
        for evt in events {
            let mut ring = self.link.input_ring();
            ring.enqueue(&evt);
        }
    }
}

impl ApplicationHandler for GpuServer {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            let attrs = Window::default_attributes()
                .with_title("KarythraGPU")
                .with_inner_size(winit::dpi::LogicalSize::new(1024u32, 768u32));

            let window = Arc::new(event_loop.create_window(attrs).unwrap());
            let size = window.inner_size();

            let renderer = MetalRenderer::new(
                window.clone(),
                size.width,
                size.height,
            );

            self.link.set_display_info(size.width, size.height, 60);

            window.request_redraw();
            self.window = Some(window);
            self.renderer = Some(renderer);

            log::info!("KarythraGPU server ready ({}x{})", size.width, size.height);
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
                    renderer.resize(size.width, size.height);
                    self.link.set_display_info(size.width, size.height, 60);
                }
            }

            WindowEvent::RedrawRequested => {
                self.metrics.begin_frame();
                self.process_commands();

                let render_items = if let Some(renderer) = &mut self.renderer {
                    let scene = self.scene.lock().unwrap();
                    let cas = self.cas.lock().unwrap();
                    let items = scene.render_list().len() as u32;
                    renderer.render_frame(&scene, &cas, self.metrics.frame_count);
                    items
                } else {
                    0
                };

                self.metrics.end_frame(render_items);

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

    let shmem_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp/karythra-gpu-shmem"));

    let shmem_size: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(16 * 1024 * 1024);

    log::info!("KarythraGPU server starting");
    log::info!("  shmem: {:?} ({}MB)", shmem_path, shmem_size / (1024 * 1024));

    let event_loop = EventLoop::new().unwrap();
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut server = GpuServer::new(shmem_path, shmem_size);
    event_loop.run_app(&mut server).unwrap();
}
