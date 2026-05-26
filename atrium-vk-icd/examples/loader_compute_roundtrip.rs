//! `examples/loader_compute_roundtrip` — end-to-end test that a
//! real Vulkan app dispatches a compute shader through the Khronos
//! loader, the shader actually runs in the tier-2 runtime, and the
//! daemon-side output reaches the client's mapped pointer via
//! `vkInvalidateMappedMemoryRanges` (Arc 136's OP_GPU_BUFFER_READ).
//!
//! Companion to `examples/loader_smoke` (which only exercises the
//! shader-upload path).  Where loader_smoke verifies "the
//! Tier2Registry compiled my SPIR-V into an .afblob", this example
//! verifies "the Tier2Backend ran my SPIR-V and I can read its
//! output back from the client".
//!
//! # The shader
//!
//! ```glsl
//! #version 450
//! layout(set=0, binding=0) buffer SSBO { uint data[]; };
//! layout(local_size_x = 1) in;
//! void main() {
//!     data[0] = 42u;
//! }
//! ```
//!
//! Built inline via `rspirv` to avoid an external `glslangValidator`
//! dependency.
//!
//! # The Vulkan call sequence
//!
//! 1. Create instance + pick device + create logical device.
//! 2. Create a 4-byte HOST_VISIBLE buffer (the SSBO).
//! 3. Allocate matching memory, bind, map.
//! 4. Create descriptor set layout / pool / set; update slot 0 to
//!    point at the buffer.
//! 5. Upload the compute SPIR-V (this triggers the Tier2Registry
//!    compile path that loader_smoke already exercises).
//! 6. Create compute pipeline.
//! 7. Record + submit a dispatch(1,1,1).
//! 8. vkDeviceWaitIdle.
//! 9. vkInvalidateMappedMemoryRanges -- pulls daemon-side buffer
//!    state back into our mapped pointer.
//! 10. Read u32 at offset 0 and assert == 42.
//!
//! # Usage
//!
//! ```sh
//! aqueduct-gpu-host --socket /tmp/x.sock \
//!     --backend tier2 --tier2 \
//!     --cache-root /tmp/cache \
//!     --compile-binary /path/to/atrium-spv-compile &
//!
//! VK_DRIVER_FILES=/path/to/atrium_icd.json \
//! ATRIUM_VK_ICD_SOCKET=/tmp/x.sock \
//!     cargo run --example loader_compute_roundtrip
//! ```
//!
//! Exit code:
//!   0 -> the shader wrote 42, vkInvalidateMappedMemoryRanges pulled it back.
//!   non-0 -> see the printed step that failed.

use ash::vk;
use rspirv::binary::Assemble;
use rspirv::spirv::{
    AddressingModel, Capability, Decoration, ExecutionMode, ExecutionModel,
    FunctionControl, MemoryModel, StorageClass,
};

/// Build SPIR-V for: `layout(binding=0) buffer SSBO { uint data[]; };
/// void main() { data[0] = 42u; }`.
fn build_write_42_spirv() -> Vec<u8> {
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let void_fn = b.type_function(void, vec![]);
    let rt_arr = b.type_runtime_array(u32_ty);
    b.decorate(rt_arr, Decoration::ArrayStride,
        vec![rspirv::dr::Operand::LiteralBit32(4)]);
    let s = b.type_struct(vec![rt_arr]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_u = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let ssbo  = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let c_zero  = b.constant_bit32(u32_ty, 0);
    let c_value = b.constant_bit32(u32_ty, 42);

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let dst = b.access_chain(ptr_u, None, ssbo, vec![c_zero, c_zero]).unwrap();
    b.store(dst, c_value, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();

    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ssbo]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);

    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

/// Find a memory type that supports the desired property flags.
/// Returns the type index, or panics if none found.
unsafe fn find_memory_type(
    instance: &ash::Instance,
    physical: vk::PhysicalDevice,
    type_filter: u32,
    want: vk::MemoryPropertyFlags,
) -> u32 {
    let mp = instance.get_physical_device_memory_properties(physical);
    for i in 0..mp.memory_type_count {
        let suitable = (type_filter & (1u32 << i)) != 0;
        let has_props = mp.memory_types[i as usize].property_flags.contains(want);
        if suitable && has_props {
            return i;
        }
    }
    panic!("no compatible memory type for filter={type_filter:#b} props={want:?}");
}

fn main() -> std::process::ExitCode {
    let entry = unsafe {
        match ash::Entry::load() {
            Ok(e)  => { println!("ash::Entry::load                  -> OK"); e }
            Err(e) => { eprintln!("ash::Entry::load                  -> ERROR: {e}"); return 1.into(); }
        }
    };

    let app_info = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3);
    let flags = vk::InstanceCreateFlags::ENUMERATE_PORTABILITY_KHR;
    let exts = [vk::KHR_PORTABILITY_ENUMERATION_NAME.as_ptr()];
    let ic_info = vk::InstanceCreateInfo::default()
        .application_info(&app_info)
        .flags(flags)
        .enabled_extension_names(&exts);

    let instance = unsafe {
        match entry.create_instance(&ic_info, None) {
            Ok(i)  => { println!("vkCreateInstance                  -> OK"); i }
            Err(e) => { eprintln!("vkCreateInstance                  -> ERROR: {e:?}"); return 1.into(); }
        }
    };

    let pds = unsafe {
        match instance.enumerate_physical_devices() {
            Ok(v) if !v.is_empty() => v,
            Ok(_)  => { eprintln!("vkEnumeratePhysicalDevices: 0 devices (daemon up?)"); return 1.into(); }
            Err(e) => { eprintln!("vkEnumeratePhysicalDevices        -> ERROR: {e:?}"); return 1.into(); }
        }
    };
    let pd = pds[0];
    println!("vkEnumeratePhysicalDevices        -> OK, picked device[0]");

    // ── Device ─────────────────────────────────────────────────
    let qp = vk::DeviceQueueCreateInfo::default()
        .queue_family_index(0)
        .queue_priorities(&[1.0]);
    let queue_infos = [qp];
    let dc_info = vk::DeviceCreateInfo::default().queue_create_infos(&queue_infos);
    let device = unsafe { instance.create_device(pd, &dc_info, None).expect("create_device") };
    let queue  = unsafe { device.get_device_queue(0, 0) };
    println!("vkCreateDevice / GetDeviceQueue   -> OK");

    // ── Buffer + Memory (HOST_VISIBLE + HOST_COHERENT) ────────
    const N: usize = 4;
    let buf_info = vk::BufferCreateInfo::default()
        .size(N as u64)
        .usage(vk::BufferUsageFlags::STORAGE_BUFFER)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let buffer = unsafe { device.create_buffer(&buf_info, None).expect("create_buffer") };
    let req = unsafe { device.get_buffer_memory_requirements(buffer) };
    let mem_ty = unsafe { find_memory_type(
        &instance, pd, req.memory_type_bits,
        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
    ) };
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(req.size)
        .memory_type_index(mem_ty);
    let mem = unsafe { device.allocate_memory(&alloc_info, None).expect("allocate_memory") };
    unsafe { device.bind_buffer_memory(buffer, mem, 0).expect("bind_buffer_memory"); }
    println!("vkCreate/Bind Buffer + Memory     -> OK (size={N})");

    // Seed the buffer with 0xDEADBEEF so we can prove the shader's
    // write overwrote it rather than the buffer happening to be
    // zero-initialised.
    let mapped = unsafe {
        device.map_memory(mem, 0, req.size, vk::MemoryMapFlags::empty())
            .expect("map_memory")
    };
    unsafe {
        std::ptr::write_unaligned(mapped as *mut u32, 0xDEAD_BEEF);
    }
    unsafe { device.unmap_memory(mem); }
    println!("seed buffer with 0xDEADBEEF       -> OK (vkUnmapMemory flushes host->daemon)");

    // ── Descriptor set layout / pool / set ────────────────────
    let dsl_binding = vk::DescriptorSetLayoutBinding::default()
        .binding(0)
        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
        .descriptor_count(1)
        .stage_flags(vk::ShaderStageFlags::COMPUTE);
    let dsl_bindings = [dsl_binding];
    let dsl_info = vk::DescriptorSetLayoutCreateInfo::default()
        .bindings(&dsl_bindings);
    let dsl = unsafe { device.create_descriptor_set_layout(&dsl_info, None).expect("DSL") };

    let pool_size = vk::DescriptorPoolSize::default()
        .ty(vk::DescriptorType::STORAGE_BUFFER)
        .descriptor_count(1);
    let pool_sizes = [pool_size];
    let pool_info = vk::DescriptorPoolCreateInfo::default()
        .max_sets(1)
        .pool_sizes(&pool_sizes);
    let pool = unsafe { device.create_descriptor_pool(&pool_info, None).expect("pool") };

    let dsl_array = [dsl];
    let alloc_info = vk::DescriptorSetAllocateInfo::default()
        .descriptor_pool(pool)
        .set_layouts(&dsl_array);
    let sets = unsafe { device.allocate_descriptor_sets(&alloc_info).expect("alloc dsets") };
    let dset = sets[0];

    let bi = vk::DescriptorBufferInfo::default()
        .buffer(buffer)
        .offset(0)
        .range(vk::WHOLE_SIZE);
    let bis = [bi];
    let write = vk::WriteDescriptorSet::default()
        .dst_set(dset)
        .dst_binding(0)
        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
        .buffer_info(&bis);
    let writes = [write];
    unsafe { device.update_descriptor_sets(&writes, &[]); }
    println!("descriptor set layout/pool/update -> OK");

    // ── Shader module + pipeline ──────────────────────────────
    let spv = build_write_42_spirv();
    let words: Vec<u32> = spv.chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
    let sm_info = vk::ShaderModuleCreateInfo::default().code(&words);
    let shader = unsafe { device.create_shader_module(&sm_info, None).expect("shader_module") };
    println!("vkCreateShaderModule              -> OK (compute SPIR-V, {} bytes)", spv.len());

    let dsl_array = [dsl];
    let pl_info = vk::PipelineLayoutCreateInfo::default().set_layouts(&dsl_array);
    let pl = unsafe { device.create_pipeline_layout(&pl_info, None).expect("pl") };

    let stage = vk::PipelineShaderStageCreateInfo::default()
        .stage(vk::ShaderStageFlags::COMPUTE)
        .module(shader)
        .name(c"main");
    let cp_info = vk::ComputePipelineCreateInfo::default()
        .stage(stage)
        .layout(pl);
    let pipelines = unsafe {
        device.create_compute_pipelines(vk::PipelineCache::null(), &[cp_info], None)
            .map_err(|(_, e)| e)
            .expect("create_compute_pipelines")
    };
    let pipeline = pipelines[0];
    println!("vkCreateComputePipelines          -> OK");

    // ── Command pool + cmdbuf + record + submit ───────────────
    let cp_info = vk::CommandPoolCreateInfo::default()
        .queue_family_index(0)
        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
    let cmd_pool = unsafe { device.create_command_pool(&cp_info, None).expect("cmd_pool") };
    let ai = vk::CommandBufferAllocateInfo::default()
        .command_pool(cmd_pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    let cbs = unsafe { device.allocate_command_buffers(&ai).expect("cmdbufs") };
    let cb = cbs[0];

    let begin = vk::CommandBufferBeginInfo::default();
    unsafe { device.begin_command_buffer(cb, &begin).expect("begin"); }
    unsafe {
        device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::COMPUTE, pipeline);
        device.cmd_bind_descriptor_sets(
            cb, vk::PipelineBindPoint::COMPUTE, pl, 0, &[dset], &[],
        );
        device.cmd_dispatch(cb, 1, 1, 1);
    }
    unsafe { device.end_command_buffer(cb).expect("end"); }

    let cbs_to_submit = [cb];
    let submit = vk::SubmitInfo::default().command_buffers(&cbs_to_submit);
    unsafe { device.queue_submit(queue, &[submit], vk::Fence::null()).expect("submit"); }
    unsafe { device.device_wait_idle().expect("wait_idle"); }
    println!("dispatch(1,1,1) + WaitIdle        -> OK");

    // ── Read back ─────────────────────────────────────────────
    let mapped = unsafe {
        device.map_memory(mem, 0, req.size, vk::MemoryMapFlags::empty())
            .expect("map_memory (readback)")
    };
    // vkInvalidateMappedMemoryRanges is the spec-correct sync
    // point that triggers our OP_GPU_BUFFER_READ.  Without this
    // the mapped pointer would still hold the seeded 0xDEADBEEF
    // (the daemon-side buffer has the shader's write, but the
    // client-side Box hasn't been refreshed yet).
    let range = vk::MappedMemoryRange::default()
        .memory(mem)
        .offset(0)
        .size(req.size);
    unsafe {
        device.invalidate_mapped_memory_ranges(&[range])
            .expect("invalidate_mapped_memory_ranges");
    }
    let got = unsafe { std::ptr::read_unaligned(mapped as *const u32) };
    unsafe { device.unmap_memory(mem); }

    println!("ssbo[0] after dispatch+invalidate -> 0x{got:08x} (want 0x0000_002a)");
    let ok = got == 42;

    // ── Cleanup ────────────────────────────────────────────────
    unsafe {
        device.free_command_buffers(cmd_pool, &cbs_to_submit);
        device.destroy_command_pool(cmd_pool, None);
        device.destroy_pipeline(pipeline, None);
        device.destroy_pipeline_layout(pl, None);
        device.destroy_shader_module(shader, None);
        device.destroy_descriptor_pool(pool, None);
        device.destroy_descriptor_set_layout(dsl, None);
        device.destroy_buffer(buffer, None);
        device.free_memory(mem, None);
        device.destroy_device(None);
        instance.destroy_instance(None);
    }

    if ok {
        println!("PASS: full compute round-trip through the Khronos loader");
        0.into()
    } else {
        eprintln!("FAIL: shader output didn't reach the client (got {got:#x})");
        1.into()
    }
}
