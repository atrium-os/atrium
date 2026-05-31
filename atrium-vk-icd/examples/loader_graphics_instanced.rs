//! `examples/loader_graphics_instanced` — Rung TT: instanced
//! rendering driven by `gl_InstanceIndex`.
//!
//! A single full-width quad (two triangles, 6 verts) covering
//! the bottom third of clip space is drawn with
//! `instanceCount = 3`.  The vertex shader reads
//! `gl_InstanceIndex` and:
//!   * shifts the quad up by `instanceIndex * (2/3)` in clip-y,
//!     so the three instances tile the framebuffer into three
//!     horizontal bands; and
//!   * emits a red intensity of `(instanceIndex + 1) / 3` as a
//!     varying, so band 0/1/2 come out R = 85 / 170 / 255.
//!
//! The daemon replays the draw once per instance (the
//! `firstInstance .. firstInstance + instanceCount` loop in
//! `dispatch_draw`), handing each invocation its index as the
//! vertex shader's `params[5]`.  Without the instance loop only
//! one band (instance 0) would be painted; without the index
//! plumbing all three bands would share instance 0's colour.
//!
//! Discriminator: the three band centres show three *distinct,
//! strictly increasing* red values — proof each instance got a
//! different `gl_InstanceIndex` and positioned itself with it.

use ash::vk;
use rspirv::binary::Assemble;
use rspirv::spirv::{
    AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode,
    ExecutionModel, FunctionControl, MemoryModel, StorageClass,
};

/// Vertex shader:
///   in  vec2  in_pos          @ Location 0
///   in  (builtin) gl_InstanceIndex
///   out vec4  gl_Position
///   out float v_r             @ Location 0
fn build_vs() -> Vec<u8> {
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let i32_ty = b.type_int(32, 1);
    let u32_ty = b.type_int(32, 0);
    let vec2 = b.type_vector(f32_ty, 2);
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
    let ptr_in_vec2  = b.type_pointer(None, StorageClass::Input, vec2);
    let ptr_in_u32   = b.type_pointer(None, StorageClass::Input, u32_ty);

    let in_pos = b.variable(ptr_in_vec2, None, StorageClass::Input, None);
    b.decorate(in_pos, Decoration::Location,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    // gl_InstanceIndex — Input int decorated BuiltIn InstanceIndex.
    let inst_idx = b.variable(ptr_in_u32, None, StorageClass::Input, None);
    b.decorate(inst_idx, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::InstanceIndex)]);
    let pv_var = b.variable(ptr_pv, None, StorageClass::Output, None);
    let out_r = b.variable(ptr_out_f32, None, StorageClass::Output, None);
    b.decorate(out_r, Decoration::Location,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let c_zero   = b.constant_bit32(i32_ty, 0u32);
    let c_zero_f = b.constant_bit32(f32_ty, 0.0f32.to_bits());
    let c_one_f  = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    // band height in clip-y = 2/3.
    let c_band   = b.constant_bit32(f32_ty, (2.0f32 / 3.0).to_bits());
    let c_third  = b.constant_bit32(f32_ty, (1.0f32 / 3.0).to_bits());

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let pos = b.load(vec2, None, in_pos, None, vec![]).unwrap();
    let inst_u = b.load(u32_ty, None, inst_idx, None, vec![]).unwrap();
    let fi = b.convert_u_to_f(f32_ty, None, inst_u).unwrap();
    let px = b.composite_extract(f32_ty, None, pos, vec![0]).unwrap();
    let py = b.composite_extract(f32_ty, None, pos, vec![1]).unwrap();
    // y' = py + fi * (2/3)
    let dy = b.f_mul(f32_ty, None, fi, c_band).unwrap();
    let py2 = b.f_add(f32_ty, None, py, dy).unwrap();
    let pos4 = b.composite_construct(vec4, None,
        vec![px, py2, c_zero_f, c_one_f]).unwrap();
    let dst = b.access_chain(ptr_out_vec4, None, pv_var, vec![c_zero]).unwrap();
    b.store(dst, pos4, None, vec![]).unwrap();
    // v_r = (fi + 1) / 3
    let fi1 = b.f_add(f32_ty, None, fi, c_one_f).unwrap();
    let r = b.f_mul(f32_ty, None, fi1, c_third).unwrap();
    b.store(out_r, r, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Vertex, main, "main",
        vec![in_pos, inst_idx, pv_var, out_r]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

/// Fragment shader: out_color = vec4(v_r, 0, 0, 1).
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
    let in_r = b.variable(ptr_in_f32, None, StorageClass::Input, None);
    b.decorate(in_r, Decoration::Location,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let out = b.variable(ptr_out_vec4, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let c_zero_f = b.constant_bit32(f32_ty, 0.0f32.to_bits());
    let c_one_f  = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let r = b.load(f32_ty, None, in_r, None, vec![]).unwrap();
    let rgba = b.composite_construct(vec4, None,
        vec![r, c_zero_f, c_zero_f, c_one_f]).unwrap();
    b.store(out, rgba, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![in_r, out]);
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

    // ── Vertex buffer: 6 verts x vec2 (a full-width quad in the
    // bottom third of clip space, as two triangles). ──
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
        // Bottom-third quad: x in [-1, 1], y in [-1, -1/3].
        let yb = -1.0f32;
        let yt = -1.0f32 / 3.0;
        let verts: [[f32; 2]; 6] = [
            [-1.0, yb], [ 1.0, yb], [ 1.0, yt], // tri 1
            [-1.0, yb], [ 1.0, yt], [-1.0, yt], // tri 2
        ];
        let mut i = 0;
        for v in verts { for f in v { p.add(i).write_unaligned(f); i += 1; } }
    }
    unsafe { device.unmap_memory(vmem); }
    println!("vbuf write 6 verts (bottom quad)   -> OK");

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

    let stage_vs = vk::PipelineShaderStageCreateInfo::default()
        .stage(vk::ShaderStageFlags::VERTEX).module(vs).name(c"main");
    let stage_fs = vk::PipelineShaderStageCreateInfo::default()
        .stage(vk::ShaderStageFlags::FRAGMENT).module(fs).name(c"main");
    let stages = [stage_vs, stage_fs];
    let bindings = [vk::VertexInputBindingDescription {
        binding: 0, stride: 8, input_rate: vk::VertexInputRate::VERTEX,
    }];
    let attrs = [
        vk::VertexInputAttributeDescription {
            location: 0, binding: 0,
            format: vk::Format::R32G32_SFLOAT, offset: 0,
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
        // 6 verts (2 triangles), 3 instances.
        device.cmd_draw(cb, 6, 3, 0, 0);
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

    let m2 = unsafe { device.map_memory(rbm, 0, rbr.size, vk::MemoryMapFlags::empty()).expect("map") };
    let range = vk::MappedMemoryRange::default().memory(rbm).offset(0).size(rbr.size);
    unsafe { device.invalidate_mapped_memory_ranges(&[range]).expect("inv"); }
    let mut pixels = vec![0u8; (W * H * 4) as usize];
    unsafe { std::ptr::copy_nonoverlapping(m2 as *const u8, pixels.as_mut_ptr(), pixels.len()); }
    unsafe { device.unmap_memory(rbm); }

    println!("--- 8x8 framebuffer R channel (instanced bands) ---");
    for y in 0..H as usize {
        let mut row = String::new();
        for x in 0..W as usize {
            let i = (y * W as usize + x) * 4;
            row.push_str(&format!("{:>3} ", pixels[i]));
        }
        println!("y={y}: {row}");
    }

    let at = |x: usize, y: usize| -> u8 { pixels[(y * W as usize + x) * 4] };
    // Band centres: instance 0 ~ y=1, instance 1 ~ y=4, instance 2 ~ y=6.
    let b0 = at(4, 1);
    let b1 = at(4, 4);
    let b2 = at(4, 6);
    println!("band R: instance0(y=1)={b0}  instance1(y=4)={b1}  \
              instance2(y=6)={b2}  (want ~85 < ~170 < ~255)");

    // Each band must be lit and strictly brighter than the one
    // below — proving the three instances received gl_InstanceIndex
    // 0, 1, 2 and used it for both position and colour.
    let ok = b0 > 0 && b1 > b0 && b2 > b1;

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
        println!("PASS: 3 instances painted 3 distinct bands via gl_InstanceIndex");
        0.into()
    } else {
        eprintln!("FAIL: instanced bands not distinct/increasing \
                   (b0={b0} b1={b1} b2={b2})");
        1.into()
    }
}
