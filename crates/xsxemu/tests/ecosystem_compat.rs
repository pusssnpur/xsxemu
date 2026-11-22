//! End-to-end integration tests for the Xbox Series X ecosystem rewrite:
//! - `MicrosoftGame.Config` manifest boot + `TitleId` propagation into `QueryApiImpl(XGame)`
//! - Expanded `xgameruntime.dll` `QueryApiImpl` COM tables (`XGame`, `XSystem`, `XUser`,
//!   `XUserGamertag`, `XGameSave`, `XPackage`, `XGameRuntimeFeature`, `XGameEvent`)
//! - `xg_xs.dll` 64 KiB RDNA 2 texture layout computer & swizzle/detile roundtrip
//! - `acphal.dll` APU heap + SHAPE contexts + `IAcpHal` audio channel
//! - `amd_fidelityfx_upscaler_xbox.dll` wired into real multithreaded AMD FSR EASU+RCAS
//! - `xcurl.dll` & `libHttpClient.GDK.dll` offline mock + deterministic fallback
//! - `bink2w64_scarlett.dll` & `XboxBvhBuilder.dll` software DXR BLAS/TLAS ray tracing
//! - PE32+ Delay-Load Import Directory (`IMAGE_DIRECTORY_ENTRY_DELAY_IMPORT`) & Ordinal `#1`
//!   resolution (`XAudio2_9.dll!#1`, `GameInput.dll!#1`)
//! - `XvdContainer` unencrypted/encrypted validation, `KeyboardGamepadMapper`, and RDNA 2
//!   disassembler + SM 6.0 HLSL recompiler.

use core::ffi::{c_char, c_void};
use std::ffi::CStr;
use std::fs;
use std::sync::Arc;

use xsx_abi::com::{
    Guid, HResult, CLSID_XGAMEEVENT_API, CLSID_XGAMERUNTIME_FEATURE_API, CLSID_XGAMESAVE_API,
    CLSID_XGAME_API, CLSID_XPACKAGE_API, CLSID_XSYSTEM_API, CLSID_XUSER_API, IID_XGAME_LATER,
    IID_XPACKAGE_LATER, IID_XSYSTEM_LATER, IID_XUSER_GAMERTAG, IID_XUSER_LATER, S_OK,
};
use xsx_abi::gdk::Handle;
use xsx_abi::services::XAsyncBlock;
use xsx_d3d12::rdna2_isa::encode;
use xsx_d3d12::{
    disassemble_rdna2, format_rdna2_disassembly, lift_rdna2_to_hlsl, FsrConfig, ShaderStage,
    TranslatorConfig,
};
use xsx_hle::acphal::{AcpHalMessage, AcpShapeContextCounts, ApuHeapState};
use xsx_hle::middleware::{
    FfxApiHeader, FfxCreateContextDescUpscale, FfxDispatchDescUpscale, XboxBvhBuildDesc,
    XboxBvhTriangle, XboxRayDesc, XboxRayHit, FFX_API_DESC_TYPE_UPSCALE_CREATE,
    FFX_API_DESC_TYPE_UPSCALE_DISPATCH, FFX_API_RETURN_OK,
};
use xsx_hle::xg_xs::{XgResourceLayout, XgSampleDesc, XgTexture2DDesc, XG_SWIZZLE_MODE_64KB_R_X};
use xsx_hle::{
    set_active_title_id, GuestRuntime, KeyCode, KeyboardGamepadMapper, ModuleRegistry, RuntimeGuard,
    VirtualFs,
};
use xsx_loader::{
    build_synthetic_xvd, GameConfig, ImportResolver, ImportSymbol, LoadedImage, PeImage,
    TitleMemoryMode, XvdContainer, XvdError, IMAGE_DIRECTORY_ENTRY_DELAY_IMPORT,
};

/// Build a minimal valid PE32+ executable that uses BOTH:
/// - a Delay-Load Import Descriptor (`DataDirectory[13]`) importing `XAudio2_9.dll!#1` (ordinal 1)
/// - a standard Import Descriptor importing `GameInput.dll!#1` (ordinal 1)
/// Entry point returns `0x1337` after checking both IAT slots were patched non-zero.
fn build_delay_and_ordinal_pe() -> Vec<u8> {
    const IMAGE_BASE: u64 = 0x0000_0001_4000_0000;
    const TEXT_RVA: u32 = 0x1000;
    const RDATA_RVA: u32 = 0x2000;
    const FILE_ALIGN: usize = 0x200;

    // .text: load both IAT slots (RDATA+0x100 and RDATA+0x110), ensure non-zero, return 0x1337
    // RIP-relative offsets from .text (0x1000):
    //   0x1000: 48 8B 05 [disp32 -> 0x2100]  ; mov rax, [rip + 0x10F9] (7 bytes, next=0x1007)
    //   0x1007: 48 85 C0                     ; test rax, rax (3 bytes, next=0x100A)
    //   0x100A: 74 13                        ; jz fail (+19 -> 0x101F) (2 bytes, next=0x100C)
    //   0x100C: 48 8B 0D [disp32 -> 0x2110]  ; mov rcx, [rip + 0x10FD] (7 bytes, next=0x1013)
    //   0x1013: 48 85 C9                     ; test rcx, rcx (3 bytes, next=0x1016)
    //   0x1016: 74 07                        ; jz fail (+7 -> 0x101F) (2 bytes, next=0x1018)
    //   0x1018: B8 37 13 00 00               ; mov eax, 0x1337 (5 bytes, next=0x101D)
    //   0x101D: C3                           ; ret (1 byte, next=0x101E)
    //   0x101E: 90                           ; nop
    //   0x101F: 31 C0                        ; xor eax, eax
    //   0x1021: C3                           ; ret
    let mut text = vec![0u8; FILE_ALIGN];
    let code: [u8; 34] = [
        0x48, 0x8B, 0x05, 0xF9, 0x10, 0x00, 0x00, // mov rax, [0x2100]
        0x48, 0x85, 0xC0, // test rax, rax
        0x74, 0x13, // jz 0x101F
        0x48, 0x8B, 0x0D, 0xFD, 0x10, 0x00, 0x00, // mov rcx, [0x2110]
        0x48, 0x85, 0xC9, // test rcx, rcx
        0x74, 0x07, // jz 0x101F
        0xB8, 0x37, 0x13, 0x00, 0x00, // mov eax, 0x1337
        0xC3, // ret
        0x90, // nop
        0x31, 0xC0, // xor eax, eax
        0xC3, // ret
    ];
    text[..code.len()].copy_from_slice(&code);

    // .rdata layout (base RVA 0x2000):
    //   +0x000: Standard Import Directory (2 * 20 bytes = 40 bytes)
    //   +0x040: Delay-Load Import Directory (2 * 32 bytes = 64 bytes)
    //   +0x0A0: "GameInput.dll\0"
    //   +0x0C0: "XAudio2_9.dll\0"
    //   +0x0E0: GameInput INT [0x8000_0000_0000_0001, 0]
    //   +0x0F0: XAudio2 INT   [0x8000_0000_0000_0001, 0]
    //   +0x100: GameInput IAT [0x8000_0000_0000_0001, 0]
    //   +0x110: XAudio2 Delay IAT [0, 0]
    let mut rdata = vec![0u8; FILE_ALIGN];
    let put32 = |buf: &mut [u8], off: usize, v: u32| buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
    let put64 = |buf: &mut [u8], off: usize, v: u64| buf[off..off + 8].copy_from_slice(&v.to_le_bytes());

    // Standard Import Descriptor for GameInput.dll!#1
    put32(&mut rdata, 0x000, RDATA_RVA + 0x0E0); // OriginalFirstThunk
    put32(&mut rdata, 0x00C, RDATA_RVA + 0x0A0); // Name
    put32(&mut rdata, 0x010, RDATA_RVA + 0x100); // FirstThunk (IAT)

    // Delay-Load Import Descriptor (attrs=1 RVA-based) for XAudio2_9.dll!#1
    put32(&mut rdata, 0x040, 1); // grAttrs (RVA-based)
    put32(&mut rdata, 0x044, RDATA_RVA + 0x0C0); // szName
    put32(&mut rdata, 0x048, 0); // phmod
    put32(&mut rdata, 0x04C, RDATA_RVA + 0x110); // pIAT
    put32(&mut rdata, 0x050, RDATA_RVA + 0x0F0); // pINT

    rdata[0x0A0..0x0AE].copy_from_slice(b"GameInput.dll\0");
    rdata[0x0C0..0x0CE].copy_from_slice(b"XAudio2_9.dll\0");

    let ord1 = (1u64 << 63) | 1u64;
    put64(&mut rdata, 0x0E0, ord1);
    put64(&mut rdata, 0x0F0, ord1);
    put64(&mut rdata, 0x100, ord1);

    // Headers
    let mut img = vec![0u8; FILE_ALIGN * 3];
    img[0..2].copy_from_slice(b"MZ");
    let pe_off = 0x80usize;
    img[0x3C..0x40].copy_from_slice(&(pe_off as u32).to_le_bytes());
    img[pe_off..pe_off + 4].copy_from_slice(b"PE\0\0");

    let coff = pe_off + 4;
    img[coff..coff + 2].copy_from_slice(&0x8664u16.to_le_bytes()); // AMD64
    img[coff + 2..coff + 4].copy_from_slice(&2u16.to_le_bytes()); // 2 sections
    img[coff + 16..coff + 18].copy_from_slice(&240u16.to_le_bytes()); // SizeOfOptionalHeader
    img[coff + 18..coff + 20].copy_from_slice(&0x0022u16.to_le_bytes()); // Executable

    let opt = coff + 20;
    img[opt..opt + 2].copy_from_slice(&0x020Bu16.to_le_bytes()); // PE32+
    img[opt + 16..opt + 20].copy_from_slice(&TEXT_RVA.to_le_bytes()); // EntryPoint
    img[opt + 24..opt + 32].copy_from_slice(&IMAGE_BASE.to_le_bytes());
    img[opt + 32..opt + 36].copy_from_slice(&0x1000u32.to_le_bytes()); // SectionAlign
    img[opt + 36..opt + 40].copy_from_slice(&(FILE_ALIGN as u32).to_le_bytes());
    img[opt + 56..opt + 60].copy_from_slice(&0x3000u32.to_le_bytes()); // SizeOfImage
    img[opt + 60..opt + 64].copy_from_slice(&(FILE_ALIGN as u32).to_le_bytes());
    img[opt + 68..opt + 70].copy_from_slice(&3u16.to_le_bytes()); // Console
    img[opt + 108..opt + 112].copy_from_slice(&16u32.to_le_bytes()); // NumberOfRvaAndSizes

    // DataDirectory[1] = Import, DataDirectory[13] = DelayImport
    let dd = opt + 112;
    img[dd + 8..dd + 12].copy_from_slice(&RDATA_RVA.to_le_bytes());
    img[dd + 12..dd + 16].copy_from_slice(&40u32.to_le_bytes());
    let dd_delay = dd + IMAGE_DIRECTORY_ENTRY_DELAY_IMPORT * 8;
    img[dd_delay..dd_delay + 4].copy_from_slice(&(RDATA_RVA + 0x040).to_le_bytes());
    img[dd_delay + 4..dd_delay + 8].copy_from_slice(&64u32.to_le_bytes());

    // Section 0: .text
    let sec0 = opt + 240;
    img[sec0..sec0 + 5].copy_from_slice(b".text");
    img[sec0 + 8..sec0 + 12].copy_from_slice(&0x1000u32.to_le_bytes());
    img[sec0 + 12..sec0 + 16].copy_from_slice(&TEXT_RVA.to_le_bytes());
    img[sec0 + 16..sec0 + 20].copy_from_slice(&(FILE_ALIGN as u32).to_le_bytes());
    img[sec0 + 20..sec0 + 24].copy_from_slice(&(FILE_ALIGN as u32).to_le_bytes());
    img[sec0 + 36..sec0 + 40].copy_from_slice(&0x6000_0020u32.to_le_bytes()); // RX

    // Section 1: .rdata (RW so IAT slots are readable after patch)
    let sec1 = sec0 + 40;
    img[sec1..sec1 + 6].copy_from_slice(b".rdata");
    img[sec1 + 8..sec1 + 12].copy_from_slice(&0x1000u32.to_le_bytes());
    img[sec1 + 12..sec1 + 16].copy_from_slice(&RDATA_RVA.to_le_bytes());
    img[sec1 + 16..sec1 + 20].copy_from_slice(&(FILE_ALIGN as u32).to_le_bytes());
    img[sec1 + 20..sec1 + 24].copy_from_slice(&((FILE_ALIGN * 2) as u32).to_le_bytes());
    img[sec1 + 36..sec1 + 40].copy_from_slice(&0xC000_0040u32.to_le_bytes()); // RW

    img[FILE_ALIGN..FILE_ALIGN * 2].copy_from_slice(&text);
    img[FILE_ALIGN * 2..FILE_ALIGN * 3].copy_from_slice(&rdata);
    img
}

#[test]
fn xbox_ecosystem_end_to_end_manifest_delayload_ordinals_and_subsystems() {
    let tmp = std::env::temp_dir().join("xsxemu_ecosystem_compat_test");
    let _ = fs::remove_dir_all(&tmp);
    let pkg = tmp.join("pkg");
    let scratch = tmp.join("scratch");
    fs::create_dir_all(&pkg).unwrap();

    // 1. MicrosoftGame.Config parsing & TitleId wiring
    let manifest_xml = r#"<?xml version="1.0" encoding="utf-8"?>
<Game configVersion="1">
  <Identity Name="EcosystemTestTitle" Publisher="CN=XsxEmu" Version="1.0.0.0"/>
  <TitleId>4B5A9901</TitleId>
  <StoreId>9NBLGGH9ECOS</StoreId>
  <ShellVisuals DefaultDisplayName="Ecosystem Test Title"/>
  <ExecutableList>
    <Executable Name="title_xsx.exe" Id="Gen9" TargetDeviceFamily="XboxGen9"/>
  </ExecutableList>
  <PersistentLocalStorage><SizeMB>1024</SizeMB></PersistentLocalStorage>
  <XboxAnacondaTitleMemory>Advanced</XboxAnacondaTitleMemory>
</Game>"#;
    fs::write(pkg.join("MicrosoftGame.Config"), manifest_xml).unwrap();
    let pe_bytes = build_delay_and_ordinal_pe();
    fs::write(pkg.join("title_xsx.exe"), &pe_bytes).unwrap();

    let (cfg, _) = GameConfig::load_from_package_dir(&pkg).unwrap();
    assert_eq!(cfg.title_id_u32, Some(0x4B5A_9901));
    assert_eq!(cfg.anaconda_memory_mode, TitleMemoryMode::Advanced);
    set_active_title_id(cfg.title_id_u32.unwrap());

    // 2. PE Delay-Load + Ordinal #1 resolution & native execution
    let exe_path = cfg.resolve_primary_executable_path(&pkg).unwrap();
    let pe = PeImage::parse(&fs::read(&exe_path).unwrap()).unwrap();
    assert_eq!(pe.imports.len(), 1);
    assert_eq!(pe.delay_imports.len(), 1);
    assert_eq!(pe.delay_imports[0].dll_name, "XAudio2_9.dll");
    assert_eq!(pe.delay_imports[0].entries[0].symbol, ImportSymbol::Ordinal(1));

    let reg = ModuleRegistry::new_default(false);
    let image = LoadedImage::load(&pe, &reg, true).unwrap();
    assert_eq!(image.resolved_imports().len(), 2);
    assert!(image.resolved_imports().iter().all(|r| !r.is_stub));

    let vfs = VirtualFs::with_overlay(pkg.clone(), tmp.join("overlay"), scratch.clone());
    vfs.ensure_roots().unwrap();
    let rt = GuestRuntime::with_options(
        TranslatorConfig {
            fsr: FsrConfig::default(),
            ..TranslatorConfig::default()
        },
        vfs,
        xsx_hle::RtcConfig::default(),
        false,
        false,
    );
    let _guard = RuntimeGuard::activate(Arc::clone(&rt));
    let exit_code = unsafe { image.execute_entry() };
    assert_eq!(exit_code, 0x1337);

    // 3. Expanded xgameruntime QueryApiImpl COM tables (XGame, XSystem, XUser, XUserGamertag, XGameSave, XPackage, Feature, Event)
    let query_api_addr = reg
        .resolve(
            "xgameruntime.dll",
            &ImportSymbol::Name {
                hint: 0,
                name: "QueryApiImpl".into(),
            },
        )
        .unwrap()
        .0;
    type FnQueryApi = unsafe extern "win64" fn(*const Guid, *const Guid, *mut *mut c_void) -> HResult;
    let query_api: FnQueryApi = unsafe { core::mem::transmute(query_api_addr) };

    unsafe {
        // XGame via later GDK IID -> XGameGetXboxTitleId reads MicrosoftGame.Config TitleId!
        let mut game_api: *mut c_void = core::ptr::null_mut();
        assert_eq!(query_api(&CLSID_XGAME_API, &IID_XGAME_LATER, &mut game_api), S_OK);
        let vtbl = *(game_api as *const *const usize);
        let get_tid: unsafe extern "win64" fn(*mut c_void, *mut u32) -> HResult =
            core::mem::transmute(*vtbl.add(3));
        let mut tid = 0u32;
        assert_eq!(get_tid(game_api, &mut tid), S_OK);
        assert_eq!(tid, 0x4B5A_9901);

        // XSystem via later GDK IID -> GetXboxLiveSandboxId
        let mut sys_api: *mut c_void = core::ptr::null_mut();
        assert_eq!(query_api(&CLSID_XSYSTEM_API, &IID_XSYSTEM_LATER, &mut sys_api), S_OK);
        let svtbl = *(sys_api as *const *const usize);
        let get_sandbox: unsafe extern "win64" fn(*mut c_void, usize, *mut u8, *mut usize) -> HResult =
            core::mem::transmute(*svtbl.add(4));
        let mut sb = [0u8; 32];
        let mut used = 0usize;
        assert_eq!(get_sandbox(sys_api, sb.len(), sb.as_mut_ptr(), &mut used), S_OK);
        assert_eq!(&sb[..6], b"RETAIL");

        // XUser -> AddAsync/AddResult + QueryInterface(IID_XUSER_GAMERTAG)
        let mut user_api: *mut c_void = core::ptr::null_mut();
        assert_eq!(query_api(&CLSID_XUSER_API, &IID_XUSER_LATER, &mut user_api), S_OK);
        let uvtbl = *(user_api as *const *const usize);
        let add_async: unsafe extern "win64" fn(*mut c_void, u32, *mut XAsyncBlock) -> HResult =
            core::mem::transmute(*uvtbl.add(7));
        let add_res: unsafe extern "win64" fn(*mut c_void, *mut XAsyncBlock, *mut Handle) -> HResult =
            core::mem::transmute(*uvtbl.add(8));
        let mut ab: XAsyncBlock = core::mem::zeroed();
        assert_eq!(add_async(user_api, 0, &mut ab), S_OK);
        let mut user_h: Handle = core::ptr::null_mut();
        assert_eq!(add_res(user_api, &mut ab, &mut user_h), S_OK);
        assert!(!user_h.is_null());

        let mut gt_api: *mut c_void = core::ptr::null_mut();
        assert_eq!(query_api(&CLSID_XUSER_API, &IID_XUSER_GAMERTAG, &mut gt_api), S_OK);
        let gtvtbl = *(gt_api as *const *const usize);
        let get_gt: unsafe extern "win64" fn(*mut c_void, Handle, u32, usize, *mut c_char, *mut usize) -> HResult =
            core::mem::transmute(*gtvtbl.add(3));
        let mut gt_buf = [0i8; 32];
        let mut gt_used = 0usize;
        assert_eq!(get_gt(gt_api, user_h, 0, gt_buf.len(), gt_buf.as_mut_ptr(), &mut gt_used), S_OK);
        assert_eq!(CStr::from_ptr(gt_buf.as_ptr()).to_str().unwrap(), "XsxPlayer1");

        // XGameSave via QueryApiImpl
        let mut save_api: *mut c_void = core::ptr::null_mut();
        assert_eq!(query_api(&CLSID_XGAMESAVE_API, &CLSID_XGAMESAVE_API, &mut save_api), S_OK);
        let save_vtbl = *(save_api as *const *const usize);
        let get_quota: unsafe extern "win64" fn(*mut c_void, Handle, *mut i64) -> HResult =
            core::mem::transmute(*save_vtbl.add(7));
        let mut quota = 0i64;
        assert_eq!(get_quota(save_api, core::ptr::null_mut(), &mut quota), S_OK);
        assert!(quota > 0);

        // XPackage, XGameRuntimeFeature, XGameEvent via QueryApiImpl
        let mut pkg_api: *mut c_void = core::ptr::null_mut();
        assert_eq!(query_api(&CLSID_XPACKAGE_API, &IID_XPACKAGE_LATER, &mut pkg_api), S_OK);
        let mut feat_api: *mut c_void = core::ptr::null_mut();
        assert_eq!(
            query_api(&CLSID_XGAMERUNTIME_FEATURE_API, &CLSID_XGAMERUNTIME_FEATURE_API, &mut feat_api),
            S_OK
        );
        let fvtbl = *(feat_api as *const *const usize);
        let is_avail: unsafe extern "win64" fn(*mut c_void, u32) -> bool =
            core::mem::transmute(*fvtbl.add(3));
        assert!(is_avail(feat_api, 10));

        let mut evt_api: *mut c_void = core::ptr::null_mut();
        assert_eq!(query_api(&CLSID_XGAMEEVENT_API, &CLSID_XGAMEEVENT_API, &mut evt_api), S_OK);
    }

    // 4. Verify all newly added Xbox ecosystem DLLs resolve cleanly through ModuleRegistry
    for (dll, name) in [
        ("xg_xs.dll", "XGComputeOptimalSwizzleMode"),
        ("xg.dll", "XGCreateTexture2DComputer"),
        ("acphal.dll", "AcpHalCreate"),
        ("xapu.dll", "ApuCreateHeap"),
        ("amd_fidelityfx_upscaler_xbox.dll", "ffxUpscalerDispatch"),
        ("amd_fidelityfx_loader_xbox.dll", "ffxCreateContext"),
        ("xcurl.dll", "curl_easy_perform"),
        ("libHttpClient.GDK.dll", "HCHttpCallPerformAsync"),
        ("bink2w64_scarlett.dll", "BinkOpen"),
        ("XboxBvhBuilder.dll", "XboxBvhBuildTlas"),
        ("pixevt.dll", "PIXBeginEvent"),
    ] {
        let resolved = reg.resolve(
            dll,
            &ImportSymbol::Name {
                hint: 0,
                name: name.into(),
            },
        );
        assert!(resolved.is_some() && !resolved.unwrap().1, "{dll}!{name} must resolve natively");
    }

    // 5. Exercise xg_xs, acphal, FidelityFX FSR upscaler, and XboxBvhBuilder via registry exports
    unsafe {
        let desc = XgTexture2DDesc {
            width: 32,
            height: 32,
            mip_levels: 1,
            array_size: 1,
            format: 28,
            sample_desc: XgSampleDesc { count: 1, quality: 0 },
            swizzle_mode: XG_SWIZZLE_MODE_64KB_R_X,
            ..Default::default()
        };
        let mut comp: *mut c_void = core::ptr::null_mut();
        assert_eq!(xsx_hle::xg_xs::hle_xg_create_texture2d_computer(&desc, &mut comp), S_OK);
        let mut layout = XgResourceLayout::default();
        let vtbl = *(comp as *const *const usize);
        let get_layout: unsafe extern "win64" fn(*mut c_void, *mut XgResourceLayout) -> HResult =
            core::mem::transmute(*vtbl.add(2));
        let release: unsafe extern "win64" fn(*mut c_void) -> u32 =
            core::mem::transmute(*vtbl.add(1));
        assert_eq!(get_layout(comp, &mut layout), S_OK);
        assert!(layout.size_bytes >= 65536);
        assert_eq!(release(comp), 0);

        let mut apu_state = ApuHeapState::default();
        assert_eq!(xsx_hle::acphal::hle_apu_create_heap(8 * 1024 * 1024, 0), S_OK);
        assert_eq!(xsx_hle::acphal::hle_apu_heap_get_state(&mut apu_state, 0), S_OK);
        let mut shape = AcpShapeContextCounts { num_src_contexts: 2, ..Default::default() };
        assert_eq!(xsx_hle::acphal::hle_acphal_allocate_shape_contexts(&mut shape), S_OK);
        assert!(!shape.src_contexts.is_null());
        let mut acp: *mut c_void = core::ptr::null_mut();
        assert_eq!(xsx_hle::acphal::hle_acphal_create(&mut acp), S_OK);
        let avtbl = *(acp as *const *const usize);
        let connect: unsafe extern "win64" fn(*mut c_void, u32, u32) -> HResult = core::mem::transmute(*avtbl);
        let pop: unsafe extern "win64" fn(*mut c_void, *mut AcpHalMessage) -> u8 = core::mem::transmute(*avtbl.add(3));
        let acp_rel: unsafe extern "win64" fn(*mut c_void) -> u32 = core::mem::transmute(*avtbl.add(5));
        assert_eq!(connect(acp, 32, 32), S_OK);
        let mut msg = AcpHalMessage::default();
        assert_eq!(pop(acp, &mut msg), 1);
        assert_eq!(msg.msg_type, 0x01);
        assert_eq!(acp_rel(acp), 0);
        assert_eq!(xsx_hle::acphal::hle_acphal_release_shape_contexts(), S_OK);

        // FidelityFX Upscaler Xbox -> real FSR EASU + RCAS
        let mut ffx_ctx: *mut c_void = core::ptr::null_mut();
        let mut create = FfxCreateContextDescUpscale {
            header: FfxApiHeader { desc_type: FFX_API_DESC_TYPE_UPSCALE_CREATE, p_next: core::ptr::null_mut() },
            flags: 0,
            max_render_width: 16,
            max_render_height: 16,
            max_upscale_width: 24,
            max_upscale_height: 24,
        };
        assert_eq!(xsx_hle::middleware::hle_ffx_upscaler_create_context(&mut ffx_ctx, &mut create.header), FFX_API_RETURN_OK);
        let in_px = vec![200u8; 16 * 16 * 4];
        let mut out_px = vec![0u8; 24 * 24 * 4];
        let mut hash = 0u64;
        let disp = FfxDispatchDescUpscale {
            header: FfxApiHeader { desc_type: FFX_API_DESC_TYPE_UPSCALE_DISPATCH, p_next: core::ptr::null_mut() },
            color_rgba8: in_px.as_ptr(),
            output_rgba8: out_px.as_mut_ptr(),
            render_width: 16,
            render_height: 16,
            upscale_width: 24,
            upscale_height: 24,
            jitter_offset_x: 0.0,
            jitter_offset_y: 0.0,
            sharpness: 0.8,
            reset: 1,
            out_frame_hash: &mut hash,
        };
        assert_eq!(xsx_hle::middleware::hle_ffx_upscaler_dispatch(&mut ffx_ctx, &disp.header), FFX_API_RETURN_OK);
        assert_ne!(hash, 0);
        assert_eq!(xsx_hle::middleware::hle_ffx_upscaler_destroy_context(&mut ffx_ctx), FFX_API_RETURN_OK);

        // XboxBvhBuilder BLAS -> TLAS -> ray hit
        let tri = XboxBvhTriangle {
            v0: [-1.0, -1.0, 4.0],
            v1: [1.0, -1.0, 4.0],
            v2: [0.0, 1.0, 4.0],
            color: [0.4, 0.8, 0.2],
            reflectivity: 0.0,
        };
        let bdesc = XboxBvhBuildDesc { triangles: &tri, triangle_count: 1, build_flags: 0 };
        let (mut blas, mut tlas) = (0u64, 0u64);
        assert_eq!(xsx_hle::middleware::hle_xbox_bvh_build_blas(&bdesc, &mut blas), S_OK);
        assert_eq!(xsx_hle::middleware::hle_xbox_bvh_build_tlas(&blas, 1, &mut tlas), S_OK);
        let ray = XboxRayDesc { origin: [0.0, 0.0, 0.0], t_min: 0.01, direction: [0.0, 0.0, 1.0], t_max: 20.0 };
        let mut hit = XboxRayHit::default();
        assert_eq!(xsx_hle::middleware::hle_xbox_bvh_trace_single_ray(tlas, &ray, &mut hit), S_OK);
        assert_eq!(hit.hit, 1);
        assert!((hit.t - 4.0).abs() < 1e-4);
        assert_eq!(xsx_hle::middleware::hle_xbox_bvh_destroy(tlas), S_OK);
        assert_eq!(xsx_hle::middleware::hle_xbox_bvh_destroy(blas), S_OK);
    }

    // 6. Keyboard-to-Gamepad bridge + XVD container + RDNA 2 Disasm & HLSL Lifting
    let mut kbd = KeyboardGamepadMapper::default();
    kbd.key_down(KeyCode::W);
    kbd.key_down(KeyCode::Space);
    let pad = kbd.key_down(KeyCode::Q);
    assert_eq!(pad.s_thumb_ly, 32767);
    assert_eq!(pad.w_buttons & 0x1100, 0x1100); // A + LB

    let xvd_ok = XvdContainer::open_bytes(build_synthetic_xvd(b"dev-package", 1, false)).unwrap();
    assert_eq!(xvd_ok.read_user_data(0, 11).unwrap(), b"dev-package");
    let xvd_enc = XvdContainer::open_bytes(build_synthetic_xvd(b"retail", 1, true)).unwrap();
    assert_eq!(xvd_enc.read_user_data(0, 6), Err(XvdError::EncryptedImageNotSupported));

    let ex = encode::exp(0, 0xF, true, [0, 1, 2, 3]);
    let isa = encode::words_to_bytes(&[
        encode::v_mov_b32(0, 242), // 1.0
        encode::v_mov_b32(1, 240), // 0.5
        encode::v_mul_f32(2, 256, 1),
        encode::v_mov_b32(3, 242), // 1.0
        ex[0],
        ex[1],
        encode::s_endpgm(),
    ]);
    let insns = disassemble_rdna2(&isa);
    let dis = format_rdna2_disassembly(&insns);
    assert!(dis.contains("v_mul_f32") && dis.contains("s_endpgm"));
    let hlsl = lift_rdna2_to_hlsl(&isa, ShaderStage::Pixel).unwrap();
    assert!(hlsl.hlsl.contains("SV_Target0") && hlsl.hlsl.contains("_out.mrt0 = float4("));

    let _ = fs::remove_dir_all(&tmp);
}
