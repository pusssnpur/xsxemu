//! Real-Game Xbox Series X ABI Compatibility Test Binary Generator (`realgame_xsx.exe`).
//!
//! Exercises the exact runtime boot flow observed in real Xbox Series X retail GDK titles:
//! 1. `api-ms-win-core-libraryloader-l1-2-0.dll!LoadLibraryW(L"xgameruntime.dll")` -> pseudo-handle
//! 2. `GetProcAddress(h_xgr, "InitializeApiImplEx2")` & `GetProcAddress(h_xgr, "QueryApiImpl")`
//! 3. `QueryApiImpl(&CLSID_XTHREADING_IMPL, &CLSID_XTHREADING_IMPL, &p_thr)` -> slot 27 (`XThreadSetTimeSensitive`)
//! 4. `xmem.dll!XMemAlloc(256, 0x5E50000)` + `XMemGetWorkingSetStatistics(0x20000001, &stats)` + `XMemFree`
//! 5. `d3d12_xs.dll!D3D12SerializeVersionedRootSignature` with Xbox high-byte flag `0x0300_0001`
//! 6. `d3d12_xs.dll!D3D12XboxCreateDevice(null, null, &IID_ID3D12DEVICE, &dev)` (4-arg real Xbox ABI)
//!    - Device Slot 14 (`CreateDescriptorHeap`) -> Heap Slot 9 (`GetCPUDescriptorHandleForHeapStart` in RAX)
//!    - Device Slot 67 (`DeserializeComputePipelineStateX` with FSPX kind-6 native RDNA 2 code)
//!    - Device Slot 76 (`XboxCreateResource` with Xbox layout `0x101`)
//! 7. `kernel32.dll!CreateFileW(L"GAME:\\data\\config.txt", ...)` reading from package & writing to CoW overlay
//! 8. `api-ms-win-crt-runtime-l1-1-0.dll!_configure_narrow_argv` + `kernel32.dll!OutputDebugStringW`

use std::collections::HashMap;

use xsx_abi::com::{
    CLSID_XTHREADING_IMPL, IID_ID3D12DESCRIPTOR_HEAP, IID_ID3D12DEVICE, IID_ID3D12PIPELINE_STATE,
    IID_ID3D12RESOURCE,
};
use xsx_abi::d3d12::{
    D3D12DescriptorHeapDesc, D3D12HeapProperties, D3D12ResourceDesc, D3D12ResourceDimension,
    D3D12RootSignatureDesc, D3D12VersionedRootSignatureDesc, DxgiFormat, DxgiSampleDesc,
    XboxNativeComputePipelineStateDesc, D3D_ROOT_SIGNATURE_VERSION_1_1,
};
use xsx_abi::gdk::{CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, GENERIC_WRITE};
use xsx_loader::{
    IMAGE_DIRECTORY_ENTRY_IMPORT, IMAGE_FILE_MACHINE_AMD64, IMAGE_SCN_MEM_EXECUTE,
    IMAGE_SCN_MEM_READ, IMAGE_SCN_MEM_WRITE, PE32_PLUS_MAGIC,
};

use crate::x64_asm::{Reg64, X64Assembler};

pub const REALGAME_EXIT_CODE: u32 = 0x600D;
pub const REALGAME_PKG_CONFIG: &[u8] = b"package_default=1\n";
pub const REALGAME_OVERLAY_CONFIG: &[u8] = b"overlay_written=1\n";

const IMAGE_BASE: u64 = 0x0000_0001_4000_0000;
const SECTION_ALIGN: u32 = 0x1000;
const FILE_ALIGN: u32 = 0x200;

/// Build `realgame_xsx.exe`, exercising real Xbox Series X retail GDK/d3d12_xs/xmem/CoW-VFS paths.
pub fn build_real_game_xsx_pe() -> Vec<u8> {
    const TEXT_RVA: u32 = 0x1000;
    const RDATA_RVA: u32 = 0x2000;
    const DATA_RVA: u32 = 0x3000;
    const IDATA_RVA: u32 = 0x4000;
    const SIZE_OF_IMAGE: u32 = 0x5000;

    let imports_spec: &[(&str, &[&str])] = &[
        (
            "api-ms-win-core-libraryloader-l1-2-0.dll",
            &["LoadLibraryW", "GetProcAddress"],
        ),
        (
            "kernel32.dll",
            &["CreateFileW", "WriteFile", "CloseHandle", "OutputDebugStringW"],
        ),
        (
            "api-ms-win-crt-runtime-l1-1-0.dll",
            &["_configure_narrow_argv"],
        ),
        (
            "xmem.dll",
            &["XMemAlloc", "XMemFree", "XMemGetWorkingSetStatistics"],
        ),
        (
            "d3d12_xs.dll",
            &[
                "D3D12SerializeVersionedRootSignature",
                "D3D12XboxCreateDevice",
            ],
        ),
    ];
    let (idata_bytes, iat, idata_dir_size) = build_idata(IDATA_RVA, imports_spec);

    // .rdata
    let mut rdata = DataBuilder::new(RDATA_RVA);
    let rva_w_xgr = rdata.push_utf16("xgameruntime.dll");
    let rva_s_init_api = rdata.push_cstr("InitializeApiImplEx2");
    let rva_s_query_api = rdata.push_cstr("QueryApiImpl");
    let rva_w_game_cfg = rdata.push_utf16(r"GAME:\data\config.txt");
    let rva_w_ok_msg = rdata.push_utf16("RealGameCompat OK");
    let rva_overlay_payload = rdata.push_bytes(REALGAME_OVERLAY_CONFIG, 4);

    let rva_clsid_thr = rdata.push_struct(&CLSID_XTHREADING_IMPL);
    let rva_iid_dev = rdata.push_struct(&IID_ID3D12DEVICE);
    let rva_iid_heap = rdata.push_struct(&IID_ID3D12DESCRIPTOR_HEAP);
    let rva_iid_pso = rdata.push_struct(&IID_ID3D12PIPELINE_STATE);
    let rva_iid_res = rdata.push_struct(&IID_ID3D12RESOURCE);

    let rva_rootsig_desc = rdata.push_struct(&D3D12VersionedRootSignatureDesc {
        version: D3D_ROOT_SIGNATURE_VERSION_1_1,
        _pad: 0,
        desc: D3D12RootSignatureDesc {
            num_parameters: 0,
            _pad0: 0,
            p_parameters: core::ptr::null(),
            num_static_samplers: 0,
            _pad1: 0,
            p_static_samplers: core::ptr::null(),
            flags: 0x0300_0001, // Xbox high-byte flag that PC d3d12.dll would reject
            _pad2: 0,
        },
    });
    let rva_heap_desc = rdata.push_struct(&D3D12DescriptorHeapDesc {
        r#type: 0, // CBV_SRV_UAV
        num_descriptors: 64,
        flags: 1,
        node_mask: 0,
    });
    let rva_heap_props = rdata.push_struct(&D3D12HeapProperties {
        r#type: 1, // DEFAULT
        ..Default::default()
    });
    let rva_tex_desc = rdata.push_struct(&D3D12ResourceDesc {
        dimension: D3D12ResourceDimension::Texture2D as u32,
        alignment: 0,
        width: 128,
        height: 128,
        depth_or_array_size: 1,
        mip_levels: 1,
        format: DxgiFormat::R8G8B8A8Unorm as u32,
        sample_desc: DxgiSampleDesc { count: 1, quality: 0 },
        layout: 0x101, // Xbox tiled layout word (rewritten by slot 76)
        flags: 0,
    });
    let rva_fake_rdna2 = rdata.push_bytes(&[0x90u8; 36], 8);
    let rdata_bytes = rdata.finish();

    // .data (mutable pointers/outputs)
    let mut data = DataBuilder::new(DATA_RVA);
    let rva_h_xgr = data.push_u64(0);
    let rva_p_thr = data.push_u64(0);
    let rva_xmem_ptr = data.push_u64(0);
    let rva_xmem_stats = data.push_bytes(&[0u8; 32], 8);
    let rva_rs_blob = data.push_u64(0);
    let rva_rs_err = data.push_u64(0);
    let rva_dev = data.push_u64(0);
    let rva_heap = data.push_u64(0);
    let rva_pso = data.push_u64(0);
    let rva_res = data.push_u64(0);
    let rva_native_desc = data.push_struct(&XboxNativeComputePipelineStateDesc {
        root_signature: core::ptr::null_mut(),
        kind0: core::ptr::null(),
        kind0_size: 28,
        kind1: core::ptr::null(),
        kind1_size: 20,
        kind6: core::ptr::null(), // filled at runtime with RIP-relative rva_fake_rdna2
        kind6_size: 36,
        kind7: core::ptr::null(),
        kind7_size: 0,
    });
    let rva_h_file = data.push_u64(0);
    let rva_bytes_written = data.push_u64(0);
    let data_bytes = data.finish();

    // .text (Win64 ABI)
    let mut asm = X64Assembler::new(TEXT_RVA);
    asm.sub_rsp_imm8(0x68); // 8 (ret) + 0x68 = 0x70 (16-byte aligned!) + 8 stack args room

    let mut fail_jumps = Vec::new();

    // 0. UCRT _configure_narrow_argv(1)
    asm.mov_reg_imm32(Reg64::Rcx, 1);
    asm.call_iat(iat["_configure_narrow_argv"]);

    // 1. LoadLibraryW(L"xgameruntime.dll") -> h_xgr
    asm.lea_rip_rva(Reg64::Rcx, rva_w_xgr);
    asm.call_iat(iat["LoadLibraryW"]);
    asm.mov_rva_from_reg(rva_h_xgr, Reg64::Rax);
    asm.test_eax_eax();
    fail_jumps.push(asm.je_placeholder());

    // 2. GetProcAddress(h_xgr, "InitializeApiImplEx2") -> call(10, 1, 0, null)
    asm.mov_reg_from_rva(Reg64::Rcx, rva_h_xgr);
    asm.lea_rip_rva(Reg64::Rdx, rva_s_init_api);
    asm.call_iat(iat["GetProcAddress"]);
    asm.mov_reg_imm32(Reg64::Rcx, 10);
    asm.mov_reg_imm32(Reg64::Rdx, 1);
    asm.xor_reg32_self(Reg64::R8);
    asm.xor_reg32_self(Reg64::R9);
    asm.call_rax();
    asm.test_eax_eax();
    fail_jumps.push(asm.js_placeholder());

    // 3. GetProcAddress(h_xgr, "QueryApiImpl") -> call(&CLSID_XTHREADING_IMPL, &CLSID_XTHREADING_IMPL, &p_thr)
    asm.mov_reg_from_rva(Reg64::Rcx, rva_h_xgr);
    asm.lea_rip_rva(Reg64::Rdx, rva_s_query_api);
    asm.call_iat(iat["GetProcAddress"]);
    asm.lea_rip_rva(Reg64::Rcx, rva_clsid_thr);
    asm.lea_rip_rva(Reg64::Rdx, rva_clsid_thr);
    asm.lea_rip_rva(Reg64::R8, rva_p_thr);
    asm.call_rax();
    asm.test_eax_eax();
    fail_jumps.push(asm.js_placeholder());

    // Call IXThreadingImpl slot 27 (XThreadSetTimeSensitive(true)) then slot 30 (XThreadIsTimeSensitive)
    asm.mov_reg_from_rva(Reg64::Rcx, rva_p_thr);
    asm.mov_reg_imm32(Reg64::Rdx, 1);
    asm.call_com_rcx_slot(27);
    asm.mov_reg_from_rva(Reg64::Rcx, rva_p_thr);
    asm.call_com_rcx_slot(30);
    asm.cmp_eax_imm32(1);
    fail_jumps.push(asm.jne_placeholder());
    // Reset back to false
    asm.mov_reg_from_rva(Reg64::Rcx, rva_p_thr);
    asm.xor_reg32_self(Reg64::Rdx);
    asm.call_com_rcx_slot(27);

    // 4. xmem.dll: XMemAlloc(256, 0x5E50000), XMemGetWorkingSetStatistics(0x20000001, &stats), XMemFree
    asm.mov_reg_imm32(Reg64::Rcx, 256);
    asm.mov_reg_imm32(Reg64::Rdx, 0x05E5_0000);
    asm.call_iat(iat["XMemAlloc"]);
    asm.mov_rva_from_reg(rva_xmem_ptr, Reg64::Rax);
    asm.test_eax_eax();
    fail_jumps.push(asm.je_placeholder());

    asm.mov_reg_imm32(Reg64::Rcx, 0x2000_0001);
    asm.lea_rip_rva(Reg64::Rdx, rva_xmem_stats);
    asm.call_iat(iat["XMemGetWorkingSetStatistics"]);
    asm.cmp_eax_imm32(1);
    fail_jumps.push(asm.jne_placeholder());

    asm.mov_reg_from_rva(Reg64::Rcx, rva_xmem_ptr);
    asm.xor_reg32_self(Reg64::Rdx);
    asm.call_iat(iat["XMemFree"]);

    // 5. d3d12_xs!D3D12SerializeVersionedRootSignature(&desc, &blob, &err)
    asm.lea_rip_rva(Reg64::Rcx, rva_rootsig_desc);
    asm.lea_rip_rva(Reg64::Rdx, rva_rs_blob);
    asm.lea_rip_rva(Reg64::R8, rva_rs_err);
    asm.call_iat(iat["D3D12SerializeVersionedRootSignature"]);
    asm.test_eax_eax();
    fail_jumps.push(asm.js_placeholder());

    // 6. d3d12_xs!D3D12XboxCreateDevice(null, null, &IID_ID3D12DEVICE, &dev)
    asm.xor_reg32_self(Reg64::Rcx);
    asm.xor_reg32_self(Reg64::Rdx);
    asm.lea_rip_rva(Reg64::R8, rva_iid_dev);
    asm.lea_rip_rva(Reg64::R9, rva_dev);
    asm.call_iat(iat["D3D12XboxCreateDevice"]);
    asm.test_eax_eax();
    fail_jumps.push(asm.js_placeholder());

    // 6a. Device slot 14: CreateDescriptorHeap -> Heap slot 9 returns non-zero handle in RAX
    asm.mov_reg_from_rva(Reg64::Rcx, rva_dev);
    asm.lea_rip_rva(Reg64::Rdx, rva_heap_desc);
    asm.lea_rip_rva(Reg64::R8, rva_iid_heap);
    asm.lea_rip_rva(Reg64::R9, rva_heap);
    asm.call_com_rcx_slot(14);
    asm.test_eax_eax();
    fail_jumps.push(asm.js_placeholder());

    asm.mov_reg_from_rva(Reg64::Rcx, rva_heap);
    asm.call_com_rcx_slot(9); // Xbox RAX return!
    asm.test_eax_eax();
    fail_jumps.push(asm.je_placeholder());

    // 6b. Device slot 67: DeserializeComputePipelineStateX (FSPX kind 6 native RDNA 2)
    // Patch native_desc.kind6 (+0x28) with RIP-relative pointer to rva_fake_rdna2
    asm.lea_rip_rva(Reg64::Rax, rva_fake_rdna2);
    asm.mov_rva_from_reg(rva_native_desc + 0x28, Reg64::Rax);
    asm.mov_reg_from_rva(Reg64::Rcx, rva_dev);
    asm.lea_rip_rva(Reg64::Rdx, rva_native_desc);
    asm.lea_rip_rva(Reg64::R8, rva_iid_pso);
    asm.lea_rip_rva(Reg64::R9, rva_pso);
    asm.call_com_rcx_slot(67);
    asm.test_eax_eax();
    fail_jumps.push(asm.js_placeholder());

    // 6c. Device slot 76: XboxCreateResource (Xbox layout 0x101 -> PC UNKNOWN)
    // args: rcx=dev, rdx=&heap_props, r8=0, r9=&tex_desc, [rsp+0x20]=0, [rsp+0x28]=0, [rsp+0x30]=&IID_RES, [rsp+0x38]=&res
    asm.mov_rsp_off_imm32(0x20, 0);
    asm.xor_reg32_self(Reg64::Rax);
    asm.mov_rsp_off_reg(0x28, Reg64::Rax);
    asm.lea_rip_rva(Reg64::Rax, rva_iid_res);
    asm.mov_rsp_off_reg(0x30, Reg64::Rax);
    asm.lea_rip_rva(Reg64::Rax, rva_res);
    asm.mov_rsp_off_reg(0x38, Reg64::Rax);
    asm.mov_reg_from_rva(Reg64::Rcx, rva_dev);
    asm.lea_rip_rva(Reg64::Rdx, rva_heap_props);
    asm.xor_reg32_self(Reg64::R8);
    asm.lea_rip_rva(Reg64::R9, rva_tex_desc);
    asm.call_com_rcx_slot(76);
    asm.test_eax_eax();
    fail_jumps.push(asm.js_placeholder());

    // 7. CoW VFS test: CreateFileW(L"GAME:\\data\\config.txt", GENERIC_WRITE, 0, null, CREATE_ALWAYS, NORMAL, null)
    asm.mov_rsp_off_imm32(0x20, CREATE_ALWAYS as i32);
    asm.mov_rsp_off_imm32(0x28, FILE_ATTRIBUTE_NORMAL as i32);
    asm.xor_reg32_self(Reg64::Rax);
    asm.mov_rsp_off_reg(0x30, Reg64::Rax);
    asm.lea_rip_rva(Reg64::Rcx, rva_w_game_cfg);
    asm.mov_reg_imm32(Reg64::Rdx, GENERIC_WRITE);
    asm.xor_reg32_self(Reg64::R8);
    asm.xor_reg32_self(Reg64::R9);
    asm.call_iat(iat["CreateFileW"]);
    asm.mov_rva_from_reg(rva_h_file, Reg64::Rax);

    // WriteFile(h_file, REALGAME_OVERLAY_CONFIG, len, &written, null)
    asm.xor_reg32_self(Reg64::Rax);
    asm.mov_rsp_off_reg(0x20, Reg64::Rax);
    asm.mov_reg_from_rva(Reg64::Rcx, rva_h_file);
    asm.lea_rip_rva(Reg64::Rdx, rva_overlay_payload);
    asm.mov_reg_imm32(Reg64::R8, REALGAME_OVERLAY_CONFIG.len() as u32);
    asm.lea_rip_rva(Reg64::R9, rva_bytes_written);
    asm.call_iat(iat["WriteFile"]);
    asm.mov_reg_from_rva(Reg64::Rcx, rva_h_file);
    asm.call_iat(iat["CloseHandle"]);

    // 8. OutputDebugStringW(L"RealGameCompat OK")
    asm.lea_rip_rva(Reg64::Rcx, rva_w_ok_msg);
    asm.call_iat(iat["OutputDebugStringW"]);

    // Success exit
    asm.mov_reg_imm32(Reg64::Rax, REALGAME_EXIT_CODE);
    let done_jmp = asm.jmp_placeholder();

    for j in fail_jumps {
        asm.patch_branch_here(j);
    }
    asm.mov_reg_imm32(Reg64::Rax, 0xBAD0_0001);

    asm.patch_branch_here(done_jmp);
    asm.add_rsp_imm8(0x68);
    asm.ret();

    let text_bytes = asm.finish();

    emit_pe64(
        IMAGE_BASE,
        TEXT_RVA,
        SIZE_OF_IMAGE,
        &[
            SectionSpec {
                name: *b".text\0\0\0",
                rva: TEXT_RVA,
                data: text_bytes,
                chars: IMAGE_SCN_MEM_EXECUTE | IMAGE_SCN_MEM_READ | 0x20,
            },
            SectionSpec {
                name: *b".rdata\0\0",
                rva: RDATA_RVA,
                data: rdata_bytes,
                chars: IMAGE_SCN_MEM_READ | 0x40,
            },
            SectionSpec {
                name: *b".data\0\0\0",
                rva: DATA_RVA,
                data: data_bytes,
                chars: IMAGE_SCN_MEM_READ | IMAGE_SCN_MEM_WRITE | 0x40,
            },
            SectionSpec {
                name: *b".idata\0\0",
                rva: IDATA_RVA,
                data: idata_bytes,
                chars: IMAGE_SCN_MEM_READ | IMAGE_SCN_MEM_WRITE | 0x40,
            },
        ],
        (IDATA_RVA, idata_dir_size),
    )
}

// ---------------------------------------------------------------------------
// Minimal PE64 + Import Table builder helpers
// ---------------------------------------------------------------------------

struct DataBuilder {
    base_rva: u32,
    bytes: Vec<u8>,
}

impl DataBuilder {
    fn new(base_rva: u32) -> Self {
        Self {
            base_rva,
            bytes: Vec::new(),
        }
    }

    fn align(&mut self, a: usize) {
        while !self.bytes.len().is_multiple_of(a) {
            self.bytes.push(0);
        }
    }

    fn push_bytes(&mut self, data: &[u8], align: usize) -> u32 {
        self.align(align);
        let rva = self.base_rva + self.bytes.len() as u32;
        self.bytes.extend_from_slice(data);
        rva
    }

    fn push_cstr(&mut self, s: &str) -> u32 {
        let mut v = s.as_bytes().to_vec();
        v.push(0);
        self.push_bytes(&v, 4)
    }

    fn push_utf16(&mut self, s: &str) -> u32 {
        let mut v = Vec::with_capacity((s.len() + 1) * 2);
        for u in s.encode_utf16() {
            v.extend_from_slice(&u.to_le_bytes());
        }
        v.extend_from_slice(&0u16.to_le_bytes());
        self.push_bytes(&v, 4)
    }

    fn push_u64(&mut self, val: u64) -> u32 {
        self.push_bytes(&val.to_le_bytes(), 8)
    }

    fn push_struct<T: Copy>(&mut self, val: &T) -> u32 {
        let bytes = unsafe {
            core::slice::from_raw_parts(val as *const T as *const u8, core::mem::size_of::<T>())
        };
        self.push_bytes(bytes, core::mem::align_of::<T>().max(8))
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

fn build_idata(
    idata_rva: u32,
    imports: &[(&str, &[&str])],
) -> (Vec<u8>, HashMap<String, u32>, u32) {
    let dir_size = ((imports.len() + 1) * 20) as u32;
    let mut buf = vec![0u8; dir_size as usize];
    let mut iat_map = HashMap::new();

    for (i, &(dll, funcs)) in imports.iter().enumerate() {
        while !buf.len().is_multiple_of(8) {
            buf.push(0);
        }
        let ilt_rva = idata_rva + buf.len() as u32;
        let ilt_off = buf.len();
        buf.resize(ilt_off + (funcs.len() + 1) * 8, 0);

        let iat_rva = idata_rva + buf.len() as u32;
        let iat_off = buf.len();
        buf.resize(iat_off + (funcs.len() + 1) * 8, 0);

        let dll_name_rva = idata_rva + buf.len() as u32;
        buf.extend_from_slice(dll.as_bytes());
        buf.push(0);

        for (j, &fname) in funcs.iter().enumerate() {
            if !buf.len().is_multiple_of(2) {
                buf.push(0);
            }
            let hn_rva = idata_rva + buf.len() as u32;
            buf.extend_from_slice(&0u16.to_le_bytes());
            buf.extend_from_slice(fname.as_bytes());
            buf.push(0);

            let thunk = (hn_rva as u64).to_le_bytes();
            buf[ilt_off + j * 8..ilt_off + j * 8 + 8].copy_from_slice(&thunk);
            buf[iat_off + j * 8..iat_off + j * 8 + 8].copy_from_slice(&thunk);
            iat_map.insert(fname.to_string(), iat_rva + (j * 8) as u32);
        }

        let d = i * 20;
        buf[d..d + 4].copy_from_slice(&ilt_rva.to_le_bytes());
        buf[d + 12..d + 16].copy_from_slice(&dll_name_rva.to_le_bytes());
        buf[d + 16..d + 20].copy_from_slice(&iat_rva.to_le_bytes());
    }

    (buf, iat_map, dir_size)
}

struct SectionSpec {
    name: [u8; 8],
    rva: u32,
    data: Vec<u8>,
    chars: u32,
}

fn align_up(v: u32, a: u32) -> u32 {
    (v + a - 1) & !(a - 1)
}

fn emit_pe64(
    image_base: u64,
    entry_rva: u32,
    size_of_image: u32,
    sections: &[SectionSpec],
    import_dir: (u32, u32),
) -> Vec<u8> {
    let pe_off: u32 = 0x80;
    let opt_size: u16 = 240;
    let headers_size = align_up(
        pe_off + 4 + 20 + opt_size as u32 + (sections.len() as u32) * 40,
        FILE_ALIGN,
    );
    let mut file = vec![0u8; headers_size as usize];

    // DOS header
    file[0..2].copy_from_slice(b"MZ");
    file[0x3C..0x40].copy_from_slice(&pe_off.to_le_bytes());

    let p = pe_off as usize;
    file[p..p + 4].copy_from_slice(b"PE\0\0");
    // COFF header (20 bytes)
    let coff = p + 4;
    file[coff..coff + 2].copy_from_slice(&IMAGE_FILE_MACHINE_AMD64.to_le_bytes());
    file[coff + 2..coff + 4].copy_from_slice(&(sections.len() as u16).to_le_bytes());
    file[coff + 16..coff + 18].copy_from_slice(&opt_size.to_le_bytes());
    file[coff + 18..coff + 20].copy_from_slice(&0x0022u16.to_le_bytes()); // EXECUTABLE | LARGE_ADDRESS_AWARE

    // Optional Header PE32+ (240 bytes)
    let opt = coff + 20;
    file[opt..opt + 2].copy_from_slice(&PE32_PLUS_MAGIC.to_le_bytes());
    file[opt + 16..opt + 20].copy_from_slice(&entry_rva.to_le_bytes());
    file[opt + 24..opt + 32].copy_from_slice(&image_base.to_le_bytes());
    file[opt + 32..opt + 36].copy_from_slice(&SECTION_ALIGN.to_le_bytes());
    file[opt + 36..opt + 40].copy_from_slice(&FILE_ALIGN.to_le_bytes());
    file[opt + 56..opt + 60].copy_from_slice(&size_of_image.to_le_bytes());
    file[opt + 60..opt + 64].copy_from_slice(&headers_size.to_le_bytes());
    file[opt + 68..opt + 70].copy_from_slice(&3u16.to_le_bytes()); // CUI
    file[opt + 108..opt + 112].copy_from_slice(&16u32.to_le_bytes()); // NumberOfRvaAndSizes

    let dd_base = opt + 112;
    let imp_off = dd_base + IMAGE_DIRECTORY_ENTRY_IMPORT * 8;
    file[imp_off..imp_off + 4].copy_from_slice(&import_dir.0.to_le_bytes());
    file[imp_off + 4..imp_off + 8].copy_from_slice(&import_dir.1.to_le_bytes());

    // Section headers
    let sec_base = opt + opt_size as usize;
    let mut raw_cursor = headers_size;
    for (idx, sec) in sections.iter().enumerate() {
        let raw_size = align_up(sec.data.len().max(1) as u32, FILE_ALIGN);
        let sh = sec_base + idx * 40;
        file[sh..sh + 8].copy_from_slice(&sec.name);
        file[sh + 8..sh + 12].copy_from_slice(&(sec.data.len() as u32).to_le_bytes());
        file[sh + 12..sh + 16].copy_from_slice(&sec.rva.to_le_bytes());
        file[sh + 16..sh + 20].copy_from_slice(&raw_size.to_le_bytes());
        file[sh + 20..sh + 24].copy_from_slice(&raw_cursor.to_le_bytes());
        file[sh + 36..sh + 40].copy_from_slice(&sec.chars.to_le_bytes());

        file.resize((raw_cursor + raw_size) as usize, 0);
        file[raw_cursor as usize..raw_cursor as usize + sec.data.len()]
            .copy_from_slice(&sec.data);
        raw_cursor += raw_size;
    }

    file
}
