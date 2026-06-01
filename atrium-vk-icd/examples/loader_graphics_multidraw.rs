//! `examples/loader_graphics_multidraw` — proves the P1b.2
//! pass-level triangle batch routes each draw to its OWN shader +
//! state.  Two draws with DIFFERENT fragment shaders (red, green)
//! are recorded in ONE render pass; the daemon accumulates both
//! into a single batched dispatch and rasterizes them together,
//! each triangle's `draw_idx` selecting its draw's `fs_main` and
//! shared state.  If the routing were wrong, the two triangles
//! would shade with the wrong (or a single shared) colour.
//!
//! Layout (16x16 framebuffer, both draws in one pass):
//!   Draw 1: pipeline with RED   FS, left triangle  (x≈[2..8)).
//!   Draw 2: pipeline with GREEN FS, right triangle (x≈[8..14)).
//!
//! Expected:
//!   px(4,8)  inside left  triangle -> RED
//!   px(11,8) inside right triangle -> GREEN
//!   px(8,1)  above both            -> CLEAR (black)
//!
//! Exit 0 -> per-draw routing correct; non-0 -> see printed step.

use ash::vk;
use rspirv::binary::Assemble;
use rspirv::spirv::{
    AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode,
    ExecutionModel, FunctionControl, MemoryModel, StorageClass,
};

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
    b.decorate(out, Decoration::Location, vec![rspirv::dr::Operand::LiteralBit32(0)]);
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

unsafe fn find_memory_type(
    instance: &ash::Instance, physical: vk::PhysicalDevice,
    type_filter: u32, want: vk::MemoryPropertyFlags,
) -> u32 {
    let mp = instance.get_physical_device_memory_properties(physical);
    for i in 0..mp.memory_type_count {
        if (type_filter & (1u32 << i)) != 0
            && mp.memory_types[i as usize].property_flags.contains(want) {
            return i;
        }
    }
    panic!("no compatible memory type");
}

const W: u32 = 16;
const H: u32 = 16;

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
        .application_info(&app_info).flags(flags).enabled_extension_names(&exts);
    let instance = unsafe { entry.create_instance(&ic_info, None).expect("instance") };
    let pds = unsafe { instance.enumerate_physical_devices().expect("pds") };
    let pd = pds[0];
    let qp = vk::DeviceQueueCreateInfo::default().queue_family_index(0).queue_priorities(&[1.0]);
    let queue_infos = [qp];
    let dc_info = vk::DeviceCreateInfo::default().queue_create_infos(&queue_infos);
    let device = unsafe { instance.create_device(pd, &dc_info, None).expect("device") };
    let queue  = unsafe { device.get_device_queue(0, 0) };
    println!("instance / device / queue         -> OK");

    let vs_spv = build_passthrough_vs();
    let fs_red_spv   = build_constant_color_fs([1.0, 0.2, 0.2, 1.0]);
    let fs_green_spv = build_constant_color_fs([0.2, 1.0, 0.2, 1.0]);
    let to_words = |spv: &[u8]| -> Vec<u32> {
        spv.chunks_exact(4).map(|c| u32::from_le_bytes([c[0],c[1],c[2],c[3]])).collect()
    };
    let vs = unsafe { device.create_shader_module(
        &vk::ShaderModuleCreateInfo::default().code(&to_words(&vs_spv)), None).expect("vs") };
    let fs_red = unsafe { device.create_shader_module(
        &vk::ShaderModuleCreateInfo::default().code(&to_words(&fs_red_spv)), None).expect("fs_red") };
    let fs_green = unsafe { device.create_shader_module(
        &vk::ShaderModuleCreateInfo::default().code(&to_words(&fs_green_spv)), None).expect("fs_green") };
    println!("vkCreateShaderModule x3           -> OK");

    // Two vertex buffers: a left triangle and a right triangle.
    // NDC x in [-1,1] maps to pixel [0,16); the left triangle sits
    // in the left half, the right triangle in the right half.
    let make_vbuf = |verts: &[[f32; 3]; 3]| -> (vk::Buffer, vk::DeviceMemory) {
        let info = vk::BufferCreateInfo::default()
            .size(48).usage(vk::BufferUsageFlags::VERTEX_BUFFER)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buf = unsafe { device.create_buffer(&info, None).expect("vbuf") };
        let req = unsafe { device.get_buffer_memory_requirements(buf) };
        let mt = unsafe { find_memory_type(&instance, pd, req.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT) };
        let mem = unsafe { device.allocate_memory(
            &vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(mt),
            None).expect("vmem") };
        unsafe { device.bind_buffer_memory(buf, mem, 0).expect("bind"); }
        let map = unsafe { device.map_memory(mem, 0, req.size, vk::MemoryMapFlags::empty()).expect("map") };
        unsafe {
            let p = map as *mut f32;
            let mut i = 0;
            for v in verts { for f in v { p.add(i).write_unaligned(*f); i += 1; } }
            device.unmap_memory(mem);
        }
        (buf, mem)
    };
    // Left triangle: NDC x in [-0.75,-0.05], spans the left half.
    let (vbuf_l, vmem_l) = make_vbuf(&[
        [-0.75, -0.5, 0.0], [-0.05, -0.5, 0.0], [-0.4, 0.5, 0.0],
    ]);
    // Right triangle: NDC x in [0.05,0.75], spans the right half.
    let (vbuf_r, vmem_r) = make_vbuf(&[
        [0.05, -0.5, 0.0], [0.75, -0.5, 0.0], [0.4, 0.5, 0.0],
    ]);
    println!("two vertex buffers (L/R triangles) -> OK");

    // Render target.
    let img_info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D).format(vk::Format::R8G8B8A8_UNORM)
        .extent(vk::Extent3D { width: W, height: H, depth: 1 })
        .mip_levels(1).array_layers(1).samples(vk::SampleCountFlags::TYPE_1)
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
        &vk::ImageViewCreateInfo::default().image(color_img)
            .view_type(vk::ImageViewType::TYPE_2D).format(vk::Format::R8G8B8A8_UNORM)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0, level_count: 1, base_array_layer: 0, layer_count: 1,
            }), None).expect("view") };

    let color_att = vk::AttachmentDescription::default()
        .format(vk::Format::R8G8B8A8_UNORM).samples(vk::SampleCountFlags::TYPE_1)
        .load_op(vk::AttachmentLoadOp::CLEAR).store_op(vk::AttachmentStoreOp::STORE)
        .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
        .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .final_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL);
    let atts = [color_att];
    let color_ref = vk::AttachmentReference::default()
        .attachment(0).layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
    let color_refs = [color_ref];
    let subpass = vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS).color_attachments(&color_refs);
    let subpasses = [subpass];
    let render_pass = unsafe { device.create_render_pass(
        &vk::RenderPassCreateInfo::default().attachments(&atts).subpasses(&subpasses),
        None).expect("rp") };
    let fb_atts = [color_view];
    let framebuffer = unsafe { device.create_framebuffer(
        &vk::FramebufferCreateInfo::default().render_pass(render_pass)
            .attachments(&fb_atts).width(W).height(H).layers(1), None).expect("fb") };
    println!("render pass + framebuffer         -> OK");

    let bindings = [vk::VertexInputBindingDescription {
        binding: 0, stride: 12, input_rate: vk::VertexInputRate::VERTEX,
    }];
    let attrs = [vk::VertexInputAttributeDescription {
        location: 0, binding: 0, format: vk::Format::R32G32B32_SFLOAT, offset: 0,
    }];
    let vi = vk::PipelineVertexInputStateCreateInfo::default()
        .vertex_binding_descriptions(&bindings).vertex_attribute_descriptions(&attrs);
    let pl = unsafe { device.create_pipeline_layout(
        &vk::PipelineLayoutCreateInfo::default(), None).expect("pl") };
    let make_pipeline = |fs: vk::ShaderModule| -> vk::Pipeline {
        let stage_vs = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX).module(vs).name(c"main");
        let stage_fs = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT).module(fs).name(c"main");
        let stages = [stage_vs, stage_fs];
        let cp = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages).vertex_input_state(&vi).layout(pl)
            .render_pass(render_pass).subpass(0);
        let pipes = unsafe { device.create_graphics_pipelines(
            vk::PipelineCache::null(), &[cp], None).map_err(|(_, e)| e).expect("pipe") };
        pipes[0]
    };
    let pipe_red   = make_pipeline(fs_red);
    let pipe_green = make_pipeline(fs_green);
    println!("vkCreateGraphicsPipelines x2      -> OK");

    let rb_size: u64 = (W * H * 4) as u64;
    let rb = unsafe { device.create_buffer(&vk::BufferCreateInfo::default()
        .size(rb_size).usage(vk::BufferUsageFlags::TRANSFER_DST)
        .sharing_mode(vk::SharingMode::EXCLUSIVE), None).expect("rb") };
    let rbr = unsafe { device.get_buffer_memory_requirements(rb) };
    let rbmt = unsafe { find_memory_type(&instance, pd, rbr.memory_type_bits,
        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT) };
    let rbm = unsafe { device.allocate_memory(
        &vk::MemoryAllocateInfo::default().allocation_size(rbr.size).memory_type_index(rbmt),
        None).expect("rbm") };
    unsafe { device.bind_buffer_memory(rb, rbm, 0).expect("bind rb"); }

    let cp = unsafe { device.create_command_pool(
        &vk::CommandPoolCreateInfo::default().queue_family_index(0)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER), None).expect("cp") };
    let cbs = unsafe { device.allocate_command_buffers(
        &vk::CommandBufferAllocateInfo::default().command_pool(cp)
            .level(vk::CommandBufferLevel::PRIMARY).command_buffer_count(1)).expect("cbs") };
    let cb = cbs[0];

    let vp = vk::Viewport { x: 0.0, y: 0.0, width: W as f32, height: H as f32,
                            min_depth: 0.0, max_depth: 1.0 };
    let full_scissor = vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent: vk::Extent2D { width: W, height: H },
    };
    let clear = vk::ClearValue { color: vk::ClearColorValue { float32: [0.0, 0.0, 0.0, 1.0] } };
    unsafe {
        device.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default()).expect("begin");
        // ONE render pass, TWO draws with different pipelines.
        device.cmd_begin_render_pass(cb,
            &vk::RenderPassBeginInfo::default().render_pass(render_pass)
                .framebuffer(framebuffer).render_area(full_scissor).clear_values(&[clear]),
            vk::SubpassContents::INLINE);
        device.cmd_set_viewport(cb, 0, &[vp]);
        device.cmd_set_scissor(cb, 0, &[full_scissor]);
        // Draw 1: red, left triangle.
        device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipe_red);
        device.cmd_bind_vertex_buffers(cb, 0, &[vbuf_l], &[0]);
        device.cmd_draw(cb, 3, 1, 0, 0);
        // Draw 2: green, right triangle.
        device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipe_green);
        device.cmd_bind_vertex_buffers(cb, 0, &[vbuf_r], &[0]);
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
    let submit_cbs = [cb];
    let submit = vk::SubmitInfo::default().command_buffers(&submit_cbs);
    unsafe { device.queue_submit(queue, &[submit], vk::Fence::null()).expect("submit"); }
    unsafe { device.device_wait_idle().expect("wait"); }
    println!("1 pass / 2 draws (red L, green R) -> OK");

    let map = unsafe { device.map_memory(rbm, 0, rbr.size, vk::MemoryMapFlags::empty()).expect("map rb") };
    let range = vk::MappedMemoryRange::default().memory(rbm).offset(0).size(rbr.size);
    unsafe { device.invalidate_mapped_memory_ranges(&[range]).expect("inv"); }
    let mut pixels = vec![0u8; (W * H * 4) as usize];
    unsafe { std::ptr::copy_nonoverlapping(map as *const u8, pixels.as_mut_ptr(), pixels.len()); }
    unsafe { device.unmap_memory(rbm); }

    let px = |x: usize, y: usize| -> [u8; 4] {
        let i = (y * W as usize + x) * 4;
        [pixels[i], pixels[i+1], pixels[i+2], pixels[i+3]]
    };
    let red   = [255u8, 51, 51, 255];
    let green = [51u8, 255, 51, 255];
    let black = [0u8, 0, 0, 255];
    let left  = px(4, 8);    // inside left triangle  -> draw 1 RED
    let right = px(11, 8);   // inside right triangle -> draw 2 GREEN
    let top   = px(8, 1);    // above both            -> CLEAR
    println!("px(4,8)  left  triangle -> {:?} (want {:?} RED)",   left,  red);
    println!("px(11,8) right triangle -> {:?} (want {:?} GREEN)", right, green);
    println!("px(8,1)  above both     -> {:?} (want {:?} CLEAR)", top,   black);
    let ok = left == red && right == green && top == black;

    unsafe {
        device.free_command_buffers(cp, &[cb]);
        device.destroy_command_pool(cp, None);
        device.destroy_buffer(rb, None); device.free_memory(rbm, None);
        device.destroy_pipeline(pipe_red, None);
        device.destroy_pipeline(pipe_green, None);
        device.destroy_pipeline_layout(pl, None);
        device.destroy_framebuffer(framebuffer, None);
        device.destroy_render_pass(render_pass, None);
        device.destroy_image_view(color_view, None);
        device.destroy_image(color_img, None); device.free_memory(imem, None);
        device.destroy_buffer(vbuf_l, None); device.free_memory(vmem_l, None);
        device.destroy_buffer(vbuf_r, None); device.free_memory(vmem_r, None);
        device.destroy_shader_module(vs, None);
        device.destroy_shader_module(fs_red, None);
        device.destroy_shader_module(fs_green, None);
        device.destroy_device(None);
        instance.destroy_instance(None);
    }
    if ok {
        println!("PASS: two draws in one pass routed to their own shaders (draw_idx)");
        0.into()
    } else {
        eprintln!("FAIL: per-draw routing wrong (shaders/state mis-assigned in the batch)");
        1.into()
    }
}
