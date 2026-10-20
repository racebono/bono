# Official whisper.cpp versioned Windows libraries

## Where can Bono obtain official versioned libraries?

### Takeaway
Official release binaries exist, but the current Windows packages are runtime DLL/executable bundles rather than complete development SDKs. v1.9.4 links to b5130 for binaries; both identify commit 927cfce.

### Cited Findings
- The v1.9.4 release explicitly points to nightly b5130, which identifies the same commit 927cfce. [v1.9.4](https://github.com/ggml-org/whisper.cpp/releases/tag/v1.9.4), [b5130](https://github.com/ggml-org/whisper.cpp/releases/tag/b5130).
- GitHub API queried during research: v1.9.4 has no uploaded assets; b5130 has `whisper-bin-x64.zip` (8,573,270 bytes), `whisper-blas-bin-x64.zip` (21,360,234), `whisper-cublas-11.8.0-bin-x64.zip` (272,982,859), and `whisper-cublas-12.4.0-bin-x64.zip` (674,539,285). No Windows Vulkan asset. [API](https://api.github.com/repos/ggml-org/whisper.cpp/releases/tags/b5130).
- Direct pinned download example: [CUDA 12.4](https://github.com/ggml-org/whisper.cpp/releases/download/b5130/whisper-cublas-12.4.0-bin-x64.zip), [CPU](https://github.com/ggml-org/whisper.cpp/releases/download/b5130/whisper-bin-x64.zip).
- Downloaded and inspected the CPU ZIP. It contains whisper.dll, ggml.dll, ggml-base.dll and CPU variant DLLs, plus executables; no `.lib`, headers, or CMake package. Local archive retained as `official-cpu-b5130.zip`. Workflow packs only build/bin/Release for CPU, BLAS and CUDA packages, not the installation SDK. [Pinned workflow](https://github.com/ggml-org/whisper.cpp/blob/v1.9.4/.github/workflows/release.yml#L204-L213).

### Inferences
- Rust integration needs build-script support for external DLL linking, matching headers/bindings, and MSVC import libraries generated from exports or separately built; these ZIPs are not drop-in replacements for a static whisper-rs build. [Packaging workflow](https://github.com/ggml-org/whisper.cpp/blob/v1.9.4/.github/workflows/release.yml#L470-L479).

### Gaps
- CUDA ZIP contents were not downloaded (roughly 675 MB for 12.4); packaging scope is verified from workflow. No binary-level cuobjdump verification of actual kernels.

## Which backends and kernels are enabled or disabled?

### Takeaway
Official x64 packages provide CPU, CPU+OpenBLAS, or CPU+CUDA. They do not provide the combined CUDA+Vulkan backend set Bono wants. CUDA is not a stripped cuBLAS-only build: its defaults include FlashAttention and custom matrix kernels.

### Cited Findings
- CPU x64 configure explicitly sets shared libraries, GGML_NATIVE=OFF, GGML_BACKEND_DL=ON and GGML_CPU_ALL_VARIANTS=ON. OpenBLAS adds GGML_BLAS=ON, vendor OpenBLAS; CUDA matrix builds toolkits 11.8/12.4, GGML_CUDA=ON, dynamic backends, all CPU variants and bundles CUDA DLLs. No Vulkan configure job exists. [Pinned release workflow](https://github.com/ggml-org/whisper.cpp/blob/v1.9.4/.github/workflows/release.yml).
- GGML defaults: CPU ON, CUDA OFF, BLAS OFF on Windows. CUDA FA ON, FA_ALL_QUANTS OFF; FORCE_MMQ OFF and FORCE_CUBLAS OFF; NO_VMM OFF and NO_PEER_COPY OFF; CUDA graphs OFF unless overridden. Release CUDA command does not override these defaults. NCCL option ON only takes effect when dependency discovery succeeds, so actual NCCL inclusion is unverified. [Pinned GGML options](https://github.com/ggml-org/whisper.cpp/blob/v1.9.4/ggml/CMakeLists.txt#L172-L189).
- CUDA CMake includes top-level `.cu`, fattn tile/mma, mmq and mmf template units. With FA_ALL_QUANTS OFF it selects fattn vector f16/f16, q4_0/q4_0, q8_0/q8_0, bf16/bf16 instead of all vector quant combinations. [Pinned CUDA CMake](https://github.com/ggml-org/whisper.cpp/blob/v1.9.4/ggml/src/ggml-cuda/CMakeLists.txt#L94-L115).
- Non-native defaults for both 11.8 and 12.4 are architectures `50-virtual;61-virtual;70-virtual;75-virtual;80-virtual;86-real;89-real;90-virtual`. Virtual means PTX/JIT; real means native device code. No explicit native Blackwell target because these toolkits predate the >=12.8 branch. [Architecture defaults](https://github.com/ggml-org/whisper.cpp/blob/v1.9.4/ggml/src/ggml-cuda/CMakeLists.txt#L6-L52).

### Inferences
- CUDA defaults above are source/workflow-derived expected configuration, not a binary disassembly manifest. The FA_ALL_QUANTS setting concerns FlashAttention KV combinations; it does not imply removing other quantized model weight matrix kernels. [CUDA source selection](https://github.com/ggml-org/whisper.cpp/blob/v1.9.4/ggml/src/ggml-cuda/CMakeLists.txt#L94-L115).
- Official artifacts can avoid CUDA compilation if the application adapts to their dynamic backend model, but cannot preserve Vulkan support without another compatible backend build. Mixing arbitrary ggml backend DLL versions is not established safe here.

### Gaps
- Exact native binary symbol/kernel inventory and runtime numerical/GPU tests remain unverified. Asset packages lack a per-build CMakeCache manifest. Current published versions can change; findings pinned to v1.9.4/b5130.
