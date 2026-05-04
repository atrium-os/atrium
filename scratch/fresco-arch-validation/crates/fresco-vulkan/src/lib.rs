//! Vulkan setup + per-frame compute/render dispatch for fresco-server.
//!
//! Step 2 of the POC: instance + surface + device + swapchain + render
//! pass + per-frame clear via `vkCmdBeginRenderPass`. Single frame in
//! flight (one fence + two semaphores). MoltenVK on macOS; vendor
//! Vulkan on Linux/FreeBSD.
//!
//! Step 4 adds bundle loading: `Renderer::load_bundle()` consumes a
//! `fresco_bundle::Bundle`, AOT-compiles its compute + graphics
//! pipelines, and registers them by op-id in the dispatch table.
//! Pipelines are not USED yet — steps 7-8 do that.
//!
//! Step 7+ adds the compute traversal kernel and indirect-instanced
//! draws on top of this scaffolding.

mod renderer;
mod pipeline;
mod resource;
mod frame;
mod reflect;

pub use renderer::{Renderer, TextureBatch};
pub use pipeline::{OpKind, OpPipelines};
pub use resource::{Resource, Texture, UploadRequest};
pub use frame::{SceneNode, TextureNode};
