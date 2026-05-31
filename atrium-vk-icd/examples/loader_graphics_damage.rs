//! `examples/loader_graphics_damage` — proves the damage /
//! dirty-rect render path: a second render pass with
//! `loadOp = LOAD` PRESERVES the prior framebuffer contents
//! instead of clearing them, and a scissor confines the second
//! pass's draw to a sub-rect.  This is the in-app partial-update
//! primitive (and the per-window compositor damage primitive):
//! redraw only the damaged rect, leave everything else intact.
//!
//! Without the ICD's loadOp -> BEGIN_RP_FLAG_NO_CLEAR wiring, the
//! second pass would clear the whole image to black, so the
//! discriminating pixels below would come back black instead of
//! their preserved pass-1 colours.
//!
//! Geometry / colours (16x16 framebuffer):
//!   Pass 1: loadOp = CLEAR -> blue (0,0,255,255); draw red
//!           triangle (255,51,51,255) covering x∈[4..12),y∈[4..12).
//!   Pass 2: loadOp = LOAD (preserve); scissor = right half
//!           x∈[8..16); draw green triangle (51,255,51,255), same
//!           geometry.
//!
//! Expected result:
//!   px(5,8)  left-half triangle   -> RED   (pass-1, scissored out of pass-2, PRESERVED)
//!   px(9,8)  right-half triangle  -> GREEN (pass-2 drew it)
//!   px(2,2)  corner, no triangle  -> BLUE  (pass-1 clear, PRESERVED)
//!   px(14,2) right half, no tri   -> BLUE  (pass-1 clear, PRESERVED; pass-2 did NOT re-clear)
//!
//! The px(5,8)==RED and px(14,2)==BLUE assertions are the ones a
//! clear-path regression would fail (they'd be black/green).
//!
//! Exit code: 0 -> damage-preserve correct; non-0 -> see printed step.

use ash::vk;
use rspirv::binary::Assemble;
use rspirv::spirv::{
    AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode,
    ExecutionModel, FunctionControl, MemoryModel, StorageClass,
};

/// Vertex shader: reads `vec3` Location=0, writes `vec4(pos, 1.0)`.
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

const W: u32 = 16;
const H: u32 = 16;
// Pass-2 scissor: right half x∈[8..16), full height.
const SC_X: i32 = 8;
const SC_Y: i32 = 0;
const SC_W: u32 = 8;
const SC_H: u32 = 16;

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

    // ── Shader modules: one VS, two constant-colour FS. ─────────
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

    // ── Vertex buffer (shared by both passes). ──────────────────
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

    // ── Render target image. ────────────────────────────────────
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
    println!("color_img + view (16x16 R8G8B8A8) -> OK");

    // ── Two render passes: CLEAR (pass 1) and LOAD (pass 2). ────
    let make_render_pass = |load_op: vk::AttachmentLoadOp,
                            initial: vk::ImageLayout| -> vk::RenderPass {
        let color_att = vk::AttachmentDescription::default()
            .format(vk::Format::R8G8B8A8_UNORM)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(load_op)
            .store_op(vk::AttachmentStoreOp::STORE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(initial)
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
        unsafe { device.create_render_pass(
            &vk::RenderPassCreateInfo::default()
                .attachments(&atts)
                .subpasses(&subpasses),
            None).expect("render_pass") }
    };
    let rp_clear = make_render_pass(vk::AttachmentLoadOp::CLEAR, vk::ImageLayout::UNDEFINED);
    let rp_load  = make_render_pass(vk::AttachmentLoadOp::LOAD,
        vk::ImageLayout::TRANSFER_SRC_OPTIMAL);
    // One framebuffer; both passes are attachment-compatible.
    let fb_atts = [color_view];
    let framebuffer = unsafe { device.create_framebuffer(
        &vk::FramebufferCreateInfo::default()
            .render_pass(rp_clear)
            .attachments(&fb_atts)
            .width(W).height(H).layers(1),
        None).expect("fb") };
    println!("rp(CLEAR) + rp(LOAD) + Framebuffer -> OK");

    // ── Two pipelines: red (pass 1), green (pass 2). ────────────
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
    let pl = unsafe { device.create_pipeline_layout(
        &vk::PipelineLayoutCreateInfo::default(), None).expect("pl") };
    let make_pipeline = |fs: vk::ShaderModule, rp: vk::RenderPass| -> vk::Pipeline {
        let stage_vs = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX).module(vs).name(c"main");
        let stage_fs = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT).module(fs).name(c"main");
        let stages = [stage_vs, stage_fs];
        let cp_info = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vi)
            .layout(pl)
            .render_pass(rp)
            .subpass(0);
        let pipelines = unsafe { device.create_graphics_pipelines(
            vk::PipelineCache::null(), &[cp_info], None).map_err(|(_, e)| e).expect("pipelines") };
        pipelines[0]
    };
    let pipe_red   = make_pipeline(fs_red, rp_clear);
    let pipe_green = make_pipeline(fs_green, rp_load);
    println!("vkCreateGraphicsPipelines x2      -> OK");

    // ── Readback buffer. ────────────────────────────────────────
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
    let map = unsafe { device.map_memory(rbm, 0, rbr.size, vk::MemoryMapFlags::empty()).expect("map rb") };
    unsafe { std::ptr::write_bytes(map as *mut u8, 0xCD, rb_size as usize); }
    unsafe { device.unmap_memory(rbm); }
    println!("readback buffer + 0xCD seed       -> OK");

    // ── Command buffer: two render passes + copy. ───────────────
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

    let full_vp = vk::Viewport { x: 0.0, y: 0.0, width: W as f32, height: H as f32,
                                 min_depth: 0.0, max_depth: 1.0 };
    let full_scissor = vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent: vk::Extent2D { width: W, height: H },
    };
    let damage_scissor = vk::Rect2D {
        offset: vk::Offset2D { x: SC_X, y: SC_Y },
        extent: vk::Extent2D { width: SC_W, height: SC_H },
    };
    let render_area = full_scissor;

    unsafe { device.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default()).expect("begin"); }
    // PASS 1: clear blue, draw red triangle, full scissor.
    let blue = vk::ClearValue { color: vk::ClearColorValue { float32: [0.0, 0.0, 1.0, 1.0] } };
    unsafe {
        device.cmd_begin_render_pass(cb,
            &vk::RenderPassBeginInfo::default()
                .render_pass(rp_clear)
                .framebuffer(framebuffer)
                .render_area(render_area)
                .clear_values(&[blue]),
            vk::SubpassContents::INLINE);
        device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipe_red);
        device.cmd_set_viewport(cb, 0, &[full_vp]);
        device.cmd_set_scissor(cb, 0, &[full_scissor]);
        device.cmd_bind_vertex_buffers(cb, 0, &[vbuf], &[0]);
        device.cmd_draw(cb, 3, 1, 0, 0);
        device.cmd_end_render_pass(cb);
    }
    // PASS 2: loadOp=LOAD (preserve), right-half scissor, draw green.
    // No clear value is used on the LOAD path, but the API still
    // wants a slot for attachment 0; pass a dummy.
    let dummy = vk::ClearValue { color: vk::ClearColorValue { float32: [0.0, 0.0, 0.0, 0.0] } };
    unsafe {
        device.cmd_begin_render_pass(cb,
            &vk::RenderPassBeginInfo::default()
                .render_pass(rp_load)
                .framebuffer(framebuffer)
                .render_area(render_area)
                .clear_values(&[dummy]),
            vk::SubpassContents::INLINE);
        device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipe_green);
        device.cmd_set_viewport(cb, 0, &[full_vp]);
        device.cmd_set_scissor(cb, 0, &[damage_scissor]);
        device.cmd_bind_vertex_buffers(cb, 0, &[vbuf], &[0]);
        device.cmd_draw(cb, 3, 1, 0, 0);
        device.cmd_end_render_pass(cb);
        // Copy color_img -> readback.
        let region = vk::BufferImageCopy::default()
            .buffer_offset(0).buffer_row_length(0).buffer_image_height(0)
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
    let submit_cbs = [cb];
    let submit = vk::SubmitInfo::default().command_buffers(&submit_cbs);
    unsafe { device.queue_submit(queue, &[submit], vk::Fence::null()).expect("submit"); }
    unsafe { device.device_wait_idle().expect("wait_idle"); }
    println!("pass1(CLEAR+red) + pass2(LOAD+green) -> OK");

    // ── Readback. ───────────────────────────────────────────────
    let map = unsafe { device.map_memory(rbm, 0, rbr.size, vk::MemoryMapFlags::empty()).expect("map rb 2") };
    let range = vk::MappedMemoryRange::default().memory(rbm).offset(0).size(rbr.size);
    unsafe { device.invalidate_mapped_memory_ranges(&[range]).expect("invalidate"); }
    let mut pixels = vec![0u8; (W * H * 4) as usize];
    unsafe { std::ptr::copy_nonoverlapping(map as *const u8, pixels.as_mut_ptr(), pixels.len()); }
    unsafe { device.unmap_memory(rbm); }

    let px = |x: usize, y: usize| -> [u8; 4] {
        let i = (y * W as usize + x) * 4;
        [pixels[i], pixels[i+1], pixels[i+2], pixels[i+3]]
    };
    let red   = [255u8, 51, 51, 255];
    let green = [51u8, 255, 51, 255];
    let blue  = [0u8, 0, 255, 255];

    // Triangle verts map to pixels (4,4),(12,4),(8,12); at y=8 it
    // spans x∈[6,10].  px(7,8) is inside the triangle AND left of
    // the pass-2 scissor (x<8), so it must keep pass-1's red.
    let left_tri   = px(7, 8);   // pass-1 red, scissored out of pass-2 -> PRESERVED red
    let right_tri  = px(9, 8);   // pass-2 green
    let corner     = px(2, 2);   // pass-1 clear blue, untouched -> PRESERVED blue
    let right_bg   = px(14, 2);  // right half but no triangle; pass-2 LOAD -> PRESERVED blue
    println!("px(7,8)  left-half triangle   -> {:?} (want {:?} RED, preserved)",   left_tri,  red);
    println!("px(9,8)  right-half triangle  -> {:?} (want {:?} GREEN)",            right_tri, green);
    println!("px(2,2)  corner (clear)       -> {:?} (want {:?} BLUE, preserved)",  corner,    blue);
    println!("px(14,2) right-half bg        -> {:?} (want {:?} BLUE, preserved)",  right_bg,  blue);
    let ok = left_tri == red
          && right_tri == green
          && corner == blue
          && right_bg == blue;

    unsafe {
        device.free_command_buffers(cp, &[cb]);
        device.destroy_command_pool(cp, None);
        device.destroy_buffer(rb, None);
        device.free_memory(rbm, None);
        device.destroy_pipeline(pipe_red, None);
        device.destroy_pipeline(pipe_green, None);
        device.destroy_pipeline_layout(pl, None);
        device.destroy_framebuffer(framebuffer, None);
        device.destroy_render_pass(rp_clear, None);
        device.destroy_render_pass(rp_load, None);
        device.destroy_image_view(color_view, None);
        device.destroy_image(color_img, None);
        device.free_memory(imem, None);
        device.destroy_buffer(vbuf, None);
        device.free_memory(vmem, None);
        device.destroy_shader_module(vs, None);
        device.destroy_shader_module(fs_red, None);
        device.destroy_shader_module(fs_green, None);
        device.destroy_device(None);
        instance.destroy_instance(None);
    }
    if ok {
        println!("PASS: loadOp=LOAD preserved pass-1 pixels; scissor confined the pass-2 damage");
        0.into()
    } else {
        eprintln!("FAIL: damage-preserve incorrect (a clear-path regression blacks out preserved pixels)");
        1.into()
    }
}
