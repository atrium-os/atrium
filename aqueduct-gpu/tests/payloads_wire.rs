//! Integration tests: payload structs roundtrip cleanly via postcard.
//!
//! Catches three common mistakes in wire-format work:
//! - struct fields in the wrong order on serialize vs deserialize
//! - non-Serialize/Deserialize types sneaking in
//! - silent breakage when adding/removing fields
//!
//! Add a new test here whenever a payload schema gains a field.

use aqueduct_gpu::{
    backends::{BackendId, GpuVendor},
    ids::{IdNamespace, ResourceId},
    payloads::*,
};

fn roundtrip<T>(value: &T) -> T
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    let bytes = postcard::to_stdvec(value).expect("serialize");
    postcard::from_bytes(&bytes).expect("deserialize")
}

#[test]
fn handshake_payload_roundtrip() {
    let p = HandshakePayload {
        protocol_version: 1,
        client_kind: ClientKind::FrescodRenderer,
    };
    let r: HandshakePayload = roundtrip(&p);
    assert_eq!(r.protocol_version, 1);
    assert_eq!(r.client_kind, ClientKind::FrescodRenderer);
}

#[test]
fn handshake_response_roundtrip() {
    let p = HandshakeResponse {
        protocol_version: 1,
        backend: BackendId::new(GpuVendor::Apple, 4),
        caps: HandshakeResponse::CAPS_COMPUTE
            | HandshakeResponse::CAPS_SHARE_SURFACE
            | HandshakeResponse::CAPS_COMPOSITION,
        max_frame_bytes: 1 << 20,
        max_fences_inflight: 64,
    };
    let r: HandshakeResponse = roundtrip(&p);
    assert_eq!(r.backend.vendor, GpuVendor::Apple);
    assert_eq!(r.backend.generation, 4);
    assert_eq!(r.caps, p.caps);
}

#[test]
fn memory_create_roundtrip() {
    let p = MemoryCreatePayload {
        region_id: ResourceId::new(IdNamespace::IcdRuntime, 0x42),
        size: 4096 * 1024,
        usage: MemoryUsage::ImageBacking,
    };
    let r: MemoryCreatePayload = roundtrip(&p);
    assert_eq!(r.region_id, p.region_id);
    assert_eq!(r.size, p.size);
    assert_eq!(r.usage, MemoryUsage::ImageBacking);
}

#[test]
fn memory_create_response_roundtrip() {
    let p = MemoryCreateResponse {
        region_id: ResourceId::new(IdNamespace::Builtin, 0x1),
        size: 16384,
        host_va_hint: 0xCAFE_0000,
        atrium_gpu_token: [0xAB; 32],
    };
    let r: MemoryCreateResponse = roundtrip(&p);
    assert_eq!(r.region_id, p.region_id);
    assert_eq!(r.atrium_gpu_token, p.atrium_gpu_token);
}

#[test]
fn image_create_roundtrip() {
    let p = ImageCreatePayload {
        image_id: ResourceId::new(IdNamespace::IcdRuntime, 0x10),
        backing_region: ResourceId::new(IdNamespace::IcdRuntime, 0x11),
        region_offset: 0,
        format: 37, // VK_FORMAT_R8G8B8A8_UNORM
        width: 1280,
        height: 720,
        depth: 1,
        mip_levels: 1,
        array_layers: 1,
        usage: 0x07,
    };
    let r: ImageCreatePayload = roundtrip(&p);
    assert_eq!(r.width, 1280);
    assert_eq!(r.height, 720);
    assert_eq!(r.image_id, p.image_id);
}

#[test]
fn buffer_create_roundtrip() {
    let p = BufferCreatePayload {
        buffer_id: ResourceId::new(IdNamespace::IcdRuntime, 0x20),
        backing_region: ResourceId::new(IdNamespace::IcdRuntime, 0x21),
        region_offset: 256,
        size: 1024 * 1024,
        usage: 0x82,
    };
    let r: BufferCreatePayload = roundtrip(&p);
    assert_eq!(r.size, 1024 * 1024);
    assert_eq!(r.region_offset, 256);
}

#[test]
fn sampler_create_roundtrip() {
    let p = SamplerCreatePayload {
        sampler_id: ResourceId::new(IdNamespace::IcdRuntime, 0x30),
        min_filter: 1,
        mag_filter: 1,
        mip_filter: 0,
        address_modes: [0, 0, 0],
        max_anisotropy: 16.0,
        min_lod: 0.0,
        max_lod: 1000.0,
    };
    let r: SamplerCreatePayload = roundtrip(&p);
    assert_eq!(r.max_anisotropy, 16.0);
    assert_eq!(r.address_modes, [0, 0, 0]);
}

#[test]
fn shader_resolve_roundtrip_hit_and_miss() {
    let hash = [0x42; 32];
    let backend = BackendId::new(GpuVendor::AtriumGpu, 1);

    let req = ShaderResolvePayload {
        bytecode_hash: hash,
        kind: ShaderKind::SpirV,
        backend,
    };
    let req_back: ShaderResolvePayload = roundtrip(&req);
    assert_eq!(req_back.bytecode_hash, hash);
    assert_eq!(req_back.kind, ShaderKind::SpirV);

    let hit = ShaderResolveResponse {
        bytecode_hash: hash,
        status: ShaderResolveStatus::Hit,
        shader_id: Some(ResourceId::new(IdNamespace::IcdRuntime, 0x100)),
    };
    let hit_back: ShaderResolveResponse = roundtrip(&hit);
    assert_eq!(hit_back.status, ShaderResolveStatus::Hit);
    assert!(hit_back.shader_id.is_some());

    let miss = ShaderResolveResponse {
        bytecode_hash: hash,
        status: ShaderResolveStatus::Miss,
        shader_id: None,
    };
    let miss_back: ShaderResolveResponse = roundtrip(&miss);
    assert_eq!(miss_back.status, ShaderResolveStatus::Miss);
    assert!(miss_back.shader_id.is_none());
}

#[test]
fn shader_upload_roundtrip() {
    let p = ShaderUploadPayload {
        bytecode_hash: [0x55; 32],
        kind: ShaderKind::SpirV,
        backend: BackendId::new(GpuVendor::Apple, 4),
        bytecode: vec![0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE],
    };
    let r: ShaderUploadPayload = roundtrip(&p);
    assert_eq!(r.bytecode.len(), 8);
    assert_eq!(r.bytecode, p.bytecode);
}

#[test]
fn pipeline_create_roundtrip() {
    let p = PipelineCreatePayload {
        pipeline_id: ResourceId::new(IdNamespace::IcdRuntime, 0x200),
        kind: PipelineKind::Graphics,
        shaders: vec![
            ResourceId::new(IdNamespace::IcdRuntime, 0x100),
            ResourceId::new(IdNamespace::IcdRuntime, 0x101),
        ],
        state_blob: vec![0; 256],
    };
    let r: PipelineCreatePayload = roundtrip(&p);
    assert_eq!(r.kind, PipelineKind::Graphics);
    assert_eq!(r.shaders.len(), 2);
    assert_eq!(r.state_blob.len(), 256);
}

#[test]
fn submit_frame_roundtrip() {
    let mut command_buf = Vec::new();
    // hand-build a synthetic frame stream (one record)
    command_buf.extend_from_slice(&0x0040u16.to_le_bytes());  // FrameOp::Draw
    command_buf.push(0);
    command_buf.push(0);
    command_buf.extend_from_slice(&12u32.to_le_bytes());
    command_buf.extend_from_slice(&[0xAB, 0xCD, 0xEF, 0x12]);

    let p = SubmitFramePayload {
        fence_id: ResourceId::new(IdNamespace::IcdRuntime, 0x300),
        timeline: 42,
        command_buf,
    };
    let r: SubmitFramePayload = roundtrip(&p);
    assert_eq!(r.timeline, 42);
    assert_eq!(r.command_buf.len(), 12);
    assert_eq!(r.fence_id, p.fence_id);
}

#[test]
fn wait_fence_roundtrip() {
    let p = WaitFencePayload {
        fence_id: ResourceId::new(IdNamespace::IcdRuntime, 0x300),
        timeout_ns: 1_000_000_000,
    };
    let r: WaitFencePayload = roundtrip(&p);
    assert_eq!(r.timeout_ns, 1_000_000_000);

    let resp = WaitFenceResponse {
        fence_id: p.fence_id,
        signalled: true,
    };
    let resp_back: WaitFenceResponse = roundtrip(&resp);
    assert!(resp_back.signalled);
}

#[test]
fn share_surface_roundtrip() {
    let p = ShareSurfacePayload {
        image_id: ResourceId::new(IdNamespace::IcdRuntime, 0x40),
        purpose: "vulkan game framebuffer".to_string(),
    };
    let r: ShareSurfacePayload = roundtrip(&p);
    assert_eq!(r.purpose, "vulkan game framebuffer");

    let resp = ShareSurfaceResponse {
        share_token: [0x99; 32],
    };
    let resp_back: ShareSurfaceResponse = roundtrip(&resp);
    assert_eq!(resp_back.share_token, [0x99; 32]);
}

#[test]
fn bundle_load_roundtrip() {
    let p = BundleLoadPayload {
        manifest_cas_hash: [0x77; 32],
        display_name: "particles@1.0".to_string(),
    };
    let r: BundleLoadPayload = roundtrip(&p);
    assert_eq!(r.display_name, "particles@1.0");
    assert_eq!(r.manifest_cas_hash, [0x77; 32]);

    let resp_ok = BundleLoadResponse {
        manifest_cas_hash: [0x77; 32],
        bundle_namespace: Some(0x5),
    };
    let resp_ok_back: BundleLoadResponse = roundtrip(&resp_ok);
    assert_eq!(resp_ok_back.bundle_namespace, Some(0x5));

    let resp_err = BundleLoadResponse {
        manifest_cas_hash: [0x77; 32],
        bundle_namespace: None,
    };
    let resp_err_back: BundleLoadResponse = roundtrip(&resp_err);
    assert_eq!(resp_err_back.bundle_namespace, None);
}

#[test]
fn async_events_roundtrip() {
    let fs = FenceSignaledEvent {
        fence_id: ResourceId::new(IdNamespace::IcdRuntime, 0x300),
        timeline: 100,
    };
    let _: FenceSignaledEvent = roundtrip(&fs);

    let dl = DeviceLostEvent {
        diagnostic: "host MoltenVK reported VK_ERROR_DEVICE_LOST".to_string(),
    };
    let dl_back: DeviceLostEvent = roundtrip(&dl);
    assert!(dl_back.diagnostic.contains("MoltenVK"));

    let ve = ValidationErrEvent {
        opcode: aqueduct_gpu::OP_GPU_IMAGE_CREATE,
        resource_id: Some(ResourceId::new(IdNamespace::IcdRuntime, 0x10)),
        diagnostic: "format 0x1234 unsupported on this backend".to_string(),
    };
    let ve_back: ValidationErrEvent = roundtrip(&ve);
    assert_eq!(ve_back.opcode, aqueduct_gpu::OP_GPU_IMAGE_CREATE);
    assert!(ve_back.resource_id.is_some());

    let ble = BundleLoadErrEvent {
        manifest_cas_hash: [0x77; 32],
        bundle_local_id: 0x42,
        diagnostic: "shader sha256:abc... not found in Tessera CAS".to_string(),
    };
    let ble_back: BundleLoadErrEvent = roundtrip(&ble);
    assert_eq!(ble_back.bundle_local_id, 0x42);
}
