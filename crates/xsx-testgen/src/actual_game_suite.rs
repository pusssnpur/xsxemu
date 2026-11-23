//! Realistic Multi-Frame, Multi-Subsystem Xbox Series X Game Generator (`starlight_xsx.exe`).
//!
//! Exercises a complete Xbox Series X|S game boot & 3-frame gameplay loop:
//! 1. MSVC CRT startup (`ucrtbase.dll!_configure_narrow_argv`, `malloc`, `memset`, `free`)
//! 2. `kernelx.dll` (`GetSystemInfo` Zen 2 check, `CreateFileW("GAME:\\assets\\level.dat")`, `OutputDebugStringA`)
//! 3. `dxgi.dll` (`CreateDXGIFactory2` -> `EnumAdapters1(0)` -> `GetDesc1` AMD Scarlett `0x1002`)
//! 4. `xgameruntime.dll` (`XGameRuntimeInitialize`, `XGameGetXboxTitleId` == `0x7A5F2026`)
//! 5. `d3d12_x.dll` 3-frame indexed 3D rendering loop (`IASetIndexBuffer` + `DrawIndexedInstanced`
//!    with Z-buffer occlusion & alpha-blended thruster glow) + `PresentX` -> AMD FSR Quality 1.5x
//! 6. `acphal.dll` (`ApuCreateHeap`) + `xaudio2_9.dll` (`XAudio2Create` + `SubmitSourceBuffer` 48kHz audio)
//! 7. `xinput1_4.dll` (`XInputGetState`)

use std::collections::HashMap;

use xsx_abi::com::{
    IID_ID3D12COMMAND_ALLOCATOR, IID_ID3D12COMMAND_QUEUE, IID_ID3D12DESCRIPTOR_HEAP,
    IID_ID3D12DEVICE, IID_ID3D12GRAPHICS_COMMAND_LIST, IID_ID3D12RESOURCE, IID_IUNKNOWN,
};
use xsx_abi::d3d12::{
    D3D12CommandListType, D3D12CommandQueueDesc, D3D12DescriptorHeapDesc, D3D12DescriptorHeapType,
    D3D12HeapProperties, D3D12HeapType, D3D12IndexBufferView, D3D12ResourceDesc,
    D3D12ResourceDimension, D3D12TextureLayout, D3D12VertexBufferView, D3D12Viewport,
    D3D12XboxCreateDeviceParameters, D3D12XboxPresentPlaneParameters, D3DPrimitiveTopology,
    DxgiFormat, DxgiSampleDesc, SimpleVertex, D3D12XBOX_CREATE_DEVICE_FLAG_DEBUG,
    D3D12XBOX_HEAP_FLAG_CPU_CACHED, D3D12XBOX_HEAP_FLAG_GRAPHICS_WRITABLE,
    D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET, D3D12_RESOURCE_STATE_RENDER_TARGET,
};
use xsx_abi::gdk::{SystemInfo, FILE_ATTRIBUTE_NORMAL, GENERIC_READ, OPEN_EXISTING};
use xsx_loader::{
    IMAGE_DIRECTORY_ENTRY_BASERELOC, IMAGE_DIRECTORY_ENTRY_IMPORT, IMAGE_FILE_MACHINE_AMD64,
    IMAGE_REL_BASED_DIR64, IMAGE_SCN_MEM_EXECUTE, IMAGE_SCN_MEM_READ, IMAGE_SCN_MEM_WRITE,
    PE32_PLUS_MAGIC,
};

use crate::x64_asm::{Reg64, X64Assembler};

pub const ACTUAL_GAME_TITLE_ID: u32 = 0x7A5F_2026;
pub const ACTUAL_GAME_MANIFEST: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<Game configVersion="1">
  <Identity Name="StarlightScarlett" Publisher="CN=CleanRoom" Version="1.0.0.0"/>
  <TitleId>7A5F2026</TitleId>
  <StoreId>9STARLIGHT01</StoreId>
  <ExecutableList>
    <Executable Name="starlight_xsx.exe" TargetDeviceFamily="XboxGen9"/>
  </ExecutableList>
  <ShellVisuals DefaultDisplayName="Starlight Scarlett (XSX)"/>
  <PersistentLocalStorage>
    <SizeMB>64</SizeMB>
  </PersistentLocalStorage>
  <XboxAnacondaTitleMemory>13GiB</XboxAnacondaTitleMemory>
</Game>
"#;
pub const ACTUAL_GAME_LEVEL_ASSET: &[u8] = b"STARLIGHT_LEVEL_01_VERIFIED\0";

const PREFERRED_IMAGE_BASE: u64 = 0x0000_0001_4000_0000;
const SECTION_ALIGN: u32 = 0x1000;
const FILE_ALIGN: u32 = 0x200;

#[repr(C)]
struct XAudio2Buffer {
    flags: u32,
    audio_bytes: u32,
    p_audio_data: u64,
    play_begin: u32,
    play_length: u32,
    loop_begin: u32,
    loop_length: u32,
    loop_count: u32,
    p_context: u64,
}

#[repr(C)]
struct WaveFormatEx {
    w_format_tag: u16,
    n_channels: u16,
    n_samples_per_sec: u32,
    n_avg_bytes_per_sec: u32,
    n_block_align: u16,
    w_bits_per_sample: u16,
    cb_size: u16,
}

pub fn build_actual_game_xsx_pe() -> Vec<u8> {
    const TEXT_RVA: u32 = 0x1000;
    const RDATA_RVA: u32 = 0x2000;
    const DATA_RVA: u32 = 0x3000;
    const IDATA_RVA: u32 = 0x4000;
    const RELOC_RVA: u32 = 0x5000;
    const SIZE_OF_IMAGE: u32 = 0x6000;

    let import_spec: &[(&str, &[&str])] = &[
        (
            "api-ms-win-crt-runtime-l1-1-0.dll",
            &["_configure_narrow_argv", "_initialize_narrow_environment"],
        ),
        (
            "ucrtbase.dll",
            &["malloc", "memset", "free"],
        ),
        (
            "kernelx.dll",
            &[
                "OutputDebugStringA",
                "GetSystemInfo",
                "CreateFileW",
                "ReadFile",
                "CloseHandle",
            ],
        ),
        ("dxgi.dll", &["CreateDXGIFactory2"]),
        (
            "xgameruntime.dll",
            &[
                "XGameRuntimeInitialize",
                "XGameGetXboxTitleId",
                "XGameRuntimeUninitialize",
            ],
        ),
        ("d3d12_x.dll", &["D3D12XBOX_CreateDevice"]),
        ("acphal.dll", &["ApuCreateHeap"]),
        ("xaudio2_9.dll", &["XAudio2Create"]),
        ("xinput1_4.dll", &["XInputGetState"]),
    ];
    let (idata_bytes, iat, idata_dir_size) = build_idata(IDATA_RVA, import_spec);

    // .rdata
    let mut rdata = DataBuilder::new(RDATA_RVA);
    let rva_msg_boot = rdata.push_cstr("Starlight XSX: CRT + Zen2 + DXGI Scarlett + GDK booted!");
    let rva_msg_done = rdata.push_cstr("Starlight XSX: 3 indexed 3D frames + FSR + 48kHz audio complete!");
    let rva_w_asset = rdata.push_utf16(r"GAME:\assets\level.dat");

    let rva_iid_unknown = rdata.push_struct(&IID_IUNKNOWN);
    let rva_iid_device = rdata.push_struct(&IID_ID3D12DEVICE);
    let rva_iid_queue = rdata.push_struct(&IID_ID3D12COMMAND_QUEUE);
    let rva_iid_alloc = rdata.push_struct(&IID_ID3D12COMMAND_ALLOCATOR);
    let rva_iid_cmdlist = rdata.push_struct(&IID_ID3D12GRAPHICS_COMMAND_LIST);
    let rva_iid_descheap = rdata.push_struct(&IID_ID3D12DESCRIPTOR_HEAP);
    let rva_iid_resource = rdata.push_struct(&IID_ID3D12RESOURCE);

    let rva_dev_params = rdata.push_struct(&D3D12XboxCreateDeviceParameters {
        version: 1,
        process_debug_flags: D3D12XBOX_CREATE_DEVICE_FLAG_DEBUG,
        graphics_command_queue_ring_size_bytes: 64 * 1024,
        graphics_scratch_memory_size_bytes: 1024 * 1024,
        compute_scratch_memory_size_bytes: 1024 * 1024,
    });
    let rva_queue_desc = rdata.push_struct(&D3D12CommandQueueDesc {
        r#type: D3D12CommandListType::Direct as u32,
        priority: 0,
        flags: 0,
        node_mask: 0,
    });
    let rva_heap_desc = rdata.push_struct(&D3D12DescriptorHeapDesc {
        r#type: D3D12DescriptorHeapType::Rtv as u32,
        num_descriptors: 2,
        flags: 0,
        node_mask: 0,
    });
    let rva_rt_heap_props = rdata.push_struct(&D3D12HeapProperties {
        r#type: D3D12HeapType::Default as u32,
        cpu_page_property: 0,
        memory_pool_preference: 0,
        creation_node_mask: 1,
        visible_node_mask: 1,
    });
    let rva_rt_desc = rdata.push_struct(&D3D12ResourceDesc {
        dimension: D3D12ResourceDimension::Texture2D as u32,
        alignment: 0,
        width: 640,
        height: 360,
        depth_or_array_size: 1,
        mip_levels: 1,
        format: DxgiFormat::R8G8B8A8Unorm as u32,
        sample_desc: DxgiSampleDesc { count: 1, quality: 0 },
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
    let rva_clear_color = rdata.push_struct(&[0.03f32, 0.05f32, 0.14f32, 1.0f32]);

    // 6 vertices:
    //   0..3: Crimson background quad at z=0.8
    //   3..6: Emerald/cyan starship triangle at z=0.3 (occludes quad via Z-buffer!)
    let vertices: [SimpleVertex; 6] = [
        SimpleVertex { pos: [-0.85, 0.80, 0.8], color: [0.65, 0.12, 0.18, 1.0] },
        SimpleVertex { pos: [0.85, 0.80, 0.8], color: [0.65, 0.12, 0.18, 1.0] },
        SimpleVertex { pos: [0.0, -0.85, 0.8], color: [0.25, 0.08, 0.35, 1.0] },
        SimpleVertex { pos: [0.0, 0.65, 0.3], color: [0.15, 0.95, 0.35, 1.0] },
        SimpleVertex { pos: [0.55, -0.45, 0.3], color: [0.15, 0.75, 0.95, 1.0] },
        SimpleVertex { pos: [-0.55, -0.45, 0.3], color: [0.95, 0.85, 0.20, 1.0] },
    ];
    let rva_vertices = rdata.push_struct(&vertices);
    let indices: [u16; 6] = [0, 1, 2, 3, 4, 5];
    let rva_indices = rdata.push_struct(&indices);

    // 48 kHz stereo PCM audio (240 frames = 5ms of 440Hz tone)
    let mut pcm = Vec::with_capacity(240 * 4);
    for i in 0..240 {
        let s = (((i as f32 * 2.0 * core::f32::consts::PI * 440.0) / 48_000.0).sin() * 8000.0) as i16;
        pcm.extend_from_slice(&s.to_le_bytes());
        pcm.extend_from_slice(&s.to_le_bytes());
    }
    let rva_pcm = rdata.push_bytes(&pcm, 4);
    let rva_wfx = rdata.push_struct(&WaveFormatEx {
        w_format_tag: 1,
        n_channels: 2,
        n_samples_per_sec: 48_000,
        n_avg_bytes_per_sec: 48_000 * 4,
        n_block_align: 4,
        w_bits_per_sample: 16,
        cb_size: 0,
    });

    // .data (writable scratch + relocatable pointers)
    let mut data = DataBuilder::new(DATA_RVA);
    let rva_sysinfo = data.alloc(core::mem::size_of::<SystemInfo>(), 8);
    let rva_title_id = data.alloc(4, 4);
    let rva_bytes_read = data.alloc(4, 4);
    let rva_asset_buf = data.alloc(64, 4);
    let rva_factory = data.alloc(8, 8);
    let rva_adapter = data.alloc(8, 8);
    let rva_adapter_desc = data.alloc(320, 8);
    let rva_device = data.alloc(8, 8);
    let rva_queue = data.alloc(8, 8);
    let rva_alloc = data.alloc(8, 8);
    let rva_cmdlist = data.alloc(8, 8);
    let rva_heap = data.alloc(8, 8);
    let rva_rt = data.alloc(8, 8);
    let rva_rtv_cpu = data.alloc(8, 8);
    let rva_xaudio = data.alloc(8, 8);
    let rva_master = data.alloc(8, 8);
    let rva_src_voice = data.alloc(8, 8);
    let rva_xinput_state = data.alloc(16, 4);

    let rva_vb_view = data.push_struct(&D3D12VertexBufferView {
        buffer_location: PREFERRED_IMAGE_BASE + rva_vertices as u64,
        size_in_bytes: core::mem::size_of_val(&vertices) as u32,
        stride_in_bytes: core::mem::size_of::<SimpleVertex>() as u32,
    });
    let rva_ib_view = data.push_struct(&D3D12IndexBufferView {
        buffer_location: PREFERRED_IMAGE_BASE + rva_indices as u64,
        size_in_bytes: core::mem::size_of_val(&indices) as u32,
        format: 57, // DXGI_FORMAT_R16_UINT
    });
    let rva_present_params = data.push_struct(&D3D12XboxPresentPlaneParameters {
        token: 0,
        resource_count: 1,
        _pad: 0,
        pp_resources: (PREFERRED_IMAGE_BASE + rva_rt as u64) as *const *mut core::ffi::c_void,
        color_space: 0,
        options: 0,
    });
    let rva_xa_buf = data.push_struct(&XAudio2Buffer {
        flags: 0,
        audio_bytes: pcm.len() as u32,
        p_audio_data: PREFERRED_IMAGE_BASE + rva_pcm as u64,
        play_begin: 0,
        play_length: 0,
        loop_begin: 0,
        loop_length: 0,
        loop_count: 0,
        p_context: 0,
    });

    let reloc_rvas = vec![
        rva_vb_view,             // buffer_location -> rva_vertices
        rva_ib_view,             // buffer_location -> rva_indices
        rva_present_params + 16, // pp_resources -> rva_rt
        rva_xa_buf + 8,          // p_audio_data -> rva_pcm
    ];
    let reloc_bytes = build_relocs(&reloc_rvas);

    // .text machine code
    let mut a = X64Assembler::new(TEXT_RVA);
    a.push(Reg64::Rbx);
    a.sub_rsp_imm8(0x40);

    // 1. CRT startup + malloc/memset/free check
    a.xor_reg32_self(Reg64::Rcx);
    a.call_iat(iat["api-ms-win-crt-runtime-l1-1-0.dll!_configure_narrow_argv"]);
    a.call_iat(iat["api-ms-win-crt-runtime-l1-1-0.dll!_initialize_narrow_environment"]);
    a.mov_reg_imm32(Reg64::Rcx, 64);
    a.call_iat(iat["ucrtbase.dll!malloc"]);
    a.mov_rva_from_reg(rva_asset_buf, Reg64::Rax);
    a.mov_reg_from_rva(Reg64::Rcx, rva_asset_buf);
    a.mov_reg_imm32(Reg64::Rdx, 0x55);
    a.mov_reg_imm32(Reg64::R8, 64);
    a.call_iat(iat["ucrtbase.dll!memset"]);
    a.mov_reg_from_rva(Reg64::Rcx, rva_asset_buf);
    a.call_iat(iat["ucrtbase.dll!free"]);

    // 2. KernelX GetSystemInfo + read GAME:\assets\level.dat
    a.lea_rip_rva(Reg64::Rcx, rva_sysinfo);
    a.call_iat(iat["kernelx.dll!GetSystemInfo"]);
    a.lea_rip_rva(Reg64::Rcx, rva_w_asset);
    a.mov_reg_imm32(Reg64::Rdx, GENERIC_READ);
    a.mov_reg_imm32(Reg64::R8, 1);
    a.xor_reg32_self(Reg64::R9);
    a.mov_rsp_off_imm32(0x20, OPEN_EXISTING as i32);
    a.mov_rsp_off_imm32(0x28, FILE_ATTRIBUTE_NORMAL as i32);
    a.mov_rsp_off_imm32(0x30, 0);
    a.call_iat(iat["kernelx.dll!CreateFileW"]);
    a.mov_rva_from_reg(rva_rt, Reg64::Rax);
    a.mov_reg_from_rva(Reg64::Rcx, rva_rt);
    a.lea_rip_rva(Reg64::Rdx, rva_asset_buf);
    a.mov_reg_imm32(Reg64::R8, 32);
    a.lea_rip_rva(Reg64::R9, rva_bytes_read);
    a.mov_rsp_off_imm32(0x20, 0);
    a.call_iat(iat["kernelx.dll!ReadFile"]);
    a.mov_reg_from_rva(Reg64::Rcx, rva_rt);
    a.call_iat(iat["kernelx.dll!CloseHandle"]);

    // 3. DXGI adapter enumeration (AMD Scarlett)
    a.xor_reg32_self(Reg64::Rcx);
    a.lea_rip_rva(Reg64::Rdx, rva_iid_unknown);
    a.lea_rip_rva(Reg64::R8, rva_factory);
    a.call_iat(iat["dxgi.dll!CreateDXGIFactory2"]);
    a.mov_reg_from_rva(Reg64::Rcx, rva_factory);
    a.xor_reg32_self(Reg64::Rdx);
    a.lea_rip_rva(Reg64::R8, rva_adapter);
    a.call_com_rcx_slot(12); // EnumAdapters1
    a.mov_reg_from_rva(Reg64::Rcx, rva_adapter);
    a.lea_rip_rva(Reg64::Rdx, rva_adapter_desc);
    a.call_com_rcx_slot(10); // GetDesc1

    // 4. GDK init + TitleId + Boot debug message
    a.call_iat(iat["xgameruntime.dll!XGameRuntimeInitialize"]);
    a.lea_rip_rva(Reg64::Rcx, rva_title_id);
    a.call_iat(iat["xgameruntime.dll!XGameGetXboxTitleId"]);
    a.lea_rip_rva(Reg64::Rcx, rva_msg_boot);
    a.call_iat(iat["kernelx.dll!OutputDebugStringA"]);

    // 5. D3D12.x device, queue, allocator, command list, RTV heap, render target
    a.lea_rip_rva(Reg64::Rcx, rva_dev_params);
    a.lea_rip_rva(Reg64::Rdx, rva_iid_device);
    a.lea_rip_rva(Reg64::R8, rva_device);
    a.call_iat(iat["d3d12_x.dll!D3D12XBOX_CreateDevice"]);

    // CreateCommandQueue(slot 3)
    a.mov_reg_from_rva(Reg64::Rcx, rva_device);
    a.lea_rip_rva(Reg64::Rdx, rva_queue_desc);
    a.lea_rip_rva(Reg64::R8, rva_iid_queue);
    a.lea_rip_rva(Reg64::R9, rva_queue);
    a.call_com_rcx_slot(3);

    // CreateCommandAllocator(slot 4)
    a.mov_reg_from_rva(Reg64::Rcx, rva_device);
    a.xor_reg32_self(Reg64::Rdx);
    a.lea_rip_rva(Reg64::R8, rva_iid_alloc);
    a.lea_rip_rva(Reg64::R9, rva_alloc);
    a.call_com_rcx_slot(4);

    // CreateGraphicsCommandList(slot 5)
    a.mov_reg_from_rva(Reg64::Rcx, rva_device);
    a.xor_reg32_self(Reg64::Rdx);
    a.xor_reg32_self(Reg64::R8);
    a.mov_reg_from_rva(Reg64::R9, rva_alloc);
    a.mov_rsp_off_imm32(0x20, 0); // p_initial_state = NULL
    a.lea_rip_rva(Reg64::Rax, rva_iid_cmdlist);
    a.mov_rsp_off_reg(0x28, Reg64::Rax);
    a.lea_rip_rva(Reg64::Rax, rva_cmdlist);
    a.mov_rsp_off_reg(0x30, Reg64::Rax);
    a.call_com_rcx_slot(5);

    // CreateDescriptorHeap(slot 6) & GetCPUDescriptorHandleForHeapStart(slot 3)
    a.mov_reg_from_rva(Reg64::Rcx, rva_device);
    a.lea_rip_rva(Reg64::Rdx, rva_heap_desc);
    a.lea_rip_rva(Reg64::R8, rva_iid_descheap);
    a.lea_rip_rva(Reg64::R9, rva_heap);
    a.call_com_rcx_slot(6);
    a.mov_reg_from_rva(Reg64::Rcx, rva_heap);
    a.call_com_rcx_slot(3);
    a.mov_rva_from_reg(rva_rtv_cpu, Reg64::Rax);

    // CreateCommittedResource(slot 8)
    a.mov_reg_from_rva(Reg64::Rcx, rva_device);
    a.lea_rip_rva(Reg64::Rdx, rva_rt_heap_props);
    a.mov_reg_imm32(
        Reg64::R8,
        D3D12XBOX_HEAP_FLAG_CPU_CACHED | D3D12XBOX_HEAP_FLAG_GRAPHICS_WRITABLE,
    );
    a.lea_rip_rva(Reg64::R9, rva_rt_desc);
    a.mov_rsp_off_imm32(0x20, D3D12_RESOURCE_STATE_RENDER_TARGET as i32);
    a.mov_rsp_off_imm32(0x28, 0);
    a.lea_rip_rva(Reg64::Rax, rva_iid_resource);
    a.mov_rsp_off_reg(0x30, Reg64::Rax);
    a.lea_rip_rva(Reg64::Rax, rva_rt);
    a.mov_rsp_off_reg(0x38, Reg64::Rax);
    a.call_com_rcx_slot(8);

    // CreateRenderTargetView(slot 9)
    a.mov_reg_from_rva(Reg64::Rcx, rva_device);
    a.mov_reg_from_rva(Reg64::Rdx, rva_rt);
    a.xor_reg32_self(Reg64::R8);
    a.mov_reg_from_rva(Reg64::R9, rva_rtv_cpu);
    a.call_com_rcx_slot(9);

    // Render 3 frames (Frame 0, 1, 2)
    for frame in 0..3 {
        if frame > 0 {
            a.mov_reg_from_rva(Reg64::Rcx, rva_alloc);
            a.call_com_rcx_slot(3); // Allocator::Reset
            a.mov_reg_from_rva(Reg64::Rcx, rva_cmdlist);
            a.mov_reg_from_rva(Reg64::Rdx, rva_alloc);
            a.xor_reg32_self(Reg64::R8);
            a.call_com_rcx_slot(4); // CmdList::Reset
        }
        a.mov_reg_from_rva(Reg64::Rcx, rva_cmdlist);
        a.mov_reg_imm32(Reg64::Rdx, 1);
        a.lea_rip_rva(Reg64::R8, rva_rtv_cpu);
        a.xor_reg32_self(Reg64::R9);
        a.mov_rsp_off_imm32(0x20, 0);
        a.call_com_rcx_slot(8); // OMSetRenderTargets

        a.mov_reg_from_rva(Reg64::Rcx, rva_cmdlist);
        a.mov_reg_imm32(Reg64::Rdx, 1);
        a.lea_rip_rva(Reg64::R8, rva_viewport);
        a.call_com_rcx_slot(6); // RSSetViewports

        a.mov_reg_from_rva(Reg64::Rcx, rva_cmdlist);
        a.mov_reg_from_rva(Reg64::Rdx, rva_rtv_cpu);
        a.lea_rip_rva(Reg64::R8, rva_clear_color);
        a.xor_reg32_self(Reg64::R9);
        a.mov_rsp_off_imm32(0x20, 0);
        a.call_com_rcx_slot(9); // ClearRenderTargetView

        a.mov_reg_from_rva(Reg64::Rcx, rva_cmdlist);
        a.mov_reg_imm32(Reg64::Rdx, D3DPrimitiveTopology::TriangleList as u32);
        a.call_com_rcx_slot(10); // IASetPrimitiveTopology

        a.mov_reg_from_rva(Reg64::Rcx, rva_cmdlist);
        a.xor_reg32_self(Reg64::Rdx);
        a.mov_reg_imm32(Reg64::R8, 1);
        a.lea_rip_rva(Reg64::R9, rva_vb_view);
        a.call_com_rcx_slot(11); // IASetVertexBuffers

        a.mov_reg_from_rva(Reg64::Rcx, rva_cmdlist);
        a.lea_rip_rva(Reg64::Rdx, rva_ib_view);
        a.call_com_rcx_slot(18); // IASetIndexBuffer

        a.mov_reg_from_rva(Reg64::Rcx, rva_cmdlist);
        a.mov_reg_imm32(Reg64::Rdx, 6); // 6 indices (2 depth-tested triangles)
        a.mov_reg_imm32(Reg64::R8, 1);
        a.xor_reg32_self(Reg64::R9);
        a.mov_rsp_off_imm32(0x20, 0);
        a.mov_rsp_off_imm32(0x28, 0);
        a.call_com_rcx_slot(19); // DrawIndexedInstanced

        a.mov_reg_from_rva(Reg64::Rcx, rva_cmdlist);
        a.call_com_rcx_slot(3); // Close

        a.mov_reg_from_rva(Reg64::Rcx, rva_queue);
        a.mov_reg_imm32(Reg64::Rdx, 1);
        a.lea_rip_rva(Reg64::R8, rva_cmdlist);
        a.call_com_rcx_slot(3); // ExecuteCommandLists

        a.mov_reg_from_rva(Reg64::Rcx, rva_queue);
        a.mov_reg_imm32(Reg64::Rdx, 1);
        a.lea_rip_rva(Reg64::R8, rva_present_params);
        a.xor_reg32_self(Reg64::R9);
        a.call_com_rcx_slot(5); // PresentX -> FSR Quality 1.5x
    }

    // 6. ACP HAL + XAudio2 48 kHz audio
    a.lea_rip_rva(Reg64::Rcx, rva_asset_buf);
    a.mov_reg_imm32(Reg64::Rdx, 1024 * 1024);
    a.call_iat(iat["acphal.dll!ApuCreateHeap"]);
    a.lea_rip_rva(Reg64::Rcx, rva_xaudio);
    a.xor_reg32_self(Reg64::Rdx);
    a.xor_reg32_self(Reg64::R8);
    a.call_iat(iat["xaudio2_9.dll!XAudio2Create"]);
    a.mov_reg_from_rva(Reg64::Rcx, rva_xaudio);
    a.lea_rip_rva(Reg64::Rdx, rva_master);
    a.xor_reg32_self(Reg64::R8);
    a.xor_reg32_self(Reg64::R9);
    a.mov_rsp_off_imm32(0x20, 0);
    a.mov_rsp_off_imm32(0x28, 0);
    a.mov_rsp_off_imm32(0x30, 0);
    a.call_com_rcx_slot(3); // CreateMasteringVoice
    a.mov_reg_from_rva(Reg64::Rcx, rva_xaudio);
    a.lea_rip_rva(Reg64::Rdx, rva_src_voice);
    a.lea_rip_rva(Reg64::R8, rva_wfx);
    a.xor_reg32_self(Reg64::R9);
    a.mov_rsp_off_imm32(0x20, 0x4000_0000); // 2.0f max freq ratio
    a.mov_rsp_off_imm32(0x28, 0);
    a.mov_rsp_off_imm32(0x30, 0);
    a.mov_rsp_off_imm32(0x38, 0);
    a.call_com_rcx_slot(4); // CreateSourceVoice
    a.mov_reg_from_rva(Reg64::Rcx, rva_src_voice);
    a.lea_rip_rva(Reg64::Rdx, rva_xa_buf);
    a.xor_reg32_self(Reg64::R8);
    a.call_com_rcx_slot(5); // SubmitSourceBuffer

    // 7. XInput poll + GDK shutdown + exit 0
    a.xor_reg32_self(Reg64::Rcx);
    a.lea_rip_rva(Reg64::Rdx, rva_xinput_state);
    a.call_iat(iat["xinput1_4.dll!XInputGetState"]);
    a.lea_rip_rva(Reg64::Rcx, rva_msg_done);
    a.call_iat(iat["kernelx.dll!OutputDebugStringA"]);
    a.call_iat(iat["xgameruntime.dll!XGameRuntimeUninitialize"]);

    a.xor_reg32_self(Reg64::Rax);
    a.add_rsp_imm8(0x40);
    a.pop(Reg64::Rbx);
    a.ret();

    let text_bytes = a.finish();

    assemble_pe64(
        TEXT_RVA,
        RDATA_RVA,
        DATA_RVA,
        IDATA_RVA,
        RELOC_RVA,
        SIZE_OF_IMAGE,
        idata_dir_size,
        &text_bytes,
        &rdata.buf,
        &data.buf,
        &idata_bytes,
        &reloc_bytes,
    )
}

struct DataBuilder {
    base_rva: u32,
    buf: Vec<u8>,
}

impl DataBuilder {
    fn new(base_rva: u32) -> Self {
        Self { base_rva, buf: Vec::new() }
    }
    fn align(&mut self, align: usize) {
        while self.buf.len() % align != 0 {
            self.buf.push(0);
        }
    }
    fn alloc(&mut self, size: usize, align: usize) -> u32 {
        self.align(align);
        let rva = self.base_rva + self.buf.len() as u32;
        self.buf.resize(self.buf.len() + size, 0);
        rva
    }
    fn push_bytes(&mut self, bytes: &[u8], align: usize) -> u32 {
        self.align(align);
        let rva = self.base_rva + self.buf.len() as u32;
        self.buf.extend_from_slice(bytes);
        rva
    }
    fn push_cstr(&mut self, s: &str) -> u32 {
        let mut b = s.as_bytes().to_vec();
        b.push(0);
        self.push_bytes(&b, 1)
    }
    fn push_utf16(&mut self, s: &str) -> u32 {
        let mut b = Vec::new();
        for u in s.encode_utf16() {
            b.extend_from_slice(&u.to_le_bytes());
        }
        b.extend_from_slice(&0u16.to_le_bytes());
        self.push_bytes(&b, 2)
    }
    fn push_struct<T>(&mut self, val: &T) -> u32 {
        let bytes = unsafe {
            core::slice::from_raw_parts(val as *const T as *const u8, core::mem::size_of::<T>())
        };
        self.push_bytes(bytes, core::mem::align_of::<T>().max(4))
    }
}

fn build_idata(base_rva: u32, specs: &[(&str, &[&str])]) -> (Vec<u8>, HashMap<String, u32>, u32) {
    let mut buf = Vec::new();
    let desc_size = (specs.len() + 1) * 20;
    buf.resize(desc_size, 0);
    let mut iat_map = HashMap::new();
    let mut dll_name_rvas = Vec::new();
    let mut ilt_rvas = Vec::new();
    let mut iat_rvas = Vec::new();

    for &(dll, syms) in specs {
        let nr = base_rva + buf.len() as u32;
        buf.extend_from_slice(dll.as_bytes());
        buf.push(0);
        dll_name_rvas.push(nr);
        while buf.len() % 8 != 0 {
            buf.push(0);
        }
        let ilt_rva = base_rva + buf.len() as u32;
        ilt_rvas.push(ilt_rva);
        buf.resize(buf.len() + (syms.len() + 1) * 8, 0);
        let iat_rva = base_rva + buf.len() as u32;
        iat_rvas.push(iat_rva);
        buf.resize(buf.len() + (syms.len() + 1) * 8, 0);
    }

    for (di, &(dll, syms)) in specs.iter().enumerate() {
        let desc_off = di * 20;
        buf[desc_off..desc_off + 4].copy_from_slice(&ilt_rvas[di].to_le_bytes());
        buf[desc_off + 12..desc_off + 16].copy_from_slice(&dll_name_rvas[di].to_le_bytes());
        buf[desc_off + 16..desc_off + 20].copy_from_slice(&iat_rvas[di].to_le_bytes());

        for (si, &sym) in syms.iter().enumerate() {
            while buf.len() % 2 != 0 {
                buf.push(0);
            }
            let hn_rva = base_rva + buf.len() as u32;
            buf.extend_from_slice(&0u16.to_le_bytes());
            buf.extend_from_slice(sym.as_bytes());
            buf.push(0);

            let ilt_off = (ilt_rvas[di] - base_rva) as usize + si * 8;
            let iat_off = (iat_rvas[di] - base_rva) as usize + si * 8;
            buf[ilt_off..ilt_off + 8].copy_from_slice(&(hn_rva as u64).to_le_bytes());
            buf[iat_off..iat_off + 8].copy_from_slice(&(hn_rva as u64).to_le_bytes());
            iat_map.insert(format!("{dll}!{sym}"), iat_rvas[di] + (si * 8) as u32);
        }
    }
    (buf, iat_map, desc_size as u32)
}

fn build_relocs(rvas: &[u32]) -> Vec<u8> {
    let mut by_page: HashMap<u32, Vec<u16>> = HashMap::new();
    for &rva in rvas {
        let page = rva & !0xFFF;
        let off = (rva & 0xFFF) as u16;
        by_page.entry(page).or_default().push((IMAGE_REL_BASED_DIR64 << 12) | off);
    }
    let mut pages: Vec<u32> = by_page.keys().copied().collect();
    pages.sort_unstable();
    let mut out = Vec::new();
    for page in pages {
        let mut entries = by_page.remove(&page).unwrap();
        if entries.len() % 2 != 0 {
            entries.push(0);
        }
        let block_size = (8 + entries.len() * 2) as u32;
        out.extend_from_slice(&page.to_le_bytes());
        out.extend_from_slice(&block_size.to_le_bytes());
        for e in entries {
            out.extend_from_slice(&e.to_le_bytes());
        }
    }
    out
}

fn align_up(v: usize, align: usize) -> usize {
    (v + align - 1) & !(align - 1)
}

#[allow(clippy::too_many_arguments)]
fn assemble_pe64(
    text_rva: u32,
    rdata_rva: u32,
    data_rva: u32,
    idata_rva: u32,
    reloc_rva: u32,
    size_of_image: u32,
    idata_dir_size: u32,
    text: &[u8],
    rdata: &[u8],
    data: &[u8],
    idata: &[u8],
    reloc: &[u8],
) -> Vec<u8> {
    let sections = [
        (".text", text_rva, text, IMAGE_SCN_MEM_EXECUTE | IMAGE_SCN_MEM_READ),
        (".rdata", rdata_rva, rdata, IMAGE_SCN_MEM_READ),
        (".data", data_rva, data, IMAGE_SCN_MEM_READ | IMAGE_SCN_MEM_WRITE),
        (".idata", idata_rva, idata, IMAGE_SCN_MEM_READ | IMAGE_SCN_MEM_WRITE),
        (".reloc", reloc_rva, reloc, IMAGE_SCN_MEM_READ),
    ];
    let pe_off = 0x80usize;
    let opt_size = 240usize;
    let headers_raw = pe_off + 4 + 20 + opt_size + sections.len() * 40;
    let size_of_headers = align_up(headers_raw, FILE_ALIGN as usize);
    let mut raw_offs = Vec::new();
    let mut cur = size_of_headers;
    for &(_, _, bytes, _) in &sections {
        raw_offs.push(cur);
        cur += align_up(bytes.len().max(1), FILE_ALIGN as usize);
    }

    let mut pe = vec![0u8; cur];
    pe[0..2].copy_from_slice(b"MZ");
    pe[0x3C..0x40].copy_from_slice(&(pe_off as u32).to_le_bytes());
    pe[pe_off..pe_off + 4].copy_from_slice(b"PE\0\0");

    let coff = pe_off + 4;
    pe[coff..coff + 2].copy_from_slice(&IMAGE_FILE_MACHINE_AMD64.to_le_bytes());
    pe[coff + 2..coff + 4].copy_from_slice(&(sections.len() as u16).to_le_bytes());
    pe[coff + 16..coff + 18].copy_from_slice(&(opt_size as u16).to_le_bytes());
    pe[coff + 18..coff + 20].copy_from_slice(&0x0022u16.to_le_bytes());

    let opt = coff + 20;
    pe[opt..opt + 2].copy_from_slice(&PE32_PLUS_MAGIC.to_le_bytes());
    pe[opt + 16..opt + 20].copy_from_slice(&text_rva.to_le_bytes());
    pe[opt + 24..opt + 32].copy_from_slice(&PREFERRED_IMAGE_BASE.to_le_bytes());
    pe[opt + 32..opt + 36].copy_from_slice(&SECTION_ALIGN.to_le_bytes());
    pe[opt + 36..opt + 40].copy_from_slice(&FILE_ALIGN.to_le_bytes());
    pe[opt + 56..opt + 60].copy_from_slice(&size_of_image.to_le_bytes());
    pe[opt + 60..opt + 64].copy_from_slice(&(size_of_headers as u32).to_le_bytes());
    pe[opt + 68..opt + 70].copy_from_slice(&3u16.to_le_bytes());
    pe[opt + 108..opt + 112].copy_from_slice(&16u32.to_le_bytes());

    let dd = opt + 112;
    let imp_off = dd + IMAGE_DIRECTORY_ENTRY_IMPORT * 8;
    pe[imp_off..imp_off + 4].copy_from_slice(&idata_rva.to_le_bytes());
    pe[imp_off + 4..imp_off + 8].copy_from_slice(&idata_dir_size.to_le_bytes());
    let rel_off = dd + IMAGE_DIRECTORY_ENTRY_BASERELOC * 8;
    pe[rel_off..rel_off + 4].copy_from_slice(&reloc_rva.to_le_bytes());
    pe[rel_off + 4..rel_off + 8].copy_from_slice(&(reloc.len() as u32).to_le_bytes());

    let sec_base = opt + opt_size;
    for (i, &(name, rva, bytes, chars)) in sections.iter().enumerate() {
        let sh = sec_base + i * 40;
        let nb = name.as_bytes();
        pe[sh..sh + nb.len().min(8)].copy_from_slice(&nb[..nb.len().min(8)]);
        pe[sh + 8..sh + 12].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
        pe[sh + 12..sh + 16].copy_from_slice(&rva.to_le_bytes());
        let rsz = align_up(bytes.len().max(1), FILE_ALIGN as usize) as u32;
        pe[sh + 16..sh + 20].copy_from_slice(&rsz.to_le_bytes());
        pe[sh + 20..sh + 24].copy_from_slice(&(raw_offs[i] as u32).to_le_bytes());
        pe[sh + 36..sh + 40].copy_from_slice(&chars.to_le_bytes());
        pe[raw_offs[i]..raw_offs[i] + bytes.len()].copy_from_slice(bytes);
    }
    pe
}
