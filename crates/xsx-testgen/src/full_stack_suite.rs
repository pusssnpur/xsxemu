//! Full-stack Phase 3 + Phase 4 synthetic Xbox Series X PE32+ executable (`full_stack_xsx.exe`).
//!
//! Executes natively via Win64 ABI and exercises:
//! - Phase 4 `XUser` offline profile + `XStore` offline license
//! - Phase 4 `XGameSave` persistent local save container write + readback
//! - Phase 4 `dstorage_x.dll` DirectStorage streaming from `G:\assets\level.bin`
//! - Phase 4 `xinput1_4.dll` controller polling + vibration feedback
//! - Phase 4 `xaudio2_9.dll` mastering voice + source voice + PCM buffer submission
//! - Phase 3 `d3d12_x.dll` RootSignature, Graphics PSO (with Xbox DXBC/DXIL chunk stripping),
//!   Compute Dispatch, triangle rasterization, and default AMD FSR `PresentX` upscaling.

use std::collections::HashMap;

use xsx_abi::com::{
    IID_ID3D12COMMAND_ALLOCATOR, IID_ID3D12COMMAND_QUEUE, IID_ID3D12DESCRIPTOR_HEAP,
    IID_ID3D12DEVICE, IID_ID3D12GRAPHICS_COMMAND_LIST, IID_ID3D12PIPELINE_STATE,
    IID_ID3D12RESOURCE, IID_ID3D12ROOT_SIGNATURE, IID_IDSTORAGE_FACTORY, IID_IDSTORAGE_FILE,
    IID_IDSTORAGE_QUEUE,
};
use xsx_abi::d3d12::{
    D3D12CommandListType, D3D12CommandQueueDesc, D3D12DescriptorHeapDesc, D3D12DescriptorHeapType,
    D3D12GraphicsPipelineStateDesc, D3D12HeapProperties, D3D12HeapType, D3D12ResourceDesc,
    D3D12ResourceDimension, D3D12ShaderBytecode, D3D12TextureLayout, D3D12VertexBufferView,
    D3D12Viewport, D3D12XboxCreateDeviceParameters, D3D12XboxPresentPlaneParameters,
    D3DPrimitiveTopology, DxgiFormat, DxgiSampleDesc, SimpleVertex,
    D3D12XBOX_CREATE_DEVICE_FLAG_DEBUG, D3D12XBOX_HEAP_FLAG_CPU_CACHED,
    D3D12XBOX_HEAP_FLAG_GRAPHICS_WRITABLE, D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
    D3D12_RESOURCE_STATE_RENDER_TARGET,
};
use xsx_abi::services::{
    DStorageQueueDesc, DStorageRequest, WaveFormatEx, XAudio2Buffer, XInputState, XInputVibration,
    XStoreGameLicense,
};
use xsx_d3d12::ShaderContainer;
use xsx_loader::{
    IMAGE_DIRECTORY_ENTRY_BASERELOC, IMAGE_DIRECTORY_ENTRY_IMPORT, IMAGE_FILE_MACHINE_AMD64,
    IMAGE_REL_BASED_DIR64, IMAGE_SCN_MEM_EXECUTE, IMAGE_SCN_MEM_READ, IMAGE_SCN_MEM_WRITE,
    PE32_PLUS_MAGIC,
};

use crate::x64_asm::{Reg64, X64Assembler};

pub const FULLSTACK_SAVE_PAYLOAD: &str = "HERO_HP=100;LVL=9";
pub const FULLSTACK_ASSET_PAYLOAD: &str = "DSTORAGE_STREAMED_LEVEL_OK";

const EXE_BASE: u64 = 0x0000_0001_4000_0000;
const SECTION_ALIGN: u32 = 0x1000;
const FILE_ALIGN: u32 = 0x200;

const TEXT_RVA: u32 = 0x1000;
const RDATA_RVA: u32 = 0x2000;
const DATA_RVA: u32 = 0x3000;
const IDATA_RVA: u32 = 0x4000;
const RELOC_RVA: u32 = 0x5000;
const SIZE_OF_IMAGE: u32 = 0x6000;

pub fn build_full_stack_xsx_pe() -> Vec<u8> {
    let import_spec: &[(&str, &[&str])] = &[
        (
            "xgameruntime.dll",
            &[
                "XGameRuntimeInitialize",
                "XGameRuntimeUninitialize",
                "XUserAddAsync",
                "XUserAddResult",
                "XUserGetGamertag",
                "XUserCloseHandle",
                "XStoreCreateContext",
                "XStoreQueryGameLicense",
                "XStoreCloseContext",
                "XGameSaveInitializeProvider",
                "XGameSaveCreateContainer",
                "XGameSaveCreateUpdate",
                "XGameSaveSubmitBlobWrite",
                "XGameSaveSubmitUpdate",
                "XGameSaveReadBlobData",
                "XGameSaveCloseUpdate",
                "XGameSaveCloseContainer",
                "XGameSaveCloseProvider",
            ],
        ),
        ("kernelx.dll", &["OutputDebugStringA"]),
        ("dstorage_x.dll", &["DStorageGetFactory"]),
        ("xinput1_4.dll", &["XInputGetState", "XInputSetState"]),
        ("xaudio2_9.dll", &["XAudio2Create"]),
        ("d3d12_x.dll", &["D3D12XBOX_CreateDevice"]),
    ];
    let (idata_bytes, iat_rvas, idata_dir_size) = build_idata(IDATA_RVA, import_spec);

    // -----------------------------------------------------------------------
    // .rdata (constants, synthetic Xbox DXBC shader, PCM audio, triangle vertices)
    // -----------------------------------------------------------------------
    let mut rdata = DataBuilder::new(RDATA_RVA);
    let rva_container_name = rdata.push_cstr("slot0");
    let rva_blob_name = rdata.push_cstr("player.sav");
    let rva_save_data = rdata.push_cstr(FULLSTACK_SAVE_PAYLOAD);
    let rva_asset_path = rdata.push_cstr(r"G:\assets\level.bin");
    let rva_msg_done = rdata.push_cstr("Full-stack Phase 3 + Phase 4 guest PE64 completed!");

    // Synthetic Xbox Series X DXBC/DXIL shader container with console-only chunks (`XBOX`, `XGCS`)
    let xbox_shader_bytes = ShaderContainer::build(&[
        (*b"ISGN", &[1, 2, 3, 4]),
        (*b"XBOX", &[0xBA, 0xAD, 0xF0, 0x0D]),
        (*b"DXIL", b"BC\xC0\xDExsx_shader_ir"),
        (*b"XGCS", &[0x11, 0x22, 0x33, 0x44]),
        (*b"RTS0", &[0x02, 0x00, 0x00, 0x00]),
    ]);
    let rva_xbox_shader = rdata.push_bytes(&xbox_shader_bytes);
    let xbox_shader_len = xbox_shader_bytes.len();

    // 256 stereo 16-bit PCM frames (1024 bytes) for XAudio2 submission
    let pcm_samples = vec![0x20u8; 1024];
    let rva_pcm_data = rdata.push_bytes(&pcm_samples);

    let rva_wave_fmt = rdata.push_struct(&WaveFormatEx {
        w_format_tag: 1, // PCM
        n_channels: 2,
        n_samples_per_sec: 48_000,
        n_avg_bytes_per_sec: 192_000,
        n_block_align: 4,
        w_bits_per_sample: 16,
        cb_size: 0,
    });

    let rva_vibration = rdata.push_struct(&XInputVibration {
        w_left_motor_speed: 16_000,
        w_right_motor_speed: 32_000,
    });

    let rva_ds_q_desc = rdata.push_struct(&DStorageQueueDesc {
        source_type: 0,
        capacity: 64,
        priority: 0,
        _pad: 0,
        name: core::ptr::null(),
        device: core::ptr::null_mut(),
    });

    let rva_iid_ds_fac = rdata.push_struct(&IID_IDSTORAGE_FACTORY);
    let rva_iid_ds_q = rdata.push_struct(&IID_IDSTORAGE_QUEUE);
    let rva_iid_ds_file = rdata.push_struct(&IID_IDSTORAGE_FILE);

    let rva_dev_params = rdata.push_struct(&D3D12XboxCreateDeviceParameters {
        version: 1,
        process_debug_flags: D3D12XBOX_CREATE_DEVICE_FLAG_DEBUG,
        graphics_command_queue_ring_size_bytes: 64 * 1024,
        graphics_scratch_memory_size_bytes: 1024 * 1024,
        compute_scratch_memory_size_bytes: 1024 * 1024,
    });
    let rva_iid_device = rdata.push_struct(&IID_ID3D12DEVICE);
    let rva_iid_queue = rdata.push_struct(&IID_ID3D12COMMAND_QUEUE);
    let rva_iid_alloc = rdata.push_struct(&IID_ID3D12COMMAND_ALLOCATOR);
    let rva_iid_cmdlist = rdata.push_struct(&IID_ID3D12GRAPHICS_COMMAND_LIST);
    let rva_iid_descheap = rdata.push_struct(&IID_ID3D12DESCRIPTOR_HEAP);
    let rva_iid_resource = rdata.push_struct(&IID_ID3D12RESOURCE);
    let rva_iid_rootsig = rdata.push_struct(&IID_ID3D12ROOT_SIGNATURE);
    let rva_iid_pso = rdata.push_struct(&IID_ID3D12PIPELINE_STATE);

    let rva_queue_desc = rdata.push_struct(&D3D12CommandQueueDesc {
        r#type: D3D12CommandListType::Direct as u32,
        priority: 0,
        flags: 0,
        node_mask: 0,
    });
    let rva_rtv_heap_desc = rdata.push_struct(&D3D12DescriptorHeapDesc {
        r#type: D3D12DescriptorHeapType::Rtv as u32,
        num_descriptors: 2,
        flags: 0,
        node_mask: 0,
    });
    let rva_heap_props = rdata.push_struct(&D3D12HeapProperties {
        r#type: D3D12HeapType::Default as u32,
        cpu_page_property: 2,
        memory_pool_preference: 1,
        creation_node_mask: 0,
        visible_node_mask: 0,
    });
    let rva_rt_desc = rdata.push_struct(&D3D12ResourceDesc {
        dimension: D3D12ResourceDimension::Texture2D as u32,
        alignment: 0,
        width: 640,
        height: 360,
        depth_or_array_size: 1,
        mip_levels: 1,
        format: DxgiFormat::R8G8B8A8Unorm as u32,
        sample_desc: DxgiSampleDesc {
            count: 1,
            quality: 0,
        },
        layout: D3D12TextureLayout::Unknown as u32,
        flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
    });
    let rva_viewport = rdata.push_struct(&D3D12Viewport {
        top_left_x: 0.0,
        top_left_y: 0.0,
        width: 640.0,
        height: 360.0,
        min_depth: 0.0,
        max_depth: 1.0,
    });
    let rva_clear_color = rdata.push_struct(&[0.05_f32, 0.12_f32, 0.16_f32, 1.0_f32]);

    let vertices: [SimpleVertex; 3] = [
        SimpleVertex {
            pos: [0.0, 0.78, 0.0],
            color: [0.10, 0.90, 0.30, 1.0],
        },
        SimpleVertex {
            pos: [0.68, -0.66, 0.0],
            color: [0.20, 0.60, 1.00, 1.0],
        },
        SimpleVertex {
            pos: [-0.68, -0.66, 0.0],
            color: [0.95, 0.30, 0.20, 1.0],
        },
    ];
    let rva_vertices = rdata.push_struct(&vertices);
    let rdata_bytes = rdata.finish();

    // -----------------------------------------------------------------------
    // .data (runtime handles, buffers, relocated descriptors)
    // -----------------------------------------------------------------------
    let mut data = DataBuilder::new(DATA_RVA);
    let mut reloc_rvas = Vec::new();

    let rva_user_h = data.push_u64(0);
    let rva_store_h = data.push_u64(0);
    let rva_provider_h = data.push_u64(0);
    let rva_container_h = data.push_u64(0);
    let rva_update_h = data.push_u64(0);
    let rva_bytes_read = data.push_u64(0);

    let rva_ds_factory = data.push_u64(0);
    let rva_ds_queue = data.push_u64(0);
    let rva_ds_file = data.push_u64(0);

    let rva_xaudio = data.push_u64(0);
    let rva_master_voice = data.push_u64(0);
    let rva_source_voice = data.push_u64(0);

    let rva_device_ptr = data.push_u64(0);
    let rva_queue_ptr = data.push_u64(0);
    let rva_alloc_ptr = data.push_u64(0);
    let rva_cmdlist_ptr = data.push_u64(0);
    let rva_rtv_heap_ptr = data.push_u64(0);
    let rva_rt_res_ptr = data.push_u64(0);
    let rva_rtv_handle = data.push_u64(0);
    let rva_rootsig_ptr = data.push_u64(0);
    let rva_pso_ptr = data.push_u64(0);

    let rva_gamertag_buf = data.push_zeroed(32);
    let rva_save_read_buf = data.push_zeroed(64);
    let rva_stream_buf = data.push_zeroed(64);
    let rva_xinput_state = data.push_struct(&XInputState::default());
    let rva_license = data.push_struct(&XStoreGameLicense::default());

    // DirectStorage request with relocated destination_buffer -> rva_stream_buf
    // (source_file at offset 0 is filled dynamically at runtime!)
    let rva_ds_req = data.push_struct(&DStorageRequest {
        source_file: core::ptr::null_mut(),
        file_offset: 0,
        size_bytes: FULLSTACK_ASSET_PAYLOAD.len() as u32,
        compression_format: 0,
        destination_buffer: (EXE_BASE + rva_stream_buf as u64) as *mut u8,
        destination_size: 64,
        _pad: 0,
    });
    // destination_buffer is at offset 24 inside DStorageRequest
    reloc_rvas.push(rva_ds_req + 24);

    // XAudio2Buffer with relocated p_audio_data -> rva_pcm_data
    let rva_xa_buf = data.push_struct(&XAudio2Buffer {
        flags: 0,
        audio_bytes: 1024,
        p_audio_data: (EXE_BASE + rva_pcm_data as u64) as *const u8,
        play_begin: 0,
        play_length: 0,
        loop_begin: 0,
        loop_length: 0,
        loop_count: 0,
        p_context: core::ptr::null_mut(),
    });
    // p_audio_data is at offset 8 inside XAudio2Buffer
    reloc_rvas.push(rva_xa_buf + 8);

    // Graphics PSO Desc with relocated VS & PS bytecode pointers -> rva_xbox_shader
    let rva_pso_desc = data.push_struct(&D3D12GraphicsPipelineStateDesc {
        p_root_signature: core::ptr::null_mut(),
        vs: D3D12ShaderBytecode {
            p_shader_bytecode: (EXE_BASE + rva_xbox_shader as u64) as *const core::ffi::c_void,
            byte_code_length: xbox_shader_len,
        },
        ps: D3D12ShaderBytecode {
            p_shader_bytecode: (EXE_BASE + rva_xbox_shader as u64) as *const core::ffi::c_void,
            byte_code_length: xbox_shader_len,
        },
        num_render_targets: 1,
        rtv_formats: [DxgiFormat::R8G8B8A8Unorm as u32, 0, 0, 0, 0, 0, 0, 0],
        dsv_format: 0,
        sample_desc: DxgiSampleDesc {
            count: 1,
            quality: 0,
        },
        flags: 0,
    });
    // vs.p_shader_bytecode at offset 8, ps.p_shader_bytecode at offset 24
    reloc_rvas.push(rva_pso_desc + 8);
    reloc_rvas.push(rva_pso_desc + 24);

    let rva_vb_view = data.push_struct(&D3D12VertexBufferView {
        buffer_location: EXE_BASE + rva_vertices as u64,
        size_in_bytes: core::mem::size_of::<[SimpleVertex; 3]>() as u32,
        stride_in_bytes: core::mem::size_of::<SimpleVertex>() as u32,
    });
    reloc_rvas.push(rva_vb_view);

    let rva_present_plane = data.push_struct(&D3D12XboxPresentPlaneParameters {
        token: 1,
        resource_count: 1,
        _pad: 0,
        pp_resources: (EXE_BASE + rva_rt_res_ptr as u64) as *const *mut core::ffi::c_void,
        color_space: 0,
        options: 0,
    });
    reloc_rvas.push(rva_present_plane + 16);

    let data_bytes = data.finish();
    let reloc_bytes = build_reloc_table(&reloc_rvas);

    // -----------------------------------------------------------------------
    // Assemble .text
    // -----------------------------------------------------------------------
    let mut asm = X64Assembler::new(TEXT_RVA);
    let mut fail = Vec::new();

    asm.push(Reg64::Rbp);
    asm.push(Reg64::Rbx);
    asm.push(Reg64::Rsi);
    asm.push(Reg64::Rdi);
    asm.sub_rsp_imm8(0x68);

    // 1. GDK Init + XUser + XStore
    asm.call_iat(iat_rvas["XGameRuntimeInitialize"]);
    asm.test_eax_eax();
    fail.push(asm.js_placeholder());

    asm.xor_reg32_self(Reg64::Rcx);
    asm.xor_reg32_self(Reg64::Rdx);
    asm.call_iat(iat_rvas["XUserAddAsync"]);

    asm.xor_reg32_self(Reg64::Rcx);
    asm.lea_rip_rva(Reg64::Rdx, rva_user_h);
    asm.call_iat(iat_rvas["XUserAddResult"]);
    asm.test_eax_eax();
    fail.push(asm.js_placeholder());

    // XUserGetGamertag(user, 0, 32, &gamertag_buf, NULL) -> log via OutputDebugStringA
    asm.mov_rsp_off_imm32(0x20, 0);
    asm.mov_reg_from_rva(Reg64::Rcx, rva_user_h);
    asm.xor_reg32_self(Reg64::Rdx);
    asm.mov_reg_imm32(Reg64::R8, 32);
    asm.lea_rip_rva(Reg64::R9, rva_gamertag_buf);
    asm.call_iat(iat_rvas["XUserGetGamertag"]);
    asm.lea_rip_rva(Reg64::Rcx, rva_gamertag_buf);
    asm.call_iat(iat_rvas["OutputDebugStringA"]);

    // XStoreCreateContext + XStoreQueryGameLicense
    asm.mov_reg_from_rva(Reg64::Rcx, rva_user_h);
    asm.lea_rip_rva(Reg64::Rdx, rva_store_h);
    asm.call_iat(iat_rvas["XStoreCreateContext"]);
    asm.mov_reg_from_rva(Reg64::Rcx, rva_store_h);
    asm.lea_rip_rva(Reg64::Rdx, rva_license);
    asm.call_iat(iat_rvas["XStoreQueryGameLicense"]);
    asm.test_eax_eax();
    fail.push(asm.js_placeholder());

    // 2. XGameSave: create container, write blob, submit update, read back & log!
    asm.mov_reg_from_rva(Reg64::Rcx, rva_user_h);
    asm.xor_reg32_self(Reg64::Rdx);
    asm.xor_reg32_self(Reg64::R8);
    asm.lea_rip_rva(Reg64::R9, rva_provider_h);
    asm.call_iat(iat_rvas["XGameSaveInitializeProvider"]);
    asm.test_eax_eax();
    fail.push(asm.js_placeholder());

    asm.mov_reg_from_rva(Reg64::Rcx, rva_provider_h);
    asm.lea_rip_rva(Reg64::Rdx, rva_container_name);
    asm.lea_rip_rva(Reg64::R8, rva_container_h);
    asm.call_iat(iat_rvas["XGameSaveCreateContainer"]);
    asm.test_eax_eax();
    fail.push(asm.js_placeholder());

    asm.mov_reg_from_rva(Reg64::Rcx, rva_container_h);
    asm.lea_rip_rva(Reg64::Rdx, rva_container_name);
    asm.lea_rip_rva(Reg64::R8, rva_update_h);
    asm.call_iat(iat_rvas["XGameSaveCreateUpdate"]);

    asm.mov_reg_from_rva(Reg64::Rcx, rva_update_h);
    asm.lea_rip_rva(Reg64::Rdx, rva_blob_name);
    asm.lea_rip_rva(Reg64::R8, rva_save_data);
    asm.mov_reg_imm32(Reg64::R9, FULLSTACK_SAVE_PAYLOAD.len() as u32);
    asm.call_iat(iat_rvas["XGameSaveSubmitBlobWrite"]);

    asm.mov_reg_from_rva(Reg64::Rcx, rva_update_h);
    asm.call_iat(iat_rvas["XGameSaveSubmitUpdate"]);
    asm.test_eax_eax();
    fail.push(asm.js_placeholder());

    // Read save blob back & print via OutputDebugStringA
    asm.lea_rip_rva(Reg64::Rax, rva_bytes_read);
    asm.mov_rsp_off_reg(0x20, Reg64::Rax);
    asm.mov_reg_from_rva(Reg64::Rcx, rva_container_h);
    asm.lea_rip_rva(Reg64::Rdx, rva_blob_name);
    asm.lea_rip_rva(Reg64::R8, rva_save_read_buf);
    asm.mov_reg_imm32(Reg64::R9, 63);
    asm.call_iat(iat_rvas["XGameSaveReadBlobData"]);
    asm.test_eax_eax();
    fail.push(asm.js_placeholder());
    asm.lea_rip_rva(Reg64::Rcx, rva_save_read_buf);
    asm.call_iat(iat_rvas["OutputDebugStringA"]);

    // 3. DirectStorage (`dstorage_x.dll`): stream `G:\assets\level.bin` into `rva_stream_buf`
    asm.lea_rip_rva(Reg64::Rcx, rva_iid_ds_fac);
    asm.lea_rip_rva(Reg64::Rdx, rva_ds_factory);
    asm.call_iat(iat_rvas["DStorageGetFactory"]);
    asm.test_eax_eax();
    fail.push(asm.js_placeholder());

    // factory->CreateQueue(&desc, &IID, &queue) [slot 3]
    asm.mov_reg_from_rva(Reg64::Rcx, rva_ds_factory);
    asm.lea_rip_rva(Reg64::Rdx, rva_ds_q_desc);
    asm.lea_rip_rva(Reg64::R8, rva_iid_ds_q);
    asm.lea_rip_rva(Reg64::R9, rva_ds_queue);
    asm.call_com_rcx_slot(3);

    // factory->OpenFileA("G:\\assets\\level.bin", &IID, &file) [slot 4]
    asm.mov_reg_from_rva(Reg64::Rcx, rva_ds_factory);
    asm.lea_rip_rva(Reg64::Rdx, rva_asset_path);
    asm.lea_rip_rva(Reg64::R8, rva_iid_ds_file);
    asm.lea_rip_rva(Reg64::R9, rva_ds_file);
    asm.call_com_rcx_slot(4);
    asm.test_eax_eax();
    fail.push(asm.js_placeholder());

    // Store ds_file into ds_req.source_file (at offset 0 of rva_ds_req)
    asm.mov_reg_from_rva(Reg64::Rax, rva_ds_file);
    asm.mov_rva_from_reg(rva_ds_req, Reg64::Rax);

    // queue->EnqueueRequest(&ds_req) [slot 3], then queue->Submit() [slot 5]
    asm.mov_reg_from_rva(Reg64::Rcx, rva_ds_queue);
    asm.lea_rip_rva(Reg64::Rdx, rva_ds_req);
    asm.call_com_rcx_slot(3);
    asm.mov_reg_from_rva(Reg64::Rcx, rva_ds_queue);
    asm.call_com_rcx_slot(5);

    // Log streamed buffer via OutputDebugStringA
    asm.lea_rip_rva(Reg64::Rcx, rva_stream_buf);
    asm.call_iat(iat_rvas["OutputDebugStringA"]);

    // 4. XInput polling & rumble
    asm.xor_reg32_self(Reg64::Rcx);
    asm.lea_rip_rva(Reg64::Rdx, rva_xinput_state);
    asm.call_iat(iat_rvas["XInputGetState"]);
    asm.test_eax_eax();
    fail.push(asm.jne_placeholder());

    asm.xor_reg32_self(Reg64::Rcx);
    asm.lea_rip_rva(Reg64::Rdx, rva_vibration);
    asm.call_iat(iat_rvas["XInputSetState"]);

    // 5. XAudio2 engine + mastering voice + source voice + SubmitSourceBuffer + Start
    asm.lea_rip_rva(Reg64::Rcx, rva_xaudio);
    asm.xor_reg32_self(Reg64::Rdx);
    asm.xor_reg32_self(Reg64::R8);
    asm.call_iat(iat_rvas["XAudio2Create"]);
    asm.test_eax_eax();
    fail.push(asm.js_placeholder());

    // xaudio->CreateMasteringVoice(&mv, 2, 48000, 0) [slot 3]
    asm.mov_rsp_off_imm32(0x20, 0);
    asm.mov_reg_from_rva(Reg64::Rcx, rva_xaudio);
    asm.lea_rip_rva(Reg64::Rdx, rva_master_voice);
    asm.mov_reg_imm32(Reg64::R8, 2);
    asm.mov_reg_imm32(Reg64::R9, 48_000);
    asm.call_com_rcx_slot(3);

    // xaudio->CreateSourceVoice(&sv, &wave_fmt, 0) [slot 4]
    asm.mov_reg_from_rva(Reg64::Rcx, rva_xaudio);
    asm.lea_rip_rva(Reg64::Rdx, rva_source_voice);
    asm.lea_rip_rva(Reg64::R8, rva_wave_fmt);
    asm.xor_reg32_self(Reg64::R9);
    asm.call_com_rcx_slot(4);

    // sv->SubmitSourceBuffer(&xa_buf) [slot 5], sv->Start(0) [slot 3]
    asm.mov_reg_from_rva(Reg64::Rcx, rva_source_voice);
    asm.lea_rip_rva(Reg64::Rdx, rva_xa_buf);
    asm.call_com_rcx_slot(5);
    asm.mov_reg_from_rva(Reg64::Rcx, rva_source_voice);
    asm.xor_reg32_self(Reg64::Rdx);
    asm.call_com_rcx_slot(3);

    // 6. Phase 3 D3D12.x: Device + RootSig + GraphicsPSO (Xbox DXBC strip!) + Dispatch + Draw + FSR PresentX
    asm.lea_rip_rva(Reg64::Rcx, rva_dev_params);
    asm.lea_rip_rva(Reg64::Rdx, rva_iid_device);
    asm.lea_rip_rva(Reg64::R8, rva_device_ptr);
    asm.call_iat(iat_rvas["D3D12XBOX_CreateDevice"]);
    asm.test_eax_eax();
    fail.push(asm.js_placeholder());

    asm.mov_reg_from_rva(Reg64::Rcx, rva_device_ptr);
    asm.lea_rip_rva(Reg64::Rdx, rva_queue_desc);
    asm.lea_rip_rva(Reg64::R8, rva_iid_queue);
    asm.lea_rip_rva(Reg64::R9, rva_queue_ptr);
    asm.call_com_rcx_slot(3);

    asm.mov_reg_from_rva(Reg64::Rcx, rva_device_ptr);
    asm.xor_reg32_self(Reg64::Rdx);
    asm.lea_rip_rva(Reg64::R8, rva_iid_alloc);
    asm.lea_rip_rva(Reg64::R9, rva_alloc_ptr);
    asm.call_com_rcx_slot(4);

    asm.mov_rsp_off_imm32(0x20, 0);
    asm.lea_rip_rva(Reg64::Rax, rva_iid_cmdlist);
    asm.mov_rsp_off_reg(0x28, Reg64::Rax);
    asm.lea_rip_rva(Reg64::Rax, rva_cmdlist_ptr);
    asm.mov_rsp_off_reg(0x30, Reg64::Rax);
    asm.mov_reg_from_rva(Reg64::Rcx, rva_device_ptr);
    asm.xor_reg32_self(Reg64::Rdx);
    asm.xor_reg32_self(Reg64::R8);
    asm.mov_reg_from_rva(Reg64::R9, rva_alloc_ptr);
    asm.call_com_rcx_slot(5);

    // device->CreateRootSignature(0, &xbox_shader, len, &IID, &rootsig) [slot 12]
    asm.lea_rip_rva(Reg64::Rax, rva_iid_rootsig);
    asm.mov_rsp_off_reg(0x20, Reg64::Rax);
    asm.lea_rip_rva(Reg64::Rax, rva_rootsig_ptr);
    asm.mov_rsp_off_reg(0x28, Reg64::Rax);
    asm.mov_reg_from_rva(Reg64::Rcx, rva_device_ptr);
    asm.xor_reg32_self(Reg64::Rdx);
    asm.lea_rip_rva(Reg64::R8, rva_xbox_shader);
    asm.mov_reg_imm32(Reg64::R9, xbox_shader_len as u32);
    asm.call_com_rcx_slot(12);

    // device->CreateGraphicsPipelineState(&pso_desc, &IID, &pso) [slot 13]
    asm.mov_reg_from_rva(Reg64::Rcx, rva_device_ptr);
    asm.lea_rip_rva(Reg64::Rdx, rva_pso_desc);
    asm.lea_rip_rva(Reg64::R8, rva_iid_pso);
    asm.lea_rip_rva(Reg64::R9, rva_pso_ptr);
    asm.call_com_rcx_slot(13);

    asm.mov_reg_from_rva(Reg64::Rcx, rva_device_ptr);
    asm.lea_rip_rva(Reg64::Rdx, rva_rtv_heap_desc);
    asm.lea_rip_rva(Reg64::R8, rva_iid_descheap);
    asm.lea_rip_rva(Reg64::R9, rva_rtv_heap_ptr);
    asm.call_com_rcx_slot(6);

    asm.mov_rsp_off_imm32(0x20, D3D12_RESOURCE_STATE_RENDER_TARGET as i32);
    asm.mov_rsp_off_imm32(0x28, 0);
    asm.lea_rip_rva(Reg64::Rax, rva_iid_resource);
    asm.mov_rsp_off_reg(0x30, Reg64::Rax);
    asm.lea_rip_rva(Reg64::Rax, rva_rt_res_ptr);
    asm.mov_rsp_off_reg(0x38, Reg64::Rax);
    asm.mov_reg_from_rva(Reg64::Rcx, rva_device_ptr);
    asm.lea_rip_rva(Reg64::Rdx, rva_heap_props);
    asm.mov_reg_imm32(
        Reg64::R8,
        D3D12XBOX_HEAP_FLAG_GRAPHICS_WRITABLE | D3D12XBOX_HEAP_FLAG_CPU_CACHED,
    );
    asm.lea_rip_rva(Reg64::R9, rva_rt_desc);
    asm.call_com_rcx_slot(8);

    asm.mov_reg_from_rva(Reg64::Rcx, rva_rtv_heap_ptr);
    asm.call_com_rcx_slot(3);
    asm.mov_rva_from_reg(rva_rtv_handle, Reg64::Rax);

    asm.mov_reg_from_rva(Reg64::Rcx, rva_device_ptr);
    asm.mov_reg_from_rva(Reg64::Rdx, rva_rt_res_ptr);
    asm.xor_reg32_self(Reg64::R8);
    asm.mov_reg_from_rva(Reg64::R9, rva_rtv_handle);
    asm.call_com_rcx_slot(9);

    // Bind RootSignature [slot 14] & PSO [slot 13]
    asm.mov_reg_from_rva(Reg64::Rcx, rva_cmdlist_ptr);
    asm.mov_reg_from_rva(Reg64::Rdx, rva_rootsig_ptr);
    asm.call_com_rcx_slot(14);

    asm.mov_reg_from_rva(Reg64::Rcx, rva_cmdlist_ptr);
    asm.mov_reg_from_rva(Reg64::Rdx, rva_pso_ptr);
    asm.call_com_rcx_slot(13);

    // Render triangle + Dispatch compute [slot 16]
    asm.mov_rsp_off_imm32(0x20, 0);
    asm.mov_reg_from_rva(Reg64::Rcx, rva_cmdlist_ptr);
    asm.mov_reg_imm32(Reg64::Rdx, 1);
    asm.lea_rip_rva(Reg64::R8, rva_rtv_handle);
    asm.xor_reg32_self(Reg64::R9);
    asm.call_com_rcx_slot(8);

    asm.mov_reg_from_rva(Reg64::Rcx, rva_cmdlist_ptr);
    asm.mov_reg_imm32(Reg64::Rdx, 1);
    asm.lea_rip_rva(Reg64::R8, rva_viewport);
    asm.call_com_rcx_slot(6);

    asm.mov_rsp_off_imm32(0x20, 0);
    asm.mov_reg_from_rva(Reg64::Rcx, rva_cmdlist_ptr);
    asm.mov_reg_from_rva(Reg64::Rdx, rva_rtv_handle);
    asm.lea_rip_rva(Reg64::R8, rva_clear_color);
    asm.xor_reg32_self(Reg64::R9);
    asm.call_com_rcx_slot(9);

    asm.mov_reg_from_rva(Reg64::Rcx, rva_cmdlist_ptr);
    asm.mov_reg_imm32(Reg64::Rdx, D3DPrimitiveTopology::TriangleList as u32);
    asm.call_com_rcx_slot(10);

    asm.mov_reg_from_rva(Reg64::Rcx, rva_cmdlist_ptr);
    asm.xor_reg32_self(Reg64::Rdx);
    asm.mov_reg_imm32(Reg64::R8, 1);
    asm.lea_rip_rva(Reg64::R9, rva_vb_view);
    asm.call_com_rcx_slot(11);

    asm.mov_rsp_off_imm32(0x20, 0);
    asm.mov_reg_from_rva(Reg64::Rcx, rva_cmdlist_ptr);
    asm.mov_reg_imm32(Reg64::Rdx, 3);
    asm.mov_reg_imm32(Reg64::R8, 1);
    asm.xor_reg32_self(Reg64::R9);
    asm.call_com_rcx_slot(12);

    // Compute Dispatch(40, 23, 1) [slot 16]
    asm.mov_reg_from_rva(Reg64::Rcx, rva_cmdlist_ptr);
    asm.mov_reg_imm32(Reg64::Rdx, 40);
    asm.mov_reg_imm32(Reg64::R8, 23);
    asm.mov_reg_imm32(Reg64::R9, 1);
    asm.call_com_rcx_slot(16);

    asm.mov_reg_from_rva(Reg64::Rcx, rva_cmdlist_ptr);
    asm.call_com_rcx_slot(3);

    asm.mov_reg_from_rva(Reg64::Rcx, rva_queue_ptr);
    asm.mov_reg_imm32(Reg64::Rdx, 1);
    asm.lea_rip_rva(Reg64::R8, rva_cmdlist_ptr);
    asm.call_com_rcx_slot(3);

    asm.mov_reg_from_rva(Reg64::Rcx, rva_queue_ptr);
    asm.mov_reg_imm32(Reg64::Rdx, 1);
    asm.lea_rip_rva(Reg64::R8, rva_present_plane);
    asm.xor_reg32_self(Reg64::R9);
    asm.call_com_rcx_slot(5);

    asm.lea_rip_rva(Reg64::Rcx, rva_msg_done);
    asm.call_iat(iat_rvas["OutputDebugStringA"]);

    // Cleanly release COM & GDK handles
    for ptr_rva in [
        rva_pso_ptr,
        rva_rootsig_ptr,
        rva_rt_res_ptr,
        rva_rtv_heap_ptr,
        rva_cmdlist_ptr,
        rva_alloc_ptr,
        rva_queue_ptr,
        rva_device_ptr,
        rva_source_voice,
        rva_master_voice,
        rva_xaudio,
        rva_ds_file,
        rva_ds_queue,
        rva_ds_factory,
    ] {
        asm.mov_reg_from_rva(Reg64::Rcx, ptr_rva);
        asm.call_com_rcx_slot(2);
    }

    asm.mov_reg_from_rva(Reg64::Rcx, rva_update_h);
    asm.call_iat(iat_rvas["XGameSaveCloseUpdate"]);
    asm.mov_reg_from_rva(Reg64::Rcx, rva_container_h);
    asm.call_iat(iat_rvas["XGameSaveCloseContainer"]);
    asm.mov_reg_from_rva(Reg64::Rcx, rva_provider_h);
    asm.call_iat(iat_rvas["XGameSaveCloseProvider"]);
    asm.mov_reg_from_rva(Reg64::Rcx, rva_store_h);
    asm.call_iat(iat_rvas["XStoreCloseContext"]);
    asm.mov_reg_from_rva(Reg64::Rcx, rva_user_h);
    asm.call_iat(iat_rvas["XUserCloseHandle"]);
    asm.call_iat(iat_rvas["XGameRuntimeUninitialize"]);

    asm.xor_reg32_self(Reg64::Rax);
    asm.add_rsp_imm8(0x68);
    asm.pop(Reg64::Rdi);
    asm.pop(Reg64::Rsi);
    asm.pop(Reg64::Rbx);
    asm.pop(Reg64::Rbp);
    asm.ret();

    for p in fail {
        asm.patch_branch_here(p);
    }
    asm.mov_reg_imm32(Reg64::Rax, 1);
    asm.add_rsp_imm8(0x68);
    asm.pop(Reg64::Rdi);
    asm.pop(Reg64::Rsi);
    asm.pop(Reg64::Rbx);
    asm.pop(Reg64::Rbp);
    asm.ret();

    let text_bytes = asm.finish();

    let sections = [
        SectionSpec {
            name: *b".text\0\0\0",
            rva: TEXT_RVA,
            data: &text_bytes,
            characteristics: IMAGE_SCN_MEM_READ | IMAGE_SCN_MEM_EXECUTE | 0x0000_0020,
        },
        SectionSpec {
            name: *b".rdata\0\0",
            rva: RDATA_RVA,
            data: &rdata_bytes,
            characteristics: IMAGE_SCN_MEM_READ | 0x0000_0040,
        },
        SectionSpec {
            name: *b".data\0\0\0",
            rva: DATA_RVA,
            data: &data_bytes,
            characteristics: IMAGE_SCN_MEM_READ | IMAGE_SCN_MEM_WRITE | 0x0000_0040,
        },
        SectionSpec {
            name: *b".idata\0\0",
            rva: IDATA_RVA,
            data: &idata_bytes,
            characteristics: IMAGE_SCN_MEM_READ | IMAGE_SCN_MEM_WRITE | 0x0000_0040,
        },
        SectionSpec {
            name: *b".reloc\0\0",
            rva: RELOC_RVA,
            data: &reloc_bytes,
            characteristics: IMAGE_SCN_MEM_READ | 0x0200_0040,
        },
    ];

    let mut dirs = [(0u32, 0u32); 16];
    dirs[IMAGE_DIRECTORY_ENTRY_IMPORT] = (IDATA_RVA, idata_dir_size);
    dirs[IMAGE_DIRECTORY_ENTRY_BASERELOC] = (RELOC_RVA, reloc_bytes.len() as u32);
    emit_pe32_plus(&sections, TEXT_RVA, &dirs)
}

struct SectionSpec<'a> {
    name: [u8; 8],
    rva: u32,
    data: &'a [u8],
    characteristics: u32,
}

struct DataBuilder {
    base_rva: u32,
    buf: Vec<u8>,
}

impl DataBuilder {
    fn new(base_rva: u32) -> Self {
        Self {
            base_rva,
            buf: Vec::new(),
        }
    }

    fn align(&mut self, a: usize) {
        while self.buf.len() % a != 0 {
            self.buf.push(0);
        }
    }

    fn push_cstr(&mut self, s: &str) -> u32 {
        let rva = self.base_rva + self.buf.len() as u32;
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
        rva
    }

    fn push_bytes(&mut self, b: &[u8]) -> u32 {
        self.align(8);
        let rva = self.base_rva + self.buf.len() as u32;
        self.buf.extend_from_slice(b);
        rva
    }

    fn push_zeroed(&mut self, n: usize) -> u32 {
        self.align(8);
        let rva = self.base_rva + self.buf.len() as u32;
        self.buf.resize(self.buf.len() + n, 0);
        rva
    }

    fn push_u64(&mut self, v: u64) -> u32 {
        self.align(8);
        let rva = self.base_rva + self.buf.len() as u32;
        self.buf.extend_from_slice(&v.to_le_bytes());
        rva
    }

    fn push_struct<T>(&mut self, val: &T) -> u32 {
        self.align(core::mem::align_of::<T>().max(8));
        let rva = self.base_rva + self.buf.len() as u32;
        let bytes = unsafe {
            core::slice::from_raw_parts((val as *const T) as *const u8, core::mem::size_of::<T>())
        };
        self.buf.extend_from_slice(bytes);
        rva
    }

    fn finish(self) -> Vec<u8> {
        self.buf
    }
}

fn build_idata(base_rva: u32, spec: &[(&str, &[&str])]) -> (Vec<u8>, HashMap<String, u32>, u32) {
    let mut buf = Vec::new();
    let idt_size = (spec.len() + 1) * 20;
    buf.resize(idt_size, 0);
    while buf.len() % 8 != 0 {
        buf.push(0);
    }

    let mut ilt_offsets = Vec::new();
    let mut iat_offsets = Vec::new();
    let mut iat_rvas = HashMap::new();

    for &(_, funcs) in spec {
        let ilt_off = buf.len();
        ilt_offsets.push(ilt_off);
        buf.resize(buf.len() + (funcs.len() + 1) * 8, 0);

        let iat_off = buf.len();
        iat_offsets.push(iat_off);
        for (i, &fn_name) in funcs.iter().enumerate() {
            iat_rvas.insert(fn_name.to_string(), base_rva + (iat_off + i * 8) as u32);
        }
        buf.resize(buf.len() + (funcs.len() + 1) * 8, 0);
    }

    for (dll_idx, &(dll_name, funcs)) in spec.iter().enumerate() {
        let dll_name_rva = base_rva + buf.len() as u32;
        buf.extend_from_slice(dll_name.as_bytes());
        buf.push(0);

        let ilt_off = ilt_offsets[dll_idx];
        let iat_off = iat_offsets[dll_idx];
        for (fn_idx, &fn_name) in funcs.iter().enumerate() {
            if buf.len() % 2 != 0 {
                buf.push(0);
            }
            let hn_rva = base_rva + buf.len() as u32;
            buf.extend_from_slice(&0u16.to_le_bytes());
            buf.extend_from_slice(fn_name.as_bytes());
            buf.push(0);

            let thunk_bytes = (hn_rva as u64).to_le_bytes();
            buf[ilt_off + fn_idx * 8..ilt_off + (fn_idx + 1) * 8].copy_from_slice(&thunk_bytes);
            buf[iat_off + fn_idx * 8..iat_off + (fn_idx + 1) * 8].copy_from_slice(&thunk_bytes);
        }

        let desc_off = dll_idx * 20;
        let ilt_rva = base_rva + ilt_off as u32;
        let iat_rva = base_rva + iat_off as u32;
        buf[desc_off..desc_off + 4].copy_from_slice(&ilt_rva.to_le_bytes());
        buf[desc_off + 12..desc_off + 16].copy_from_slice(&dll_name_rva.to_le_bytes());
        buf[desc_off + 16..desc_off + 20].copy_from_slice(&iat_rva.to_le_bytes());
    }

    (buf, iat_rvas, idt_size as u32)
}

fn build_reloc_table(rvas: &[u32]) -> Vec<u8> {
    let mut by_page: HashMap<u32, Vec<u16>> = HashMap::new();
    for &rva in rvas {
        let page = rva & !0xFFF;
        let off = (rva & 0xFFF) as u16;
        by_page
            .entry(page)
            .or_default()
            .push((IMAGE_REL_BASED_DIR64 << 12) | off);
    }
    let mut pages: Vec<u32> = by_page.keys().copied().collect();
    pages.sort_unstable();

    let mut buf = Vec::new();
    for page in pages {
        let mut entries = by_page.remove(&page).unwrap();
        if entries.len() % 2 != 0 {
            entries.push(0);
        }
        let block_size = (8 + entries.len() * 2) as u32;
        buf.extend_from_slice(&page.to_le_bytes());
        buf.extend_from_slice(&block_size.to_le_bytes());
        for e in entries {
            buf.extend_from_slice(&e.to_le_bytes());
        }
    }
    buf
}

fn emit_pe32_plus(sections: &[SectionSpec<'_>], entry_rva: u32, dirs: &[(u32, u32); 16]) -> Vec<u8> {
    let size_of_headers: u32 = 0x400;
    let mut file = vec![0u8; size_of_headers as usize];

    file[0..2].copy_from_slice(b"MZ");
    let pe_off: usize = 0x80;
    write_u32(&mut file, 0x3C, pe_off as u32);
    file[pe_off..pe_off + 4].copy_from_slice(b"PE\0\0");

    let coff = pe_off + 4;
    let opt_size: u16 = 240;
    write_u16(&mut file, coff, IMAGE_FILE_MACHINE_AMD64);
    write_u16(&mut file, coff + 2, sections.len() as u16);
    write_u16(&mut file, coff + 16, opt_size);
    write_u16(&mut file, coff + 18, 0x0022);

    let opt = coff + 20;
    write_u16(&mut file, opt, PE32_PLUS_MAGIC);
    write_u32(&mut file, opt + 16, entry_rva);
    write_u64(&mut file, opt + 24, EXE_BASE);
    write_u32(&mut file, opt + 32, SECTION_ALIGN);
    write_u32(&mut file, opt + 36, FILE_ALIGN);
    write_u16(&mut file, opt + 40, 10);
    write_u16(&mut file, opt + 48, 10);
    write_u32(&mut file, opt + 56, SIZE_OF_IMAGE);
    write_u32(&mut file, opt + 60, size_of_headers);
    write_u16(&mut file, opt + 68, 3);
    write_u16(&mut file, opt + 70, 0x0160);
    write_u32(&mut file, opt + 108, 16);

    for (i, &(rva, size)) in dirs.iter().enumerate() {
        write_u32(&mut file, opt + 112 + i * 8, rva);
        write_u32(&mut file, opt + 112 + i * 8 + 4, size);
    }

    let sec_table = opt + opt_size as usize;
    for (i, sec) in sections.iter().enumerate() {
        let raw_ptr = file.len() as u32;
        let raw_size = align_up(sec.data.len() as u32, FILE_ALIGN);
        file.extend_from_slice(sec.data);
        file.resize(raw_ptr as usize + raw_size as usize, 0);

        let hdr = sec_table + i * 40;
        file[hdr..hdr + 8].copy_from_slice(&sec.name);
        write_u32(&mut file, hdr + 8, sec.data.len() as u32);
        write_u32(&mut file, hdr + 12, sec.rva);
        write_u32(&mut file, hdr + 16, raw_size);
        write_u32(&mut file, hdr + 20, raw_ptr);
        write_u32(&mut file, hdr + 36, sec.characteristics);
    }
    file
}

fn align_up(v: u32, align: u32) -> u32 {
    (v + align - 1) & !(align - 1)
}

fn write_u16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

fn write_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn write_u64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
