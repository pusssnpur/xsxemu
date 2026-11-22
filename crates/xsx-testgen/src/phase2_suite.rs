//! Phase 2 multi-module PE32+ test generator:
//! - `xsx_helper.dll`: secondary guest PE32+ DLL with `.edata` Export Table, `.pdata`, and `DllMain`.
//! - `phase2_xsx.exe`: PE32+ executable exercising TLS callbacks, `.pdata` exception table,
//!   secondary DLL imports (`xsx_helper.dll!GetHelperMagic`), virtual Xbox `T:\` file I/O,
//!   real `CreateThread`/`WaitForSingleObject`, and D3D12.x + default FSR upscaling.

use std::collections::HashMap;

use xsx_abi::com::{
    IID_ID3D12COMMAND_ALLOCATOR, IID_ID3D12COMMAND_QUEUE, IID_ID3D12DESCRIPTOR_HEAP,
    IID_ID3D12DEVICE, IID_ID3D12GRAPHICS_COMMAND_LIST, IID_ID3D12RESOURCE,
};
use xsx_abi::d3d12::{
    D3D12CommandListType, D3D12CommandQueueDesc, D3D12DescriptorHeapDesc, D3D12DescriptorHeapType,
    D3D12HeapProperties, D3D12HeapType, D3D12ResourceDesc, D3D12ResourceDimension,
    D3D12TextureLayout, D3D12VertexBufferView, D3D12Viewport, D3D12XboxCreateDeviceParameters,
    D3D12XboxPresentPlaneParameters, D3DPrimitiveTopology, DxgiFormat, DxgiSampleDesc,
    SimpleVertex, D3D12XBOX_CREATE_DEVICE_FLAG_DEBUG, D3D12XBOX_HEAP_FLAG_CPU_CACHED,
    D3D12XBOX_HEAP_FLAG_GRAPHICS_WRITABLE, D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
    D3D12_RESOURCE_STATE_RENDER_TARGET,
};
use xsx_abi::gdk::{
    RuntimeFunction, XSystemDeviceType, CREATE_ALWAYS, DLL_PROCESS_ATTACH, FILE_ATTRIBUTE_NORMAL,
    GENERIC_READ, GENERIC_WRITE, INFINITE,
};
use xsx_loader::{
    IMAGE_DIRECTORY_ENTRY_BASERELOC, IMAGE_DIRECTORY_ENTRY_EXCEPTION, IMAGE_DIRECTORY_ENTRY_EXPORT,
    IMAGE_DIRECTORY_ENTRY_IMPORT, IMAGE_DIRECTORY_ENTRY_TLS, IMAGE_FILE_DLL,
    IMAGE_FILE_MACHINE_AMD64, IMAGE_REL_BASED_DIR64, IMAGE_SCN_MEM_EXECUTE, IMAGE_SCN_MEM_READ,
    IMAGE_SCN_MEM_WRITE, PE32_PLUS_MAGIC,
};

use crate::x64_asm::{Reg64, X64Assembler};

pub const HELPER_MAGIC: u32 = 0x5853_5832; // "XSX2"
pub const WORKER_EXIT_CODE: u32 = 42;
pub const PHASE2_FILE_PAYLOAD: &str = "XBOX_SERIES_X_VFS_OK";

const SECTION_ALIGN: u32 = 0x1000;
const FILE_ALIGN: u32 = 0x200;

/// Build secondary guest PE32+ DLL (`xsx_helper.dll`) exporting `GetHelperMagic`.
pub fn build_helper_dll_pe() -> Vec<u8> {
    const DLL_BASE: u64 = 0x0000_0001_8000_0000;
    const TEXT_RVA: u32 = 0x1000;
    const RDATA_RVA: u32 = 0x2000;
    const IDATA_RVA: u32 = 0x3000;
    const PDATA_RVA: u32 = 0x4000;
    const SIZE_OF_IMAGE: u32 = 0x5000;

    let (idata_bytes, iat_rvas, idata_dir_size) =
        build_idata(IDATA_RVA, &[("kernelx.dll", &["OutputDebugStringA"])]);

    let mut rdata = DataBuilder::new(RDATA_RVA);
    let rva_attach_msg = rdata.push_cstr("xsx_helper.dll DllMain(DLL_PROCESS_ATTACH) ran!");

    // Assemble .text first so we know exact RVAs of DllMain and GetHelperMagic
    let mut asm = X64Assembler::new(TEXT_RVA);

    // Entry: DllMain(rcx=hinst, edx=reason, r8=reserved) -> i32
    let dll_main_rva = asm.current_rva();
    asm.sub_rsp_imm8(0x28);
    asm.cmp_edx_imm32(DLL_PROCESS_ATTACH);
    let skip_log = asm.jne_placeholder();
    asm.lea_rip_rva(Reg64::Rcx, rva_attach_msg);
    asm.call_iat(iat_rvas["OutputDebugStringA"]);
    asm.patch_branch_here(skip_log);
    asm.mov_reg_imm32(Reg64::Rax, 1); // TRUE
    asm.add_rsp_imm8(0x28);
    asm.ret();
    let dll_main_end = asm.current_rva();

    // Exported function: GetHelperMagic() -> u32
    let get_magic_rva = asm.current_rva();
    asm.mov_reg_imm32(Reg64::Rax, HELPER_MAGIC);
    asm.ret();
    let get_magic_end = asm.current_rva();

    let text_bytes = asm.finish();

    // Build .edata Export Directory inside .rdata
    let (export_rva, export_size) =
        rdata.push_export_dir("xsx_helper.dll", &[("GetHelperMagic", get_magic_rva)]);
    let rdata_bytes = rdata.finish();

    // Build .pdata Exception Directory
    let mut pdata = DataBuilder::new(PDATA_RVA);
    pdata.push_struct(&RuntimeFunction {
        begin_address: dll_main_rva,
        end_address: dll_main_end,
        unwind_info_address: 0,
    });
    pdata.push_struct(&RuntimeFunction {
        begin_address: get_magic_rva,
        end_address: get_magic_end,
        unwind_info_address: 0,
    });
    let pdata_bytes = pdata.finish();

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
            name: *b".idata\0\0",
            rva: IDATA_RVA,
            data: &idata_bytes,
            characteristics: IMAGE_SCN_MEM_READ | IMAGE_SCN_MEM_WRITE | 0x0000_0040,
        },
        SectionSpec {
            name: *b".pdata\0\0",
            rva: PDATA_RVA,
            data: &pdata_bytes,
            characteristics: IMAGE_SCN_MEM_READ | 0x0000_0040,
        },
    ];

    let mut dirs = [(0u32, 0u32); 16];
    dirs[IMAGE_DIRECTORY_ENTRY_EXPORT] = (export_rva, export_size);
    dirs[IMAGE_DIRECTORY_ENTRY_IMPORT] = (IDATA_RVA, idata_dir_size);
    dirs[IMAGE_DIRECTORY_ENTRY_EXCEPTION] = (PDATA_RVA, pdata_bytes.len() as u32);

    emit_pe32_plus_ex(
        DLL_BASE,
        SIZE_OF_IMAGE,
        0x0022 | IMAGE_FILE_DLL,
        dll_main_rva,
        &sections,
        &dirs,
    )
}

/// Build Phase 2 PE32+ executable (`phase2_xsx.exe`) exercising:
/// - PE TLS callback (`DLL_PROCESS_ATTACH`)
/// - `.pdata` runtime function table
/// - Secondary guest DLL import (`xsx_helper.dll!GetHelperMagic`)
/// - Virtual Xbox `T:\` file I/O (`CreateFileA`, `WriteFile`, `CloseHandle`)
/// - Real multithreading (`CreateThread`, `WaitForSingleObject`, `GetExitCodeThread`)
/// - D3D12.x rendering + default FSR upscaling on `PresentX`
pub fn build_phase2_xsx_pe() -> Vec<u8> {
    const EXE_BASE: u64 = 0x0000_0001_4000_0000;
    const TEXT_RVA: u32 = 0x1000;
    const RDATA_RVA: u32 = 0x2000;
    const DATA_RVA: u32 = 0x3000;
    const IDATA_RVA: u32 = 0x4000;
    const RELOC_RVA: u32 = 0x5000;
    const PDATA_RVA: u32 = 0x6000;
    const SIZE_OF_IMAGE: u32 = 0x7000;

    let import_spec: &[(&str, &[&str])] = &[
        (
            "xgameruntime.dll",
            &[
                "XGameRuntimeInitialize",
                "XSystemGetDeviceType",
                "XGameRuntimeUninitialize",
            ],
        ),
        (
            "kernelx.dll",
            &[
                "OutputDebugStringA",
                "CreateFileA",
                "WriteFile",
                "CloseHandle",
                "CreateThread",
                "WaitForSingleObject",
                "GetExitCodeThread",
            ],
        ),
        ("d3d12_x.dll", &["D3D12XBOX_CreateDevice"]),
        ("xsx_helper.dll", &["GetHelperMagic"]),
    ];
    let (idata_bytes, iat_rvas, idata_dir_size) = build_idata(IDATA_RVA, import_spec);

    // -----------------------------------------------------------------------
    // .rdata constants
    // -----------------------------------------------------------------------
    let mut rdata = DataBuilder::new(RDATA_RVA);
    let rva_msg_tls = rdata.push_cstr("Phase 2 TLS callback executed (DLL_PROCESS_ATTACH)!");
    let rva_msg_worker = rdata.push_cstr("Phase 2 worker thread executed guest x86_64 code!");
    let rva_msg_done = rdata.push_cstr("Phase 2 guest PE64 completed all checks + FSR PresentX!");
    let rva_vfs_path = rdata.push_cstr(r"T:\phase2_check.txt");
    let rva_vfs_payload = rdata.push_cstr(PHASE2_FILE_PAYLOAD);

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
    let rva_clear_color = rdata.push_struct(&[0.06_f32, 0.14_f32, 0.10_f32, 1.0_f32]);

    let vertices: [SimpleVertex; 3] = [
        SimpleVertex {
            pos: [0.0, 0.75, 0.0],
            color: [0.06, 0.85, 0.25, 1.0],
        },
        SimpleVertex {
            pos: [0.65, -0.65, 0.0],
            color: [0.15, 0.55, 0.98, 1.0],
        },
        SimpleVertex {
            pos: [-0.65, -0.65, 0.0],
            color: [0.95, 0.25, 0.35, 1.0],
        },
    ];
    let rva_vertices = rdata.push_struct(&vertices);

    // -----------------------------------------------------------------------
    // .data mutable slots + relocated pointers + TLS directory
    // -----------------------------------------------------------------------
    let mut data = DataBuilder::new(DATA_RVA);
    let mut reloc_rvas = Vec::new();

    let rva_device_ptr = data.push_u64(0);
    let rva_queue_ptr = data.push_u64(0);
    let rva_alloc_ptr = data.push_u64(0);
    let rva_cmdlist_ptr = data.push_u64(0);
    let rva_rtv_heap_ptr = data.push_u64(0);
    let rva_rt_res_ptr = data.push_u64(0);
    let rva_rtv_handle = data.push_u64(0);
    let rva_file_handle = data.push_u64(0);
    let rva_bytes_written = data.push_u64(0);
    let rva_thread_handle = data.push_u64(0);
    let rva_thread_exit_code = data.push_u64(0);
    let rva_tls_index = data.push_u64(0);

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

    // -----------------------------------------------------------------------
    // Assemble .text: tls_callback, worker_thread_proc, and main
    // -----------------------------------------------------------------------
    let mut asm = X64Assembler::new(TEXT_RVA);

    // 1) TLS Callback: void tls_callback(void* hinst, u32 reason, void* reserved)
    let tls_cb_rva = asm.current_rva();
    asm.sub_rsp_imm8(0x28);
    asm.cmp_edx_imm32(DLL_PROCESS_ATTACH);
    let skip_tls = asm.jne_placeholder();
    asm.lea_rip_rva(Reg64::Rcx, rva_msg_tls);
    asm.call_iat(iat_rvas["OutputDebugStringA"]);
    asm.patch_branch_here(skip_tls);
    asm.add_rsp_imm8(0x28);
    asm.ret();
    let tls_cb_end = asm.current_rva();

    // 2) Worker Thread Proc: u32 worker_proc(void* param)
    let worker_rva = asm.current_rva();
    asm.sub_rsp_imm8(0x28);
    // rcx already holds param (pointer to worker debug message)
    asm.call_iat(iat_rvas["OutputDebugStringA"]);
    asm.mov_reg_imm32(Reg64::Rax, WORKER_EXIT_CODE);
    asm.add_rsp_imm8(0x28);
    asm.ret();
    let worker_end = asm.current_rva();

    // 3) Main Entry Point
    let main_rva = asm.current_rva();
    let mut fail_branches = Vec::new();
    asm.push(Reg64::Rbp);
    asm.push(Reg64::Rbx);
    asm.push(Reg64::Rsi);
    asm.push(Reg64::Rdi);
    asm.sub_rsp_imm8(0x68);

    // Check secondary guest DLL export: xsx_helper.dll!GetHelperMagic() == HELPER_MAGIC
    asm.call_iat(iat_rvas["GetHelperMagic"]);
    asm.cmp_eax_imm32(HELPER_MAGIC);
    fail_branches.push(asm.jne_placeholder());

    // Write file to Xbox scratch drive: CreateFileA("T:\\phase2_check.txt", RW, 0, NULL, CREATE_ALWAYS, NORMAL, NULL)
    asm.mov_rsp_off_imm32(0x20, CREATE_ALWAYS as i32);
    asm.mov_rsp_off_imm32(0x28, FILE_ATTRIBUTE_NORMAL as i32);
    asm.mov_rsp_off_imm32(0x30, 0);
    asm.lea_rip_rva(Reg64::Rcx, rva_vfs_path);
    asm.mov_reg_imm32(Reg64::Rdx, GENERIC_READ | GENERIC_WRITE);
    asm.xor_reg32_self(Reg64::R8);
    asm.xor_reg32_self(Reg64::R9);
    asm.call_iat(iat_rvas["CreateFileA"]);
    asm.mov_rva_from_reg(rva_file_handle, Reg64::Rax);

    // WriteFile(hFile, payload, len, &written, NULL)
    asm.mov_rsp_off_imm32(0x20, 0);
    asm.mov_reg_from_rva(Reg64::Rcx, rva_file_handle);
    asm.lea_rip_rva(Reg64::Rdx, rva_vfs_payload);
    asm.mov_reg_imm32(Reg64::R8, PHASE2_FILE_PAYLOAD.len() as u32);
    asm.lea_rip_rva(Reg64::R9, rva_bytes_written);
    asm.call_iat(iat_rvas["WriteFile"]);
    asm.test_eax_eax();
    fail_branches.push(asm.je_placeholder());

    // CloseHandle(hFile)
    asm.mov_reg_from_rva(Reg64::Rcx, rva_file_handle);
    asm.call_iat(iat_rvas["CloseHandle"]);

    // Spawn real OS worker thread: CreateThread(NULL, 0, worker_proc, &msg_worker, 0, NULL)
    asm.mov_rsp_off_imm32(0x20, 0);
    asm.mov_rsp_off_imm32(0x28, 0);
    asm.xor_reg32_self(Reg64::Rcx);
    asm.xor_reg32_self(Reg64::Rdx);
    asm.lea_rip_rva(Reg64::R8, worker_rva);
    asm.lea_rip_rva(Reg64::R9, rva_msg_worker);
    asm.call_iat(iat_rvas["CreateThread"]);
    asm.mov_rva_from_reg(rva_thread_handle, Reg64::Rax);

    // WaitForSingleObject(hThread, INFINITE)
    asm.mov_reg_from_rva(Reg64::Rcx, rva_thread_handle);
    asm.mov_reg_imm32(Reg64::Rdx, INFINITE);
    asm.call_iat(iat_rvas["WaitForSingleObject"]);

    // Verify GetExitCodeThread(hThread, &exit_code) -> exit_code == 42
    asm.mov_reg_from_rva(Reg64::Rcx, rva_thread_handle);
    asm.lea_rip_rva(Reg64::Rdx, rva_thread_exit_code);
    asm.call_iat(iat_rvas["GetExitCodeThread"]);
    asm.mov_eax_from_rva32(rva_thread_exit_code);
    asm.cmp_eax_imm32(WORKER_EXIT_CODE);
    fail_branches.push(asm.jne_placeholder());

    asm.mov_reg_from_rva(Reg64::Rcx, rva_thread_handle);
    asm.call_iat(iat_rvas["CloseHandle"]);

    // GDK + D3D12.x render & PresentX (upscaled automatically via FSR!)
    asm.call_iat(iat_rvas["XGameRuntimeInitialize"]);
    asm.test_eax_eax();
    fail_branches.push(asm.js_placeholder());

    asm.call_iat(iat_rvas["XSystemGetDeviceType"]);
    asm.cmp_eax_imm32(XSystemDeviceType::XboxScarlettAnaconda as u32);
    fail_branches.push(asm.jne_placeholder());

    asm.lea_rip_rva(Reg64::Rcx, rva_dev_params);
    asm.lea_rip_rva(Reg64::Rdx, rva_iid_device);
    asm.lea_rip_rva(Reg64::R8, rva_device_ptr);
    asm.call_iat(iat_rvas["D3D12XBOX_CreateDevice"]);
    asm.test_eax_eax();
    fail_branches.push(asm.js_placeholder());

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

    for ptr_rva in [
        rva_rt_res_ptr,
        rva_rtv_heap_ptr,
        rva_cmdlist_ptr,
        rva_alloc_ptr,
        rva_queue_ptr,
        rva_device_ptr,
    ] {
        asm.mov_reg_from_rva(Reg64::Rcx, ptr_rva);
        asm.call_com_rcx_slot(2);
    }

    asm.call_iat(iat_rvas["XGameRuntimeUninitialize"]);

    asm.xor_reg32_self(Reg64::Rax);
    asm.add_rsp_imm8(0x68);
    asm.pop(Reg64::Rdi);
    asm.pop(Reg64::Rsi);
    asm.pop(Reg64::Rbx);
    asm.pop(Reg64::Rbp);
    asm.ret();

    for patch in fail_branches {
        asm.patch_branch_here(patch);
    }
    asm.mov_reg_imm32(Reg64::Rax, 1);
    asm.add_rsp_imm8(0x68);
    asm.pop(Reg64::Rdi);
    asm.pop(Reg64::Rsi);
    asm.pop(Reg64::Rbx);
    asm.pop(Reg64::Rbp);
    asm.ret();
    let main_end = asm.current_rva();

    let text_bytes = asm.finish();

    // Now write TLS callback array + IMAGE_TLS_DIRECTORY64 in .data (with DIR64 relocations!)
    let rva_tls_cb_array = data.push_u64(EXE_BASE + tls_cb_rva as u64);
    reloc_rvas.push(rva_tls_cb_array);
    let _rva_tls_cb_null = data.push_u64(0);

    let rva_tls_dir = data.push_u64(0); // StartAddressOfRawData
    let _ = data.push_u64(0); // EndAddressOfRawData
    let rva_tls_dir_idx_field = data.push_u64(EXE_BASE + rva_tls_index as u64); // AddressOfIndex
    reloc_rvas.push(rva_tls_dir_idx_field);
    let rva_tls_dir_cb_field = data.push_u64(EXE_BASE + rva_tls_cb_array as u64); // AddressOfCallBacks
    reloc_rvas.push(rva_tls_dir_cb_field);
    let _ = data.push_u64(0); // SizeOfZeroFill (u32) + Characteristics (u32)

    let rdata_bytes = rdata.finish();
    let data_bytes = data.finish();
    let reloc_bytes = build_reloc_table(&reloc_rvas);

    // Build .pdata Exception Directory
    let mut pdata = DataBuilder::new(PDATA_RVA);
    pdata.push_struct(&RuntimeFunction {
        begin_address: tls_cb_rva,
        end_address: tls_cb_end,
        unwind_info_address: 0,
    });
    pdata.push_struct(&RuntimeFunction {
        begin_address: worker_rva,
        end_address: worker_end,
        unwind_info_address: 0,
    });
    pdata.push_struct(&RuntimeFunction {
        begin_address: main_rva,
        end_address: main_end,
        unwind_info_address: 0,
    });
    let pdata_bytes = pdata.finish();

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
        SectionSpec {
            name: *b".pdata\0\0",
            rva: PDATA_RVA,
            data: &pdata_bytes,
            characteristics: IMAGE_SCN_MEM_READ | 0x0000_0040,
        },
    ];

    let mut dirs = [(0u32, 0u32); 16];
    dirs[IMAGE_DIRECTORY_ENTRY_IMPORT] = (IDATA_RVA, idata_dir_size);
    dirs[IMAGE_DIRECTORY_ENTRY_EXCEPTION] = (PDATA_RVA, pdata_bytes.len() as u32);
    dirs[IMAGE_DIRECTORY_ENTRY_BASERELOC] = (RELOC_RVA, reloc_bytes.len() as u32);
    dirs[IMAGE_DIRECTORY_ENTRY_TLS] = (rva_tls_dir, 40);

    emit_pe32_plus_ex(EXE_BASE, SIZE_OF_IMAGE, 0x0022, main_rva, &sections, &dirs)
}

// ===========================================================================
// Local PE32+ emission helpers
// ===========================================================================

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

    fn push_u64(&mut self, v: u64) -> u32 {
        self.align(8);
        let rva = self.base_rva + self.buf.len() as u32;
        self.buf.extend_from_slice(&v.to_le_bytes());
        rva
    }

    fn push_struct<T>(&mut self, val: &T) -> u32 {
        self.align(core::mem::align_of::<T>().max(4));
        let rva = self.base_rva + self.buf.len() as u32;
        let bytes = unsafe {
            core::slice::from_raw_parts((val as *const T) as *const u8, core::mem::size_of::<T>())
        };
        self.buf.extend_from_slice(bytes);
        rva
    }

    fn push_export_dir(&mut self, dll_name: &str, exports: &[(&str, u32)]) -> (u32, u32) {
        self.align(4);
        let dir_off = self.buf.len();
        let dir_rva = self.base_rva + dir_off as u32;
        self.buf.resize(dir_off + 40, 0);

        let funcs_rva = self.base_rva + self.buf.len() as u32;
        for &(_, fn_rva) in exports {
            self.buf.extend_from_slice(&fn_rva.to_le_bytes());
        }

        let names_ptr_off = self.buf.len();
        let names_ptr_rva = self.base_rva + names_ptr_off as u32;
        self.buf.resize(names_ptr_off + exports.len() * 4, 0);

        let ords_rva = self.base_rva + self.buf.len() as u32;
        for idx in 0..exports.len() {
            self.buf.extend_from_slice(&(idx as u16).to_le_bytes());
        }

        let dll_name_rva = self.push_cstr(dll_name);
        for (i, &(name, _)) in exports.iter().enumerate() {
            let name_rva = self.push_cstr(name);
            write_u32(&mut self.buf, names_ptr_off + i * 4, name_rva);
        }

        // Fill IMAGE_EXPORT_DIRECTORY (40 bytes)
        write_u32(&mut self.buf, dir_off + 12, dll_name_rva);
        write_u32(&mut self.buf, dir_off + 16, 1); // OrdinalBase
        write_u32(&mut self.buf, dir_off + 20, exports.len() as u32);
        write_u32(&mut self.buf, dir_off + 24, exports.len() as u32);
        write_u32(&mut self.buf, dir_off + 28, funcs_rva);
        write_u32(&mut self.buf, dir_off + 32, names_ptr_rva);
        write_u32(&mut self.buf, dir_off + 36, ords_rva);

        let dir_size = (self.buf.len() - dir_off) as u32;
        (dir_rva, dir_size)
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
        write_u32(&mut buf, desc_off, ilt_rva);
        write_u32(&mut buf, desc_off + 12, dll_name_rva);
        write_u32(&mut buf, desc_off + 16, iat_rva);
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

fn emit_pe32_plus_ex(
    image_base: u64,
    size_of_image: u32,
    characteristics: u16,
    entry_rva: u32,
    sections: &[SectionSpec<'_>],
    dirs: &[(u32, u32); 16],
) -> Vec<u8> {
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
    write_u16(&mut file, coff + 18, characteristics);

    let opt = coff + 20;
    write_u16(&mut file, opt, PE32_PLUS_MAGIC);
    write_u32(&mut file, opt + 16, entry_rva);
    write_u64(&mut file, opt + 24, image_base);
    write_u32(&mut file, opt + 32, SECTION_ALIGN);
    write_u32(&mut file, opt + 36, FILE_ALIGN);
    write_u16(&mut file, opt + 40, 10);
    write_u16(&mut file, opt + 48, 10);
    write_u32(&mut file, opt + 56, size_of_image);
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
