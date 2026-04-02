use crate::scene::graph::SceneGraph;
use crate::cas::store::CasStore;
use std::sync::Arc;
use winit::window::Window;

pub trait GpuBackend: Send {
    fn new(window: Arc<Window>, width: u32, height: u32) -> Self where Self: Sized;
    fn resize(&mut self, width: u32, height: u32);
    fn set_scale(&mut self, scale: f64);
    fn render_frame(&mut self, scene: &SceneGraph, cas: &CasStore, frame: u64, cursor: Option<(f32, f32)>);
    fn tessellate_path(&mut self, segments_data: &[u8], tolerance: f32, fill: bool) -> Option<(Vec<f32>, Vec<u16>)>;
}
