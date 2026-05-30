//! `examples/loader_graphics_mrt` — multiple render targets.
//! A single triangle + FS writes two colour outputs
//! (Location 0 = red, Location 1 = green) into a 2-attachment
//! framebuffer; each attachment is copied back to its own
//! buffer and asserted independently.  Exercises the
//! cranelift FS multi-output routing + the daemon's
//! BindColorAttachments + per-attachment scatter.
//!
//! Original round-trip doc retained below.
//!
//! end-to-end test that
//! a real Vulkan app renders a triangle through the Khronos
//! loader, the rasterized pixels reach the daemon-side image,
//! and `vkCmdCopyImageToBuffer` + `vkInvalidateMappedMemoryRanges`
//! pull them back into the client's mapped pointer.
//!
//! Graphics-path companion to `loader_compute_roundtrip`.  Where
//! the compute round-trip verifies SSBO writes survive the wire,
//! this one verifies the entire render pipeline (vertex shader +
//! fragment shader + rasterizer + framebuffer + image-to-buffer
//! copy) survives the loader → ICD → daemon → readback chain.
//!
//! Pipeline:
//!
//!   1.  vkCreateInstance / vkEnumeratePhysicalDevices /
//!       vkCreateDevice / vkGetDeviceQueue (loader handshake).
//!   2.  Build VS + FS SPIR-V inline via rspirv (passthrough
//!       VS that emits `vec4(pos, 1.0)` as Position; constant-
//!       color FS that writes `(1.0, 0.2, 0.2, 1.0)` to
//!       Location=0).
//!   3.  vkCreateBuffer + vkAllocateMemory + vkBindBufferMemory
//!       + vkMapMemory: 3 NDC vec3 vertices forming a triangle
//!       centred on the origin.
//!   4.  vkCreateImage (8x8 R8G8B8A8_UNORM, COLOR_ATTACHMENT
//!       + TRANSFER_SRC, DEVICE_LOCAL) + view + render pass +
//!       framebuffer.
//!   5.  vkCreateGraphicsPipelines with real shader stages,
//!       vertex-input layout, viewport, scissor, raster, blend
//!       state.
//!   6.  vkCreateBuffer for the readback target (8*8*4 bytes,
//!       HOST_VISIBLE, TRANSFER_DST).
//!   7.  Record + submit: BeginRenderPass + BindPipeline +
//!       SetViewport/Scissor + BindVertexBuffers + Draw(3,1,0,0)
//!       + EndRenderPass + CmdCopyImageToBuffer.
//!   8.  vkDeviceWaitIdle + vkInvalidateMappedMemoryRanges.
//!   9.  Read 8x8x4 = 256 bytes from the mapped pointer.
//!       Assert the triangle interior pixel (3,3) matches the
//!       FS colour `(255, 51, 51, 255)` (= 1.0/0.2/0.2 * 255
//!       quantised).
//!
//! Exit code:
//!   0 -> triangle pixels reached the client unmodified.
//!   non-0 -> see the printed step that failed.

use ash::vk;
use rspirv::binary::Assemble;
use rspirv::spirv::{
    AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode,
    ExecutionModel, FunctionControl, MemoryModel, StorageClass,
};

/// Vertex shader: reads `vec3` Location=0, writes `vec4(pos, 1.0)`
/// as `gl_Position`.  Same shape as the test-suite passthrough VS
/// but built inline so the example has no internal-test
/// dependencies.
fn build_passthrough_vs() -> Vec<u8> {
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
    let ptr_in_vec3  = b.type_pointer(None, StorageClass::Input, vec3);
    let in_pos = b.variable(ptr_in_vec3, None, StorageClass::Input, None);
    b.decorate(in_pos, Decoration::Location,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let pv_var = b.variable(ptr_pv, None, StorageClass::Output, None);
    let c_zero  = b.constant_bit32(i32_ty, 0u32);
    let c_one_f = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let pos = b.load(vec3, None, in_pos, None, vec![]).unwrap();
    let x = b.composite_extract(f32_ty, None, pos, vec![0]).unwrap();
    let y = b.composite_extract(f32_ty, None, pos, vec![1]).unwrap();
    let z = b.composite_extract(f32_ty, None, pos, vec![2]).unwrap();
    let pos4 = b.composite_construct(vec4, None, vec![x, y, z, c_one_f]).unwrap();
    let dst = b.access_chain(ptr_out_vec4, None, pv_var, vec![c_zero]).unwrap();
    b.store(dst, pos4, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Vertex, main, "main", vec![in_pos, pv_var]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

/// Fragment shader: writes a constant RGBA colour to Location=0.
fn build_constant_color_fs(rgba: [f32; 4]) -> Vec<u8> {
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let vec4_f32 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let ptr_out = b.type_pointer(None, StorageClass::Output, vec4_f32);
    let cs: Vec<_> = rgba.iter()
        .map(|x| b.constant_bit32(f32_ty, x.to_bits())).collect();
    let color = b.constant_composite(vec4_f32, cs);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    b.store(out, color, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

/// MRT fragment shader: writes `rgba0` to Location=0 and
/// `rgba1` to Location=1.  Exercises the cranelift FS
/// multi-output routing (Location L -> out_color + L*16) and
/// the daemon's per-attachment scatter.
fn build_mrt_fs(rgba0: [f32; 4], rgba1: [f32; 4]) -> Vec<u8> {
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let vec4_f32 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let ptr_out = b.type_pointer(None, StorageClass::Output, vec4_f32);
    let mk = |b: &mut rspirv::dr::Builder, rgba: [f32; 4]| {
        let cs: Vec<_> = rgba.iter()
            .map(|x| b.constant_bit32(f32_ty, x.to_bits())).collect();
        b.constant_composite(vec4_f32, cs)
    };
    let color0 = mk(&mut b, rgba0);
    let color1 = mk(&mut b, rgba1);
    let out0 = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out0, Decoration::Location, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let out1 = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out1, Decoration::Location, vec![rspirv::dr::Operand::LiteralBit32(1)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    b.store(out0, color0, None, vec![]).unwrap();
    b.store(out1, color1, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![out0, out1]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

/// Pick a memory type that satisfies `type_filter` and carries
/// every flag in `want`.  Panics if none match.
unsafe fn find_memory_type(
    instance: &ash::Instance,
    physical: vk::PhysicalDevice,
    type_filter: u32,
    want: vk::MemoryPropertyFlags,
) -> u32 {
    let mp = instance.get_physical_device_memory_properties(physical);
    for i in 0..mp.memory_type_count {
        let suitable = (type_filter & (1u32 << i)) != 0;
        let has = mp.memory_types[i as usize].property_flags.contains(want);
        if suitable && has { return i; }
    }
    panic!("no compatible memory type for filter={type_filter:#b} props={want:?}");
}

const W: u32 = 8;
const H: u32 = 8;

fn main() -> std::process::ExitCode {
    let entry = unsafe {
        match ash::Entry::load() {
            Ok(e)  => { println!("ash::Entry::load                  -> OK"); e }
            Err(e) => { eprintln!("ash::Entry::load                  -> ERROR: {e}"); return 1.into(); }
        }
    };

    let app_info = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3);
    let flags = vk::InstanceCreateFlags::ENUMERATE_PORTABILITY_KHR;
    let exts  = [vk::KHR_PORTABILITY_ENUMERATION_NAME.as_ptr()];
    let ic_info = vk::InstanceCreateInfo::default()
        .application_info(&app_info)
        .flags(flags)
        .enabled_extension_names(&exts);
    let instance = unsafe { entry.create_instance(&ic_info, None).expect("instance") };
    println!("vkCreateInstance                  -> OK");

    let pds = unsafe { instance.enumerate_physical_devices().expect("pds") };
    assert!(!pds.is_empty(), "no physical devices");
    let pd = pds[0];
    println!("vkEnumeratePhysicalDevices        -> OK, picked device[0]");

    let qp = vk::DeviceQueueCreateInfo::default()
        .queue_family_index(0)
        .queue_priorities(&[1.0]);
    let queue_infos = [qp];
    let dc_info = vk::DeviceCreateInfo::default().queue_create_infos(&queue_infos);
    let device = unsafe { instance.create_device(pd, &dc_info, None).expect("device") };
    let queue  = unsafe { device.get_device_queue(0, 0) };
    println!("vkCreateDevice / GetDeviceQueue   -> OK");

    // ── Shader modules. ─────────────────────────────────────────
    let _ = build_constant_color_fs; // (single-output FS retained for reference)
    let vs_spv = build_passthrough_vs();
    // MRT FS: red -> attachment 0, green -> attachment 1.
    let fs_spv = build_mrt_fs([1.0, 0.0, 0.0, 1.0], [0.0, 1.0, 0.0, 1.0]);
    let vs_words: Vec<u32> = vs_spv.chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0],c[1],c[2],c[3]])).collect();
    let fs_words: Vec<u32> = fs_spv.chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0],c[1],c[2],c[3]])).collect();
    let vs = unsafe { device.create_shader_module(
        &vk::ShaderModuleCreateInfo::default().code(&vs_words), None).expect("vs") };
    let fs = unsafe { device.create_shader_module(
        &vk::ShaderModuleCreateInfo::default().code(&fs_words), None).expect("fs") };
    println!("vkCreateShaderModule x2           -> OK");

    // ── Vertex buffer (HOST_VISIBLE | HOST_COHERENT, VERTEX usage). ──
    // 3 NDC vec3 vertices = 36 bytes; round up to satisfy mem
    // requirements alignment.
    let vbuf_info = vk::BufferCreateInfo::default()
        .size(48)
        .usage(vk::BufferUsageFlags::VERTEX_BUFFER)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let vbuf = unsafe { device.create_buffer(&vbuf_info, None).expect("vbuf") };
    let vreq = unsafe { device.get_buffer_memory_requirements(vbuf) };
    let vmt  = unsafe { find_memory_type(&instance, pd, vreq.memory_type_bits,
        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT) };
    let vmem = unsafe { device.allocate_memory(
        &vk::MemoryAllocateInfo::default().allocation_size(vreq.size).memory_type_index(vmt),
        None).expect("vmem") };
    unsafe { device.bind_buffer_memory(vbuf, vmem, 0).expect("bind vbuf"); }
    let vp_map = unsafe { device.map_memory(vmem, 0, vreq.size, vk::MemoryMapFlags::empty()).expect("map vbuf") };
    unsafe {
        let p = vp_map as *mut f32;
        let verts: [[f32; 3]; 3] = [
            [-0.5, -0.5, 0.0],
            [ 0.5, -0.5, 0.0],
            [ 0.0,  0.5, 0.0],
        ];
        let mut i = 0;
        for v in verts { for f in v { p.add(i).write_unaligned(f); i += 1; } }
    }
    unsafe { device.unmap_memory(vmem); }
    println!("vbuf write 3 NDC verts            -> OK");

    // ── Two render-target images (8x8 RGBA8) for MRT. ──────────
    let make_color_image = || -> (vk::Image, vk::DeviceMemory, vk::ImageView) {
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
        let img = unsafe { device.create_image(&img_info, None).expect("color_img") };
        let req = unsafe { device.get_image_memory_requirements(img) };
        let mt  = unsafe { find_memory_type(&instance, pd, req.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL) };
        let mem = unsafe { device.allocate_memory(
            &vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(mt),
            None).expect("imem") };
        unsafe { device.bind_image_memory(img, mem, 0).expect("bind img"); }
        let view = unsafe { device.create_image_view(
            &vk::ImageViewCreateInfo::default()
                .image(img)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(vk::Format::R8G8B8A8_UNORM)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0, level_count: 1,
                    base_array_layer: 0, layer_count: 1,
                }),
            None).expect("view") };
        (img, mem, view)
    };
    let (color_img, imem, color_view)  = make_color_image();   // attachment 0
    let (color_img1, imem1, color_view1) = make_color_image(); // attachment 1
    println!("2 color_img + views (8x8 R8G8B8A8) -> OK");

    // ── Render pass + framebuffer (2 colour attachments). ───────
    let mk_att = || vk::AttachmentDescription::default()
        .format(vk::Format::R8G8B8A8_UNORM)
        .samples(vk::SampleCountFlags::TYPE_1)
        .load_op(vk::AttachmentLoadOp::CLEAR)
        .store_op(vk::AttachmentStoreOp::STORE)
        .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
        .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .final_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL);
    let atts = [mk_att(), mk_att()];
    let color_refs = [
        vk::AttachmentReference::default().attachment(0)
            .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL),
        vk::AttachmentReference::default().attachment(1)
            .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL),
    ];
    let subpass = vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
        .color_attachments(&color_refs);
    let subpasses = [subpass];
    let render_pass = unsafe { device.create_render_pass(
        &vk::RenderPassCreateInfo::default()
            .attachments(&atts)
            .subpasses(&subpasses),
        None).expect("render_pass") };
    let fb_atts = [color_view, color_view1];
    let framebuffer = unsafe { device.create_framebuffer(
        &vk::FramebufferCreateInfo::default()
            .render_pass(render_pass)
            .attachments(&fb_atts)
            .width(W).height(H).layers(1),
        None).expect("fb") };
    println!("vkCreateRenderPass + Framebuffer (MRT x2) -> OK");

    // ── Graphics pipeline. ──────────────────────────────────────
    let stage_vs = vk::PipelineShaderStageCreateInfo::default()
        .stage(vk::ShaderStageFlags::VERTEX).module(vs).name(c"main");
    let stage_fs = vk::PipelineShaderStageCreateInfo::default()
        .stage(vk::ShaderStageFlags::FRAGMENT).module(fs).name(c"main");
    let stages = [stage_vs, stage_fs];
    let bindings = [vk::VertexInputBindingDescription {
        binding: 0, stride: 12, input_rate: vk::VertexInputRate::VERTEX,
    }];
    let attrs = [vk::VertexInputAttributeDescription {
        location: 0, binding: 0,
        format: vk::Format::R32G32B32_SFLOAT, offset: 0,
    }];
    let vi = vk::PipelineVertexInputStateCreateInfo::default()
        .vertex_binding_descriptions(&bindings)
        .vertex_attribute_descriptions(&attrs);
    let pl_info = vk::PipelineLayoutCreateInfo::default();
    let pl = unsafe { device.create_pipeline_layout(&pl_info, None).expect("pl") };
    let cp_info = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vi)
        .layout(pl)
        .render_pass(render_pass)
        .subpass(0);
    let pipelines = unsafe { device.create_graphics_pipelines(
        vk::PipelineCache::null(), &[cp_info], None).map_err(|(_, e)| e).expect("pipelines") };
    let pipeline = pipelines[0];
    println!("vkCreateGraphicsPipelines         -> OK");

    // ── Two readback buffers (one per colour attachment). ───────
    let rb_size: u64 = (W * H * 4) as u64;
    let make_readback = || -> (vk::Buffer, vk::DeviceMemory) {
        let rb_info = vk::BufferCreateInfo::default()
            .size(rb_size)
            .usage(vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let rb = unsafe { device.create_buffer(&rb_info, None).expect("rb") };
        let rbr = unsafe { device.get_buffer_memory_requirements(rb) };
        let rbmt = unsafe { find_memory_type(&instance, pd, rbr.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT) };
        let rbm = unsafe { device.allocate_memory(
            &vk::MemoryAllocateInfo::default().allocation_size(rbr.size).memory_type_index(rbmt),
            None).expect("rbm") };
        unsafe { device.bind_buffer_memory(rb, rbm, 0).expect("bind rb"); }
        let map = unsafe { device.map_memory(rbm, 0, rbr.size, vk::MemoryMapFlags::empty()).expect("map rb") };
        unsafe { std::ptr::write_bytes(map as *mut u8, 0xCD, rb_size as usize); }
        unsafe { device.unmap_memory(rbm); }
        (rb, rbm)
    };
    let (rb, rbm)   = make_readback();   // attachment 0
    let (rb1, rbm1) = make_readback();   // attachment 1
    let rbr = unsafe { device.get_buffer_memory_requirements(rb) };
    println!("2 readback buffers + 0xCD seed   -> OK");

    // ── Command buffer: render + copy. ──────────────────────────
    let cp = unsafe { device.create_command_pool(
        &vk::CommandPoolCreateInfo::default()
            .queue_family_index(0)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
        None).expect("cmd_pool") };
    let cbs = unsafe { device.allocate_command_buffers(
        &vk::CommandBufferAllocateInfo::default()
            .command_pool(cp).level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1)).expect("cbs") };
    let cb = cbs[0];

    unsafe { device.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default()).expect("begin"); }
    let clear = vk::ClearValue { color: vk::ClearColorValue { float32: [0.0, 0.0, 0.0, 0.0] } };
    let clears = [clear, clear]; // one per colour attachment
    unsafe {
        device.cmd_begin_render_pass(cb,
            &vk::RenderPassBeginInfo::default()
                .render_pass(render_pass)
                .framebuffer(framebuffer)
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
        // Copy each colour attachment -> its readback buffer.
        let region = vk::BufferImageCopy::default()
            .buffer_offset(0)
            .buffer_row_length(0)
            .buffer_image_height(0)
            .image_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0, base_array_layer: 0, layer_count: 1,
            })
            .image_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
            .image_extent(vk::Extent3D { width: W, height: H, depth: 1 });
        device.cmd_copy_image_to_buffer(
            cb, color_img, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, rb, &[region]);
        device.cmd_copy_image_to_buffer(
            cb, color_img1, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, rb1, &[region]);
        device.end_command_buffer(cb).expect("end");
    }
    let cbs_to_submit = [cb];
    let submit = vk::SubmitInfo::default().command_buffers(&cbs_to_submit);
    unsafe { device.queue_submit(queue, &[submit], vk::Fence::null()).expect("submit"); }
    unsafe { device.device_wait_idle().expect("wait_idle"); }
    println!("render + copy + WaitIdle          -> OK");

    // ── Pull both readback buffers. ─────────────────────────────
    let read_pixels = |rbm: vk::DeviceMemory| -> Vec<u8> {
        let map = unsafe { device.map_memory(rbm, 0, rbr.size, vk::MemoryMapFlags::empty()).expect("map rb 2") };
        let range = vk::MappedMemoryRange::default()
            .memory(rbm).offset(0).size(rbr.size);
        unsafe { device.invalidate_mapped_memory_ranges(&[range]).expect("invalidate"); }
        let mut px = vec![0u8; (W * H * 4) as usize];
        unsafe { std::ptr::copy_nonoverlapping(
            map as *const u8, px.as_mut_ptr(), px.len()); }
        unsafe { device.unmap_memory(rbm); }
        px
    };
    let pixels0 = read_pixels(rbm);
    let pixels1 = read_pixels(rbm1);

    // ── Assert attachment 0 = red, attachment 1 = green. ────────
    let px = |buf: &[u8], x: usize, y: usize| -> [u8; 4] {
        let i = (y * W as usize + x) * 4;
        [buf[i], buf[i+1], buf[i+2], buf[i+3]]
    };
    let red   = [255u8, 0, 0, 255];
    let green = [0u8, 255, 0, 255];
    let c0 = px(&pixels0, 3, 3);
    let c1 = px(&pixels1, 3, 3);
    println!("attachment0 pixel(3,3) = {:?} (want red   {:?})", c0, red);
    println!("attachment1 pixel(3,3) = {:?} (want green {:?})", c1, green);
    // The FS writes Location 0 -> attachment 0 (red) and
    // Location 1 -> attachment 1 (green) in one invocation.
    // If MRT routing/scatter were broken, attachment 1 would
    // be the clear value (or a copy of attachment 0).
    let ok = c0 == red && c1 == green;

    // ── Cleanup. ────────────────────────────────────────────────
    unsafe {
        device.free_command_buffers(cp, &[cb]);
        device.destroy_command_pool(cp, None);
        device.destroy_buffer(rb, None);
        device.free_memory(rbm, None);
        device.destroy_buffer(rb1, None);
        device.free_memory(rbm1, None);
        device.destroy_pipeline(pipeline, None);
        device.destroy_pipeline_layout(pl, None);
        device.destroy_framebuffer(framebuffer, None);
        device.destroy_render_pass(render_pass, None);
        device.destroy_image_view(color_view, None);
        device.destroy_image(color_img, None);
        device.free_memory(imem, None);
        device.destroy_image_view(color_view1, None);
        device.destroy_image(color_img1, None);
        device.free_memory(imem1, None);
        device.destroy_buffer(vbuf, None);
        device.free_memory(vmem, None);
        device.destroy_shader_module(vs, None);
        device.destroy_shader_module(fs, None);
        device.destroy_device(None);
        instance.destroy_instance(None);
    }
    if ok {
        println!("PASS: MRT wrote red->attachment0, green->attachment1");
        0.into()
    } else {
        eprintln!("FAIL: MRT outputs didn't reach distinct attachments");
        1.into()
    }
}
