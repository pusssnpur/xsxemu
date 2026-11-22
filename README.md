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

### Generate, Inspect, and Execute the Vertical-Slice Sample

```bash
# 1. Generate a synthetic Xbox Series X PE32+ test executable
cargo run -p xsxemu -- gen-sample hello_xsx.exe

# 2. Inspect PE32+ headers, sections, Xbox DLL imports, and base relocations
cargo run -p xsxemu -- inspect hello_xsx.exe

# 3. Execute natively with call tracing and dump the PresentX framebuffer
cargo run -p xsxemu -- run hello_xsx.exe --trace --force-relocate --dump-frame frame.bmp
```

### Selecting a GPU Backend

- `--backend auto` (default): Uses native Windows Direct3D 12 (`d3d12.dll`) on Windows x86_64, falling back to `SoftwareD3D12` on Linux or headless hosts.
- `--backend d3d12`: Requires Windows x86_64 with hardware/WARP Direct3D 12 support.
- `--backend software`: Uses the deterministic software D3D12 command executor and triangle rasterizer.

## License

MIT — see [LICENSE](LICENSE).
