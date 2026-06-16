//! Per-op per-frame GPU buffers + descriptors.
//!
//! For every loaded op we allocate, once at startup, the three buffers
//! its compute kernel binds (scene / instance / counter) plus a
//! descriptor pool + set wired to them. `max_instances` from the bundle
//! manifest's `gpu_resources` block sizes the buffers; the renderer
//! drops nodes past that cap and warns rather than reallocating mid-
//! frame.
//!
//! Threading: lives on the main (renderer) thread alongside `Renderer`.
//! Scene buffer is HOST_VISIBLE+HOST_COHERENT and persistently mapped
//! so per-frame writes are a plain memcpy. Counter buffer is also
//! host-visible so step 7 can read back the post-dispatch instance
//! count to confirm the kernel actually ran; step 8+ can move it to
//! device-local once `vkCmdDrawIndexedIndirect{Count}` reads it on the
//! GPU.

use std::collections::HashMap;

use anyhow::{anyhow, Context, Result};
use ash::vk;

use crate::pipeline::OpKind;

/// One scene node for the rect op. Mirrors the node layout read by
/// `bundles/atrium-core/compute/op_rectangle.slang` (48 bytes: 8 pos + 8 size +
/// 16 color + 4 radius + 12 pad, kept 16-aligned for clean ByteAddressBuffer reads).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SceneNode {
    pub position: [f32; 2],
    pub size:     [f32; 2],
    pub color:    [f32; 4],
    /// Corner radius in pixels (0 = square).
    pub radius:   f32,
    pub _pad:     [f32; 3],
}

/// One scene node for the texture op. Mirrors `SceneNode` in
/// `bundles/atrium-core/compute/op_texture.comp` (16 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct TextureNode {
    pub model: [f32; 4],   /* x, y, w, h */
}

/// One scene node for the path op (rotated quad). Mirrors `SceneNode`
/// in `bundles/atrium-core/compute/op_path.comp` (48 bytes, three
/// vec4s for std430 alignment).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PathNode {
    pub model: [f32; 4],   /* cx, cy, length, width */
    pub extra: [f32; 4],   /* angle, _pad, _pad, _pad */
    pub color: [f32; 4],
}

/// One scene node for the glyph_run op. Mirrors `SceneNode` in
/// `bundles/atrium-text/compute/op_glyph_run.comp` (96 bytes: three
/// vec4s + one ivec4 for std430 alignment). One node = one shaped
/// text run; the kernel expands it into N InstanceRecords (one per
/// glyph) by indexing into a separate glyphs storage buffer keyed by
/// `meta[1]` (glyph_offset).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct GlyphRunNode {
    pub origin:    [f32; 4],   /* x, y, _, _ */
    pub atlas_dim: [f32; 4],   /* width, height, _, _ */
    pub color:     [f32; 4],   /* r, g, b, a */
    pub meta:      [i32; 4],   /* glyph_count, glyph_offset, _, _ */
}

/// One glyph within a glyph run. Mirrors `GlyphInstance` in the GPU
/// kernel under std430 layout: `vec2 d_offset`, `vec4 atlas_uv`,
/// `vec2 bearing`. std430 forces 16-byte alignment on vec4, padding
/// the vec2s out to 48 bytes total. Explicit padding fields here
/// make that match exact and keep `bytemuck::cast_slice` happy.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct GlyphInstance {
    pub d_offset: [f32; 2],    /* dx, dy from run origin */
    pub _pad0:    [f32; 2],
    pub atlas_uv: [f32; 4],    /* u, v, w, h in atlas pixel coords */
    pub bearing:  [f32; 2],    /* bearing_x, bearing_y */
    pub _pad1:    [f32; 2],
}

/// Header word that precedes the SceneNode array in the scene buffer.
/// Mirrors the `SceneBuf { uint node_count; uint _pad0[3]; SceneNode
/// nodes[]; }` layout in the compute shader.
#[repr(C)]
struct SceneHeader {
    node_count: u32,
    _pad:       [u32; 3],
}
const SCENE_HEADER_BYTES: vk::DeviceSize =
    std::mem::size_of::<SceneHeader>() as vk::DeviceSize;

fn node_size_for(kind: OpKind) -> vk::DeviceSize {
    match kind {
        OpKind::Rect     => std::mem::size_of::<SceneNode>()    as vk::DeviceSize,
        OpKind::Texture  => std::mem::size_of::<TextureNode>()  as vk::DeviceSize,
        OpKind::Path     => std::mem::size_of::<PathNode>()     as vk::DeviceSize,
        OpKind::GlyphRun => std::mem::size_of::<GlyphRunNode>() as vk::DeviceSize,
    }
}

/// InstanceRecord size matches node size for these ops (rect: vec4
/// model + vec4 color = 32; texture: vec4 model = 16; path: 3×vec4 = 48;
/// glyph_run: 3×vec4 = 48 — dst_rect + src_rect + color).
fn instance_size_for(kind: OpKind) -> vk::DeviceSize {
    match kind {
        OpKind::Rect     => 32,
        OpKind::Texture  => 16,
        OpKind::Path     => 48,
        OpKind::GlyphRun => 48,
    }
}

/// Bytes per `GlyphInstance` in the per-frame glyphs storage buffer.
const GLYPH_INSTANCE_BYTES: vk::DeviceSize =
    std::mem::size_of::<GlyphInstance>() as vk::DeviceSize;

/// Per-atlas-slot dispatch buffers for the GlyphRun op. Each batch (one
/// per atlas page) needs its own scene/glyphs/instance/counter buffers
/// so the host-side per-frame writes don't race the GPU's consumption
/// when multiple batches are dispatched in the same submit. Allocated
/// lazily via `OpFrameResources::ensure_glyph_slot`.
pub struct GlyphSlotBuffers {
    pub scene_buf:    vk::Buffer,
    pub scene_mem:    vk::DeviceMemory,
    scene_ptr:        *mut u8,

    pub glyphs_buf:   vk::Buffer,
    pub glyphs_mem:   vk::DeviceMemory,
    glyphs_ptr:       *mut u8,

    pub instance_buf: vk::Buffer,
    pub instance_mem: vk::DeviceMemory,

    pub counter_buf:  vk::Buffer,
    pub counter_mem:  vk::DeviceMemory,

    pub compute_set:  vk::DescriptorSet,
    pub render_set:   vk::DescriptorSet,

    max_instances:    u32,
}

unsafe impl Send for GlyphSlotBuffers {}

impl GlyphSlotBuffers {
    pub fn write_scene(&self, nodes: &[GlyphRunNode]) -> u32 {
        let n = nodes.len().min(self.max_instances as usize) as u32;
        unsafe {
            let header = self.scene_ptr as *mut SceneHeader;
            (*header).node_count = n;
            (*header)._pad = [0; 3];
            if n > 0 {
                let dst = self.scene_ptr.add(SCENE_HEADER_BYTES as usize);
                std::ptr::copy_nonoverlapping(
                    nodes.as_ptr() as *const u8, dst,
                    n as usize * std::mem::size_of::<GlyphRunNode>());
            }
        }
        n
    }

    pub fn write_glyphs(&self, instances: &[GlyphInstance]) -> u32 {
        let n = instances.len().min(self.max_instances as usize) as u32;
        if n == 0 { return n; }
        unsafe {
            std::ptr::copy_nonoverlapping(
                instances.as_ptr() as *const u8, self.glyphs_ptr,
                n as usize * std::mem::size_of::<GlyphInstance>());
        }
        n
    }

    unsafe fn destroy(&self, device: &ash::Device) {
        device.unmap_memory(self.scene_mem);
        device.destroy_buffer(self.scene_buf, None);
        device.free_memory(self.scene_mem, None);
        device.unmap_memory(self.glyphs_mem);
        device.destroy_buffer(self.glyphs_buf, None);
        device.free_memory(self.glyphs_mem, None);
        device.destroy_buffer(self.instance_buf, None);
        device.free_memory(self.instance_mem, None);
        device.destroy_buffer(self.counter_buf, None);
        device.free_memory(self.counter_mem, None);
        /* Compute set lives in glyph_slot_pool, render set in
         * render_pool; both are reset/destroyed when the parent
         * OpFrameResources goes away. */
    }
}

pub struct OpFrameResources {
    pub kind:          OpKind,
    pub max_instances: u32,
    pub node_bytes:    vk::DeviceSize,

    pub scene_buf:    vk::Buffer,
    pub scene_mem:    vk::DeviceMemory,
    /// Persistently mapped pointer into scene_mem. Valid for the
    /// lifetime of this struct (unmapped in destroy()). HOST_COHERENT,
    /// so writes are visible without explicit flush.
    scene_ptr:        *mut u8,

    pub instance_buf: vk::Buffer,
    pub instance_mem: vk::DeviceMemory,

    pub counter_buf:  vk::Buffer,
    pub counter_mem:  vk::DeviceMemory,
    counter_ptr:      *mut u8,

    pub descriptor_pool: vk::DescriptorPool,
    pub descriptor_set:  vk::DescriptorSet,

    /// Compute set layout — kept around for GlyphRun's per-slot
    /// descriptor allocation. Owned by `OpPipelines`; we only hold
    /// the handle for re-use.
    pub compute_set_layout: vk::DescriptorSetLayout,

    /// Cached memory properties — used by `ensure_glyph_slot` to
    /// allocate per-slot buffers without re-querying the physical
    /// device.
    mem_props: vk::PhysicalDeviceMemoryProperties,

    /// Per-frame uniform consumed by the render pipeline's vertex
    /// shader (`Screen { vec2 size; }`). Host-visible; rewritten at
    /// the start of each frame (and after resize).
    pub screen_buf:      vk::Buffer,
    pub screen_mem:      vk::DeviceMemory,
    screen_ptr:          *mut u8,

    /// Render-side descriptor pool. For Rect, holds one set
    /// (`render_set`). For Texture, sized for many sets — one per
    /// bound slot — which `texture_descriptor_for(slot)` allocates
    /// lazily into `slot_descriptors`.
    pub render_pool:     vk::DescriptorPool,
    /// Single-set rect render binding. `None` for Texture.
    pub render_set:      Option<vk::DescriptorSet>,

    /// Texture-only: sampler shared across slots; per-slot render
    /// descriptor sets keyed by slot_id; render layout cached so
    /// new sets can be allocated on demand.
    pub sampler:           Option<vk::Sampler>,
    pub render_set_layout: Option<vk::DescriptorSetLayout>,
    pub slot_descriptors:  HashMap<u32, vk::DescriptorSet>,

    /// GlyphRun-only: legacy "shared" glyphs storage buffer. Kept
    /// allocated for backward-compat with the older single-batch
    /// rendering path; the per-batch rendering path uses
    /// `glyph_slots` instead and ignores this. None for non-GlyphRun
    /// ops.
    pub glyphs_buf: Option<vk::Buffer>,
    pub glyphs_mem: Option<vk::DeviceMemory>,
    #[allow(dead_code)]
    glyphs_ptr:     *mut u8,

    /// GlyphRun-only: pool for per-atlas-slot compute descriptor sets.
    /// Sized for `SLOT_CAP` sets, each with 4 storage buffers (the
    /// glyph_run kernel's binding count). None for non-GlyphRun ops.
    pub glyph_slot_pool: Option<vk::DescriptorPool>,

    /// GlyphRun-only: per-atlas-slot dedicated buffers + descriptor
    /// sets. Allocated lazily by `ensure_glyph_slot`. The renderer
    /// iterates this map to issue one compute+draw per batch within
    /// a single render-pass instance — host writes to slot A's scene
    /// buffer don't race slot B's GPU consumption because they live
    /// in independent buffers.
    pub glyph_slots: HashMap<u32, GlyphSlotBuffers>,
}

/* SAFETY: the raw pointers point into Vulkan-owned memory tied to this
 * struct's lifetime (mapped in create, unmapped in destroy). They are
 * only dereferenced from the renderer thread. */
unsafe impl Send for OpFrameResources {}

impl OpFrameResources {
    pub fn create(
        device:            &ash::Device,
        instance:          &ash::Instance,
        physical_device:   vk::PhysicalDevice,
        kind:              OpKind,
        set_layout:        vk::DescriptorSetLayout,
        render_set_layout: vk::DescriptorSetLayout,
        max_instances:     u32,
    ) -> Result<Self> {
        let mem_props = unsafe {
            instance.get_physical_device_memory_properties(physical_device)
        };

        let node_bytes     = node_size_for(kind);
        let instance_bytes = instance_size_for(kind);
        let scene_size: vk::DeviceSize =
            SCENE_HEADER_BYTES + node_bytes * max_instances as vk::DeviceSize;
        let instance_size: vk::DeviceSize =
            instance_bytes * max_instances as vk::DeviceSize;
        let counter_size: vk::DeviceSize = 4;

        let (scene_buf, scene_mem) = create_buffer(
            device, &mem_props, scene_size,
            vk::BufferUsageFlags::STORAGE_BUFFER,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;
        let scene_ptr = unsafe {
            device.map_memory(scene_mem, 0, scene_size, vk::MemoryMapFlags::empty())?
        } as *mut u8;

        let (instance_buf, instance_mem) = create_buffer(
            device, &mem_props, instance_size,
            vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::VERTEX_BUFFER,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )?;

        /* counter is host-visible for step-7 readback verification; can
         * move to device-local at step 8 once the draw call reads it
         * from GPU via indirect. TRANSFER_DST so cmd_fill_buffer can
         * zero it each frame. */
        let (counter_buf, counter_mem) = create_buffer(
            device, &mem_props, counter_size,
            vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;
        let counter_ptr = unsafe {
            device.map_memory(counter_mem, 0, counter_size, vk::MemoryMapFlags::empty())?
        } as *mut u8;

        /* GlyphRun's compute set has 4 storage buffers (scene,
         * glyphs, instance, counter); other ops have 3 (scene,
         * instance, counter). Pool size matches what we'll write. */
        let n_storage = if kind == OpKind::GlyphRun { 4 } else { 3 };
        let pool_sizes = [vk::DescriptorPoolSize {
            ty: vk::DescriptorType::STORAGE_BUFFER,
            descriptor_count: n_storage,
        }];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(1)
            .pool_sizes(&pool_sizes);
        let descriptor_pool = unsafe {
            device.create_descriptor_pool(&pool_info, None)
        }?;

        let layouts = [set_layout];
        let alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(descriptor_pool)
            .set_layouts(&layouts);
        let descriptor_set = unsafe { device.allocate_descriptor_sets(&alloc) }?[0];

        /* GlyphRun-only: allocate the per-frame glyphs storage
         * buffer. Sized to hold up to max_instances GlyphInstances
         * — i.e., one frame can carry max_instances glyphs total
         * across all runs. Host-visible so per-frame writes are a
         * memcpy. */
        let (glyphs_buf, glyphs_mem, glyphs_ptr) =
            if kind == OpKind::GlyphRun {
                let glyphs_size = GLYPH_INSTANCE_BYTES
                    * max_instances as vk::DeviceSize;
                let (buf, mem) = create_buffer(
                    device, &mem_props, glyphs_size,
                    vk::BufferUsageFlags::STORAGE_BUFFER,
                    vk::MemoryPropertyFlags::HOST_VISIBLE
                        | vk::MemoryPropertyFlags::HOST_COHERENT,
                )?;
                let ptr = unsafe {
                    device.map_memory(mem, 0, glyphs_size,
                        vk::MemoryMapFlags::empty())?
                } as *mut u8;
                (Some(buf), Some(mem), ptr)
            } else {
                (None, None, std::ptr::null_mut())
            };

        let scene_info = [vk::DescriptorBufferInfo {
            buffer: scene_buf, offset: 0, range: vk::WHOLE_SIZE,
        }];
        let inst_info = [vk::DescriptorBufferInfo {
            buffer: instance_buf, offset: 0, range: vk::WHOLE_SIZE,
        }];
        let counter_info = [vk::DescriptorBufferInfo {
            buffer: counter_buf, offset: 0, range: vk::WHOLE_SIZE,
        }];

        /* Compute descriptor-set bindings depend on op:
         *   Rect/Texture/Path:   binding 0=scene, 1=instance, 2=counter
         *   GlyphRun:            binding 0=scene, 1=glyphs,
         *                        binding 2=instance, 3=counter
         * Layout was derived from SPIR-V reflection so bindings match
         * the kernel's declarations. */
        if kind == OpKind::GlyphRun {
            let glyphs_info = [vk::DescriptorBufferInfo {
                buffer: glyphs_buf.unwrap(), offset: 0, range: vk::WHOLE_SIZE,
            }];
            let writes = [
                vk::WriteDescriptorSet::default()
                    .dst_set(descriptor_set).dst_binding(0)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(&scene_info),
                vk::WriteDescriptorSet::default()
                    .dst_set(descriptor_set).dst_binding(1)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(&glyphs_info),
                vk::WriteDescriptorSet::default()
                    .dst_set(descriptor_set).dst_binding(2)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(&inst_info),
                vk::WriteDescriptorSet::default()
                    .dst_set(descriptor_set).dst_binding(3)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(&counter_info),
            ];
            unsafe { device.update_descriptor_sets(&writes, &[]); }
        } else {
            let writes = [
                vk::WriteDescriptorSet::default()
                    .dst_set(descriptor_set).dst_binding(0)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(&scene_info),
                vk::WriteDescriptorSet::default()
                    .dst_set(descriptor_set).dst_binding(1)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(&inst_info),
                vk::WriteDescriptorSet::default()
                    .dst_set(descriptor_set).dst_binding(2)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(&counter_info),
            ];
            unsafe { device.update_descriptor_sets(&writes, &[]); }
        }

        /* ── Render-side resources: uniform + descriptor set(s). ── */
        let (screen_buf, screen_mem) = create_buffer(
            device, &mem_props, 16,  /* vec2 + std140 padding */
            vk::BufferUsageFlags::UNIFORM_BUFFER,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;
        let screen_ptr = unsafe {
            device.map_memory(screen_mem, 0, 16, vk::MemoryMapFlags::empty())?
        } as *mut u8;

        /* Render-pool sizing depends on kind. Texture allocates one
         * descriptor set per bound slot; reserve room for SLOT_CAP
         * concurrent slots (POC: 64 — far above expected). Rect just
         * needs one. */
        const SLOT_CAP: u32 = 64;
        let (render_pool, render_set, sampler, render_set_layout_keep) = match kind {
            /* Rect and Path share the same render-set shape: one
             * storage buffer (instances) + one uniform (screen). The
             * only difference is the per-record byte size, which is
             * sized via node_bytes / instance_size_for above. */
            OpKind::Rect | OpKind::Path => {
                let sizes = [
                    vk::DescriptorPoolSize {
                        ty: vk::DescriptorType::STORAGE_BUFFER, descriptor_count: 1,
                    },
                    vk::DescriptorPoolSize {
                        ty: vk::DescriptorType::UNIFORM_BUFFER, descriptor_count: 1,
                    },
                ];
                let info = vk::DescriptorPoolCreateInfo::default()
                    .max_sets(1).pool_sizes(&sizes);
                let pool = unsafe { device.create_descriptor_pool(&info, None) }?;
                let layouts = [render_set_layout];
                let alloc = vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(pool).set_layouts(&layouts);
                let set = unsafe { device.allocate_descriptor_sets(&alloc) }?[0];

                let r_inst = [vk::DescriptorBufferInfo {
                    buffer: instance_buf, offset: 0, range: vk::WHOLE_SIZE,
                }];
                let r_screen = [vk::DescriptorBufferInfo {
                    buffer: screen_buf, offset: 0, range: vk::WHOLE_SIZE,
                }];
                let writes = [
                    vk::WriteDescriptorSet::default()
                        .dst_set(set).dst_binding(0)
                        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                        .buffer_info(&r_inst),
                    vk::WriteDescriptorSet::default()
                        .dst_set(set).dst_binding(1)
                        .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                        .buffer_info(&r_screen),
                ];
                unsafe { device.update_descriptor_sets(&writes, &[]); }
                (pool, Some(set), None, None)
            }
            /* Texture and GlyphRun share the same render-set shape:
             * one storage buffer (instances) + one uniform (screen) +
             * one combined-image-sampler (the texture / atlas).
             * Per-slot descriptor sets are allocated lazily via
             * `ensure_texture_descriptor`. */
            OpKind::Texture | OpKind::GlyphRun => {
                let sizes = [
                    vk::DescriptorPoolSize {
                        ty: vk::DescriptorType::STORAGE_BUFFER,
                        descriptor_count: SLOT_CAP,
                    },
                    vk::DescriptorPoolSize {
                        ty: vk::DescriptorType::UNIFORM_BUFFER,
                        descriptor_count: SLOT_CAP,
                    },
                    vk::DescriptorPoolSize {
                        ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                        descriptor_count: SLOT_CAP,
                    },
                ];
                let info = vk::DescriptorPoolCreateInfo::default()
                    .max_sets(SLOT_CAP).pool_sizes(&sizes);
                let pool = unsafe { device.create_descriptor_pool(&info, None) }?;

                /* One sampler shared across slots: linear filter, clamp
                 * to edge. */
                let sampler_info = vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::LINEAR)
                    .min_filter(vk::Filter::LINEAR)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .min_lod(0.0).max_lod(0.0);
                let sampler = unsafe { device.create_sampler(&sampler_info, None) }?;
                (pool, None, Some(sampler), Some(render_set_layout))
            }
        };

        /* GlyphRun-only: dedicated descriptor pool for per-atlas-slot
         * compute sets (each set binds 4 storage buffers; we want
         * SLOT_CAP independent sets so up to SLOT_CAP atlases can be
         * rendered concurrently in one frame). */
        let glyph_slot_pool = if kind == OpKind::GlyphRun {
            let sizes = [vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_BUFFER,
                descriptor_count: SLOT_CAP * 4,
            }];
            let info = vk::DescriptorPoolCreateInfo::default()
                .max_sets(SLOT_CAP).pool_sizes(&sizes);
            Some(unsafe { device.create_descriptor_pool(&info, None) }?)
        } else {
            None
        };

        Ok(Self {
            kind, max_instances, node_bytes,
            scene_buf, scene_mem, scene_ptr,
            instance_buf, instance_mem,
            counter_buf, counter_mem, counter_ptr,
            descriptor_pool, descriptor_set,
            compute_set_layout: set_layout,
            mem_props,
            screen_buf, screen_mem, screen_ptr,
            render_pool, render_set,
            sampler, render_set_layout: render_set_layout_keep,
            slot_descriptors: HashMap::new(),
            glyphs_buf, glyphs_mem, glyphs_ptr,
            glyph_slot_pool,
            glyph_slots: HashMap::new(),
        })
    }

    /// Texture-only: get (allocating if needed) the render descriptor
    /// set bound to `slot_id`'s ImageView. Caller must ensure the
    /// ImageView remains valid until the GPU stops using the set.
    pub fn ensure_texture_descriptor(
        &mut self,
        device:    &ash::Device,
        slot_id:   u32,
        view:      vk::ImageView,
    ) -> Result<vk::DescriptorSet> {
        if let Some(set) = self.slot_descriptors.get(&slot_id) {
            return Ok(*set);
        }
        let layout = self.render_set_layout
            .ok_or_else(|| anyhow!("ensure_texture_descriptor on non-Texture op"))?;
        let sampler = self.sampler
            .ok_or_else(|| anyhow!("Texture op missing sampler"))?;

        let layouts = [layout];
        let alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.render_pool).set_layouts(&layouts);
        let set = unsafe { device.allocate_descriptor_sets(&alloc) }?[0];

        let r_inst = [vk::DescriptorBufferInfo {
            buffer: self.instance_buf, offset: 0, range: vk::WHOLE_SIZE,
        }];
        let r_screen = [vk::DescriptorBufferInfo {
            buffer: self.screen_buf, offset: 0, range: vk::WHOLE_SIZE,
        }];
        let r_img = [vk::DescriptorImageInfo {
            sampler,
            image_view: view,
            image_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        }];
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(set).dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&r_inst),
            vk::WriteDescriptorSet::default()
                .dst_set(set).dst_binding(1)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                .buffer_info(&r_screen),
            vk::WriteDescriptorSet::default()
                .dst_set(set).dst_binding(2)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&r_img),
        ];
        unsafe { device.update_descriptor_sets(&writes, &[]); }
        self.slot_descriptors.insert(slot_id, set);
        Ok(set)
    }

    /// Texture-only: drop a slot's cached descriptor set on SLOT_CLEAR
    /// or texture replacement. Pool memory isn't reclaimed (we'd need
    /// FREE_DESCRIPTOR_SET pool flag); fine for the POC's slot churn.
    /// Also drops the per-glyph-slot dedicated buffers if present
    /// (for the GlyphRun op), since they reference an ImageView that
    /// is about to become invalid.
    pub fn drop_texture_slot(&mut self, slot_id: u32) {
        self.slot_descriptors.remove(&slot_id);
        /* GlyphRun's per-slot buffers live in `glyph_slots`; we just
         * remove the entry. The Vulkan handles inside leak until the
         * parent OpFrameResources is destroyed — fine for vestibulum's
         * static atlas set. Plumbing &device + an idle guarantee for
         * dynamic SLOT_CLEAR churn is a separate hook. */
        let _ = self.glyph_slots.remove(&slot_id);
    }

    /// GlyphRun-only: get (allocating if needed) the per-atlas-slot
    /// dedicated buffer set + descriptor sets for `slot_id`. The
    /// `atlas_view` is the ImageView for the slot's atlas texture
    /// (same lifetime contract as `ensure_texture_descriptor`).
    ///
    /// Each slot owns its own scene/glyphs/instance/counter buffers
    /// so the renderer can emit one compute+draw per batch in a
    /// single render-pass instance without host-write/GPU-read races.
    pub fn ensure_glyph_slot(
        &mut self,
        device:     &ash::Device,
        slot_id:    u32,
        atlas_view: vk::ImageView,
    ) -> Result<&GlyphSlotBuffers> {
        if self.glyph_slots.contains_key(&slot_id) {
            return Ok(self.glyph_slots.get(&slot_id).unwrap());
        }
        if self.kind != OpKind::GlyphRun {
            return Err(anyhow!("ensure_glyph_slot called on non-GlyphRun op"));
        }
        let pool = self.glyph_slot_pool
            .ok_or_else(|| anyhow!("GlyphRun op missing glyph_slot_pool"))?;
        let render_set_layout = self.render_set_layout
            .ok_or_else(|| anyhow!("GlyphRun op missing render_set_layout"))?;
        let sampler = self.sampler
            .ok_or_else(|| anyhow!("GlyphRun op missing sampler"))?;

        let max_instances = self.max_instances;
        let scene_size: vk::DeviceSize = SCENE_HEADER_BYTES
            + std::mem::size_of::<GlyphRunNode>() as vk::DeviceSize
              * max_instances as vk::DeviceSize;
        let glyphs_size: vk::DeviceSize = GLYPH_INSTANCE_BYTES
            * max_instances as vk::DeviceSize;
        let instance_size: vk::DeviceSize = 48
            * max_instances as vk::DeviceSize;
        let counter_size: vk::DeviceSize = 4;

        let (scene_buf, scene_mem) = create_buffer(
            device, &self.mem_props, scene_size,
            vk::BufferUsageFlags::STORAGE_BUFFER,
            vk::MemoryPropertyFlags::HOST_VISIBLE
                | vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;
        let scene_ptr = unsafe {
            device.map_memory(scene_mem, 0, scene_size,
                vk::MemoryMapFlags::empty())?
        } as *mut u8;

        let (glyphs_buf, glyphs_mem) = create_buffer(
            device, &self.mem_props, glyphs_size,
            vk::BufferUsageFlags::STORAGE_BUFFER,
            vk::MemoryPropertyFlags::HOST_VISIBLE
                | vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;
        let glyphs_ptr = unsafe {
            device.map_memory(glyphs_mem, 0, glyphs_size,
                vk::MemoryMapFlags::empty())?
        } as *mut u8;

        let (instance_buf, instance_mem) = create_buffer(
            device, &self.mem_props, instance_size,
            vk::BufferUsageFlags::STORAGE_BUFFER
                | vk::BufferUsageFlags::VERTEX_BUFFER,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )?;

        /* Counter is device-local — the kernel writes it via atomics
         * and the indirect draw reads it (when wired up); no host-side
         * readback needed for per-slot dispatches. */
        let (counter_buf, counter_mem) = create_buffer(
            device, &self.mem_props, counter_size,
            vk::BufferUsageFlags::STORAGE_BUFFER
                | vk::BufferUsageFlags::TRANSFER_DST,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )?;

        /* Compute descriptor set: 4 storage bindings
         * (0=scene, 1=glyphs, 2=instance, 3=counter). Allocated from
         * the per-slot pool; layout matches the kernel's reflection. */
        let layouts = [self.compute_set_layout];
        let alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(pool).set_layouts(&layouts);
        let compute_set = unsafe { device.allocate_descriptor_sets(&alloc) }?[0];

        let scene_info   = [vk::DescriptorBufferInfo {
            buffer: scene_buf,    offset: 0, range: vk::WHOLE_SIZE }];
        let glyphs_info  = [vk::DescriptorBufferInfo {
            buffer: glyphs_buf,   offset: 0, range: vk::WHOLE_SIZE }];
        let inst_info    = [vk::DescriptorBufferInfo {
            buffer: instance_buf, offset: 0, range: vk::WHOLE_SIZE }];
        let counter_info = [vk::DescriptorBufferInfo {
            buffer: counter_buf,  offset: 0, range: vk::WHOLE_SIZE }];
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(compute_set).dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&scene_info),
            vk::WriteDescriptorSet::default()
                .dst_set(compute_set).dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&glyphs_info),
            vk::WriteDescriptorSet::default()
                .dst_set(compute_set).dst_binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&inst_info),
            vk::WriteDescriptorSet::default()
                .dst_set(compute_set).dst_binding(3)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&counter_info),
        ];
        unsafe { device.update_descriptor_sets(&writes, &[]); }

        /* Render descriptor set: 1 storage (per-slot instance_buf) +
         * 1 uniform (shared screen) + 1 image-sampler (the slot's
         * atlas). Allocated from render_pool, same layout as the
         * texture-style render set. */
        let r_layouts = [render_set_layout];
        let r_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.render_pool).set_layouts(&r_layouts);
        let render_set = unsafe { device.allocate_descriptor_sets(&r_alloc) }?[0];

        let r_inst = [vk::DescriptorBufferInfo {
            buffer: instance_buf, offset: 0, range: vk::WHOLE_SIZE }];
        let r_screen = [vk::DescriptorBufferInfo {
            buffer: self.screen_buf, offset: 0, range: vk::WHOLE_SIZE }];
        let r_img = [vk::DescriptorImageInfo {
            sampler,
            image_view: atlas_view,
            image_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        }];
        let r_writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(render_set).dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&r_inst),
            vk::WriteDescriptorSet::default()
                .dst_set(render_set).dst_binding(1)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                .buffer_info(&r_screen),
            vk::WriteDescriptorSet::default()
                .dst_set(render_set).dst_binding(2)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&r_img),
        ];
        unsafe { device.update_descriptor_sets(&r_writes, &[]); }

        let slot = GlyphSlotBuffers {
            scene_buf, scene_mem, scene_ptr,
            glyphs_buf, glyphs_mem, glyphs_ptr,
            instance_buf, instance_mem,
            counter_buf, counter_mem,
            compute_set, render_set,
            max_instances,
        };
        self.glyph_slots.insert(slot_id, slot);
        Ok(self.glyph_slots.get(&slot_id).unwrap())
    }

    /// Update the screen-size uniform. Call on resize and at startup.
    pub fn write_screen(&self, width: u32, height: u32) {
        unsafe {
            let p = self.screen_ptr as *mut [f32; 2];
            *p = [width as f32, height as f32];
        }
    }

    /// Write rect-shaped scene nodes into the scene buffer (capped at
    /// `max_instances`). Returns the count actually written.
    pub fn write_scene(&self, nodes: &[SceneNode]) -> u32 {
        debug_assert_eq!(self.kind, OpKind::Rect);
        self.write_raw(nodes.as_ptr() as *const u8,
                       nodes.len(),
                       std::mem::size_of::<SceneNode>())
    }

    /// Write texture-shaped scene nodes into the scene buffer.
    pub fn write_texture_scene(&self, nodes: &[TextureNode]) -> u32 {
        debug_assert_eq!(self.kind, OpKind::Texture);
        self.write_raw(nodes.as_ptr() as *const u8,
                       nodes.len(),
                       std::mem::size_of::<TextureNode>())
    }

    /// Write path-op (rotated quad) scene nodes into the scene buffer.
    pub fn write_path_scene(&self, nodes: &[PathNode]) -> u32 {
        debug_assert_eq!(self.kind, OpKind::Path);
        self.write_raw(nodes.as_ptr() as *const u8,
                       nodes.len(),
                       std::mem::size_of::<PathNode>())
    }

    fn write_raw(&self, src: *const u8, count: usize, item_bytes: usize) -> u32 {
        debug_assert_eq!(item_bytes as vk::DeviceSize, self.node_bytes);
        let n = count.min(self.max_instances as usize) as u32;
        unsafe {
            let header = self.scene_ptr as *mut SceneHeader;
            (*header).node_count = n;
            (*header)._pad = [0; 3];
            if n > 0 {
                let dst = self.scene_ptr.add(SCENE_HEADER_BYTES as usize);
                std::ptr::copy_nonoverlapping(src, dst, n as usize * item_bytes);
            }
        }
        n
    }

    /// Read the post-dispatch atomic counter. Caller must ensure a
    /// memory barrier from COMPUTE_SHADER → HOST has already executed
    /// (in practice: queue/fence wait completed since the dispatch).
    pub fn read_counter(&self) -> u32 {
        unsafe { *(self.counter_ptr as *const u32) }
    }

    pub unsafe fn destroy(&self, device: &ash::Device) {
        for (_, slot) in self.glyph_slots.iter() {
            slot.destroy(device);
        }
        if let Some(pool) = self.glyph_slot_pool {
            device.destroy_descriptor_pool(pool, None);
        }
        if let (Some(buf), Some(mem)) = (self.glyphs_buf, self.glyphs_mem) {
            device.unmap_memory(mem);
            device.destroy_buffer(buf, None);
            device.free_memory(mem, None);
        }
        if let Some(s) = self.sampler {
            device.destroy_sampler(s, None);
        }
        device.destroy_descriptor_pool(self.render_pool, None);
        device.unmap_memory(self.screen_mem);
        device.destroy_buffer(self.screen_buf, None);
        device.free_memory(self.screen_mem, None);
        device.destroy_descriptor_pool(self.descriptor_pool, None);
        device.unmap_memory(self.counter_mem);
        device.destroy_buffer(self.counter_buf, None);
        device.free_memory(self.counter_mem, None);
        device.destroy_buffer(self.instance_buf, None);
        device.free_memory(self.instance_mem, None);
        device.unmap_memory(self.scene_mem);
        device.destroy_buffer(self.scene_buf, None);
        device.free_memory(self.scene_mem, None);
    }
}

// ── helpers (mirror those in resource.rs; keeping them local avoids a
//    cross-module Vulkan-helper dep at the cost of a little duplication
//    until the POC settles) ──────────────────────────────────────────

fn create_buffer(
    device:    &ash::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    size:      vk::DeviceSize,
    usage:     vk::BufferUsageFlags,
    props:     vk::MemoryPropertyFlags,
) -> Result<(vk::Buffer, vk::DeviceMemory)> {
    let info = vk::BufferCreateInfo::default()
        .size(size).usage(usage)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let buf = unsafe { device.create_buffer(&info, None) }
        .context("create_buffer")?;
    let req = unsafe { device.get_buffer_memory_requirements(buf) };
    let mem_type = find_memory_type(mem_props, req.memory_type_bits, props)?;
    let alloc = vk::MemoryAllocateInfo::default()
        .allocation_size(req.size).memory_type_index(mem_type);
    let mem = unsafe { device.allocate_memory(&alloc, None) }?;
    unsafe { device.bind_buffer_memory(buf, mem, 0) }?;
    Ok((buf, mem))
}

fn find_memory_type(
    props: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
    needed:  vk::MemoryPropertyFlags,
) -> Result<u32> {
    for i in 0..props.memory_type_count {
        let bit = 1u32 << i;
        if (type_bits & bit) != 0
           && props.memory_types[i as usize].property_flags.contains(needed)
        {
            return Ok(i);
        }
    }
    Err(anyhow!("no memory type matches bits={type_bits:#x} props={needed:?}"))
}
