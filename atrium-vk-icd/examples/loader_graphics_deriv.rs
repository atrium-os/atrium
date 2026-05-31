//! `examples/loader_graphics_deriv` — Rung SS: screen-space
//! derivatives of an arbitrary (non-affine) expression via
//! the daemon's 2x2-quad lockstep rasterization path.
//!
//! The fragment shader receives a single scalar varying `u`
//! that ramps linearly with screen-x (0 at the left edge,
//! growing to the right).  It then computes a **non-affine**
//! expression `e = u * u` and emits `dFdx(e)` into the red
//! channel.
//!
//! Why this is a real test of the quad path (not the
//! implicit-LOD special case): the derivative of `u*u` is
//! `2*u*du/dx`, which is *position dependent* — it grows as
//! you move right across the framebuffer.  A trivial
//! "derivative of a varying is a constant" shortcut cannot
//! produce this; only genuine per-quad finite differencing
//! (`e(x+1) - e(x)` evaluated by re-running the FS at all
//! four quad lanes) does.  And if derivatives were still
//! lowered to zero (the pre-quad behaviour), every pixel
//! would be pure black.
//!
//! Discriminator:
//!   * some covered pixel has R > 0   (derivatives non-zero)
//!   * R at a right-column pixel  >  R at a left-column pixel
//!     (the derivative is position-dependent ⇒ the expression
//!      `u*u` was really differenced, not faked)
//!
//! Geometry: one oversized triangle covering the whole 8x8
//! viewport.  The two `u=0` vertices share clip-x = -1 and the
//! `u=4` vertex sits at clip-x = 3, so `u` depends only on
//! clip-x (≈ screen-x / 4) and `dFdy(e) == 0`.

use ash::vk;
use rspirv::binary::Assemble;
use rspirv::spirv::{
    AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode,
    ExecutionModel, FunctionControl, MemoryModel, StorageClass,
};

/// Vertex shader:
///   in  vec3  in_pos @ Location 0
///   in  float in_u   @ Location 1
///   out vec4  gl_Position
///   out float v_u    @ Location 0
fn build_vs() -> Vec<u8> {
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let i32_ty = b.type_int(32, 1);
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
    let ptr_out_f32  = b.type_pointer(None, StorageClass::Output, f32_ty);
    let ptr_in_vec3  = b.type_pointer(None, StorageClass::Input, vec3);
    let ptr_in_f32   = b.type_pointer(None, StorageClass::Input, f32_ty);

    let in_pos = b.variable(ptr_in_vec3, None, StorageClass::Input, None);
    b.decorate(in_pos, Decoration::Location,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let in_u = b.variable(ptr_in_f32, None, StorageClass::Input, None);
    b.decorate(in_u, Decoration::Location,
        vec![rspirv::dr::Operand::LiteralBit32(1)]);
    let pv_var = b.variable(ptr_pv, None, StorageClass::Output, None);
    let out_u = b.variable(ptr_out_f32, None, StorageClass::Output, None);
    b.decorate(out_u, Decoration::Location,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let c_zero  = b.constant_bit32(i32_ty, 0u32);
    let c_one_f = b.constant_bit32(f32_ty, 1.0f32.to_bits());

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let pos = b.load(vec3, None, in_pos, None, vec![]).unwrap();
    let uu  = b.load(f32_ty, None, in_u, None, vec![]).unwrap();
    let x = b.composite_extract(f32_ty, None, pos, vec![0]).unwrap();
    let y = b.composite_extract(f32_ty, None, pos, vec![1]).unwrap();
    let z = b.composite_extract(f32_ty, None, pos, vec![2]).unwrap();
    let pos4 = b.composite_construct(vec4, None, vec![x, y, z, c_one_f]).unwrap();
    let dst = b.access_chain(ptr_out_vec4, None, pv_var, vec![c_zero]).unwrap();
    b.store(dst, pos4, None, vec![]).unwrap();
    b.store(out_u, uu, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Vertex, main, "main",
        vec![in_pos, in_u, pv_var, out_u]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

/// Fragment shader:
///   in  float v_u      @ Location 0
///   out vec4  out_color @ Location 0
///
///   e   = v_u * v_u                  (non-affine in screen-x)
///   d   = dFdx(e)                    (per-quad finite diff)
///   out = vec4(d, 0, 0, 1)
fn build_fs() -> Vec<u8> {
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let ptr_in_f32   = b.type_pointer(None, StorageClass::Input, f32_ty);
    let ptr_out_vec4 = b.type_pointer(None, StorageClass::Output, vec4);
    let in_u = b.variable(ptr_in_f32, None, StorageClass::Input, None);
    b.decorate(in_u, Decoration::Location,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let out = b.variable(ptr_out_vec4, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let c_zero_f = b.constant_bit32(f32_ty, 0.0f32.to_bits());
    let c_one_f  = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let u = b.load(f32_ty, None, in_u, None, vec![]).unwrap();
    // e = u * u  -- the non-affine expression we differentiate.
    let e = b.f_mul(f32_ty, None, u, u).unwrap();
    // d = dFdx(e)  -- OpDPdx.
    let d = b.d_pdx(f32_ty, None, e).unwrap();
    let rgba = b.composite_construct(vec4, None,
        vec![d, c_zero_f, c_zero_f, c_one_f]).unwrap();
    b.store(out, rgba, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![in_u, out]);
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

    // ── Shaders. ─────────────────────────────────────────────
    let vs_spv = build_vs();
    let fs_spv = build_fs();
    let vs_words: Vec<u32> = vs_spv.chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0],c[1],c[2],c[3]])).collect();
    let fs_words: Vec<u32> = fs_spv.chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0],c[1],c[2],c[3]])).collect();
    let vs = unsafe { device.create_shader_module(
        &vk::ShaderModuleCreateInfo::default().code(&vs_words), None).expect("vs") };
    let fs = unsafe { device.create_shader_module(
        &vk::ShaderModuleCreateInfo::default().code(&fs_words), None).expect("fs") };

    // ── Vertex buffer: pos(vec3) + u(float) = 16 B/v, 3 v. ──
    let vbuf_info = vk::BufferCreateInfo::default()
        .size(64)
        .usage(vk::BufferUsageFlags::VERTEX_BUFFER)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let vbuf = unsafe { device.create_buffer(&vbuf_info, None).expect("vbuf") };
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
        // Oversized triangle covering the whole viewport.  The
        // two u=0 verts share clip-x = -1; the u=4 vert is at
        // clip-x = 3, so u ramps linearly with clip-x only.
        //   V0: pos(-1, -1, 0)  u = 0
        //   V1: pos( 3, -1, 0)  u = 4
        //   V2: pos(-1,  3, 0)  u = 0
        let verts: [[f32; 4]; 3] = [
            [-1.0, -1.0, 0.0,  0.0],
            [ 3.0, -1.0, 0.0,  4.0],
            [-1.0,  3.0, 0.0,  0.0],
        ];
        let mut i = 0;
        for v in verts { for f in v { p.add(i).write_unaligned(f); i += 1; } }
    }
    unsafe { device.unmap_memory(vmem); }
    println!("vbuf write 3 verts (pos+u)         -> OK");

    // ── Render target. ──────────────────────────────────────
    let img_info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(vk::Format::R8G8B8A8_UNORM)
        .extent(vk::Extent3D { width: W, height: H, depth: 1 })
        .mip_levels(1).array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED);
    let color_img = unsafe { device.create_image(&img_info, None).expect("img") };
    let ireq = unsafe { device.get_image_memory_requirements(color_img) };
    let imt = unsafe { find_memory_type(&instance, pd, ireq.memory_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL) };
    let imem = unsafe { device.allocate_memory(
        &vk::MemoryAllocateInfo::default().allocation_size(ireq.size).memory_type_index(imt),
        None).expect("imem") };
    unsafe { device.bind_image_memory(color_img, imem, 0).expect("bind img"); }
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

    // ── Render pass + framebuffer. ──────────────────────────
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

    // ── Pipeline. ───────────────────────────────────────────
    let stage_vs = vk::PipelineShaderStageCreateInfo::default()
        .stage(vk::ShaderStageFlags::VERTEX).module(vs).name(c"main");
    let stage_fs = vk::PipelineShaderStageCreateInfo::default()
        .stage(vk::ShaderStageFlags::FRAGMENT).module(fs).name(c"main");
    let stages = [stage_vs, stage_fs];
    let bindings = [vk::VertexInputBindingDescription {
        binding: 0, stride: 16, input_rate: vk::VertexInputRate::VERTEX,
    }];
    let attrs = [
        vk::VertexInputAttributeDescription {
            location: 0, binding: 0,
            format: vk::Format::R32G32B32_SFLOAT, offset: 0,
        },
        vk::VertexInputAttributeDescription {
            location: 1, binding: 0,
            format: vk::Format::R32_SFLOAT, offset: 12,
        },
    ];
    let vi = vk::PipelineVertexInputStateCreateInfo::default()
        .vertex_binding_descriptions(&bindings)
        .vertex_attribute_descriptions(&attrs);
    let pl = unsafe { device.create_pipeline_layout(
        &vk::PipelineLayoutCreateInfo::default(), None).expect("pl") };
    let cp_info = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages).vertex_input_state(&vi)
        .layout(pl).render_pass(render_pass).subpass(0);
    let pipelines = unsafe { device.create_graphics_pipelines(
        vk::PipelineCache::null(), &[cp_info], None).map_err(|(_,e)|e).expect("pipelines") };
    let pipeline = pipelines[0];

    // ── Readback buffer. ────────────────────────────────────
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
    let m2 = unsafe { device.map_memory(rbm, 0, rbr.size, vk::MemoryMapFlags::empty()).expect("map rb") };
    unsafe { std::ptr::write_bytes(m2 as *mut u8, 0xCD, rb_size as usize); }
    unsafe { device.unmap_memory(rbm); }

    // ── Command buffer. ─────────────────────────────────────
    let cp = unsafe { device.create_command_pool(
        &vk::CommandPoolCreateInfo::default().queue_family_index(0)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
        None).expect("cp") };
    let cbs = unsafe { device.allocate_command_buffers(
        &vk::CommandBufferAllocateInfo::default()
            .command_pool(cp).level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1)).expect("cbs") };
    let cb = cbs[0];
    unsafe { device.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default()).expect("begin"); }
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

    // ── Readback. ───────────────────────────────────────────
    let m2 = unsafe { device.map_memory(rbm, 0, rbr.size, vk::MemoryMapFlags::empty()).expect("map") };
    let range = vk::MappedMemoryRange::default().memory(rbm).offset(0).size(rbr.size);
    unsafe { device.invalidate_mapped_memory_ranges(&[range]).expect("inv"); }
    let mut pixels = vec![0u8; (W * H * 4) as usize];
    unsafe { std::ptr::copy_nonoverlapping(m2 as *const u8, pixels.as_mut_ptr(), pixels.len()); }
    unsafe { device.unmap_memory(rbm); }

    println!("--- 8x8 framebuffer R channel (dFdx(u*u)) ---");
    for y in 0..H as usize {
        let mut row = String::new();
        for x in 0..W as usize {
            let i = (y * W as usize + x) * 4;
            row.push_str(&format!("{:>3} ", pixels[i]));
        }
        println!("y={y}: {row}");
    }

    let at = |x: usize, y: usize| -> u8 { pixels[(y * W as usize + x) * 4] };
    let r_left  = at(1, 4);
    let r_right = at(6, 4);
    println!("R(1,4) = {r_left}   R(6,4) = {r_right}  \
              (want 0 < left < right -- derivative grows with x)");

    // Any non-zero red anywhere proves the quad mechanism ran
    // (the old zero-lowering would leave the whole frame black).
    let any_nonzero = pixels.iter().step_by(4).any(|&r| r > 0);

    let ok = any_nonzero
        && r_left > 0
        && r_right > r_left;

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
        device.free_memory(imem, None);
        device.destroy_buffer(vbuf, None);
        device.free_memory(vmem, None);
        device.destroy_shader_module(vs, None);
        device.destroy_shader_module(fs, None);
        device.destroy_device(None);
        instance.destroy_instance(None);
    }
    if ok {
        println!("PASS: dFdx(u*u) produced a position-dependent \
                  derivative via 2x2-quad lockstep");
        0.into()
    } else {
        eprintln!("FAIL: derivative not position-dependent \
                   (left={r_left} right={r_right} any_nonzero={any_nonzero})");
        1.into()
    }
}
