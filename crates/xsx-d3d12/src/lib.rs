//! Xbox Series X Direct3D 12.x (`d3d12_x.dll` / `d3d12_xs.dll`) translation layer,
//! multithreaded default AMD FSR upscaling pipeline, DXBC/DXIL/FSSM/FSPX shader unpacker
//! + persistent cache, AMD GFX10/RDNA 2 descriptors & 64KB tiling/swizzle, Xbox BCPack
//! MDU rANS hardware texture decoder, FramePacer, measured Xbox Device/Heap COM ABI proxy,
//! UMA staging ring, and true hardware Windows / software GPU backends.

pub mod backend;
pub mod bcpack;
pub mod bvh;
pub mod com_objects;
pub mod frame_pacer;
pub mod fsr;
pub mod gfx10_desc;
pub mod native_win;
pub mod rdna2_isa;
pub mod rdna2_shader;
pub mod rdna2_to_hlsl;
pub mod rootsig;
pub mod shader;
pub mod shader_cache;
pub mod software;
pub mod staging;
pub mod tiling;
pub mod translate;
pub mod xs_proxy;

pub use backend::{
    BackendKind, D3D12Translator, GpuBackend, GpuCommand, PresentedFrame, ResourceKind,
    TranslatorConfig,
};
pub use bvh::{
    fnv1a_hash, Aabb, BvhAccelerationStructure, RtDispatchParams, RtHit, RtRay, RtTriangle, Vec3,
};
pub use com_objects::create_device_x_com;
pub use frame_pacer::{
    decide_present_sync, FrameClock, FramePacer, FrameStatistic, ManualFrameClock, PresentSync,
    SteadyFrameClock, TokenIssue, FRAME_EVENT_ORIGIN,
};
pub use fsr::{
    FsrComputePlan, FsrConfig, FsrDispatchConstants, FsrFrameStats, FsrQualityMode,
    FsrTemporalInputs,
};
pub use gfx10_desc::{
    decode_buffer_desc, decode_image_desc, encode_buffer_desc, encode_image_desc,
    is_image_descriptor, Gfx10BufferDesc, Gfx10ImageDesc, Gfx10ImageType,
};
pub use rdna2_isa::{disassemble_rdna2, format_rdna2_disassembly, Rdna2Aux, Rdna2Enc, Rdna2Insn, Rdna2Opnd};
pub use rdna2_shader::{
    extract_fssm_program, extract_fspx_parts, inspect_xbox_shader_blob, is_fspx_bare_program,
    sha256_hex, zlib_inflate, zlib_store, FspxParts, XboxShaderInspection,
};
pub use rdna2_to_hlsl::{lift_rdna2_to_hlsl, LiftedHlsl, ShaderStage};
pub use rootsig::{
    clear_xbox_root_sig_flags, downgrade_root_signature_1_2_to_1_1, sanitize_root_signature_desc,
    sanitize_rts0_blob, sanitize_versioned_root_signature_desc,
};
pub use shader::{compute_dxbc_hash, translate_xbox_shader_container, ShaderContainer, TranslatedShader};
pub use shader_cache::ShaderCache;
pub use software::SoftwareD3D12Backend;
pub use staging::UmaStagingRing;
pub use tiling::{
    compute_resource_tiling, detile_2d, element_tile_shape, small_tile_shape, swizzle_2d,
    ResourceTiling, SubresourceTiling, SwizzleEquation, TileShape, TiledKind, TilingDesc,
};
pub use xs_proxy::{create_xs_device_proxy, xs_device_slot_hits};
