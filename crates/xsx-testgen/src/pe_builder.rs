//! Spec-compliant PE32+ (`AMD64`) emitter for our synthetic Xbox Series X title (`hello_xsx.exe`).

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
use xsx_abi::gdk::XSystemDeviceType;
use xsx_loader::{
    IMAGE_FILE_MACHINE_AMD64, IMAGE_REL_BASED_DIR64, IMAGE_SCN_MEM_EXECUTE, IMAGE_SCN_MEM_READ,
    IMAGE_SCN_MEM_WRITE, PE32_PLUS_MAGIC,
};

use crate::x64_asm::{Reg64, X64Assembler};

const PREFERRED_IMAGE_BASE: u64 = 0x0000_0001_4000_0000;
const SECTION_ALIGN: u32 = 0x1000;
const FILE_ALIGN: u32 = 0x200;

const TEXT_RVA: u32 = 0x1000;
const RDATA_RVA: u32 = 0x2000;
const DATA_RVA: u32 = 0x3000;
const IDATA_RVA: u32 = 0x4000;
const RELOC_RVA: u32 = 0x5000;
const SIZE_OF_IMAGE: u32 = 0x6000;

/// Build a runnable Xbox Series X PE32+ executable (`hello_xsx.exe`).
///
/// When executed by `xsxemu`:
/// 1. Calls `xgameruntime.dll!XGameRuntimeInitialize`
/// 2. Calls `xgameruntime.dll!XSystemGetDeviceType` and verifies Series X (`Anaconda`)
/// 3. Logs a diagnostic message through a `.reloc`-patched pointer via `kernelx.dll!OutputDebugStringA`
/// 4. Creates an `ID3D12DeviceX` via `d3d12_x.dll!D3D12XBOX_CreateDevice`
/// 5. Creates CommandQueue, CommandAllocator, GraphicsCommandList, RTV DescriptorHeap,
///    and a 640x360 `Texture2D` Render Target (using Xbox-specific heap flags to exercise translation!)
/// 6. Records `OMSetRenderTargets`, `RSSetViewports`, `ClearRenderTargetView`,
///    `IASetPrimitiveTopology(TRIANGLELIST)`, `IASetVertexBuffers` (via `.reloc`-patched `.rdata` vertices),
///    `DrawInstanced(3, 1, 0, 0)`, and `Close`
/// 7. Executes the command list and presents the frame via `ID3D12CommandQueueX::PresentX`
/// 8. Releases COM interfaces and returns exit code `0`.
pub fn build_hello_xsx_pe() -> Vec<u8> {
    // -----------------------------------------------------------------------
    // 1. Build .idata (Import Directory Table + ILT + IAT + Hint/Names)
    // -----------------------------------------------------------------------
    let import_spec: &[(&str, &[&str])] = &[
        (
            "xgameruntime.dll",
            &[
                "XGameRuntimeInitialize",
                "XSystemGetDeviceType",
                "XGameRuntimeUninitialize",
            ],
        ),
        ("kernelx.dll", &["OutputDebugStringA"]),
        ("d3d12_x.dll", &["D3D12XBOX_CreateDevice"]),
    ];
    let (idata_bytes, iat_rvas, idata_dir_size) = build_idata(IDATA_RVA, import_spec);

    // -----------------------------------------------------------------------
    // 2. Build .rdata (constants, structs, strings, triangle vertices)
    // -----------------------------------------------------------------------
    let mut rdata = DataBuilder::new(RDATA_RVA);
    let rva_msg_boot = rdata.push_cstr("xsxemu guest PE64 booted on Xbox Series X (Anaconda)!");
    let rva_msg_done = rdata.push_cstr("xsxemu guest PE64 presented RGB triangle via PresentX!");
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
        cpu_page_property: 2, // Xbox UMA CPU-cached hint (sanitized by translator!)
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
    // Xbox dark slate-green background color
    let rva_clear_color = rdata.push_struct(&[0.06_f32, 0.14_f32, 0.10_f32, 1.0_f32]);

    // RGB Triangle vertices (clip-space)
    let vertices: [SimpleVertex; 3] = [
        SimpleVertex {
            pos: [0.0, 0.75, 0.0],
            color: [0.06, 0.85, 0.25, 1.0], // Xbox Green top vertex
        },
        SimpleVertex {
            pos: [0.65, -0.65, 0.0],
            color: [0.15, 0.55, 0.98, 1.0], // Cyan-blue bottom-right vertex
        },
        SimpleVertex {
            pos: [-0.65, -0.65, 0.0],
            color: [0.95, 0.25, 0.35, 1.0], // Crimson bottom-left vertex
        },
    ];
    let rva_vertices = rdata.push_struct(&vertices);
    let rdata_bytes = rdata.finish();

    // -----------------------------------------------------------------------
    // 3. Build .data (runtime slots + absolute pointers patched via .reloc!)
    // -----------------------------------------------------------------------
    let mut data = DataBuilder::new(DATA_RVA);
    let mut reloc_rvas: Vec<u32> = Vec::new();

    let rva_device_ptr = data.push_u64(0);
    let rva_queue_ptr = data.push_u64(0);
    let rva_alloc_ptr = data.push_u64(0);
    let rva_cmdlist_ptr = data.push_u64(0);
    let rva_rtv_heap_ptr = data.push_u64(0);
    let rva_rt_res_ptr = data.push_u64(0);
    let rva_rtv_handle = data.push_u64(0);
    let rva_frame_token = data.push_u64(0);

    // Relocated absolute pointer to boot message (verifies DIR64 relocation!)
    let rva_reloc_boot_msg_ptr = data.push_u64(PREFERRED_IMAGE_BASE + rva_msg_boot as u64);
    reloc_rvas.push(rva_reloc_boot_msg_ptr);

    // D3D12VertexBufferView with relocated buffer_location pointing at `vertices` in .rdata!
    let rva_vb_view = data.push_struct(&D3D12VertexBufferView {
        buffer_location: PREFERRED_IMAGE_BASE + rva_vertices as u64,
        size_in_bytes: core::mem::size_of::<[SimpleVertex; 3]>() as u32,
        stride_in_bytes: core::mem::size_of::<SimpleVertex>() as u32,
    });
    // buffer_location is at offset 0 of D3D12VertexBufferView
    reloc_rvas.push(rva_vb_view);

    // D3D12XboxPresentPlaneParameters with relocated pp_resources pointing to `rva_rt_res_ptr`!
    let rva_present_plane = data.push_struct(&D3D12XboxPresentPlaneParameters {
        token: 1,
        resource_count: 1,
        _pad: 0,
        pp_resources: (PREFERRED_IMAGE_BASE + rva_rt_res_ptr as u64) as *const *mut core::ffi::c_void,
        color_space: 0,
        options: 0,
    });
    // pp_resources is at offset 16 inside D3D12XboxPresentPlaneParameters
    reloc_rvas.push(rva_present_plane + 16);

    let data_bytes = data.finish();

    // -----------------------------------------------------------------------
    // 4. Build .reloc (Base Relocation Table)
    // -----------------------------------------------------------------------
    let reloc_bytes = build_reloc_table(&reloc_rvas);

    // -----------------------------------------------------------------------
    // 5. Assemble .text (real Win64 x86_64 machine code)
    // -----------------------------------------------------------------------
    let mut asm = X64Assembler::new(TEXT_RVA);
    let mut fail_branches = Vec::new();

    // Prologue: save callee-saved regs + align stack to 16 bytes + allocate 0x68 bytes
    asm.push(Reg64::Rbp);
    asm.push(Reg64::Rbx);
    asm.push(Reg64::Rsi);
    asm.push(Reg64::Rdi);
    asm.sub_rsp_imm8(0x68);

    // 1) XGameRuntimeInitialize()
    asm.call_iat(iat_rvas["XGameRuntimeInitialize"]);
    asm.test_eax_eax();
    fail_branches.push(asm.js_placeholder());

    // 2) Verify XSystemGetDeviceType() == XboxScarlettAnaconda (7)
    asm.call_iat(iat_rvas["XSystemGetDeviceType"]);
    asm.cmp_eax_imm32(XSystemDeviceType::XboxScarlettAnaconda as u32);
    fail_branches.push(asm.jne_placeholder());

    // 3) OutputDebugStringA(*reloc_boot_msg_ptr) -- tests DIR64 relocation!
    asm.mov_reg_from_rva(Reg64::Rcx, rva_reloc_boot_msg_ptr);
    asm.call_iat(iat_rvas["OutputDebugStringA"]);

    // 4) D3D12XBOX_CreateDevice(&dev_params, &IID_ID3D12Device, &device)
    asm.lea_rip_rva(Reg64::Rcx, rva_dev_params);
    asm.lea_rip_rva(Reg64::Rdx, rva_iid_device);
    asm.lea_rip_rva(Reg64::R8, rva_device_ptr);
    asm.call_iat(iat_rvas["D3D12XBOX_CreateDevice"]);
    asm.test_eax_eax();
    fail_branches.push(asm.js_placeholder());

    // 5) device->CreateCommandQueue(&queue_desc, &IID_ID3D12CommandQueue, &queue) [slot 3]
    asm.mov_reg_from_rva(Reg64::Rcx, rva_device_ptr);
    asm.lea_rip_rva(Reg64::Rdx, rva_queue_desc);
    asm.lea_rip_rva(Reg64::R8, rva_iid_queue);
    asm.lea_rip_rva(Reg64::R9, rva_queue_ptr);
    asm.call_com_rcx_slot(3);
    asm.test_eax_eax();
    fail_branches.push(asm.js_placeholder());

    // 6) device->CreateCommandAllocator(Direct=0, &IID, &allocator) [slot 4]
    asm.mov_reg_from_rva(Reg64::Rcx, rva_device_ptr);
    asm.xor_reg32_self(Reg64::Rdx);
    asm.lea_rip_rva(Reg64::R8, rva_iid_alloc);
    asm.lea_rip_rva(Reg64::R9, rva_alloc_ptr);
    asm.call_com_rcx_slot(4);
    asm.test_eax_eax();
    fail_branches.push(asm.js_placeholder());

    // 7) device->CreateGraphicsCommandList(0, Direct=0, alloc, NULL, &IID, &cmdlist) [slot 5]
    asm.mov_rsp_off_imm32(0x20, 0); // p_initial_state = NULL
    asm.lea_rip_rva(Reg64::Rax, rva_iid_cmdlist);
    asm.mov_rsp_off_reg(0x28, Reg64::Rax);
    asm.lea_rip_rva(Reg64::Rax, rva_cmdlist_ptr);
    asm.mov_rsp_off_reg(0x30, Reg64::Rax);
    asm.mov_reg_from_rva(Reg64::Rcx, rva_device_ptr);
    asm.xor_reg32_self(Reg64::Rdx); // node_mask = 0
    asm.xor_reg32_self(Reg64::R8);  // type = Direct
    asm.mov_reg_from_rva(Reg64::R9, rva_alloc_ptr);
    asm.call_com_rcx_slot(5);
    asm.test_eax_eax();
    fail_branches.push(asm.js_placeholder());

    // 8) device->CreateDescriptorHeap(&rtv_heap_desc, &IID, &rtv_heap) [slot 6]
    asm.mov_reg_from_rva(Reg64::Rcx, rva_device_ptr);
    asm.lea_rip_rva(Reg64::Rdx, rva_rtv_heap_desc);
    asm.lea_rip_rva(Reg64::R8, rva_iid_descheap);
    asm.lea_rip_rva(Reg64::R9, rva_rtv_heap_ptr);
    asm.call_com_rcx_slot(6);
    asm.test_eax_eax();
    fail_branches.push(asm.js_placeholder());

    // 9) device->CreateCommittedResource(&heap_props, XBOX_FLAGS, &rt_desc, RENDER_TARGET, NULL, &IID, &rt_res) [slot 8]
    asm.mov_rsp_off_imm32(0x20, D3D12_RESOURCE_STATE_RENDER_TARGET as i32);
    asm.mov_rsp_off_imm32(0x28, 0); // optimized clear = NULL
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
    asm.test_eax_eax();
    fail_branches.push(asm.js_placeholder());

    // 10) rtv_handle = rtv_heap->GetCPUDescriptorHandleForHeapStart() [slot 3]
    asm.mov_reg_from_rva(Reg64::Rcx, rva_rtv_heap_ptr);
    asm.call_com_rcx_slot(3);
    asm.mov_rva_from_reg(rva_rtv_handle, Reg64::Rax);

    // 11) device->CreateRenderTargetView(rt_res, NULL, rtv_handle) [slot 9]
    asm.mov_reg_from_rva(Reg64::Rcx, rva_device_ptr);
    asm.mov_reg_from_rva(Reg64::Rdx, rva_rt_res_ptr);
    asm.xor_reg32_self(Reg64::R8); // p_desc = NULL
    asm.mov_reg_from_rva(Reg64::R9, rva_rtv_handle);
    asm.call_com_rcx_slot(9);

    // 12) device->WaitFrameEventX(0, 1000, NULL, &frame_token) [slot 11]
    asm.lea_rip_rva(Reg64::Rax, rva_frame_token);
    asm.mov_rsp_off_reg(0x20, Reg64::Rax);
    asm.mov_reg_from_rva(Reg64::Rcx, rva_device_ptr);
    asm.xor_reg32_self(Reg64::Rdx);
    asm.mov_reg_imm32(Reg64::R8, 1000);
    asm.xor_reg32_self(Reg64::R9);
    asm.call_com_rcx_slot(11);

    // 13) cmdlist->OMSetRenderTargets(1, &rtv_handle, 0, NULL) [slot 8]
    asm.mov_rsp_off_imm32(0x20, 0);
    asm.mov_reg_from_rva(Reg64::Rcx, rva_cmdlist_ptr);
    asm.mov_reg_imm32(Reg64::Rdx, 1);
    asm.lea_rip_rva(Reg64::R8, rva_rtv_handle);
    asm.xor_reg32_self(Reg64::R9);
    asm.call_com_rcx_slot(8);

    // 14) cmdlist->RSSetViewports(1, &viewport) [slot 6]
    asm.mov_reg_from_rva(Reg64::Rcx, rva_cmdlist_ptr);
    asm.mov_reg_imm32(Reg64::Rdx, 1);
    asm.lea_rip_rva(Reg64::R8, rva_viewport);
    asm.call_com_rcx_slot(6);

    // 15) cmdlist->ClearRenderTargetView(rtv_handle, &clear_color, 0, NULL) [slot 9]
    asm.mov_rsp_off_imm32(0x20, 0);
    asm.mov_reg_from_rva(Reg64::Rcx, rva_cmdlist_ptr);
    asm.mov_reg_from_rva(Reg64::Rdx, rva_rtv_handle);
    asm.lea_rip_rva(Reg64::R8, rva_clear_color);
    asm.xor_reg32_self(Reg64::R9);
    asm.call_com_rcx_slot(9);

    // 16) cmdlist->IASetPrimitiveTopology(TriangleList = 4) [slot 10]
    asm.mov_reg_from_rva(Reg64::Rcx, rva_cmdlist_ptr);
    asm.mov_reg_imm32(Reg64::Rdx, D3DPrimitiveTopology::TriangleList as u32);
    asm.call_com_rcx_slot(10);

    // 17) cmdlist->IASetVertexBuffers(0, 1, &vb_view) [slot 11]
    asm.mov_reg_from_rva(Reg64::Rcx, rva_cmdlist_ptr);
    asm.xor_reg32_self(Reg64::Rdx);
    asm.mov_reg_imm32(Reg64::R8, 1);
    asm.lea_rip_rva(Reg64::R9, rva_vb_view);
    asm.call_com_rcx_slot(11);

    // 18) cmdlist->DrawInstanced(3, 1, 0, 0) [slot 12]
    asm.mov_rsp_off_imm32(0x20, 0);
    asm.mov_reg_from_rva(Reg64::Rcx, rva_cmdlist_ptr);
    asm.mov_reg_imm32(Reg64::Rdx, 3);
    asm.mov_reg_imm32(Reg64::R8, 1);
    asm.xor_reg32_self(Reg64::R9);
    asm.call_com_rcx_slot(12);

    // 19) cmdlist->Close() [slot 3]
    asm.mov_reg_from_rva(Reg64::Rcx, rva_cmdlist_ptr);
    asm.call_com_rcx_slot(3);

    // 20) queue->ExecuteCommandLists(1, &cmdlist_ptr) [slot 3]
    asm.mov_reg_from_rva(Reg64::Rcx, rva_queue_ptr);
    asm.mov_reg_imm32(Reg64::Rdx, 1);
    asm.lea_rip_rva(Reg64::R8, rva_cmdlist_ptr);
    asm.call_com_rcx_slot(3);

    // 21) queue->PresentX(1, &present_plane, NULL) [slot 5]
    asm.mov_reg_from_rva(Reg64::Rcx, rva_queue_ptr);
    asm.mov_reg_imm32(Reg64::Rdx, 1);
    asm.lea_rip_rva(Reg64::R8, rva_present_plane);
    asm.xor_reg32_self(Reg64::R9);
    asm.call_com_rcx_slot(5);
    asm.test_eax_eax();
    fail_branches.push(asm.js_placeholder());

    // 22) OutputDebugStringA(done_msg)
    asm.lea_rip_rva(Reg64::Rcx, rva_msg_done);
    asm.call_iat(iat_rvas["OutputDebugStringA"]);

    // 23) Release COM objects (IUnknown::Release = slot 2)
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

    // 24) XGameRuntimeUninitialize()
    asm.call_iat(iat_rvas["XGameRuntimeUninitialize"]);

    // Success exit code = 0
    asm.xor_reg32_self(Reg64::Rax);
    asm.add_rsp_imm8(0x68);
    asm.pop(Reg64::Rdi);
    asm.pop(Reg64::Rsi);
    asm.pop(Reg64::Rbx);
    asm.pop(Reg64::Rbp);
    asm.ret();

    // Error exit path (exit code = 1)
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

    let text_bytes = asm.finish();

    // -----------------------------------------------------------------------
    // 6. Assemble complete PE32+ file
    // -----------------------------------------------------------------------
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

    emit_pe32_plus(
        &sections,
        TEXT_RVA,
        IDATA_RVA,
        idata_dir_size,
        RELOC_RVA,
        reloc_bytes.len() as u32,
    )
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

fn build_idata(
    base_rva: u32,
    spec: &[(&str, &[&str])],
) -> (Vec<u8>, HashMap<String, u32>, u32) {
    let mut buf = Vec::new();
    let num_dlls = spec.len();
    let idt_size = (num_dlls + 1) * 20;
    buf.resize(idt_size, 0);

    // Reserve space for ILTs and IATs (8-byte aligned)
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

    // Write DLL names & Hint/Name entries and fill IDT + ILT + IAT
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
            buf.extend_from_slice(&0u16.to_le_bytes()); // Hint = 0
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
            entries.push(0); // IMAGE_REL_BASED_ABSOLUTE padding for 4-byte alignment
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

fn emit_pe32_plus(
    sections: &[SectionSpec<'_>],
    entry_rva: u32,
    import_rva: u32,
    import_size: u32,
    reloc_rva: u32,
    reloc_size: u32,
) -> Vec<u8> {
    let size_of_headers: u32 = 0x400; // 1024 bytes (aligned to FILE_ALIGN 0x200)
    let mut file = vec![0u8; size_of_headers as usize];

    // DOS Header
    file[0..2].copy_from_slice(b"MZ");
    let pe_off: usize = 0x80;
    file[0x3C..0x40].copy_from_slice(&(pe_off as u32).to_le_bytes());

    // PE Signature
    file[pe_off..pe_off + 4].copy_from_slice(b"PE\0\0");

    // COFF File Header (20 bytes)
    let coff = pe_off + 4;
    let opt_size: u16 = 240;
    write_u16(&mut file, coff, IMAGE_FILE_MACHINE_AMD64);
    write_u16(&mut file, coff + 2, sections.len() as u16);
    write_u16(&mut file, coff + 16, opt_size);
    write_u16(&mut file, coff + 18, 0x0022); // EXECUTABLE_IMAGE | LARGE_ADDRESS_AWARE

    // Optional Header (PE32+, 240 bytes)
    let opt = coff + 20;
    write_u16(&mut file, opt, PE32_PLUS_MAGIC);
    write_u32(&mut file, opt + 16, entry_rva);
    write_u64(&mut file, opt + 24, PREFERRED_IMAGE_BASE);
    write_u32(&mut file, opt + 32, SECTION_ALIGN);
    write_u32(&mut file, opt + 36, FILE_ALIGN);
    write_u16(&mut file, opt + 40, 10); // MajorOperatingSystemVersion
    write_u16(&mut file, opt + 48, 10); // MajorSubsystemVersion
    write_u32(&mut file, opt + 56, SIZE_OF_IMAGE);
    write_u32(&mut file, opt + 60, size_of_headers);
    write_u16(&mut file, opt + 68, 3); // IMAGE_SUBSYSTEM_WINDOWS_CUI
    write_u16(&mut file, opt + 70, 0x0160); // HIGH_ENTROPY_VA | DYNAMIC_BASE | NX_COMPAT
    write_u32(&mut file, opt + 108, 16); // NumberOfRvaAndSizes

    // Data Directories (16 * 8 = 128 bytes at opt + 112)
    // Index 1: Import Table
    write_u32(&mut file, opt + 112 + 8, import_rva);
    write_u32(&mut file, opt + 112 + 12, import_size);
    // Index 5: Base Relocation Table
    write_u32(&mut file, opt + 112 + 5 * 8, reloc_rva);
    write_u32(&mut file, opt + 112 + 5 * 8 + 4, reloc_size);

    // Section Headers & Raw Data
    let sec_table = opt + opt_size as usize;
    for (i, sec) in sections.iter().enumerate() {
        let raw_ptr = file.len() as u32;
        let raw_size = align_up(sec.data.len() as u32, FILE_ALIGN);
        file.extend_from_slice(sec.data);
        file.resize(raw_ptr as usize + raw_size as usize, 0);

        let hdr = sec_table + i * 40;
        file[hdr..hdr + 8].copy_from_slice(&sec.name);
        write_u32(&mut file, hdr + 8, sec.data.len() as u32); // VirtualSize
        write_u32(&mut file, hdr + 12, sec.rva); // VirtualAddress
        write_u32(&mut file, hdr + 16, raw_size); // SizeOfRawData
        write_u32(&mut file, hdr + 20, raw_ptr); // PointerToRawData
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
