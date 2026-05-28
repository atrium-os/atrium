//! `examples/loader_graphics_ubo` — uniform-buffer (UBO)
//! through the Khronos loader.  FS reads its output colour
//! from a host-supplied `Block { vec4 color; }` uniform
//! buffer instead of inline SPIR-V constants or push
//! constants.  Real apps use this for per-frame data
//! (view/projection matrices, time, light state) -- push
//! constants are limited to 128 bytes spec-min, UBOs are
//! 64 KiB+ on most HW.
//!
//! Lays an `(R=0.0, G=0.5, B=0.75, A=1.0)` colour into a 16-
//! byte HOST_VISIBLE buffer, writes it through the
//! COMBINED_IMAGE_SAMPLER-style descriptor path with type
//! `VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER` at binding 0.
//! Daemon's dispatch_draw copies the buffer bytes into the
//! uniforms scratch; the FS resolves
//! `Block.color` through `OpAccessChain` at offset 0 +
//! `OpLoad %vec4` and writes the value to its output.
//! Pixel(3,3) should land at the rasterizer's
//! `f32 * 255.0 + 0.5 as u8` quantisation of those f32s ->
//! (0, 128, 191, 255).

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

/// FS: reads `Block { vec4 color; }` from a UBO at
/// `(set=0, binding=0)`, writes color to output.
fn build_ubo_color_fs() -> Vec<u8> {
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let f32_ty = b.type_float(32, None);
    let vec4   = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);

    // Block { vec4 color; }   --   StorageClass::Uniform
    let block_ty = b.type_struct(vec![vec4]);
    b.decorate(block_ty, Decoration::Block, vec![]);
    b.member_decorate(block_ty, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_block = b.type_pointer(None, StorageClass::Uniform, block_ty);
    let ptr_ubo_vec4 = b.type_pointer(None, StorageClass::Uniform, vec4);
    let ubo = b.variable(ptr_block, None, StorageClass::Uniform, None);
    b.decorate(ubo, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ubo, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let ptr_out = b.type_pointer(None, StorageClass::Output, vec4);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let c_zero = b.constant_bit32(u32_ty, 0);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let src = b.access_chain(ptr_ubo_vec4, None, ubo, vec![c_zero]).unwrap();
    let color = b.load(vec4, None, src, None, vec![]).unwrap();
    b.store(out, color, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    // Entry-point interface for SPIR-V 1.0/1.3 lists only
    // Input + Output OpVariables.  Uniform-storage `ubo`
    // is NOT in the interface even though it's referenced.
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

    let vs_spv = build_passthrough_vs();
    let fs_spv = build_ubo_color_fs();
    let to_words = |spv: &[u8]| -> Vec<u32> {
        spv.chunks_exact(4).map(|c| u32::from_le_bytes([c[0],c[1],c[2],c[3]])).collect()
    };
    let vs = unsafe { device.create_shader_module(
        &vk::ShaderModuleCreateInfo::default().code(&to_words(&vs_spv)), None).expect("vs") };
    let fs = unsafe { device.create_shader_module(
        &vk::ShaderModuleCreateInfo::default().code(&to_words(&fs_spv)), None).expect("fs") };

    // Vertex buffer.
    let vbuf = unsafe { device.create_buffer(
        &vk::BufferCreateInfo::default()
            .size(48)
            .usage(vk::BufferUsageFlags::VERTEX_BUFFER)
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
        let verts: [[f32; 3]; 3] = [
            [-0.5, -0.5, 0.0],
            [ 0.5, -0.5, 0.0],
            [ 0.0,  0.5, 0.0],
        ];
        let mut i = 0;
        for v in verts { for f in v { p.add(i).write_unaligned(f); i += 1; } }
    }
    unsafe { device.unmap_memory(vmem); }

    // ── UBO: 16-byte HOST_VISIBLE buffer with vec4 color. ──
    let ubo = unsafe { device.create_buffer(
        &vk::BufferCreateInfo::default()
            .size(16)
            .usage(vk::BufferUsageFlags::UNIFORM_BUFFER)
            .sharing_mode(vk::SharingMode::EXCLUSIVE),
        None).expect("ubo") };
    let ureq = unsafe { device.get_buffer_memory_requirements(ubo) };
    let umt = unsafe { find_memory_type(&instance, pd, ureq.memory_type_bits,
        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT) };
    let umem = unsafe { device.allocate_memory(
        &vk::MemoryAllocateInfo::default().allocation_size(ureq.size).memory_type_index(umt),
        None).expect("umem") };
    unsafe { device.bind_buffer_memory(ubo, umem, 0).expect("bind ubo"); }
    let um = unsafe { device.map_memory(umem, 0, ureq.size, vk::MemoryMapFlags::empty()).expect("map ubo") };
    unsafe {
        let p = um as *mut f32;
        // Match Rung I's push-constant colour so the
        // pixel-match assertion is the same value:
        // (0, 128, 191, 255).
        let color: [f32; 4] = [0.0, 0.5, 0.75, 1.0];
        for (k, c) in color.iter().enumerate() {
            p.add(k).write_unaligned(*c);
        }
    }
    unsafe { device.unmap_memory(umem); }

    // Descriptor set: 1 UBO at binding 0.
    let dsl_bindings = [vk::DescriptorSetLayoutBinding::default()
        .binding(0)
        .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
        .descriptor_count(1)
        .stage_flags(vk::ShaderStageFlags::FRAGMENT)];
    let dsl = unsafe { device.create_descriptor_set_layout(
        &vk::DescriptorSetLayoutCreateInfo::default().bindings(&dsl_bindings),
        None).expect("dsl") };
    let pool_size = vk::DescriptorPoolSize::default()
        .ty(vk::DescriptorType::UNIFORM_BUFFER)
        .descriptor_count(1);
    let pool_sizes = [pool_size];
    let pool = unsafe { device.create_descriptor_pool(
        &vk::DescriptorPoolCreateInfo::default().max_sets(1).pool_sizes(&pool_sizes),
        None).expect("pool") };
    let dsl_array = [dsl];
    let dset = unsafe { device.allocate_descriptor_sets(
        &vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(pool).set_layouts(&dsl_array))
        .expect("dset")[0] };
    let buf_info = [vk::DescriptorBufferInfo::default()
        .buffer(ubo).offset(0).range(16)];
    let write = vk::WriteDescriptorSet::default()
        .dst_set(dset).dst_binding(0)
        .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
        .buffer_info(&buf_info);
    unsafe { device.update_descriptor_sets(&[write], &[]); }

    // Render target.
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

    let pl = unsafe { device.create_pipeline_layout(
        &vk::PipelineLayoutCreateInfo::default().set_layouts(&dsl_array),
        None).expect("pl") };

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

    let i = (3 * W as usize + 3) * 4;
    let got = [pixels[i], pixels[i+1], pixels[i+2], pixels[i+3]];
    let want = [0u8, 128, 191, 255];
    println!("pixel(3,3) = {:?} (want {:?})", got, want);
    let ok = got == want;

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
        device.destroy_descriptor_pool(pool, None);
        device.destroy_descriptor_set_layout(dsl, None);
        device.destroy_buffer(ubo, None);
        device.free_memory(umem, None);
        device.destroy_buffer(vbuf, None);
        device.free_memory(vmem, None);
        device.destroy_shader_module(vs, None);
        device.destroy_shader_module(fs, None);
        device.destroy_device(None);
        instance.destroy_instance(None);
    }
    if ok {
        println!("PASS: FS UBO read returned the host-supplied colour");
        0.into()
    } else {
        eprintln!("FAIL: pixel(3,3) doesn't match the UBO colour");
        1.into()
    }
}
