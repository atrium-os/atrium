//! `examples/loader_graphics_mipmap` — explicit-LOD mip
//! sampling.  Uploads a 2-level mip chain:
//!   level 0 (2×2): all red
//!   level 1 (1×1): all blue
//! FS samples with explicit LOD = 1.0 (`SampleExplicitLod`),
//! so the helper must fetch from level 1 -> the framebuffer
//! gets painted blue.  With explicit LOD = 0.0 the existing
//! texture rung shape would have rendered red.
//!
//! Discriminator: pixel(4, 4) = blue (0, 0, 255, 255).
//! Pre-fix, the daemon dropped `mipLevel > 0` regions in
//! CopyBufToImg on the floor (only `pixels` existed, no
//! `mip_levels` storage) and `TexDesc.mip_descs` was always
//! null, so the runtime helper's `pick_tex_mip(lod=1)`
//! quietly fell back to level 0 (red).
//!
//!   * vkCreateSampler reaches the daemon and lands as a
//!     `SamplerDesc`.
//!   * vkCreateImage with SAMPLED usage gets an
//!     `ImageStorage` slot allocated.
//!   * vkCmdCopyBufferToImage walks staging-buffer bytes
//!     into image pixels (this is the daemon's first
//!     loader-mediated coverage of that wire op).
//!   * vkCmdBindDescriptorSets with COMBINED_IMAGE_SAMPLER
//!     gets stashed as `state.bound_textures`.
//!   * `dispatch_draw` populates the uniforms buffer with
//!     helper fn ptrs + TexDesc/SamplerDesc per binding.
//!   * Cranelift's `OpImageSampleImplicitLod` lowering loads
//!     the fn ptr + descriptor pointers at the right
//!     UNIFORMS_DESC_BASE offsets + calls
//!     `atrium_tex_sample_2d`.
//!   * The runtime helper samples the 2×2 texture and the
//!     rasterizer's barycentric UV interpolation reaches all
//!     four texels.
//!
//! Texture layout (2x2 RGBA8):
//!   (0,0) red      (1,0) green
//!   (0,1) blue     (1,1) white
//!
//! Triangle UVs:
//!   V0 at (-0.5, -0.5) -> UV (0, 0) -> red
//!   V1 at ( 0.5, -0.5) -> UV (1, 0) -> green
//!   V2 at ( 0.0,  0.5) -> UV (0.5, 1) -> blue/white blend
//!
//! pixel(4,4) sits roughly at the triangle centroid where
//! all four texture corners contribute via barycentric +
//! sampler interpolation; asserts all of R, G, B > 0
//! AND alpha == 255.

use ash::vk;
use rspirv::binary::Assemble;
use rspirv::spirv::{
    AddressingModel, BuiltIn, Capability, Decoration, Dim, ExecutionMode,
    ExecutionModel, FunctionControl, ImageFormat, MemoryModel, StorageClass,
};

/// VS: in_pos vec3 @ Loc 0, in_uv vec2 @ Loc 1; outputs
/// gl_Position + Loc 0 vec2 varying.
fn build_pos_uv_vs() -> Vec<u8> {
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let i32_ty = b.type_int(32, 1);
    let vec2 = b.type_vector(f32_ty, 2);
    let vec3 = b.type_vector(f32_ty, 3);
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let per_vertex = b.type_struct(vec![vec4]);
    b.member_decorate(per_vertex, 0, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::Position)]);
    b.member_decorate(per_vertex, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(per_vertex, Decoration::Block, vec![]);
    let ptr_pv = b.type_pointer(None, StorageClass::Output, per_vertex);
    let ptr_out_vec4 = b.type_pointer(None, StorageClass::Output, vec4);
    let ptr_out_vec2 = b.type_pointer(None, StorageClass::Output, vec2);
    let ptr_in_vec3  = b.type_pointer(None, StorageClass::Input, vec3);
    let ptr_in_vec2  = b.type_pointer(None, StorageClass::Input, vec2);

    let in_pos = b.variable(ptr_in_vec3, None, StorageClass::Input, None);
    b.decorate(in_pos, Decoration::Location,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let in_uv = b.variable(ptr_in_vec2, None, StorageClass::Input, None);
    b.decorate(in_uv, Decoration::Location,
        vec![rspirv::dr::Operand::LiteralBit32(1)]);
    let pv_var = b.variable(ptr_pv, None, StorageClass::Output, None);
    let out_uv = b.variable(ptr_out_vec2, None, StorageClass::Output, None);
    b.decorate(out_uv, Decoration::Location,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let c_zero  = b.constant_bit32(i32_ty, 0u32);
    let c_one_f = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let pos = b.load(vec3, None, in_pos, None, vec![]).unwrap();
    let uv  = b.load(vec2, None, in_uv,  None, vec![]).unwrap();
    let x = b.composite_extract(f32_ty, None, pos, vec![0]).unwrap();
    let y = b.composite_extract(f32_ty, None, pos, vec![1]).unwrap();
    let z = b.composite_extract(f32_ty, None, pos, vec![2]).unwrap();
    let pos4 = b.composite_construct(vec4, None, vec![x, y, z, c_one_f]).unwrap();
    let dst = b.access_chain(ptr_out_vec4, None, pv_var, vec![c_zero]).unwrap();
    b.store(dst, pos4, None, vec![]).unwrap();
    b.store(out_uv, uv, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Vertex, main, "main",
        vec![in_pos, in_uv, pv_var, out_uv]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

/// FS: samples `texture(sampler2D s, vec2 uv)` via
/// `OpImageSampleImplicitLod` at the in_uv Loc 0 varying.
fn build_textured_fs() -> Vec<u8> {
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let vec2 = b.type_vector(f32_ty, 2);
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);

    let ptr_in_vec2  = b.type_pointer(None, StorageClass::Input, vec2);
    let ptr_out_vec4 = b.type_pointer(None, StorageClass::Output, vec4);
    let in_uv = b.variable(ptr_in_vec2, None, StorageClass::Input, None);
    b.decorate(in_uv, Decoration::Location,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let out = b.variable(ptr_out_vec4, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);

    // sampler2D = combined image+sampler at descriptor set 0,
    // binding 0.
    let img_ty = b.type_image(f32_ty, Dim::Dim2D, 0, 0, 0, 1, ImageFormat::Unknown, None);
    let sampled_ty = b.type_sampled_image(img_ty);
    let ptr_sampled = b.type_pointer(None, StorageClass::UniformConstant, sampled_ty);
    let sampler_var = b.variable(ptr_sampled, None, StorageClass::UniformConstant, None);
    b.decorate(sampler_var, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(sampler_var, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);

    // Constant LOD = 1.0 for the explicit-LOD sample call.
    let c_lod_1 = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let uv = b.load(vec2, None, in_uv, None, vec![]).unwrap();
    let sampler_val = b.load(sampled_ty, None, sampler_var, None, vec![]).unwrap();
    // `OpImageSampleExplicitLod`: the runtime helper picks
    // the right mip via `TexDesc.mip_descs[lod]`.
    let color = b.image_sample_explicit_lod(
        vec4, None, sampler_val, uv,
        rspirv::spirv::ImageOperands::LOD, vec![rspirv::dr::Operand::IdRef(c_lod_1)]
    ).unwrap();
    b.store(out, color, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    // SPIR-V 1.0/1.3 rule: entry-point interface lists only
    // Input + Output OpVariables.  UniformConstant variables
    // (sampler_var) are NOT in the interface even though they
    // ARE referenced.  spirv-opt rejects 1.0/1.3 shaders that
    // include UniformConstants here.
    b.entry_point(ExecutionModel::Fragment, main, "main",
        vec![in_uv, out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

unsafe fn find_memory_type(
    instance: &ash::Instance,
    physical: vk::PhysicalDevice,
    type_filter: u32,
    want: vk::MemoryPropertyFlags,
) -> u32 {
    let mp = instance.get_physical_device_memory_properties(physical);
    for i in 0..mp.memory_type_count {
        if (type_filter & (1u32 << i)) != 0
            && mp.memory_types[i as usize].property_flags.contains(want)
        { return i; }
    }
    panic!("no compatible memory type for filter={type_filter:#b} props={want:?}");
}

const W: u32 = 8;
const H: u32 = 8;
const TEX_W: u32 = 2;
const TEX_H: u32 = 2;

fn main() -> std::process::ExitCode {
    let entry = unsafe { ash::Entry::load().expect("entry") };
    let app_info = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3);
    let flags = vk::InstanceCreateFlags::ENUMERATE_PORTABILITY_KHR;
    let exts  = [vk::KHR_PORTABILITY_ENUMERATION_NAME.as_ptr()];
    let ic_info = vk::InstanceCreateInfo::default()
        .application_info(&app_info).flags(flags).enabled_extension_names(&exts);
    let instance = unsafe { entry.create_instance(&ic_info, None).expect("instance") };
    let pds = unsafe { instance.enumerate_physical_devices().expect("pds") };
    assert!(!pds.is_empty());
    let pd = pds[0];
    let qp = vk::DeviceQueueCreateInfo::default()
        .queue_family_index(0).queue_priorities(&[1.0]);
    let queue_infos = [qp];
    let dc_info = vk::DeviceCreateInfo::default().queue_create_infos(&queue_infos);
    let device = unsafe { instance.create_device(pd, &dc_info, None).expect("device") };
    let queue  = unsafe { device.get_device_queue(0, 0) };
    println!("bootstrap                          -> OK");

    let vs_spv = build_pos_uv_vs();
    let fs_spv = build_textured_fs();
    let to_words = |spv: &[u8]| -> Vec<u32> {
        spv.chunks_exact(4).map(|c| u32::from_le_bytes([c[0],c[1],c[2],c[3]])).collect()
    };
    let vs = unsafe { device.create_shader_module(
        &vk::ShaderModuleCreateInfo::default().code(&to_words(&vs_spv)), None).expect("vs") };
    let fs = unsafe { device.create_shader_module(
        &vk::ShaderModuleCreateInfo::default().code(&to_words(&fs_spv)), None).expect("fs") };

    // ── Vertex buffer: pos(vec3) + UV(vec2) per vertex (20 B). ──
    let vbuf = unsafe { device.create_buffer(
        &vk::BufferCreateInfo::default()
            .size(64).usage(vk::BufferUsageFlags::VERTEX_BUFFER)
            .sharing_mode(vk::SharingMode::EXCLUSIVE),
        None).expect("vbuf") };
    let vreq = unsafe { device.get_buffer_memory_requirements(vbuf) };
    let vmt = unsafe { find_memory_type(&instance, pd, vreq.memory_type_bits,
        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT) };
    let vmem = unsafe { device.allocate_memory(
        &vk::MemoryAllocateInfo::default().allocation_size(vreq.size).memory_type_index(vmt),
        None).expect("vmem") };
    unsafe { device.bind_buffer_memory(vbuf, vmem, 0).expect("bind"); }
    let map = unsafe { device.map_memory(vmem, 0, vreq.size, vk::MemoryMapFlags::empty()).expect("map") };
    unsafe {
        let p = map as *mut f32;
        // Layout per vertex (20 B, stride 20):
        //   0..12  position (vec3)
        //   12..20 UV (vec2)
        let verts: [([f32; 3], [f32; 2]); 3] = [
            ([-0.5, -0.5, 0.0], [0.0, 0.0]),
            ([ 0.5, -0.5, 0.0], [1.0, 0.0]),
            ([ 0.0,  0.5, 0.0], [0.5, 1.0]),
        ];
        for (i, (pos, uv)) in verts.iter().enumerate() {
            let base = i * 5;
            for j in 0..3 { p.add(base + j).write_unaligned(pos[j]); }
            for j in 0..2 { p.add(base + 3 + j).write_unaligned(uv[j]); }
        }
    }
    unsafe { device.unmap_memory(vmem); }

    // ── 2x2 texture image (2 mip levels) + view. ──────────
    let tex_info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(vk::Format::R8G8B8A8_UNORM)
        .extent(vk::Extent3D { width: TEX_W, height: TEX_H, depth: 1 })
        .mip_levels(2).array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED);
    let tex_img = unsafe { device.create_image(&tex_info, None).expect("tex img") };
    let tex_req = unsafe { device.get_image_memory_requirements(tex_img) };
    let tex_mt = unsafe { find_memory_type(&instance, pd, tex_req.memory_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL) };
    let tex_mem = unsafe { device.allocate_memory(
        &vk::MemoryAllocateInfo::default().allocation_size(tex_req.size).memory_type_index(tex_mt),
        None).expect("tex mem") };
    unsafe { device.bind_image_memory(tex_img, tex_mem, 0).expect("bind tex"); }
    let tex_view = unsafe { device.create_image_view(
        &vk::ImageViewCreateInfo::default()
            .image(tex_img).view_type(vk::ImageViewType::TYPE_2D)
            .format(vk::Format::R8G8B8A8_UNORM)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0, level_count: 2,
                base_array_layer: 0, layer_count: 1,
            }),
        None).expect("tex view") };

    // ── Staging buffer with 2x2x4 = 16 bytes of texel data. ──
    let stage = unsafe { device.create_buffer(
        &vk::BufferCreateInfo::default()
            .size(64) // alignment slack
            .usage(vk::BufferUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE),
        None).expect("stage") };
    let sreq = unsafe { device.get_buffer_memory_requirements(stage) };
    let smt = unsafe { find_memory_type(&instance, pd, sreq.memory_type_bits,
        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT) };
    let smem = unsafe { device.allocate_memory(
        &vk::MemoryAllocateInfo::default().allocation_size(sreq.size).memory_type_index(smt),
        None).expect("smem") };
    unsafe { device.bind_buffer_memory(stage, smem, 0).expect("bind stage"); }
    let sm = unsafe { device.map_memory(smem, 0, sreq.size, vk::MemoryMapFlags::empty()).expect("map stage") };
    unsafe {
        let p = sm as *mut u8;
        // 2x2 mip 0 (all red, 16 B at offset 0).
        let l0: [u8; 4] = [255, 0, 0, 255];
        for i in 0..4 {
            for k in 0..4 { *p.add(i * 4 + k) = l0[k]; }
        }
        // 1x1 mip 1 (single blue texel, 4 B at offset 16).
        let l1: [u8; 4] = [0, 0, 255, 255];
        for k in 0..4 { *p.add(16 + k) = l1[k]; }
    }
    unsafe { device.unmap_memory(smem); }

    // ── Sampler (LINEAR filter, CLAMP_TO_EDGE wrap). ─────
    let sampler = unsafe { device.create_sampler(
        &vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::LINEAR)
            .min_filter(vk::Filter::LINEAR)
            .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .max_anisotropy(1.0)
            .min_lod(0.0).max_lod(0.0),
        None).expect("sampler") };

    // ── Descriptor set with one COMBINED_IMAGE_SAMPLER. ──
    let dsl_bindings = [vk::DescriptorSetLayoutBinding::default()
        .binding(0)
        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
        .descriptor_count(1)
        .stage_flags(vk::ShaderStageFlags::FRAGMENT)];
    let dsl = unsafe { device.create_descriptor_set_layout(
        &vk::DescriptorSetLayoutCreateInfo::default().bindings(&dsl_bindings),
        None).expect("dsl") };
    let pool_size = vk::DescriptorPoolSize::default()
        .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
        .descriptor_count(1);
    let pool_sizes = [pool_size];
    let pool = unsafe { device.create_descriptor_pool(
        &vk::DescriptorPoolCreateInfo::default()
            .max_sets(1).pool_sizes(&pool_sizes),
        None).expect("pool") };
    let dsl_array = [dsl];
    let dset = unsafe { device.allocate_descriptor_sets(
        &vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(pool).set_layouts(&dsl_array))
        .expect("dset")[0] };
    let img_info = [vk::DescriptorImageInfo::default()
        .image_view(tex_view)
        .sampler(sampler)
        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
    let write = vk::WriteDescriptorSet::default()
        .dst_set(dset).dst_binding(0)
        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
        .image_info(&img_info);
    unsafe { device.update_descriptor_sets(&[write], &[]); }

    // ── Render target. ───────────────────────────────────
    let rt_info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(vk::Format::R8G8B8A8_UNORM)
        .extent(vk::Extent3D { width: W, height: H, depth: 1 })
        .mip_levels(1).array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED);
    let color_img = unsafe { device.create_image(&rt_info, None).expect("rt img") };
    let rt_req = unsafe { device.get_image_memory_requirements(color_img) };
    let rt_mt = unsafe { find_memory_type(&instance, pd, rt_req.memory_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL) };
    let rt_mem = unsafe { device.allocate_memory(
        &vk::MemoryAllocateInfo::default().allocation_size(rt_req.size).memory_type_index(rt_mt),
        None).expect("rt mem") };
    unsafe { device.bind_image_memory(color_img, rt_mem, 0).expect("bind rt"); }
    let color_view = unsafe { device.create_image_view(
        &vk::ImageViewCreateInfo::default()
            .image(color_img).view_type(vk::ImageViewType::TYPE_2D)
            .format(vk::Format::R8G8B8A8_UNORM)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0, level_count: 1,
                base_array_layer: 0, layer_count: 1,
            }),
        None).expect("view") };
    let color_att = vk::AttachmentDescription::default()
        .format(vk::Format::R8G8B8A8_UNORM)
        .samples(vk::SampleCountFlags::TYPE_1)
        .load_op(vk::AttachmentLoadOp::CLEAR)
        .store_op(vk::AttachmentStoreOp::STORE)
        .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
        .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .final_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL);
    let atts = [color_att];
    let color_ref = vk::AttachmentReference::default()
        .attachment(0).layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
    let color_refs = [color_ref];
    let subpass = vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
        .color_attachments(&color_refs);
    let subpasses = [subpass];
    let render_pass = unsafe { device.create_render_pass(
        &vk::RenderPassCreateInfo::default().attachments(&atts).subpasses(&subpasses),
        None).expect("rp") };
    let fb_atts = [color_view];
    let framebuffer = unsafe { device.create_framebuffer(
        &vk::FramebufferCreateInfo::default()
            .render_pass(render_pass).attachments(&fb_atts)
            .width(W).height(H).layers(1),
        None).expect("fb") };

    let pl = unsafe { device.create_pipeline_layout(
        &vk::PipelineLayoutCreateInfo::default().set_layouts(&dsl_array),
        None).expect("pl") };

    let stage_vs = vk::PipelineShaderStageCreateInfo::default()
        .stage(vk::ShaderStageFlags::VERTEX).module(vs).name(c"main");
    let stage_fs = vk::PipelineShaderStageCreateInfo::default()
        .stage(vk::ShaderStageFlags::FRAGMENT).module(fs).name(c"main");
    let stages = [stage_vs, stage_fs];
    let bindings = [vk::VertexInputBindingDescription {
        binding: 0, stride: 20, input_rate: vk::VertexInputRate::VERTEX,
    }];
    let attrs = [
        vk::VertexInputAttributeDescription {
            location: 0, binding: 0,
            format: vk::Format::R32G32B32_SFLOAT, offset: 0,
        },
        vk::VertexInputAttributeDescription {
            location: 1, binding: 0,
            format: vk::Format::R32G32_SFLOAT, offset: 12,
        },
    ];
    let vi = vk::PipelineVertexInputStateCreateInfo::default()
        .vertex_binding_descriptions(&bindings)
        .vertex_attribute_descriptions(&attrs);
    let cp_info = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages).vertex_input_state(&vi)
        .layout(pl).render_pass(render_pass).subpass(0);
    let pipelines = unsafe { device.create_graphics_pipelines(
        vk::PipelineCache::null(), &[cp_info], None)
        .map_err(|(_,e)|e).expect("pipelines") };
    let pipeline = pipelines[0];

    let rb_size: u64 = (W * H * 4) as u64;
    let rb = unsafe { device.create_buffer(
        &vk::BufferCreateInfo::default()
            .size(rb_size).usage(vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE),
        None).expect("rb") };
    let rbr = unsafe { device.get_buffer_memory_requirements(rb) };
    let rbmt = unsafe { find_memory_type(&instance, pd, rbr.memory_type_bits,
        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT) };
    let rbm = unsafe { device.allocate_memory(
        &vk::MemoryAllocateInfo::default().allocation_size(rbr.size).memory_type_index(rbmt),
        None).expect("rbm") };
    unsafe { device.bind_buffer_memory(rb, rbm, 0).expect("bind rb"); }
    let m = unsafe { device.map_memory(rbm, 0, rbr.size, vk::MemoryMapFlags::empty()).expect("map rb") };
    unsafe { std::ptr::write_bytes(m as *mut u8, 0xCD, rb_size as usize); }
    unsafe { device.unmap_memory(rbm); }

    let cp = unsafe { device.create_command_pool(
        &vk::CommandPoolCreateInfo::default()
            .queue_family_index(0)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
        None).expect("cp") };
    let cbs = unsafe { device.allocate_command_buffers(
        &vk::CommandBufferAllocateInfo::default()
            .command_pool(cp).level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1)).expect("cbs") };
    let cb = cbs[0];
    unsafe { device.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default()).expect("begin"); }
    // Upload both mip levels in a single CopyBufferToImage.
    let tex_copy_l0 = vk::BufferImageCopy::default()
        .buffer_offset(0).buffer_row_length(0).buffer_image_height(0)
        .image_subresource(vk::ImageSubresourceLayers {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: 0, base_array_layer: 0, layer_count: 1,
        })
        .image_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
        .image_extent(vk::Extent3D { width: TEX_W, height: TEX_H, depth: 1 });
    let tex_copy_l1 = vk::BufferImageCopy::default()
        .buffer_offset(16).buffer_row_length(0).buffer_image_height(0)
        .image_subresource(vk::ImageSubresourceLayers {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: 1, base_array_layer: 0, layer_count: 1,
        })
        .image_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
        .image_extent(vk::Extent3D { width: 1, height: 1, depth: 1 });
    unsafe {
        device.cmd_copy_buffer_to_image(cb, stage, tex_img,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &[tex_copy_l0, tex_copy_l1]);
    }
    let clear = vk::ClearValue { color: vk::ClearColorValue { float32: [0.0, 0.0, 0.0, 0.0] } };
    let clears = [clear];
    unsafe {
        device.cmd_begin_render_pass(cb,
            &vk::RenderPassBeginInfo::default()
                .render_pass(render_pass).framebuffer(framebuffer)
                .render_area(vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent: vk::Extent2D { width: W, height: H },
                })
                .clear_values(&clears),
            vk::SubpassContents::INLINE);
        device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipeline);
        device.cmd_bind_descriptor_sets(cb, vk::PipelineBindPoint::GRAPHICS,
            pl, 0, &[dset], &[]);
        let vp = vk::Viewport { x: 0.0, y: 0.0, width: W as f32, height: H as f32,
                                min_depth: 0.0, max_depth: 1.0 };
        device.cmd_set_viewport(cb, 0, &[vp]);
        let scissor = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D { width: W, height: H },
        };
        device.cmd_set_scissor(cb, 0, &[scissor]);
        device.cmd_bind_vertex_buffers(cb, 0, &[vbuf], &[0]);
        device.cmd_draw(cb, 3, 1, 0, 0);
        device.cmd_end_render_pass(cb);
        let region = vk::BufferImageCopy::default()
            .buffer_offset(0).buffer_row_length(0).buffer_image_height(0)
            .image_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0, base_array_layer: 0, layer_count: 1,
            })
            .image_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
            .image_extent(vk::Extent3D { width: W, height: H, depth: 1 });
        device.cmd_copy_image_to_buffer(cb, color_img,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL, rb, &[region]);
        device.end_command_buffer(cb).expect("end");
    }
    let cbs_to_submit = [cb];
    let submit = vk::SubmitInfo::default().command_buffers(&cbs_to_submit);
    unsafe { device.queue_submit(queue, &[submit], vk::Fence::null()).expect("submit"); }
    unsafe { device.device_wait_idle().expect("wait"); }

    let m = unsafe { device.map_memory(rbm, 0, rbr.size, vk::MemoryMapFlags::empty()).expect("map") };
    let range = vk::MappedMemoryRange::default().memory(rbm).offset(0).size(rbr.size);
    unsafe { device.invalidate_mapped_memory_ranges(&[range]).expect("inv"); }
    let mut pixels = vec![0u8; (W * H * 4) as usize];
    unsafe { std::ptr::copy_nonoverlapping(m as *const u8, pixels.as_mut_ptr(), pixels.len()); }
    unsafe { device.unmap_memory(rbm); }

    // Print the framebuffer for diagnostic.
    println!("--- 8x8 framebuffer (RGBA u8) ---");
    for y in 0..H as usize {
        let mut row = String::new();
        for x in 0..W as usize {
            let i = (y * W as usize + x) * 4;
            row.push_str(&format!("[{:>3} {:>3} {:>3} {:>3}] ",
                pixels[i], pixels[i+1], pixels[i+2], pixels[i+3]));
        }
        println!("y={y}: {row}");
    }

    let i = (4 * W as usize + 4) * 4;
    let r = pixels[i];
    let g = pixels[i + 1];
    let b = pixels[i + 2];
    let a = pixels[i + 3];
    println!("pixel(4,4) = ({r},{g},{b},{a})  (want pure blue 0,0,255,255 from mip level 1)");
    // Explicit LOD=1.0 -> sample the 1x1 mip level 1
    // (single blue texel).  No interpolation between levels
    // here -- we don't yet honour `sampler.mipmapMode = LINEAR`
    // (that's a trilinear-filter follow-up); the helper
    // picks `mip_descs[1]` discretely.
    let ok = r == 0 && g == 0 && b == 255 && a == 255;

    unsafe {
        device.free_command_buffers(cp, &[cb]);
        device.destroy_command_pool(cp, None);
        device.destroy_buffer(rb, None);
        device.free_memory(rbm, None);
        device.destroy_pipeline(pipeline, None);
        device.destroy_pipeline_layout(pl, None);
        device.destroy_framebuffer(framebuffer, None);
        device.destroy_render_pass(render_pass, None);
        device.destroy_image_view(color_view, None);
        device.destroy_image(color_img, None);
        device.free_memory(rt_mem, None);
        device.destroy_descriptor_pool(pool, None);
        device.destroy_descriptor_set_layout(dsl, None);
        device.destroy_sampler(sampler, None);
        device.destroy_image_view(tex_view, None);
        device.destroy_image(tex_img, None);
        device.free_memory(tex_mem, None);
        device.destroy_buffer(stage, None);
        device.free_memory(smem, None);
        device.destroy_buffer(vbuf, None);
        device.free_memory(vmem, None);
        device.destroy_shader_module(vs, None);
        device.destroy_shader_module(fs, None);
        device.destroy_device(None);
        instance.destroy_instance(None);
    }
    if ok {
        println!("PASS: explicit LOD=1 fetched mip level 1's blue texel");
        0.into()
    } else {
        eprintln!("FAIL: pixel(4,4) not pure blue -- mip level 1 wasn't fetched");
        1.into()
    }
}
