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
```

### AMD FSR Upscaling (Enabled by Default)

`xsxemu` incorporates **AMD FidelityFX Super Resolution (FSR)** — **EASU** (12-tap edge-adaptive spatial upsampling) + **RCAS** (5-tap contrast-adaptive sharpening) — directly into the `PresentX` presentation pipeline **by default** (`--fsr quality`, 1.5x scale, sharpness `0.8`):

- `--fsr <ultra-quality|quality|balanced|performance|ultra-performance|native-aa|off>`
- `--fsr-sharpness <0.0..=1.0>` (default `0.8`)
- `--output-res <WxH>` (e.g. `1920x1080`, `3840x2160`)

### Selecting a GPU Backend

- `--backend auto` (default): Uses native Windows Direct3D 12 (`d3d12.dll`) on Windows x86_64, falling back to `SoftwareD3D12` on Linux or headless hosts.
- `--backend d3d12`: Requires Windows x86_64 with hardware/WARP Direct3D 12 support.
- `--backend software`: Uses the deterministic software D3D12 command executor and triangle rasterizer.

## License

MIT — see [LICENSE](LICENSE).
