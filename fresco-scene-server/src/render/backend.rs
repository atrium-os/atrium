use crate::scene::graph::SceneGraph;
use crate::cas::store::CasStore;
use std::collections::HashMap;

/// One window's overlay rect in logical pixels. The screen pass
/// composites the window's FBO color texture into this rect.
#[derive(Clone, Copy)]
pub struct WindowOverlay {
    pub id: u16,
    pub x:  f32,
    pub y:  f32,
    pub w:  f32,
    pub h:  f32,
}

/// Backend-neutral runtime methods. Construction (`new`) is platform-
/// specific and lives on the concrete backend type — winit-based
/// backends take an `Arc<winit::window::Window>`, the FreeBSD-native
/// tiny-skia backend takes an `atrium_gpu::Display` + scanout BO. Each
/// binary instantiates the backend it knows about; this trait covers
/// only the operations the per-frame rendering loop calls into.
pub trait GpuBackend: Send {
    fn resize(&mut self, width: u32, height: u32);
    fn set_scale(&mut self, scale: f64);
    fn render_frame(&mut self, scene: &SceneGraph, cas: &CasStore, frame: u64, cursor: Option<(f32, f32)>);
    /// Same as render_frame, plus a list of windows whose FBOs should
    /// be composited into the screen pass at the given pixel rects.
    /// Default impl ignores overlays and just calls render_frame.
    fn render_frame_with_overlays(
        &mut self,
        scene: &SceneGraph,
        cas: &CasStore,
        frame: u64,
        cursor: Option<(f32, f32)>,
        _overlays: &[WindowOverlay],
    ) {
        self.render_frame(scene, cas, frame, cursor);
    }
    fn tessellate_path(&mut self, segments_data: &[u8], tolerance: f32, fill: bool) -> Option<(Vec<f32>, Vec<u16>)>;
    /// Reconcile per-window FBOs against the compositor's live windows.
    /// `live` maps window_id → (physical_w, physical_h). Default impl
    /// is a no-op so non-FBO backends compile.
    fn sync_fbos(&mut self, _live: &HashMap<u16, (u32, u32)>) {}
    /// Render one window's scene into its FBO. No-op default for
    /// non-FBO backends.
    fn render_window_to_fbo(&mut self, _id: u16, _scene: &SceneGraph, _cas: &CasStore) {}
}
