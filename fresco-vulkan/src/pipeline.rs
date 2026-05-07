//! Per-op Vulkan pipelines built from a loaded bundle.
//!
//! For each op in the bundle, we create:
//!   - a compute pipeline (the per-frame traversal kernel)
//!   - a graphics pipeline (the indirect-instanced render pipeline)
//!
//! Plus the descriptor-set layouts the shaders bind against, plus the
//! pipeline layouts that wrap them. Everything is destroyed in the
//! Renderer's Drop impl in reverse creation order.
//!
//! Descriptor-set layouts are derived from SPIR-V reflection (see
//! `reflect.rs`) — the shader IS the source of truth for which
//! (set, binding) slots exist and what descriptor type each one is.
//! The earlier hardcoded layouts (with an `OpKind` switch in this
//! file) were the step-4 starting point; SPIR-V reflection is what
//! the spec's §3 always intended.
//!
//! `OpKind` survives only as a runtime hint for the per-op buffer
//! sizing in `frame.rs` (rect uses 32-byte nodes/instances, texture
//! uses 16; per-slot descriptor pool when the render set has a
//! sampler). Whether to upgrade that to manifest-driven sizing is a
//! separate decision tracked in `frame.rs`.

use anyhow::Context;
use ash::vk;

use crate::reflect;

/// Per-op runtime shape — does the op's render set sample a texture?
/// Derived from SPIR-V reflection; controls per-slot descriptor pool
/// sizing in `OpFrameResources` (and the rect/texture node-byte split,
/// which is still a hardcoded heuristic until manifest-driven sizing
/// lands).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpKind {
    Rect,
    Texture,
    /// Rotated quad ("oriented rectangle"). 48-byte node/instance
    /// records (model + extra + color). First flavour of the path op
    /// family — generic Bezier paths land in `OpKind::Glyph` (0x1003)
    /// when atrium-text ships.
    Path,
    /// atrium-text glyph run (op_id 0x2000). 96-byte SceneNode
    /// (origin + atlas dims + color + glyph_count/offset meta) plus
    /// a separate per-frame glyphs storage buffer (one
    /// `GlyphInstance` per glyph, 32 bytes each). Compute kernel
    /// expands one node into N instance records (one per glyph).
    /// Render side binds the atlas as a per-slot texture, identical
    /// to Texture's render-set shape.
    GlyphRun,
}

/// Map op-id → kind. Hardcoded for buffer sizing only (see
/// `frame.rs::node_size_for`); descriptor layouts come from
/// reflection and don't depend on this.
pub fn op_kind(op_id: u32) -> OpKind {
    match op_id {
        0x1000 => OpKind::Rect,
        0x1001 => OpKind::Texture,
        0x1002 => OpKind::Path,
        0x2000 => OpKind::GlyphRun,
        _      => OpKind::Rect,
    }
}

/// Vulkan objects associated with one op.
pub struct OpPipelines {
    pub op_id:   u32,
    pub op_name: String,
    pub kind:    OpKind,

    pub compute_set_layout:  vk::DescriptorSetLayout,
    pub compute_pipe_layout: vk::PipelineLayout,
    pub compute_pipeline:    vk::Pipeline,

    pub render_set_layout:   vk::DescriptorSetLayout,
    pub render_pipe_layout:  vk::PipelineLayout,
    pub render_pipeline:     vk::Pipeline,
}

impl OpPipelines {
    /// Create the descriptor-set / pipeline layouts and AOT-compile
    /// both pipelines from a `LoadedOp`'s SPIR-V. The render pipeline
    /// is created compatible with `render_pass`.
    pub fn create(
        device:      &ash::Device,
        op:          &fresco_bundle::LoadedOp,
        render_pass: vk::RenderPass,
        viewport:    vk::Extent2D,
    ) -> anyhow::Result<Self> {
        let kind = op_kind(op.id);

        /* Reflect every shader's binding shape and build set layouts
         * from the union. The shader is the source of truth; if a
         * future bundle adds a uniform or a sampler, it lands here
         * automatically. */
        let compute_refl  = reflect::reflect(&op.compute_spirv)
            .with_context(|| format!("reflect compute op {} ({})", op.id, op.name))?;
        let vertex_refl   = reflect::reflect(&op.vertex_spirv)
            .with_context(|| format!("reflect vertex op {} ({})", op.id, op.name))?;
        let fragment_refl = reflect::reflect(&op.fragment_spirv)
            .with_context(|| format!("reflect fragment op {} ({})", op.id, op.name))?;

        let compute_bindings = reflect::build_set_layout_bindings(
            &[(compute_refl, vk::ShaderStageFlags::COMPUTE)], 0);
        let render_bindings  = reflect::build_set_layout_bindings(
            &[
                (vertex_refl,   vk::ShaderStageFlags::VERTEX),
                (fragment_refl, vk::ShaderStageFlags::FRAGMENT),
            ], 0);

        let compute_set_layout = create_set_layout(device, &compute_bindings)?;
        let compute_pipe_layout = create_pipe_layout(device, &[compute_set_layout])?;
        let compute_pipeline = create_compute_pipeline(
            device, &op.compute_spirv, &op.compute_entry, compute_pipe_layout)
            .with_context(|| format!("compute pipeline op {} ({})", op.id, op.name))?;

        let render_set_layout = create_set_layout(device, &render_bindings)?;
        let render_pipe_layout = create_pipe_layout(device, &[render_set_layout])?;
        let render_pipeline = create_render_pipeline(
            device, &op.vertex_spirv, &op.fragment_spirv,
            render_pipe_layout, render_pass, viewport)
            .with_context(|| format!("render pipeline op {} ({})", op.id, op.name))?;

        Ok(Self {
            op_id: op.id, op_name: op.name.clone(), kind,
            compute_set_layout, compute_pipe_layout, compute_pipeline,
            render_set_layout, render_pipe_layout, render_pipeline,
        })
    }

    /// Destroy all owned Vulkan objects. Caller must hold device idle.
    pub unsafe fn destroy(&self, device: &ash::Device) {
        device.destroy_pipeline(self.render_pipeline, None);
        device.destroy_pipeline_layout(self.render_pipe_layout, None);
        device.destroy_descriptor_set_layout(self.render_set_layout, None);
        device.destroy_pipeline(self.compute_pipeline, None);
        device.destroy_pipeline_layout(self.compute_pipe_layout, None);
        device.destroy_descriptor_set_layout(self.compute_set_layout, None);
    }
}

// ── descriptor-set layouts ──────────────────────────────────────────

fn create_set_layout(
    device:   &ash::Device,
    bindings: &[vk::DescriptorSetLayoutBinding],
) -> anyhow::Result<vk::DescriptorSetLayout> {
    let info = vk::DescriptorSetLayoutCreateInfo::default().bindings(bindings);
    Ok(unsafe { device.create_descriptor_set_layout(&info, None) }?)
}

fn create_pipe_layout(
    device: &ash::Device,
    set_layouts: &[vk::DescriptorSetLayout],
) -> anyhow::Result<vk::PipelineLayout> {
    let info = vk::PipelineLayoutCreateInfo::default().set_layouts(set_layouts);
    Ok(unsafe { device.create_pipeline_layout(&info, None) }?)
}

// ── pipeline creation ───────────────────────────────────────────────

fn create_shader_module(device: &ash::Device, spirv: &[u32])
    -> anyhow::Result<vk::ShaderModule>
{
    let info = vk::ShaderModuleCreateInfo::default().code(spirv);
    Ok(unsafe { device.create_shader_module(&info, None) }?)
}

fn create_compute_pipeline(
    device: &ash::Device,
    spirv:  &[u32],
    entry:  &str,
    layout: vk::PipelineLayout,
) -> anyhow::Result<vk::Pipeline> {
    let module = create_shader_module(device, spirv)?;
    let entry_c = std::ffi::CString::new(entry)?;
    let stage = vk::PipelineShaderStageCreateInfo::default()
        .stage(vk::ShaderStageFlags::COMPUTE)
        .module(module)
        .name(entry_c.as_c_str());
    let info = vk::ComputePipelineCreateInfo::default()
        .stage(stage)
        .layout(layout);
    let pipelines = unsafe {
        device.create_compute_pipelines(vk::PipelineCache::null(), &[info], None)
    }.map_err(|(_, e)| anyhow::anyhow!("create_compute_pipelines: {e:?}"))?;
    /* Shader modules can be destroyed after pipeline creation — they're
     * baked into the pipeline at this point. */
    unsafe { device.destroy_shader_module(module, None); }
    Ok(pipelines[0])
}

fn create_render_pipeline(
    device:      &ash::Device,
    vert_spirv:  &[u32],
    frag_spirv:  &[u32],
    layout:      vk::PipelineLayout,
    render_pass: vk::RenderPass,
    viewport:    vk::Extent2D,
) -> anyhow::Result<vk::Pipeline> {
    let vert = create_shader_module(device, vert_spirv)?;
    let frag = create_shader_module(device, frag_spirv)?;
    let main = std::ffi::CString::new("main")?;

    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vert).name(main.as_c_str()),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(frag).name(main.as_c_str()),
    ];

    /* No vertex buffer bindings — pipe_rectangle.vert generates corners
     * from gl_VertexIndex. */
    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();

    let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
        .topology(vk::PrimitiveTopology::TRIANGLE_STRIP)
        .primitive_restart_enable(false);

    let viewports = [vk::Viewport {
        x: 0.0, y: 0.0,
        width:  viewport.width  as f32,
        height: viewport.height as f32,
        min_depth: 0.0, max_depth: 1.0,
    }];
    let scissors = [vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent: viewport,
    }];
    let viewport_state = vk::PipelineViewportStateCreateInfo::default()
        .viewports(&viewports)
        .scissors(&scissors);

    /* Mark viewport + scissor dynamic so we don't have to recreate
     * pipelines on resize. The layout above is a starting value; the
     * real dimensions are bound per-frame via vkCmdSetViewport /
     * vkCmdSetScissor (step 8). */
    let dyn_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic = vk::PipelineDynamicStateCreateInfo::default()
        .dynamic_states(&dyn_states);

    let raster = vk::PipelineRasterizationStateCreateInfo::default()
        .polygon_mode(vk::PolygonMode::FILL)
        .cull_mode(vk::CullModeFlags::NONE)
        .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
        .line_width(1.0);

    let multisample = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);

    let blend_attachments = [
        vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA)
            .blend_enable(true)
            .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
            .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .color_blend_op(vk::BlendOp::ADD)
            /* Preserve the framebuffer alpha channel (cleared to 1.0
             * each frame). The previous shape — src=ONE, dst=ZERO —
             * wrote src.a=coverage straight into the framebuffer for
             * glyph_run, producing transparent regions inside glyph
             * cells. Downstream consumers (PNG readback in the smoke
             * harness, future scanout that may premultiply, alpha-
             * compositing viewers) interpreted that as "this pixel is
             * transparent" and the visible-glyph-shape area dropped
             * out, leaving only the antialiased glyph outline visible.
             * src=ZERO, dst=ONE keeps the cleared 1.0 in the FB alpha
             * regardless of what shaders write to src.a. */
            .src_alpha_blend_factor(vk::BlendFactor::ZERO)
            .dst_alpha_blend_factor(vk::BlendFactor::ONE)
            .alpha_blend_op(vk::BlendOp::ADD),
    ];
    let color_blend = vk::PipelineColorBlendStateCreateInfo::default()
        .attachments(&blend_attachments);

    let info = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&input_assembly)
        .viewport_state(&viewport_state)
        .rasterization_state(&raster)
        .multisample_state(&multisample)
        .color_blend_state(&color_blend)
        .dynamic_state(&dynamic)
        .layout(layout)
        .render_pass(render_pass)
        .subpass(0);

    let pipelines = unsafe {
        device.create_graphics_pipelines(vk::PipelineCache::null(), &[info], None)
    }.map_err(|(_, e)| anyhow::anyhow!("create_graphics_pipelines: {e:?}"))?;
    unsafe {
        device.destroy_shader_module(vert, None);
        device.destroy_shader_module(frag, None);
    }
    Ok(pipelines[0])
}
