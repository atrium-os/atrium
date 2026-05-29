//! `examples/loader_graphics_stencil` — two-pass stencil
//! masking through the Khronos loader.
//!
//! Pass 1: small NDC triangle around the centre, FS paints
//! red, stencil REPLACE on ALWAYS with reference=1.  The
//! red is written + every covered pixel gets stencil=1.
//!
//! Pass 2: fullscreen NDC triangle covering the whole 8x8
//! framebuffer, FS paints blue, stencil compare EQUAL
//! reference=1 with KEEP everywhere.  Blue only writes
//! where stencil==1 (= where pass 1 painted).
//!
//! Expected:
//!   pixel(4, 4) inside small tri  -> blue (255 -> 0 quant'd)
//!   pixel(0, 0) outside small tri -> clear (the framebuffer
//!                                    clear value, [0,0,0,0])
//!
//! Without the stencil fix, pass 2 would paint blue over
//! every pixel and pixel(0,0) would also be blue.
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

    // ── Shader modules.  Three FS: red for pass 1, blue
    // for pass 2 (sentinel colour for the stencil mask),
    // and one shared passthrough VS.
    let vs_spv = build_passthrough_vs();
    let fs_red_spv  = build_constant_color_fs([1.0, 0.0, 0.0, 1.0]); // pass 1
    let fs_blue_spv = build_constant_color_fs([0.0, 0.0, 1.0, 1.0]); // pass 2
    let vs_words: Vec<u32> = vs_spv.chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0],c[1],c[2],c[3]])).collect();
    let fs_r_words: Vec<u32> = fs_red_spv.chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0],c[1],c[2],c[3]])).collect();
    let fs_b_words: Vec<u32> = fs_blue_spv.chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0],c[1],c[2],c[3]])).collect();
    let vs     = unsafe { device.create_shader_module(
        &vk::ShaderModuleCreateInfo::default().code(&vs_words),  None).expect("vs") };
    let fs_red = unsafe { device.create_shader_module(
        &vk::ShaderModuleCreateInfo::default().code(&fs_r_words), None).expect("fs_red") };
    let fs_blue = unsafe { device.create_shader_module(
        &vk::ShaderModuleCreateInfo::default().code(&fs_b_words), None).expect("fs_blue") };
    println!("vkCreateShaderModule x3 (vs + 2 FS) -> OK");

    // ── Vertex buffer.  6 vec3 verts = 72 B (round to 96).
    // Layout:
    //   verts[0..3]  = pass-1 small centre triangle
    //   verts[3..6]  = pass-2 fullscreen NDC triangle
    let vbuf_info = vk::BufferCreateInfo::default()
        .size(96)
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
        let verts: [[f32; 3]; 6] = [
            // Pass 1: medium centred triangle (the round-
            // trip rung geometry).  Pixel(3, 3) sits
            // solidly inside.
            [-0.5, -0.5, 0.0],
            [ 0.5, -0.5, 0.0],
            [ 0.0,  0.5, 0.0],
            // Pass 2: oversized triangle that covers the
            // whole NDC quad in [-1,1]^2 (fullscreen).
            [-1.0, -1.0, 0.0],
            [ 3.0, -1.0, 0.0],
            [-1.0,  3.0, 0.0],
        ];
        let mut i = 0;
        for v in verts { for f in v { p.add(i).write_unaligned(f); i += 1; } }
    }
    unsafe { device.unmap_memory(vmem); }
    println!("vbuf write 3 NDC verts            -> OK");

    // ── Render target image (8x8 RGBA8, COLOR_ATTACHMENT + TRANSFER_SRC). ──
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
    let color_img = unsafe { device.create_image(&img_info, None).expect("color_img") };
    let ireq = unsafe { device.get_image_memory_requirements(color_img) };
    let imt  = unsafe { find_memory_type(&instance, pd, ireq.memory_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL) };
    let imem = unsafe { device.allocate_memory(
        &vk::MemoryAllocateInfo::default().allocation_size(ireq.size).memory_type_index(imt),
        None).expect("imem") };
    unsafe { device.bind_image_memory(color_img, imem, 0).expect("bind img"); }
    let color_view = unsafe { device.create_image_view(
        &vk::ImageViewCreateInfo::default()
            .image(color_img)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(vk::Format::R8G8B8A8_UNORM)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0, level_count: 1,
                base_array_layer: 0, layer_count: 1,
            }),
        None).expect("view") };
    println!("color_img + view (8x8 R8G8B8A8)   -> OK");

    // ── Render pass + framebuffer. ──────────────────────────────
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
        .attachment(0)
        .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
    let color_refs = [color_ref];
    let subpass = vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
        .color_attachments(&color_refs);
    let subpasses = [subpass];
    let render_pass = unsafe { device.create_render_pass(
        &vk::RenderPassCreateInfo::default()
            .attachments(&atts)
            .subpasses(&subpasses),
        None).expect("render_pass") };
    let fb_atts = [color_view];
    let framebuffer = unsafe { device.create_framebuffer(
        &vk::FramebufferCreateInfo::default()
            .render_pass(render_pass)
            .attachments(&fb_atts)
            .width(W).height(H).layers(1),
        None).expect("fb") };
    println!("vkCreateRenderPass + Framebuffer  -> OK");

    // ── Graphics pipelines.  Two of them differing only in
    // FS module + depth-stencil state.
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

    // Pass 1: write stencil = 1 everywhere this triangle covers.
    let write_face = vk::StencilOpState {
        fail_op: vk::StencilOp::KEEP,
        pass_op: vk::StencilOp::REPLACE,
        depth_fail_op: vk::StencilOp::KEEP,
        compare_op: vk::CompareOp::ALWAYS,
        compare_mask: 0xff,
        write_mask: 0xff,
        reference: 1,
    };
    let ds_write = vk::PipelineDepthStencilStateCreateInfo::default()
        .stencil_test_enable(true)
        .front(write_face)
        .back(write_face);

    // Pass 2: only paint where stencil == 1; don't modify it.
    let read_face = vk::StencilOpState {
        fail_op: vk::StencilOp::KEEP,
        pass_op: vk::StencilOp::KEEP,
        depth_fail_op: vk::StencilOp::KEEP,
        compare_op: vk::CompareOp::EQUAL,
        compare_mask: 0xff,
        write_mask: 0x00,
        reference: 1,
    };
    let ds_read = vk::PipelineDepthStencilStateCreateInfo::default()
        .stencil_test_enable(true)
        .front(read_face)
        .back(read_face);

    use ash::vk::Handle;
    let make_pipeline = |fs_mod: vk::ShaderModule,
                         ds: &vk::PipelineDepthStencilStateCreateInfo<'_>| -> u64 {
        let stage_vs = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX).module(vs).name(c"main");
        let stage_fs = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT).module(fs_mod).name(c"main");
        let stages = [stage_vs, stage_fs];
        let cp_info = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages).vertex_input_state(&vi)
            .depth_stencil_state(ds)
            .layout(pl).render_pass(render_pass).subpass(0);
        let ps = unsafe { device.create_graphics_pipelines(
            vk::PipelineCache::null(), &[cp_info], None)
            .map_err(|(_,e)|e).expect("pipelines") };
        ps[0].as_raw()
    };
    let pipeline_write: u64 = make_pipeline(fs_red,  &ds_write);
    let pipeline_read:  u64 = make_pipeline(fs_blue, &ds_read);
    println!("vkCreateGraphicsPipelines x2     -> OK");

    // ── Readback buffer (HOST_VISIBLE, TRANSFER_DST). ───────────
    let rb_size: u64 = (W * H * 4) as u64;
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
    // Seed the readback buffer with a sentinel so we can prove the
    // copy actually wrote it (vs the seed sticking through).
    let map = unsafe { device.map_memory(rbm, 0, rbr.size, vk::MemoryMapFlags::empty()).expect("map rb") };
    unsafe {
        std::ptr::write_bytes(map as *mut u8, 0xCD, rb_size as usize);
    }
    unsafe { device.unmap_memory(rbm); }
    println!("readback buffer (256B) + 0xCD seed -> OK");

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
    let clears = [clear];
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
        let vp = vk::Viewport { x: 0.0, y: 0.0, width: W as f32, height: H as f32,
                                min_depth: 0.0, max_depth: 1.0 };
        device.cmd_set_viewport(cb, 0, &[vp]);
        let scissor = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D { width: W, height: H },
        };
        device.cmd_set_scissor(cb, 0, &[scissor]);
        device.cmd_bind_vertex_buffers(cb, 0, &[vbuf], &[0]);

        // Pass 1: small red triangle writes stencil = 1.
        device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS,
            vk::Pipeline::from_raw(pipeline_write));
        device.cmd_draw(cb, 3, 1, 0, 0); // verts[0..3]

        // Pass 2: fullscreen blue triangle, but stencil EQUAL=1
        // means it only paints where pass 1 covered.
        device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS,
            vk::Pipeline::from_raw(pipeline_read));
        device.cmd_draw(cb, 3, 1, 3, 0); // firstVertex=3 -> verts[3..6]
        device.cmd_end_render_pass(cb);
        // Copy color_img -> readback buffer (full image, tight pack).
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
        device.end_command_buffer(cb).expect("end");
    }
    let cbs_to_submit = [cb];
    let submit = vk::SubmitInfo::default().command_buffers(&cbs_to_submit);
    unsafe { device.queue_submit(queue, &[submit], vk::Fence::null()).expect("submit"); }
    unsafe { device.device_wait_idle().expect("wait_idle"); }
    println!("render + copy + WaitIdle          -> OK");

    // ── Pull readback through OP_GPU_BUFFER_READ. ───────────────
    let map = unsafe { device.map_memory(rbm, 0, rbr.size, vk::MemoryMapFlags::empty()).expect("map rb 2") };
    let range = vk::MappedMemoryRange::default()
        .memory(rbm).offset(0).size(rbr.size);
    unsafe { device.invalidate_mapped_memory_ranges(&[range]).expect("invalidate"); }
    let mut pixels = vec![0u8; (W * H * 4) as usize];
    unsafe { std::ptr::copy_nonoverlapping(
        map as *const u8, pixels.as_mut_ptr(), pixels.len()); }
    unsafe { device.unmap_memory(rbm); }

    // ── Assert the triangle interior is the FS colour. ──────────
    // FS writes (1.0, 0.2, 0.2, 1.0) -> u8 quantised
    // (255, 51, 51, 255).
    let px = |x: usize, y: usize| -> [u8; 4] {
        let i = (y * W as usize + x) * 4;
        [pixels[i], pixels[i+1], pixels[i+2], pixels[i+3]]
    };
    // Pass-2 blue = (0.0, 0.0, 1.0, 1.0) -> u8 (0, 0, 255, 255).
    let blue  = [0u8, 0, 255, 255];
    let clear = [0u8, 0, 0, 0];
    // pixel(3, 3) sits solidly inside the pass-1 triangle
    // region in screen coords (verts at (2,2), (6,2), (4,6))
    // so stencil = 1 -> pass 2's blue paints it.
    let inside = px(3, 3);
    // pixel(0, 0) is outside the pass-1 region (stencil = 0)
    // so pass 2 was rejected and the framebuffer stays at
    // the clear value.
    let outside = px(0, 0);
    println!("pixel(3,3) inside  stencil mask -> {:?} (want blue {:?})", inside,  blue);
    println!("pixel(0,0) outside stencil mask -> {:?} (want clear {:?})", outside, clear);
    let ok = inside == blue && outside == clear;

    // ── Cleanup. ────────────────────────────────────────────────
    unsafe {
        device.free_command_buffers(cp, &[cb]);
        device.destroy_command_pool(cp, None);
        device.destroy_buffer(rb, None);
        device.free_memory(rbm, None);
        device.destroy_pipeline(vk::Pipeline::from_raw(pipeline_write), None);
        device.destroy_pipeline(vk::Pipeline::from_raw(pipeline_read), None);
        device.destroy_pipeline_layout(pl, None);
        device.destroy_framebuffer(framebuffer, None);
        device.destroy_render_pass(render_pass, None);
        device.destroy_image_view(color_view, None);
        device.destroy_image(color_img, None);
        device.free_memory(imem, None);
        device.destroy_buffer(vbuf, None);
        device.free_memory(vmem, None);
        device.destroy_shader_module(vs, None);
        device.destroy_shader_module(fs_red, None);
        device.destroy_shader_module(fs_blue, None);
        device.destroy_device(None);
        instance.destroy_instance(None);
    }
    if ok {
        println!("PASS: stencil mask gated pass-2 blue to the pass-1 region");
        0.into()
    } else {
        eprintln!("FAIL: stencil mask didn't gate pass-2 (blue painted outside pass-1)");
        1.into()
    }
}
