# Spike: GPU acceleration

Status: research complete, 2026-10-09. No code merged. Any implementation needs its own ADR, and the owner's acceptance of it.

## Recommendation: revisit when a trigger is reached (no-go today)

None of the current hot paths is shaped for a GPU. The real limits are the single redb writer, during indexing, and hot-term walks on reads. Both are CPU and layout problems that the GPU cannot reach.

| Candidate | Share of time today | Amdahl bound | Verdict |
|---|---|---|---|
| Tokenizing / extractor scan | 0% of the critical path: parse threads about 10% busy, writer about 100% busy (measured: dotnet-runtime, 104.9M tokens, 116.3 s) | 1.00x | No-go |
| SHA-256 fingerprint | about 0.1% of parse CPU (ESTIMATE) | 1.00x | No-go |
| Posting encode/decode | read: 0.2% of query time (measured, read-cache spike). Write: at most 10% of writer time (ESTIMATE) | 1.002x read, at most 1.1x write | No-go |
| Compaction / vacuum | I/O and B-tree bound; not timed | about 1.0x (ESTIMATE) | No-go |
| Hot-term scan (#246) | p95 about 206 ms vs p50 0.07 ms (measured, read-cache spike) | at most 2-3x on hot queries, only if the data stays in GPU memory (ESTIMATE) | Revisit |
| Embedding / semantic search (future) | not built | 10-50x on the embedding stage (ESTIMATE) | Go, once that feature is scoped |

**Triggers for revisiting:**
1. Embedding or semantic search is added to scope.
2. Hot-term p95 is still above 100 ms at 1B tokens or more after the CPU fixes in #246, and a layout that keeps the token streams resident in GPU memory is being considered anyway (PCIe upload otherwise erases the gain).

No ADR 0011 is proposed now.

*Trigger 2 check, 2026-10-10 (epic story 61, #262):* after #246 and story 61's compact per-file sort key, the hot-term warm p95 on the A5 index (877M tokens) is **45.8 ms**: the median of 5 readbench runs, which ranged from 45.0 to 46.5 ms, down from 106.7 ms on main in the same series (see [read-cache.md](read-cache.md), story 61 section). That is well under the 100 ms threshold. Trigger 2 is not met, and the GPU stays no-go.

**Measured micro-benchmark:** a 1 GiB term count took 264-406 ms end to end on GPU (RTX 3080 Ti: 380-406 ms; RX 7900 XT: 264 ms), against 27 ms with 32-thread rayon on a Ryzen 9 7950X in the same run. Host-to-GPU upload dominates (191-368 ms). Note that this goes through wgpu's `write_buffer` staging path (about 3 GB/s), not raw PCIe bandwidth, so a pipelined upload would narrow the gap. The kernel time (32-69 ms, wall-clock around submit+poll, so an upper bound, with a naive kernel) did not beat rayon either. The outputs were byte-identical on Vulkan and DX12, on both NVIDIA and AMD.

**Benchmark limits:**
- 3 steady-state runs, spread under 2%. Whether a warm-up run was discarded was not recorded. No GPU timestamp queries were used.
- The workload is a synthetic byte scan. It bounds the transfer cost, not the store's real hot-term walk (varint decode + checkpoint seek), so the hot-term verdict rests on the architect's estimate, not on this benchmark.

**Technology:**
- `wgpu` 30 passes `check-no-c-deps.py` on all 8 targets the script checks (a superset of the six shipped targets) with `default-features = false, features = ["vulkan","dx12","metal","wgsl","std"]`. With default features it fails, because the GLES backend pulls in khronos-egl and wayland-sys.
- `cubecl` fails the gate in every feature combination tried, including `ring`, which is on the deny-list.
- `wgpu` adds about 5.9 MiB to the release binary and about 70 s to a clean `-j 2` build.

**Docker:**
- A static musl binary can never load a GPU driver (no runtime dlopen; reasoned, not run on Linux), so the `scratch` image stays GPU-free.
- A future `memory-graph:gpu` image would be a glibc build on `debian:bookworm-slim` with `libvulkan1`, which adds about 80 MB (ESTIMATE).
- On Linux hosts it would run with `--gpus all` and `NVIDIA_DRIVER_CAPABILITIES` including `graphics`, or with `/dev/dri` for AMD/Intel. This Linux path is **unverified**: no Linux GPU host was available.
- Measured on this Windows machine: Docker Desktop (WSL2) exposes CUDA only. `wgpu` inside a container saw only llvmpipe (a software CPU adapter), with or without `--gpus all` (probe used wgpu 24.0.3 with default features, not the wgpu 30 Vulkan/DX12/Metal subset; the WSL2 finding is about missing ICDs, so it should not depend on the version). macOS has no GPU passthrough.

**Rules any future GPU path must follow:**
- A `gpu` Cargo feature, off by default, plus `--gpu auto|off` / `MEMORY_GRAPH_GPU`.
- The CPU implementation is the reference. The GPU only runs integer or order-independent pure functions, verified by a `run_differential` case.
- Nothing about the GPU goes into stored bytes, so Raft clusters can mix GPU and non-GPU nodes.
- OTel and `sysinfo` report the adapter and any fallback.

The sections below are the working notes from the three research roles. They contain the commands, the hardware and the citations.


## Appendix: architect notes

## GPU acceleration spike: where CPU time goes (architect)

Date 2026-10-09. Read-only. [M] = measured, [E] = ESTIMATE.

### New measurement (this spike)

Release build of this worktree (`cargo build --release -j 2`), Ryzen 9 7950X (32 threads), NVMe, Windows 11. `memory-graph index --stats --json`, fresh db, 31 parse threads:

| repo | files | tokens | source | wall | parse busy (sum of 31 thr) | parse util | writer busy | verdict |
|---|---|---|---|---|---|---|---|---|
| tokio | 883 | 1.0 M | 6.3 MB | 0.886 s | 2.975 s | ~11% | 0.814 s (92%) | writer-bound |
| dotnet-runtime | 58,219 | 104.9 M | 686 MB | 116.3 s | 351.3 s | ~9.7% | 116.2 s (~100%) | writer-bound |

Parse CPU cost: 351 s / 104.9 M tokens = 3.35 us/token-CPU (walk + decode + SHA-256 + tokenize + extract + posting encode). The single redb writer (`V2Store` commit, `crates/graph-store/src/v2.rs` ~4200-4290: intern, `post`/`sym_idx`/`sym_fold` inserts, `codec::encode` of the stream) is the whole critical path. Scratch deleted.

Consistent with A5 (`docs/spikes/read-cache.md:267`): 877.3 M tokens in 2,462 s = 356 k tok/s, i.e. writer-bound in kind like dotnet-runtime, though at a lower rate (356 k vs 902 k tok/s; A5 includes more small files and 31 repos).

### Candidates

#### 1. Tokenizing / extractor scanning
- Code: `graph-core/src/tokenizer.rs:tokenize`, `graph_core::scan`, `graph-lang-*` extractors (Rust: `syn` on an internal thread), run on parse workers from `graph-cli/src/lib.rs:index_dir` (line 1226; the parse-worker spawn loop is around line 817).
- Share: parse threads ~10% utilised; **0% of wall on the critical path** [M, table above]. data-model.md:139 already noted ingest "not extractor-bound (>=1 M tokens/s including tokenizing)".
- Data-parallel: across files ~100%; within a file, sequential state machines (string/comment/dialect modes), `syn` is a recursive parser.
- PCIe: ~1 B in per source byte, ~20-30 B out per token (span records) - output > input. Compute ~3 us/token is branch-bound, not FLOP-bound.
- Irregularity: extreme (per-language branching, nesting, variable-length tokens, recursion).
- Amdahl: 1.00x end-to-end while writer-bound (infinite speedup of a 0% critical-path share). Even if the writer were free, 31 CPU threads already give roughly 300 k-1 M tok/s of parse capacity is available [E], ample headroom over the writer.
- Verdict: **NO-GO.**

#### 2. SHA-256 fingerprinting
- Code: `graph-store/src/common.rs:120 fingerprint` (`Sha256::digest`, sha2 crate, uses SHA-NI on Zen4/modern Intel).
- Share: [E] 686 MB at ~1.5-2 GB/s per core = ~0.4 s CPU of 351 s parse CPU (~0.1%), on parse threads, off the critical path -> **~0% of wall**.
- Data-parallel: across files only; SHA-256 is serial within a message.
- PCIe: 1 byte moved per byte hashed; PCIe (~25 GB/s) is barely faster than SHA-NI across 31 cores (~50 GB/s aggregate) - transfer alone loses.
- Amdahl: 1.00x.
- Verdict: **NO-GO** (unchanged-file skip already makes rerun cost = hash + read).

#### 3. Posting encode/decode, compression (varint/delta codec)
- Code: `graph-store/src/codec.rs` (`encode_posting`, `encode`, stream decode with checkpoints every 64 records, dict blocks). Postings are encoded on parse threads (`v2.rs:4759`); the stream `codec::encode` runs on the writer (`v2.rs:4275`) because it needs global term ids.
- Share, write: [E] stream encode <=5-10% of writer time; the writer is dominated by redb B-tree inserts (intern lookups, one `post` row per (term,file), symbol index rows) and page writes. Read: timed decode = **0.2% of query time** on 877 M tokens [M, read-cache.md:284-296]; 23-38% only on the small corpus pre-fix (read-cache.md:118-123), fixed to below gate by phase 1.
- Data-parallel: varint delta streams are a serial dependency chain; parallel only per checkpoint block (64 records) and per file.
- PCIe: decode output ~5-10x the encoded bytes; 1 MiB/query encoded (read-cache.md:291) = ~40 us transfer vs CPU decode of the same in well under 1 ms.
- Irregularity: variable-length varints, data-dependent branches.
- Amdahl: write <=1.05-1.1x [E]; read 1.002x [M-derived].
- Verdict: **NO-GO.**

#### 4. Compaction and vacuum
- Code: `v2.rs:3168 vacuum` (dead-term scan/removal over dictionary + postings), `v2.rs:3457 compact` (copy to a new redb file), server `log_store.rs:445 compact_if_sparse`.
- Share: not part of index/query wall; offline maintenance. No timing in docs; v2-checkpoint.md:111-133 measures size only. [E] I/O + B-tree rebuild bound (redb page copy, fsync).
- Data-parallel: low; redb is a single-writer B-tree, ordered inserts.
- PCIe: would round-trip the whole db (22.5 GB at A5) for little compute.
- Amdahl: ~1.0x (no arithmetic to offload).
- Verdict: **NO-GO.**

#### 5. Hot-term scan behind large `search`
- Code: `V2Store` term search (`Lazy::tokens` stream walks, posting reads; v2-checkpoint.md:84, ~12 ns/token walk), tracked as **#246**.
- Share: this is where read time goes on big indexes - p50 0.069 ms vs p95 ~206 ms, store 30 ms/q mean, dominated by six hot terms (`const`, `this`, `return`...) at token/file grain [M, read-cache.md:283-295]. At 9.9 M: `(` token 935 ms, symbol 587 ms (v2-checkpoint.md:35-36).
- Data-parallel: high across candidate files and across 64-record checkpoint blocks; a filter/count over records is SIMD/GPU-shaped.
- PCIe: the walk reads hot-term data scattered across redb pages; unless the token streams (~20 GB at 877 M) are resident in VRAM, every query ships MBs-to-GBs over PCIe, which costs about what the CPU walk does. Results (rows) must come back and be materialised on CPU anyway (847 k rows for `(`).
- Irregularity: medium (varint decode + checkpoint seek; redb page lookups are pointer-chasing).
- Amdahl: hot-term queries are ~all of mean read time, so an ideal kernel could approach the walk share [E ~50-70% of those queries; rest is redb page access + row building] -> **<=2-3x** on those queries, ~1x on p50. Cheaper CPU routes first: multi-threading the per-file walk (31 idle cores), roll-up from counts without walking, `--limit` push-down already exists, caching hot-term roll-ups.
- Verdict: **REVISIT** - trigger: after #246's CPU fixes (parallel per-file walk, count-only roll-ups), hot-term p95 still > 100 ms at >= 1 B tokens AND a resident-columnar layout (streams decoded in VRAM) is being considered anyway.

#### 6. Future embedding / semantic search
- Code: none today (no vector store in ADRs).
- Share: [E] would be new work, not a share of existing time. Embedding 877 M tokens as ~3.4 M chunks of 256 tokens with a small encoder (~20-30 M params): CPU ~ hours (order 10^2-10^3 chunks/s across cores), GPU ~ 5-20 k chunks/s -> tens of minutes. Query: brute-force kNN over 3.4 M x 384 f32 = 5.2 GB, ~90 ms CPU memory-bandwidth bound vs ~5 ms resident on GPU.
- Data-parallel: ~100% (dense GEMM, dot products).
- PCIe: favourable - chunk text in (~1 KB), 1.5 KB vector out per chunk for MFLOPs-GFLOPs of compute; index resident in VRAM for queries.
- Irregularity: none.
- Amdahl: on the embedding stage itself 10-50x [E]; on an index run that adds embeddings, embedding would dominate the CPU baseline, so GPU is close to its full gain.
- Constraints: pure-Rust gate (`scripts/check-no-c-deps.py`) rules out CUDA toolkits/`-sys` C builds; viable via dynamically loaded drivers (e.g. wgpu, cudarc-style dlopen) or an optional out-of-process embedder; must stay optional with an exact CPU fallback (configuration-equivalence invariant: embeddings must not change results by device unless explicitly approximate).
- Verdict: **GO (conditional)** - if/when semantic search is scoped, design it GPU-optional from day one; this is the only candidate with GPU-shaped compute.

### Ranked table

| rank | candidate | measured/est. share of wall | data-parallel | PCIe vs compute | irregularity | Amdahl bound (end-to-end) | verdict |
|---|---|---|---|---|---|---|---|
| 1 | Embedding / semantic search (future) | new stage; would dominate its own run [E] | ~100% | good (KB in, GFLOPs) | low | 10-50x on that stage [E] | GO (conditional on scoping) |
| 2 | Hot-term scan (#246) | ~all of p95 read time; p95 206 ms vs p50 0.07 ms [M] | high across files/blocks | poor unless VRAM-resident (~20 GB streams) | medium | <=2-3x on hot queries, ~1x p50 [E] | REVISIT (after CPU #246 fixes, >=1 B tokens) |
| 3 | Posting/stream encode-decode | read 0.2% [M]; write <=10% of writer [E] | low (serial varints) | poor | high | ~1.002x read, <=1.1x write | NO-GO |
| 4 | Tokenize / extract | 0% of critical path (parse ~10% utilised, writer-bound) [M] | across files only | output > input | very high | 1.00x | NO-GO |
| 5 | SHA-256 fingerprint | ~0.1% of parse CPU, 0% of wall [E] | across files only | 1 B/B, loses to SHA-NI | n/a | 1.00x | NO-GO |
| 6 | Compaction / vacuum | offline, I/O + B-tree bound [E] | low | whole db round trip | pointer-chasing | ~1.0x | NO-GO |

### Bottom line

Ingest is single-writer (redb) bound: 92-100% writer busy, parse threads ~10% busy. No GPU kernel touches that critical path. Reads are fast except hot-term walks, which are a CPU-parallelism and layout problem first. The only GPU-shaped workload is a future embedding/semantic search.

## Appendix: developer notes

## GPU spike: developer notes (2026-10-09)

Env: rustc 1.99.0 (b940084d7 2026-09-28), cargo 1.99.0; Windows 11 Pro 26200; AMD Ryzen 9 7950X (16C/32T), 63 GB RAM;
GPUs: NVIDIA RTX 3080 Ti (driver 610.62 / 32.0.16.1062), AMD RX 7900 XT (26.8.1), AMD iGPU.
Throwaway projects in D:/tmp/gpu-spike-dev (deleted afterwards). Crates: wgpu 30.0.1, cubecl 0.11.0.

### 1. Pure-Rust gate

The script supports `--manifest-path`, so it was run unchanged:
`python scripts/check-no-c-deps.py --manifest-path D:/tmp/gpu-spike-dev/<proj>/Cargo.toml`.
Note: the script actually checks 8 targets (the six in the brief plus aarch64-unknown-linux-musl and aarch64-pc-windows-msvc).

| Config | Result | Failing crates (reason, targets) |
|---|---|---|
| `wgpu = "30.0.1"` (default features) | FAIL | khronos-egl 6.0.0: build.rs uses pkg-config (all targets); wayland-sys 0.31.11: build.rs uses pkg-config (linux gnu/musl x86_64+aarch64, both macOS) |
| `wgpu` `default-features=false, features=["vulkan","dx12","metal","wgsl","std"]` | **PASS** (82 deps) | none. renderdoc-sys is noted as a pure-Rust -sys (no links key, no C build) |
| `cubecl = { features=["wgpu"] }` (defaults on) | FAIL (440 deps) | aegis (cc), clang-sys (links clang), cubecl-llvm (cc), io-uring (bindgen, Linux), khronos-egl (pkg-config), liblzma-sys (links lzma, cc), llvm-sys (links llvm-23, cc), prettyplease 0.2/0.3 (links key), **ring** (deny-list, links, cc), simsimd (cc), turso_sdk_kit (bindgen), wayland-sys (pkg-config) |
| `cubecl` `default-features=false, features=["wgpu"]` | FAIL (215 deps) | khronos-egl, wayland-sys (cubecl-wgpu enables wgpu's default features, i.e. GLES), prettyplease 0.3.0 (`links = "prettyplease03"`) |

The suspects from the brief:
- ash, libloading, metal/objc2, windows/windows-sys and renderdoc-sys all pass. None has a `links` key or a C build script; they load the drivers at runtime.
- The only real offenders in wgpu are in the GLES/EGL backend (khronos-egl, wayland-sys). Leaving out the `gles` feature fixes it.
- prettyplease and rayon-core are false positives. They use `links` as a uniqueness marker and compile no C, but the gate as written fails them. rayon is not in the repo's Cargo.lock today, so adding rayon would need a `c-deps-exceptions.txt` entry.
- cubecl would need an upstream feature to turn off GLES in cubecl-wgpu, plus a prettyplease exception. It is not usable as-is.

### 2. Feasibility

- **Static musl:** the build compiles. `cargo check --target x86_64-unknown-linux-musl` on the wgpu subset passes, and libloading comes in via ash → gpu-allocator/wgpu-hal. At runtime, though, ash's `loaded` mode dlopens libvulkan.so.1. In a statically linked musl binary, musl's dlopen is a stub that always fails. So the static musl/scratch image would never see a GPU, and enumerate_adapters returns no Vulkan adapter. The GPU path must be optional and must fall back to the CPU cleanly. Getting GPU on Linux would need a glibc (dynamic) build plus the Vulkan loader and ICD inside the container (`--gpus`, nvidia-container-toolkit). That rules out `FROM scratch`. This is reasoned from musl/ash behaviour and was not run on Linux here.
- **Binary size (Windows MSVC release, default profile):** hello-world 130,560 B, against 6,226,432 B for the wgpu subset plus the benchmark. The benchmark includes rayon and pollster, which add only a small amount. Delta: **about +5.9 MiB**. wgsl/naga is the bulk of it, and dx12+vulkan add to it.
- **Clean compile time (-j 2, release, fresh target dir):** hello-world 0.4 s, against **70.2 s** for the wgpu subset (82 deps). cubecl was not timed; with 215 to 440 deps it would be several times more.
- **Determinism of integer compute:** WGSL defines integer semantics fully. u32/i32 arithmetic wraps, shift amounts are taken modulo the bit width, and integer divide or modulo by zero is defined. There is no fast-math or float in the path, so per-element results are deterministic across backends. The exceptions are anything order-dependent: atomic-append slot order, races, and subgroup-op ordering. Use atomics only for commutative reductions, or write to fixed positions. Measured: identical output bytes on Vulkan and DX12, NVIDIA and AMD, across 3 runs each, all equal to the CPU.

### 3. Micro-benchmark: 4-byte term count over 1 GiB

The kernel counts the occurrences of `abca` at every byte offset. It emits one u32 count per 256-byte block (4M u32 = 16 MiB of output). Data is 1 GiB of LCG text over `abcdefgh \n`, with 107,785 matches. The GPU works in 4 chunks of 256 MiB. Upload is `write_buffer`+wait, kernel is dispatch+wait, and readback is copy+map. The CPU baselines run in the same process; the comparison is `cast_slice` byte equality against the CPU single-thread output.

Commands: `cargo build --release -j 2` (CARGO_TARGET_DIR=D:/tmp/gpu-spike-dev/target), then `GPU=NVIDIA BK=vulkan|dx12 wg2.exe` (and with no GPU filter for the 7900 XT).

Steady-state run (3 runs each, varied <2%):

| Device / backend | upload | kernel | readback | GPU total | CPU 1 thread | CPU rayon (32) | identical |
|---|---|---|---|---|---|---|---|
| RTX 3080 Ti / Vulkan | 368 ms | 32.3 ms | 4.7 ms | 406 ms | 172 ms | 26.9 ms | yes |
| RTX 3080 Ti / DX12 | 338 ms | 36.5 ms | 5.0 ms | 380 ms | 172 ms | 27.7 ms | yes |
| RX 7900 XT / Vulkan | 191 ms | 68.5 ms | 4.3 ms | 264 ms | 181 ms | 29.5 ms | yes |
| RX 7900 XT / DX12 | 264 ms | 70.0 ms | 5.0 ms | 339 ms | 176 ms | 27.5 ms | yes |

Reading:
- Upload over PCIe, about 3 GB/s through wgpu's staging copy, dominates. Even the kernel alone (32 ms) is no faster than 32-thread rayon (27 ms).
- End to end, the GPU is about 14x slower than rayon and about 2x slower than one CPU thread.
- A GPU only pays off if the data is already resident in VRAM and queried repeatedly, or if the kernel is far more compute-heavy than a byte scan.
- The kernel is naive (byte extraction per u32 load, no shared memory). An optimised one might run 5-10x faster, but it is still bounded by the upload.

## Appendix: ops notes

## OPS: GPU in containers for memory-graph (spike, read-only)

Repo state read: `Dockerfile` (rust:1.98-bookworm build, rust-lld, static musl, `FROM scratch`, uid 65532, /data, HEALTHCHECK via `health --server`), `.github/workflows/docker.yml` (amd64 smoke build + multi-arch amd64/arm64 publish to ghcr), `.github/workflows/compose.yml`. Nothing was changed in the repo.

### 1. Proof on this machine (RTX 3080 Ti, Docker Desktop 29.6.1 / WSL2, `nvidia` runtime registered)

Throwaway image: wgpu 24.0.3 (default features) adapter enumerator, built in rust:1.98-bookworm (-j 2), runtime debian:bookworm-slim + libvulkan1 + mesa-vulkan-drivers (345 MB). Second variant on ubuntu:24.04 + Mesa 25.2.8 (369 MB) to look for Dozen (dzn). Images and D:/tmp/gpu-spike-ops removed afterwards.

| Run | Adapters wgpu saw |
|---|---|
| `docker run` (no GPU) | 1: `Vulkan / Cpu / llvmpipe (Mesa 22.3.6)` |
| `--gpus all` | same: llvmpipe only |
| `--gpus all -e NVIDIA_DRIVER_CAPABILITIES=all` | same: llvmpipe only |
| ubuntu 24.04 Mesa 25.2.8, `--gpus all`, caps=all | same: llvmpipe only (Ubuntu Mesa ships no `dzn_icd.json`) |

What the toolkit injected under WSL2: `/dev/dxg`, `nvidia-smi` (works: "GPU 0: NVIDIA GeForce RTX 3080 Ti"), `libcuda.so*`, `libdxcore.so`, `libnvidia-ml`, `-encode`, `-opticalflow`, `-ngx`, `-gpucomp`, `-ptxjitcompiler` from `/usr/lib/wsl/drivers/...`. **No** `nvidia_icd.json`, no `libGLX_nvidia`/`libnvidia-glcore`, no `/dev/dri`, no `/usr/lib/wsl/lib` (so no `libd3d12.so` for dzn). Noise: GLES backend logged `XDG_RUNTIME_DIR` / `swrast_dri.so` warnings (EGL probe), harmless but should be silenced (Vulkan-only backend mask).

**Conclusion: on Docker Desktop for Windows, a container gets CUDA/NVML only; wgpu sees no hardware adapter, even with caps=all.** The GPU path is a Linux-host feature. Fallback must be routine, not exceptional, and llvmpipe must not be counted as "a GPU".

### 2. How containers reach a GPU

- **NVIDIA Container Toolkit (Linux hosts)**: `docker run --gpus all` (or `--runtime nvidia`) triggers the prestart hook/CDI spec, which bind-mounts the host's user-space driver libs and device nodes (`/dev/nvidia*`). Which libs are mounted is controlled by `NVIDIA_DRIVER_CAPABILITIES` (default `compute,utility`). Vulkan needs `graphics` (or `all`): it adds `libGLX_nvidia.so.0`, `libnvidia-glcore`, `libnvidia-glvkspirv` and the ICD JSON `/etc/vulkan/icd.d/nvidia_icd.json` (plus `/usr/share/glvnd/egl_vendor.d/10_nvidia.json`). The image only needs the Khronos loader (`libvulkan1`). Sources: https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/docker-specialized.html (driver capabilities), https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/cdi-support.html. Caveat: `--gpus` injects `utility,compute` unless the env/caps say otherwise, hence set `NVIDIA_DRIVER_CAPABILITIES=compute,utility,graphics` in the image ENV.
- **Compose**: `deploy.resources.reservations.devices: [{driver: nvidia, count: all, capabilities: [gpu, graphics]}]` (https://docs.docker.com/compose/how-tos/gpu-support/). Capabilities in compose map to toolkit capabilities; still set the env var.
- **AMD / Intel (Linux)**: no toolkit; pass `--device /dev/dri` (render node `renderD128`) and `--group-add $(stat -c %g /dev/dri/renderD128)` (the `render`/`video` gid) since the image runs as 65532. The user-space driver (Mesa RADV / ANV) must be **in the image** (`mesa-vulkan-drivers`), unlike NVIDIA where it comes from the host. AMD ROCm's `/dev/kfd` is not needed for Vulkan.
- **Docker Desktop on Windows (WSL2 GPU-PV)**: `/dev/dxg` + `/usr/lib/wsl/lib` (libd3d12, libdxcore, libcuda). Officially CUDA/DirectML only (https://docs.docker.com/desktop/features/gpu/, https://learn.microsoft.com/windows/wsl/tutorials/gpu-compute). Vulkan would need Mesa's Dozen (dzn, Vulkan-on-D3D12) + `libd3d12.so`; Debian/Ubuntu Mesa packages don't ship dzn and Docker Desktop doesn't mount `/usr/lib/wsl/lib` into containers (measured above). Treat as unsupported; native Windows binary (wgpu DX12/Vulkan) is the Windows GPU path.
- **macOS (Docker Desktop / colima / Podman)**: Linux VM via Hypervisor.framework with no GPU passthrough; Metal is not reachable from a container (Podman's krunkit venus/virtio-gpu is experimental). Native macOS binary (wgpu Metal) is the path.

### 3. wgpu backend, musl vs glibc

- In a Linux container wgpu uses **Vulkan** (GLES/EGL is a fallback that needs a display/EGL device; disable it: `Backends::VULKAN` for compute-only). wgpu-hal dlopens `libvulkan.so.1` at runtime (ash `loaded`), so the binary has no link-time dependency, which keeps `check-no-c-deps.py` happy (ash/wgpu are pure Rust; verify naga/wgpu-hal deps like `libloading` - pure Rust - and that no `khronos-egl` static feature is enabled).
- **Static musl cannot dlopen**: in a `+crt-static` musl binary `dlopen` is a stub that fails ("Dynamic loading not supported"), so wgpu would always find zero Vulkan adapters. And even a dynamic musl binary can't load NVIDIA's glibc-built driver libs. **The GPU image needs a glibc build (`x86_64-unknown-linux-gnu`, dynamically linked) on a glibc base.** The scratch/musl image stays exactly as today and simply reports "no adapter".
- arm64: NVIDIA toolkit supports arm64 (Grace/Jetson), but start amd64-only for `:gpu`.

### 4. Proposed optional image `memory-graph:gpu`

Base: `debian:bookworm-slim` (~75 MB unpacked) + `libvulkan1` (~0.4 MB) = ~80 MB + binary. Do **not** install `mesa-vulkan-drivers` by default (adds ~90 MB with LLVM; llvmpipe would be "found" and must be ignored anyway); offer AMD/Intel via a `gpu-mesa` build arg/target. Alternatives: `nvidia/cuda:*-base` (~240 MB, pointless: we need Vulkan not CUDA), `gcr.io/distroless/cc-debian12` (~25 MB, glibc but no apt, so copying libvulkan in by hand is fiddly; viable later).

```dockerfile
## Dockerfile, new stages (scratch `runtime` stays the default last target... see note)
FROM --platform=$BUILDPLATFORM rust:1.98-bookworm AS build-gpu
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY examples ./examples
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=shared \
    --mount=type=cache,target=/src/target,id=memory-graph-target-gpu \
    cargo build --release --locked -p graph-cli --features gpu \
 && cp target/release/memory-graph /memory-graph && mkdir -p /data

FROM debian:bookworm-slim AS runtime-gpu
RUN apt-get update && apt-get install -y --no-install-recommends libvulkan1 \
 && rm -rf /var/lib/apt/lists/*
ENV NVIDIA_VISIBLE_DEVICES=all \
    NVIDIA_DRIVER_CAPABILITIES=compute,utility,graphics \
    MEMORY_GRAPH_GPU=auto
COPY --from=build-gpu /memory-graph /usr/local/bin/memory-graph
COPY --from=build-gpu --chown=65532:65532 /data /data
USER 65532:65532
WORKDIR /data
VOLUME ["/data"]
EXPOSE 7000
HEALTHCHECK ... (same as runtime, path /usr/local/bin/memory-graph)
ENTRYPOINT ["/usr/local/bin/memory-graph"]
CMD ["--help"]

# keep scratch as the final stage so `docker build .` is unchanged
FROM runtime AS default
```
Note: put `runtime-gpu` *before* the scratch `runtime` stage, or re-alias at the end, so a plain `docker build .` still produces the scratch image; the GPU image is `docker build --target runtime-gpu -t memory-graph:gpu .`. Image would have a shell (Debian); the smoke test "no shell" assertion applies only to the default image.

**docker.yml impact**: add a matrix `variant: [default, gpu]` (or a second job): gpu = `target: runtime-gpu`, `platforms: linux/amd64` only, tags suffixed `-gpu` (`main-gpu`, `sha-xxx-gpu`, `1.2.3-gpu`, `gpu` for latest) via `docker/metadata-action` `flavor: suffix=-gpu`; separate gha cache scope (`scope=gpu`). Smoke test on hosted runners (no GPU): `sysinfo --json` must show `gpu.adapter: none`, `fallback_reason`, and index/search results byte-identical to the default image (diff the JSON). Real-GPU job only on a self-hosted labelled runner, optional/manual. Roughly +1 build (~same compile time, gnu target, no cross) per run.

**compose snippet**:
```yaml
services:
  memory-graph:
    image: ghcr.io/p47phoenix/memory-graph:gpu
    command: serve --data-dir /data --bootstrap --node-id 1 --listen 0.0.0.0:7000 --gpu auto
    volumes: [mg-data:/data]
    environment:
      NVIDIA_DRIVER_CAPABILITIES: compute,utility,graphics
    deploy:
      resources:
        reservations:
          devices:
            - driver: nvidia
              count: 1
              capabilities: [gpu, graphics]
  # AMD/Intel variant (image built with mesa drivers):
  #   devices: ["/dev/dri:/dev/dri"]
  #   group_add: ["${RENDER_GID:-109}"]
volumes: { mg-data: {} }
```

Default scratch image: unchanged (musl, no `gpu` feature, no libs).

### 5. Fallback when there is no adapter

`--gpu auto` (default in the gpu build): request adapter with `Backends::VULKAN` (+ DX12/Metal natively), `force_fallback_adapter: false`; reject `DeviceType::Cpu` (llvmpipe/lavapipe/WARP) unless `MEMORY_GRAPH_GPU=force` for tests. No adapter, device creation error, device lost, OOM or a timeout -> log once at INFO with the reason, run the CPU path. Never fail a command because the GPU is missing; `--gpu on`/`require` (optional) is the only mode that errors (exit code to be chosen in the ADR). Per-batch: a GPU error mid-run retries that batch on CPU, never partial output. Init under a short deadline (adapter enumeration took <1 s here but driver init can hang).

### 6. Raft with mixed GPU / non-GPU nodes

Bytes written to the store must be identical regardless of node hardware: the GPU only accelerates **pure functions whose CPU implementation is the oracle** (e.g. a hot-term scan or embedding similarity computed in integers or fixed point). The leader applies `LogCommand`s that already carry deterministic inputs; each node's state machine must produce identical redb contents, and snapshots are installed cross-node. Rules: no float reductions with order-dependent results (or use integer/fixed-point only); GPU output is verified-equivalent by a `run_differential` case (gpu vs cpu store, identical answers) plus a property test; no GPU info in fingerprints, `derived_version`, schema version or any stored byte; the fingerprint must not include "gpu used". A mixed cluster then needs no coordination; `cluster status` can show per-node accelerator for operators only. Reads served by any node are identical, which also keeps the configuration-equivalence invariant (GPU is just another "jobs/cache" knob).

### 7. OTel observability

Resource/startup attributes (once): `mg.gpu.mode` (auto/off), `mg.gpu.backend` (vulkan/dx12/metal/none), `mg.gpu.adapter.name`, `mg.gpu.adapter.vendor`/`device_type`, `mg.gpu.driver` + `driver_info`, `mg.gpu.fallback_reason` (no_adapter, cpu_adapter_rejected, disabled, init_timeout, feature_not_built). Metrics: `mg.gpu.offload.batches` / `mg.gpu.offload.bytes` (counter, by stage), `mg.gpu.fallbacks` (counter, attr `reason`, `stage`), `mg.gpu.kernel.duration` (histogram), `mg.gpu.device_lost` (counter), gauge `mg.gpu.available` (0/1). Same fields in `sysinfo --json` (`gpu` object) so the CI smoke test and `check-probes.py` can assert on them, and in the per-stage progress output.

### 8. Config surface

- Cargo feature `gpu` on `graph-cli` (forwarding to a new `graph-gpu`/store feature), **off by default**, not in the default `lang-*` set; the default/musl/scratch build and the pure-Rust gate are unaffected (still run `check-no-c-deps.py --features gpu` in CI to keep wgpu's tree honest; wgpu pulls no C build scripts with Vulkan-only features, but `gles` pulls `khronos-egl` - keep it off).
- `--gpu auto|off` (global flag; `serve` too). Precedence: flag > `MEMORY_GRAPH_GPU` env (`auto|off`, optionally `force` for tests) > default `auto` when built with the feature, `off` (and a clear "not built with gpu" in sysinfo) otherwise. `--gpu auto` on a non-gpu build: warn and continue, don't error, so one compose file works with both images.

### Risks / open questions
- On Windows Docker Desktop and macOS the gpu image gives zero benefit; document that clearly.
- glibc image adds a shell and Debian CVE surface; scan it (Trivy) and pin the digest.
- NVIDIA Vulkan in headless containers sometimes needs `NVIDIA_DRIVER_CAPABILITIES` with `display` on older drivers; not verifiable here (no Linux GPU host). Validate on a Linux GPU runner before shipping.

## Cleanup

All scratch directories (`D:/tmp/gpu-spike-arch`, `-dev`, `-ops`), the throwaway Docker images (`gpu-spike-*`), the temporary musl rustup target and the agents' worktrees were removed. No throwaway branches were pushed. The only branch is this PR's `spike/gpu-acceleration`.

The Dockerfile in the ops notes is a sketch (pseudocode), not a tested file.

Follow-up: `rayon-core` and `prettyplease` trip the gate's `links` check without compiling C. That only matters if a GPU feature or rayon is adopted.
