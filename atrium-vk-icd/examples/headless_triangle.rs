//! `examples/headless_triangle` — drive atrium-vk-icd through its
//! C ABI like the Khronos Vulkan loader would.
//!
//! Demonstrates that the ICD is usable as a real Vulkan target
//! without depending on libvulkan.so.1 / MoltenVK / the loader.
//! Apps that link `libatrium_vk_icd` directly (or that point
//! the loader at our atrium_icd.json manifest) see the same
//! call graph this example walks.
//!
//! No WSI — we render to an offscreen color attachment. Real
//! windowed presentation arrives with `VK_KHR_swapchain` later.
//!
//! Usage:
//!   # First spawn an aqueduct-gpu host endpoint:
//!   $ FRESCOD_AQUEDUCT_SOCK=/tmp/atrium-vk-headless.sock \
//!     ./aqueduct-gpu-host --backend software &
//!   $ ATRIUM_VK_ICD_SOCKET=/tmp/atrium-vk-headless.sock \
//!     cargo run --example headless_triangle
//!
//! Or it'll silently no-op (zero physical devices, valid handle
//! chain) when no daemon is reachable.

use atrium_vk_icd::{
    cmdbuf_recorded_bytes, vkAllocateCommandBuffers, vkAllocateMemory,
    vkBeginCommandBuffer, vkBindImageMemory, vkCmdBeginRenderPass,
    vkCmdBindPipeline, vkCmdDraw, vkCmdEndRenderPass, vkCmdSetScissor,
    vkCmdSetViewport, vkCreateCommandPool, vkCreateDevice, vkCreateFramebuffer,
    vkCreateGraphicsPipelines, vkCreateImage, vkCreateImageView,
    vkCreateInstance, vkCreatePipelineLayout, vkCreateRenderPass,
    vkEndCommandBuffer, vkEnumeratePhysicalDevices, vkGetDeviceQueue,
    vkGetImageMemoryRequirements, vkQueueSubmit,
};
use std::ffi::c_void;

type VkInstance = *mut c_void;
type VkDevice = *mut c_void;
type VkQueue = *mut c_void;
type VkCommandBuffer = *mut c_void;

fn main() {
    let mut instance: VkInstance = std::ptr::null_mut();
    unsafe { vkCreateInstance(std::ptr::null(), std::ptr::null(), &mut instance); }
    println!("vkCreateInstance         → instance = {:?}", instance);

    let mut count: u32 = 0;
    unsafe { vkEnumeratePhysicalDevices(instance, &mut count, std::ptr::null_mut()); }
    println!("vkEnumeratePhysicalDevices → {count} device(s) visible");

    if count == 0 {
        eprintln!("no aqueduct-gpu host endpoint reachable — set ATRIUM_VK_ICD_SOCKET and spawn aqueduct-gpu-host first");
        return;
    }

    let mut pds = vec![std::ptr::null_mut::<c_void>(); count as usize];
    unsafe { vkEnumeratePhysicalDevices(instance, &mut count, pds.as_mut_ptr()); }

    let mut device: VkDevice = std::ptr::null_mut();
    unsafe { vkCreateDevice(pds[0], std::ptr::null(), std::ptr::null(), &mut device); }
    println!("vkCreateDevice           → device = {:?}", device);

    let mut queue: VkQueue = std::ptr::null_mut();
    unsafe { vkGetDeviceQueue(device, 0, 0, &mut queue); }

    // Color attachment image.
    let mut img_info = [0u8; 88];
    img_info[ 0.. 4].copy_from_slice(&14u32.to_le_bytes());
    img_info[24..28].copy_from_slice(&37u32.to_le_bytes()); // R8G8B8A8_UNORM
    img_info[28..32].copy_from_slice(&64u32.to_le_bytes());
    img_info[32..36].copy_from_slice(&64u32.to_le_bytes());
    img_info[36..40].copy_from_slice(&1u32.to_le_bytes());
    img_info[40..44].copy_from_slice(&1u32.to_le_bytes());
    img_info[44..48].copy_from_slice(&1u32.to_le_bytes());
    img_info[56..60].copy_from_slice(&0x10u32.to_le_bytes());
    let mut image: u64 = 0;
    unsafe { vkCreateImage(device, img_info.as_ptr() as *const _, std::ptr::null(), &mut image); }
    let mut req = ash::vk::MemoryRequirements::default();
    unsafe { vkGetImageMemoryRequirements(device, image, &mut req); }
    let mut alloc = [0u8; 32];
    alloc[0..4].copy_from_slice(&5u32.to_le_bytes());
    alloc[16..24].copy_from_slice(&req.size.to_le_bytes());
    let mut mem: u64 = 0;
    unsafe { vkAllocateMemory(device, alloc.as_ptr() as *const _, std::ptr::null(), &mut mem); }
    unsafe { vkBindImageMemory(device, image, mem, 0); }
    let mut view_info = [0u8; 80];
    view_info[ 0.. 4].copy_from_slice(&15u32.to_le_bytes());
    view_info[24..32].copy_from_slice(&image.to_le_bytes());
    let mut view: u64 = 0;
    unsafe { vkCreateImageView(device, view_info.as_ptr() as *const _, std::ptr::null(), &mut view); }

    // Pipeline + render pass + framebuffer + command pool + buffer.
    let mut pl_layout: u64 = 0;
    unsafe { vkCreatePipelineLayout(device, std::ptr::null(), std::ptr::null(), &mut pl_layout); }
    let mut pipeline: u64 = 0;
    unsafe { vkCreateGraphicsPipelines(device, 0, 1, std::ptr::null(), std::ptr::null(), &mut pipeline); }

    let mut rp_info = [0u8; 64];
    rp_info[0..4].copy_from_slice(&38u32.to_le_bytes());
    let mut render_pass: u64 = 0;
    unsafe { vkCreateRenderPass(device, rp_info.as_ptr() as *const _, std::ptr::null(), &mut render_pass); }
    let mut fb_info = [0u8; 64];
    fb_info[ 0.. 4].copy_from_slice(&37u32.to_le_bytes());
    fb_info[24..32].copy_from_slice(&render_pass.to_le_bytes());
    fb_info[32..36].copy_from_slice(&1u32.to_le_bytes());
    let atts = [view];
    fb_info[40..48].copy_from_slice(&(atts.as_ptr() as u64).to_le_bytes());
    fb_info[48..52].copy_from_slice(&64u32.to_le_bytes());
    fb_info[52..56].copy_from_slice(&64u32.to_le_bytes());
    let mut framebuffer: u64 = 0;
    unsafe { vkCreateFramebuffer(device, fb_info.as_ptr() as *const _, std::ptr::null(), &mut framebuffer); }

    let mut pool: u64 = 0;
    unsafe { vkCreateCommandPool(device, std::ptr::null(), std::ptr::null(), &mut pool); }
    let mut cb_info = [0u8; 40];
    cb_info[0..4].copy_from_slice(&40u32.to_le_bytes());
    cb_info[16..24].copy_from_slice(&pool.to_le_bytes());
    cb_info[28..32].copy_from_slice(&1u32.to_le_bytes());
    let mut cbs: [VkCommandBuffer; 1] = [std::ptr::null_mut(); 1];
    unsafe { vkAllocateCommandBuffers(device, cb_info.as_ptr() as *const _, cbs.as_mut_ptr()); }
    let cb = cbs[0];

    // Record the triangle frame.
    unsafe { vkBeginCommandBuffer(cb, std::ptr::null()); }
    let mut rpb = [0u8; 64];
    rpb[ 0.. 4].copy_from_slice(&43u32.to_le_bytes());
    rpb[16..24].copy_from_slice(&render_pass.to_le_bytes());
    rpb[24..32].copy_from_slice(&framebuffer.to_le_bytes());
    rpb[48..52].copy_from_slice(&1u32.to_le_bytes());
    let clear: [f32; 4] = [0.1, 0.4, 0.7, 1.0];
    rpb[56..64].copy_from_slice(&(clear.as_ptr() as u64).to_le_bytes());
    unsafe { vkCmdBeginRenderPass(cb, rpb.as_ptr() as *const _, 0); }
    unsafe { vkCmdBindPipeline(cb, 0, pipeline); }
    let vp = ash::vk::Viewport { x: 0.0, y: 0.0, width: 64.0, height: 64.0, min_depth: 0.0, max_depth: 1.0 };
    unsafe { vkCmdSetViewport(cb, 0, 1, &vp); }
    let sc = ash::vk::Rect2D {
        offset: ash::vk::Offset2D { x: 0, y: 0 },
        extent: ash::vk::Extent2D { width: 64, height: 64 },
    };
    unsafe { vkCmdSetScissor(cb, 0, 1, &sc); }
    unsafe { vkCmdDraw(cb, 3, 1, 0, 0); }
    unsafe { vkCmdEndRenderPass(cb); }
    unsafe { vkEndCommandBuffer(cb); }

    let bytes = cmdbuf_recorded_bytes(cb);
    println!("Recorded {} bytes of FrameOps:", bytes.len());
    let mut off = 0;
    while off + 8 <= bytes.len() {
        let opcode = u16::from_le_bytes([bytes[off], bytes[off + 1]]);
        let total = u32::from_le_bytes([
            bytes[off + 4], bytes[off + 5], bytes[off + 6], bytes[off + 7],
        ]) as usize;
        println!("  opcode 0x{opcode:04x}, {total}-byte record");
        off += total;
    }

    let mut submit_info = [0u8; 72];
    submit_info[0..4].copy_from_slice(&4u32.to_le_bytes());
    submit_info[40..44].copy_from_slice(&1u32.to_le_bytes());
    let cb_arr = [cb];
    submit_info[48..56].copy_from_slice(&(cb_arr.as_ptr() as u64).to_le_bytes());
    let r = unsafe {
        vkQueueSubmit(queue, 1, submit_info.as_ptr() as *const _, std::ptr::null_mut())
    };
    println!("vkQueueSubmit            → result = {r}");
    println!("done — host endpoint received the frame.");
}
