//! fresco-server-poc — proof-of-concept Fresco server.
//!
//! Step 2 of the POC: open a winit window and clear it to Atrium teal
//! via a Vulkan render pass. Validates instance/device/swapchain/
//! render-pass plumbing end-to-end on macOS via MoltenVK. Steps 3+
//! add the bundle dispatch model and per-frame compute traversal.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowAttributes, WindowId};

use std::collections::HashMap;

use atrium_rpc_display::{decode, scene_ops, RectParams, TextureParams};
use fresco_vulkan::{Renderer, SceneNode, TextureBatch, TextureNode};

mod dispatch;
use dispatch::SceneState;

/// Workspace root → bundles/atrium-core. Resolved relative to the
/// running binary's CARGO_MANIFEST_DIR (compile-time constant), so we
/// don't depend on cwd. For shipped builds this becomes a config flag.
fn atrium_core_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap()
        .parent().unwrap()
        .join("bundles/atrium-core")
}

const TITLE: &str = "fresco-server-poc — step 9 (rect + texture ops live)";
const SIZE:  (u32, u32) = (1920, 1080);

struct App {
    /* Window held in `Option` because winit creates it inside `resumed`,
     * not at app construction. Arc so we can hand a non-owning ref to
     * the renderer (raw-window-handle borrows from this). */
    window:   Option<Arc<Window>>,
    renderer: Option<Renderer>,
    /* SceneState is shared with the dispatcher thread. Step 6 drains
     * the upload queue before each render(); step 7+ also reads
     * `nodes` to populate the GPU scene buffer. */
    state:    Arc<Mutex<SceneState>>,
}

impl App {
    fn new(state: Arc<Mutex<SceneState>>) -> Self {
        Self { window: None, renderer: None, state }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = WindowAttributes::default()
            .with_title(TITLE)
            .with_inner_size(winit::dpi::PhysicalSize::new(SIZE.0, SIZE.1));
        let win = event_loop
            .create_window(attrs)
            .expect("create_window failed");
        log::info!("window created: {} {:?}", TITLE, win.inner_size());

        let mut renderer = Renderer::new(&win)
            .expect("Renderer::new — install vulkan-loader + MoltenVK");
        log::info!("vulkan renderer initialised");

        let bundle_path = atrium_core_path();
        renderer.load_bundle(&bundle_path)
            .unwrap_or_else(|e| panic!(
                "load_bundle({}): {e:?}\n\
                 (did you run bundles/atrium-core/build.sh?)",
                 bundle_path.display()));
        log::info!("clearing to Atrium teal; pipelines AOT-compiled but not yet dispatched");

        self.window   = Some(Arc::new(win));
        self.renderer = Some(renderer);
        /* Kick off the first frame. */
        if let Some(w) = &self.window { w.request_redraw(); }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop,
                    _id: WindowId, event: WindowEvent)
    {
        match event {
            WindowEvent::CloseRequested => {
                log::info!("close requested");
                event_loop.exit();
            }
            WindowEvent::Resized(new_size) => {
                log::info!("resized: {:?}", new_size);
                if let Some(r) = &mut self.renderer {
                    if let Err(e) = r.resize() {
                        log::error!("resize failed: {e:?}");
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(r) = &mut self.renderer {
                    /* Drain whatever the dispatcher queued since the
                     * last frame (uploads + clears). Holding the lock
                     * for the duration is fine — the dispatcher only
                     * pushes; it doesn't block on read-back. */
                    let (uploads, clears, rects, textures) = {
                        let mut s = self.state.lock().unwrap();
                        let mut rects = Vec::new();
                        let mut tex_by_slot: HashMap<u32, Vec<TextureNode>>
                            = HashMap::new();
                        for (op_id, params) in s.nodes.values() {
                            match *op_id {
                                scene_ops::ATRIUM_CORE_RECT => {
                                    if let Ok(p) = decode::<RectParams>(params) {
                                        rects.push(SceneNode {
                                            position: [p.x, p.y],
                                            size:     [p.w, p.h],
                                            color:    [p.r, p.g, p.b, p.a],
                                        });
                                    }
                                }
                                scene_ops::ATRIUM_CORE_TEXTURE => {
                                    if let Ok(p) = decode::<TextureParams>(params) {
                                        tex_by_slot.entry(p.slot_id).or_default()
                                            .push(TextureNode {
                                                model: [p.x, p.y, p.w, p.h],
                                            });
                                    }
                                }
                                _ => {}
                            }
                        }
                        let textures: Vec<TextureBatch> = tex_by_slot.into_iter()
                            .map(|(slot_id, nodes)| TextureBatch { slot_id, nodes })
                            .collect();
                        (
                            std::mem::take(&mut s.pending_uploads),
                            std::mem::take(&mut s.pending_clears),
                            rects,
                            textures,
                        )
                    };
                    if !uploads.is_empty() || !clears.is_empty() {
                        if let Err(e) = r.process_uploads(uploads, clears) {
                            log::error!("process_uploads failed: {e:?}");
                        }
                    }
                    r.set_rect_nodes(rects);
                    r.set_texture_batches(textures);
                    if let Err(e) = r.render() {
                        log::error!("render failed: {e:?}");
                    }
                }
                /* Continuous redraw so animation works once we have one.
                 * Step 2 just keeps clearing; cheap. */
                if let Some(w) = &self.window { w.request_redraw(); }
            }
            _ => {}
        }
    }
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info"),
    ).init();

    log::info!("fresco-server-poc step 9 — rect + texture ops dispatching to GPU");

    /* Shared scene state. Dispatcher thread mutates; render thread
     * reads (step 7+). */
    let state = Arc::new(Mutex::new(SceneState::new()));

    /* Spawn the listener BEFORE the event loop takes over the main
     * thread. The test client can connect any time after this. */
    dispatch::spawn_listener(Arc::clone(&state))?;

    let event_loop = EventLoop::new()?;
    /* Poll, not Wait — we drive continuous render requests for now. */
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut app = App::new(state);
    event_loop.run_app(&mut app)?;
    Ok(())
}
