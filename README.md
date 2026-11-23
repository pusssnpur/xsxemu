# xsxemu

A clean-room **Xbox Series X|S (Scarlett / Anaconda)** game translation layer written in **Rust** with zero external dependencies, targeting **Windows x86_64 with native Direct3D 12** (plus a built-in software Direct3D 12 backend for cross-platform headless CI and verification).

> **Clean-room & Legal Notice:** `xsxemu` is an independent clean-room compatibility research project. It contains no proprietary Microsoft SDK headers, copyrighted binaries, firmware, or cryptographic keys, and does not bypass DRM or package encryption. It targets unencrypted PE32+ executables (homebrew, Dev Mode builds, and clean-room test binaries).

## Architecture Overview

Because Xbox Series X|S runs on an **AMD Zen 2 (x86_64)** CPU, an **AMD RDNA 2** GPU, and a Windows-derived Game OS (GDK / Era) using PE32+ binaries and Direct3D 12.x (`d3d12_x.dll`), `xsxemu` translates titles at the OS and graphics API boundary rather than emulating CPU instructions:

| Crate | Responsibility |
| --- | --- |
| `crates/xsx-abi` | `#[repr(C)]` Win64 ABI structs, `GUID`/`HRESULT`/`IUnknown`, public GDK types, and `d3d12_x` COM vtable layouts (`extern "win64"`). |
| `crates/xsx-loader` | PE32+ (`AMD64`) binary parser, OS executable virtual memory mapper (`VirtualAlloc` on Windows / `mmap` on Unix), `IMAGE_REL_BASED_DIR64` base relocator, IAT patcher, and native Win64 entry executor. |
| `crates/xsx-d3d12` | Xbox `d3d12_x` -> PC Direct3D 12 translation rules (UMA heap flag sanitization, `D3D12XBOX_CreateDevice`, `PresentX` -> swapchain translation), binary-compatible COM objects, Windows native `d3d12.dll` backend, and software D3D12 command executor/rasterizer. |
| `crates/xsx-hle` | High-Level Emulation (HLE) of `kernelx.dll` (with `api-ms-win-core-*` forwarding), `xgameruntime.dll`, and `d3d12_x.dll`, backed by a thread-safe `GuestRuntime`. |
| `crates/xsx-testgen` | Clean-room x86_64 PE32+ assembler & emitter that generates `hello_xsx.exe` (a runnable Xbox Series X vertical-slice test binary). |
| `crates/xsxemu` | CLI frontend (`gen-sample`, `inspect`, `run`) and end-to-end integration test suite. |

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the full design specification and multi-phase roadmap.

## Quickstart

### Build & Run Tests

```bash
cargo test --workspace
```

### Launch the GUI (`xsxemu gui`)

```bash
# On Windows: opens native Win32 desktop launcher (plus optional Web Dashboard button)
# On Linux / cross-platform: opens embedded zero-dependency Web GUI in browser
cargo run --release -p xsxemu -- gui

# Force embedded Web Dashboard on a custom port without auto-opening browser
cargo run --release -p xsxemu -- gui --web --port 8080 --no-browser
```

### Generate, Inspect, and Execute the Samples

```bash
# Vertical-slice single-binary sample (FSR Quality 1.5x enabled by default -> 960x540)
cargo run -p xsxemu -- gen-sample hello_xsx.exe
cargo run -p xsxemu -- inspect hello_xsx.exe
cargo run -p xsxemu -- run hello_xsx.exe --trace --force-relocate --dump-frame frame.bmp

# Phase 2 multi-module suite (secondary guest DLL, TLS callbacks, .pdata, Xbox T:\ VFS, threads, FSR)
cargo run -p xsxemu -- gen-phase2 ./phase2_out
cargo run -p xsxemu -- inspect ./phase2_out/xsx_helper.dll
cargo run -p xsxemu -- inspect ./phase2_out/phase2_xsx.exe
cargo run -p xsxemu -- run ./phase2_out/phase2_xsx.exe --trace --force-relocate --dump-frame fsr_frame.bmp

# Phase 3 + Phase 4 + Phase 5 full-stack suite (RTC, DXBC MD5 + ShaderCache, PSO/Dispatch, DirectStorage, XInput, XAudio2 WAV, XUser, XGameSave, multithreaded FSR)
cargo run --release -p xsxemu -- gen-fullstack ./fullstack_out
cargo run --release -p xsxemu -- run ./fullstack_out/full_stack_xsx.exe \
  --trace --force-relocate --rtc-offset 0 \
  --shader-cache ./shader_cache \
  --dump-frame fullstack_fsr.bmp --dump-audio fullstack_audio.wav

# Benchmark multithreaded AMD FSR (EASU + RCAS)
cargo run --release -p xsxemu -- bench-fsr --width 1280 --height 720 --fsr quality

# Real-game Xbox Series X ABI suite (d3d12_xs 84-slot proxy, xgameruntime QueryApiImpl, xmem, CoW GAME:\ overlay, --dry-run, JSONL trace)
cargo run -p xsxemu -- gen-realgame ./realgame_out
cargo run -p xsxemu -- run ./realgame_out/realgame_xsx.exe --dry-run
cargo run -p xsxemu -- run ./realgame_out/realgame_xsx.exe \
  --game-dir ./realgame_out --overlay ./realgame_overlay \
  --unimplemented fail --trace-jsonl ./startup.jsonl --trace

# Boot an unpacked Xbox Series X package directory via MicrosoftGame.Config (auto-resolves XboxGen9 exe + TitleId + memory budget)
cargo run -p xsxemu -- boot <package_dir> --overlay ./overlay --unimplemented fail

# Inspect real Xbox Series X shaders, disassemble RDNA 2 ISA, lift to SM 6.0 HLSL, or inspect unencrypted XVDs
cargo run -p xsxemu -- inspect-shader <shader.bin>
cargo run -p xsxemu -- disasm <shader.bin>
cargo run -p xsxemu -- lift-hlsl <shader.bin> --stage ps
cargo run -p xsxemu -- inspect-xvd <container.xvd>
```

### Real-Game Xbox Series X Compatibility Features

- **Package Boot, `MicrosoftGame.Config`, Unencrypted `.xvd`, PE Delay-Load & Ordinal Imports (`crates/xsx-loader`)**:
  - Parses UTF-8 / UTF-16 LE `MicrosoftGame.Config` manifests (`Identity`, `TitleId` → `XGameGetXboxTitleId`, `StoreId`, `XboxGen9`/`Scarlett` primary executable selection, `PersistentLocalStorage` `S:\` quota, `XboxAnacondaTitleMemory` mode).
  - Parses public `msft-xvd` (`0x200` magic) container headers and reads unencrypted Dev Mode / synthetic XVD images while cleanly rejecting encrypted retail XVDs.
  - Parses and eagerly patches **PE Delay-Load Import Directories (`IMAGE_DIRECTORY_ENTRY_DELAY_IMPORT`)** and resolves **ordinal imports** (e.g., `XAudio2_9.dll!#1`, `GameInput.dll!#1`, `XInput1_4.dll!#2/#3`).
- **Measured `d3d12_xs.dll` ABI Proxy, `xg_xs.dll`, RDNA 2 Disassembler/HLSL Lifter & Software DXR BVH (`crates/xsx-d3d12`, `crates/xsx-hle/src/xg_xs.rs`)**:
  - 84-slot `ID3D12Device` proxy covering `ID3D12Device`..`ID3D12Device14` with measured Xbox Series X deviations: **Slot 62** (`CreateComputePipelineState` with 2 extra words), **Slot 67** (`DeserializeComputePipelineStateX` for FSPX native RDNA 2 compute shaders), **Slot 76** (`CreateCommittedResource` rewriting Xbox `0x101`/`0x1` layout words), and **Heap Slots 9/10** returning 8-byte CPU/GPU descriptor handles directly in `RAX`.
  - **`xg_xs.dll` / `xg.dll`**: `XGComputeOptimalSwizzleMode` and `XGCreateTexture2DComputer` returning the 9-slot non-IUnknown `IXGTexture2DComputer` (`0x1B40`-byte `XG_RESOURCE_LAYOUT`, 64 KiB RDNA 2 `SW_64KB_S` / `R_X` swizzle & detile).
  - Root signature high-byte Xbox flag stripping (`0xFF00_0000`, observed `0x0300_0000`) on legacy, versioned (1.0/1.1/1.2), and serialized `DXBC`/`RTS0` blobs (with exact Microsoft `d3dcompiler` DXBC MD5-variant checksum recomputation), plus automatic v1.2 → v1.1 downgrade.
  - Zero-dependency RFC 1950/1951 zlib inflater + SHA-256 (`DXBC` → `DXIL` → `XDAT` → `0x0D` `XBCH` → `FSSM` / `FSPX`), **RDNA 2 (GFX10.3) ISA decoder/disassembler** (`rdna2_isa.rs`), **RDNA 2 → SM 6.0 HLSL recompiler** (`rdna2_to_hlsl.rs` for VS/PS/CS), AMD GFX10 `T#`/`V#` descriptors, Xbox **BCPack MDU rANS** hardware texture decoder, deterministic **software DXR BVH** (`bvh.rs` + `XboxBvhBuilder.dll`), and Xbox **Frame Pacer**.
- **Full GDK Runtime, `acphal.dll`, Xbox Middleware & Offline Networking (`crates/xsx-hle`)**:
  - `xgameruntime.lib` dynamic bootstrap (`LoadLibraryW("xgameruntime.dll")` pseudo-handles + `InitializeApiImplEx2` + `QueryApiImpl` serving **10 COM API tables** with later-GDK IID aliases: `IXThreadingImpl`, `IXErrorImpl`, `IXNetworkingImpl2`, `XGame`, `XSystem`, `XUser` 37-slot + `XUserGamertag`, `XGameSave` 33-slot, `XPackage` 39-slot, `XGameRuntimeFeature`, `XGameEvent`).
  - **`acphal.dll` / `xapu.dll`**: All 10 verified console audio HAL & APU/SHAPE heap exports (`ApuCreateHeap`, `ApuAlloc`/`Free`/`Map*`, `AcpHalAllocateShapeContexts`, `AcpHalCreate` + 8-slot `IAcpHal` COM channel with 128-sample/48 kHz frame ticks).
  - **Middleware & Networking Shims**: `amd_fidelityfx_upscaler_xbox.dll` / `amd_fidelityfx_loader_xbox.dll` (wired straight into `xsx-d3d12`'s multithreaded **AMD FSR EASU + RCAS** upscaler), `bink2w64_scarlett.dll`, `XboxBvhBuilder.dll`, `pixevt.dll`, offline deterministic `xcurl.dll` & `libHttpClient.GDK.dll` (with mock rule registries), `xmem.dll`, `KeyboardGamepadMapper`, non-destructive `GAME:\` Copy-on-Write VFS overlay (`--overlay`), `--dry-run`, `--unimplemented <log|fail|stop>`, and crash-safe `--trace-jsonl`.

### AMD FSR Upscaling (Enabled by Default)

`xsxemu` incorporates **AMD FidelityFX Super Resolution (FSR)** — **EASU** (12-tap edge-adaptive spatial upsampling) + **RCAS** (5-tap contrast-adaptive sharpening) — directly into the `PresentX` presentation pipeline **by default** (`--fsr quality`, 1.5x scale, sharpness `0.8`):

- `--fsr <ultra-quality|quality|balanced|performance|ultra-performance|native-aa|off>`
- `--fsr-sharpness <0.0..=1.0>` (default `0.8`)
- `--output-res <WxH>` (e.g. `1920x1080`, `3840x2160`)

### Selecting a GPU Backend

- `--backend auto` (default): Uses native Windows Direct3D 12 (`d3d12.dll`) on Windows x86_64, falling back to `SoftwareD3D12` on Linux or headless hosts.
- `--backend d3d12`: Requires Windows x86_64 with hardware/WARP Direct3D 12 support.
- `--backend software`: Uses the deterministic software D3D12 command executor and triangle rasterizer.

## License & Third-Party Notices

MIT — see [LICENSE](LICENSE). The BCPack MDU rANS entropy decoder in `crates/xsx-d3d12/src/bcpack/` is MIT OR Apache-2.0 licensed (see `crates/xsx-d3d12/src/bcpack/THIRD_PARTY_NOTICES.md`).
