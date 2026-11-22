# xsxemu Architecture & Roadmap

## 1. Why a Translation Layer (Not Full Hardware Emulation)?

The Xbox Series X|S hardware and software stack shares deep architectural lineage with Windows x86_64 PCs:

- **CPU:** 8-core AMD Zen 2 (x86_64) — guest PE32+ (`AMD64`) code can execute natively on x86_64 hosts using the Microsoft x64 (`win64`) calling convention without dynamic binary translation.
- **Executable Format:** Standard PE32+ (`.exe` / `.dll`) with COFF sections, Import Address Tables (IAT), base relocations (`.reloc`), TLS directories, and `.pdata`/`.xdata` SEH unwind tables.
- **OS / Runtime:** Windows-derived Game OS exposing a curated Win32 subset (`kernelx.dll`, `kernelbase.dll`, `api-ms-win-core-*`) plus Microsoft GDK (`xgameruntime.dll`, `xuser`, `GameInput`, `XAudio2`, `dstorage_x.dll`).
- **Graphics API:** Direct3D 12.x (`d3d12_x.dll` / `d3d12_xs.dll`) — a console-specialized superset/variant of PC Direct3D 12 targeting AMD RDNA 2, adding unified-memory heap flags, direct-flip presentation (`PresentX` + `WaitFrameEventX`), hardware-specific resource layouts, and console shader microcode/DXIL pipelines.

By targeting **Windows x86_64 with native Direct3D 12**, `xsxemu` achieves near-native CPU speed and maps `d3d12_x` calls directly onto the host GPU driver's `ID3D12Device`, minimizing translation overhead compared to cross-API (D3D12 -> Vulkan/Metal) layers.

## 2. Crate Architecture

```
                 +-----------------------------------+
                 |         xsxemu (CLI)              |
                 |  gen-sample | inspect | run       |
                 +-----------------+-----------------+
                                   |
          +------------------------+------------------------+
          |                        |                        |
          v                        v                        v
+-------------------+   +--------------------+   +--------------------+
|   xsx-loader      |   |     xsx-hle        |   |    xsx-testgen     |
| - PE32+ parser    |   | - ModuleRegistry   |   | - x64 assembler    |
| - VirtualMemory   |-->| - kernelx.dll      |   | - PE32+ emitter    |
| - Relocations     |   | - xgameruntime.dll |   +--------------------+
| - IAT patching    |   | - d3d12_x.dll      |
+-------------------+   +---------+----------+
                                  |
                                  v
                        +--------------------+
                        |     xsx-d3d12      |
                        | - COM vtable objs  |
                        | - d3d12_x -> D3D12 |
                        |   translation rules|
                        | - NativeWinD3D12   |
                        | - SoftwareD3D12    |
                        +---------+----------+
                                  |
                                  v
                        +--------------------+
                        |      xsx-abi       |
                        | - #[repr(C)] ABI   |
                        | - extern "win64"   |
                        +--------------------+
```

## 3. Execution Lifecycle (Vertical Slice)

1. **PE32+ Parse (`xsx-loader::pe`):** Reads DOS/COFF/Optional PE32+ headers, section headers, Import Directory Table, and Base Relocation blocks.
2. **Virtual Mapping (`xsx-loader::memory`):** Allocates page-aligned memory via `VirtualAlloc` (Windows) or `mmap` (Unix), copies headers and sections into their RVA offsets.
3. **Base Relocation (`xsx-loader::loader`):** If mapped away from `ImageBase`, applies `IMAGE_REL_BASED_DIR64` and `IMAGE_REL_BASED_HIGHLOW` deltas.
4. **IAT Resolution (`xsx-hle::registry`):** Normalizes DLL names (forwarding `api-ms-win-core-*` / `kernel32.dll` / `kernelbase.dll` -> `kernelx.dll`, and `d3d12_xs.dll` -> `d3d12_x.dll`) and patches every IAT slot with a real `extern "win64"` Rust function pointer.
5. **Memory Protection:** Applies `RX` / `R` / `RW` section protections via `VirtualProtect` / `mprotect`.
6. **Guest Execution:** Invokes the PE entry point as `unsafe extern "win64" fn() -> u32`.
7. **COM & D3D12.x Translation (`xsx-d3d12`):** Guest COM double-indirection calls enter `#[repr(C)]` vtables (`ID3D12DeviceX`, `ID3D12CommandQueueX`, `ID3D12GraphicsCommandListX`, etc.), sanitize Xbox-specific UMA heap flags and `PresentX` direct-flip calls, and execute via the active `GpuBackend`.

## 4. Multi-Phase Roadmap

### Phase 1 — Vertical Slice (Complete)
- [x] Zero-dependency Rust workspace with `extern "win64"` ABI definitions (`xsx-abi`)
- [x] PE32+ parser, executable mapper, `DIR64` base relocator, and IAT patcher (`xsx-loader`)
- [x] `kernelx.dll`, `xgameruntime.dll`, and `d3d12_x.dll` HLE foundation (`xsx-hle`)
- [x] `d3d12_x` COM vtable objects, heap flag sanitization, `PresentX` translation, Windows `d3d12.dll` device bring-up, and software D3D12 rasterizer (`xsx-d3d12`)
- [x] Clean-room x86_64 PE32+ test title generator (`xsx-testgen`) and end-to-end integration tests

### Phase 2 — Real PE32+ Runtime Completeness & Default FSR Integration (Complete)
- [x] PE Export Table parsing & recursive secondary guest DLL loading (`LoadedModuleSet`, `DllMain` lifecycle, `GetModuleHandleA`/`LoadLibraryA`/`GetProcAddress`)
- [x] PE TLS Directory (`IMAGE_DIRECTORY_ENTRY_TLS`) & automatic `PIMAGE_TLS_CALLBACK` execution (`DLL_PROCESS_ATTACH`/`DLL_PROCESS_DETACH`)
- [x] Windows x64 SEH / `.pdata` runtime function table parsing & OS registration (`RtlAddFunctionTable` / `RtlDeleteFunctionTable` on Windows)
- [x] Virtual Xbox drive filesystem (`G:\`, `T:\`, `D:\`, `S:\` with path-traversal containment) and `kernelx.dll` File I/O (`CreateFileA/W`, `ReadFile`, `WriteFile`, `SetFilePointerEx`, `GetFileSizeEx`, `GetFileAttributesA/W`)
- [x] Real OS multithreading & synchronization (`CreateThread`, `WaitForSingleObject`, `GetExitCodeThread`, `SRWLOCK`, `CRITICAL_SECTION`, `Heap*`, `Tls*`)
- [x] First-class default-enabled **AMD FSR** pipeline in `xsx-d3d12` (12-tap EASU + 5-tap RCAS, quality presets, Halton(2, 3) temporal jitter sequence, and SM 6.0 HLSL compute shaders ready for Phase 3 hardware dispatch)

### Phase 3 — Native Windows Direct3D 12 Full Hardware Pipeline & GPU FSR Compute Dispatch (Complete)
- [x] Win32 window + `IDXGIFactory4` + `IDXGISwapChain3` presentation bound to `PresentX`
- [x] Real host `ID3D12CommandAllocator`, `ID3D12GraphicsCommandList`, `ID3D12DescriptorHeap`, `ID3D12Fence`, and COM `ID3D12RootSignatureX` / `ID3D12PipelineStateX` / `Dispatch` / `CopyResource` translation
- [x] Native GPU D3D12 HLSL compilation (`d3dcompiler_47.dll` for `FSR_EASU_HLSL` / `FSR_RCAS_HLSL`), `FsrComputePlan`, and FSR 2/3 temporal hook structures (`FsrTemporalInputs`, Halton(2, 3) jitter)
- [x] Xbox UMA upload staging ring buffer (`UmaStagingRing`) with alignment and fence-based retirement for discrete PC GPUs
- [x] Shader container inspection (`DXBC` / `DXIL` parser & Xbox-only chunk stripper for `XBOX`, `XGCS`, `XDBG`, `GCN0`, `RDNA`)
- [x] Hardware fence & frame-pacing synchronization (`WaitFrameEventX` <-> DXGI `GetFrameLatencyWaitableObject`)

### Phase 4 — Game Input, Audio, Storage & Services (Complete)
- [x] `GameInput.dll` / `XInput1_4.dll` -> Windows `XInput` live bridge + `VirtualGamepad` fallback & `IGameInput` COM interface
- [x] `XAudio2_9.dll` -> `IXAudio2`, `IXAudio2MasteringVoice`, `IXAudio2SourceVoice` COM engine with PCM buffer capture
- [x] `dstorage_x.dll` / `dstorage.dll` -> `IDStorageFactoryX`, `IDStorageFileX`, `IDStorageQueueX` streaming from `VirtualFs`
- [x] `XUser`, `XStore`, and persistent `XGameSave` local profile, license, and on-disk save container/blob storage (`S:\`)

### Phase 5 — Improvements & Real-Title Compatibility (Next)
- [ ] Full hardware GPU command replay (`ID3D12GraphicsCommandList` PSO/RootSignature/VB/RTV/DrawInstanced + GPU compute FSR EASU/RCAS dispatch onto `IDXGISwapChain3` backbuffers)
- [ ] FSR 2/3 temporal reprojection & motion-vector integration
- [ ] WASAPI real-time audio playback backend for `XAudio2_9` mixed PCM stream
- [ ] Expanded CRT / Win32 / WinRT HLE coverage for real Dev Mode & GDK homebrew binaries
- [ ] Performance profiling, SIMD-accelerated software FSR, and shader cache persistence
