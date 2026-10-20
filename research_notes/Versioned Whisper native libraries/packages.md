# Third party versioned whisper.cpp libraries

## Where can Windows libraries be downloaded?

### Takeaway
Whisper.net publishes practical versioned native Windows DLLs, both stable NuGet packages and commit-labelled GitHub preview archives. These contain native whisper.cpp libraries usable outside .NET in principle, but are not turnkey whisper-rs static libraries.

### Cited Findings
- Stable release 1.9.1 is latest stable (June 1, 2026), with upstream whisper.cpp 1.8.5; exact git submodule f24588a272ae8e23280d9c220536437164e6ed28 verified through GitHub contents API. [Release](https://github.com/sandrohanea/whisper.net/releases/tag/1.9.1), [submodule API](https://api.github.com/repos/sandrohanea/whisper.net/contents/whisper.cpp?ref=1.9.1).
- Version-pinnable NuGet packages: Whisper.net.Runtime (CPU), Whisper.net.Runtime.NoAvx, Whisper.net.Runtime.Cuda.Windows (CUDA13), Whisper.net.Runtime.Cuda12.Windows, Whisper.net.Runtime.Vulkan, Whisper.net.Runtime.OpenVino. Use 1.9.1 for stable; package indexes currently also expose 1.9.2-preview1. [CPU](https://www.nuget.org/packages/Whisper.net.Runtime/1.9.1), [CUDA13](https://www.nuget.org/packages/Whisper.net.Runtime.Cuda.Windows/1.9.1), [CUDA12](https://www.nuget.org/packages/Whisper.net.Runtime.Cuda12.Windows/1.9.1), [Vulkan](https://www.nuget.org/packages/Whisper.net.Runtime.Vulkan/1.9.1).
- Preview archive from October 12, 2026 is keyed to upstream d1be6fde11ac6e0407606b4e42fe72d34add8037: native-runtimes.zip (548,249,311 bytes from release API). [Release](https://github.com/sandrohanea/whisper.net/releases/tag/preview-nativelibs-d1be6fd), [download](https://github.com/sandrohanea/whisper.net/releases/download/preview-nativelibs-d1be6fd/native-runtimes.zip).
- CPU package 1.9.1 downloaded and ZIP inventory verified: build/win-x64/{whisper.dll,ggml-whisper.dll,ggml-base-whisper.dll,ggml-cpu-whisper.dll}; no .lib or .h files in entire package. Script likewise copies DLLs only (and PDBs for Debug), GPU adds ggml-cuda-whisper.dll or ggml-vulkan-whisper.dll. [package download](https://api.nuget.org/v3-flatcontainer/whisper.net.runtime/1.9.1/whisper.net.runtime.1.9.1.nupkg), [pinned build script](https://github.com/sandrohanea/whisper.net/blob/1.9.1/windows-scripts.ps1).

### Inferences
- Rust integration needs source/build-script work: dynamically load exported C API or generate an MSVC import .lib and link DLLs, matching whisper-rs bindings to exact upstream revision; package DLLs beside application. These are shared libraries, not static .lib archives.
- Separate CUDA and Vulkan distributions each contain their own whisper/ggml core DLLs, and backend DLL dependencies are renamed. Do not assume mixing the separate variants gives a combined backend build. [target renaming](https://github.com/sandrohanea/whisper.net/blob/1.9.1/CMakeLists.txt).

### Gaps
- No inference smoke test or ABI comparison with Bono performed; no claim of drop-in compatibility.
- CUDA/Vulkan ZIP content not independently downloaded; packaging confirmed from pinned build and nuspec, CPU package inspected directly.

## Which backends and kernels are included?

### Takeaway
Whisper.net keeps upstream default kernel families; it is not a stripped Whisper-only CUDA kernel distribution. Stable 1.9.1 builds GPU backends separately with CPU fallback and FlashAttention enabled; it does not enable every FA quantization combination.

### Cited Findings
- Common Windows flags GGML_NATIVE=OFF, x64 AVX/AVX2/FMA/F16C ON; NoAvx sets all four OFF. CUDA adds GGML_CUDA=1; Vulkan adds GGML_VULKAN=1; OpenVino adds WHISPER_OPENVINO=1. No explicit architecture or kernel-family overrides. [script](https://github.com/sandrohanea/whisper.net/blob/1.9.1/windows-scripts.ps1).
- CUDA build matrix toolkits 13.0.1 and 12.4.1, Windows2022 x64. Vulkan SDK 1.4.321.1. [CUDA workflow](https://github.com/sandrohanea/whisper.net/blob/1.9.1/.github/workflows/windows-cuda-native-build.yml), [Vulkan workflow](https://github.com/sandrohanea/whisper.net/blob/1.9.1/.github/workflows/windows-vulkan-native-build.yml).
- Exact pinned upstream defaults: GGML_CPU ON; BLAS OFF (non-Apple); CUDA FA ON; FA_ALL_QUANTS OFF; FORCE_MMQ OFF; FORCE_CUBLAS OFF; CUDA_GRAPHS OFF (described llama.cpp only); NO_VMM OFF; NO_PEER_COPY OFF. CUDA/Vulkan/HIP/SYCL/OpenCL/RPC default OFF until a package explicitly enables its backend. [pinned GGML options](https://github.com/ggml-org/whisper.cpp/blob/f24588a272ae8e23280d9c220536437164e6ed28/ggml/CMakeLists.txt).
- CUDA sources glob all top-level .cu, fattn-tile*, fattn-mma*, mmq*, mmf* template instances. With FA_ALL_QUANTS OFF, FA vec specializations f16-f16, q4_0-q4_0, q8_0-q8_0, bf16-bf16 are explicitly included; other FA vec quant combinations excluded. Normal MMQ quantized kernels are not removed by that setting. [pinned CUDA build](https://github.com/ggml-org/whisper.cpp/blob/f24588a272ae8e23280d9c220536437164e6ed28/ggml/src/ggml-cuda/CMakeLists.txt).
- With native OFF and no architecture override: CUDA12.4.1 defaults 50-virtual;61-virtual;70-virtual;75-virtual;80-virtual;86-real;89-real;90-virtual. CUDA13.0.1 defaults 75-virtual;80-virtual;86-real;89-real;90-virtual;120a-real;121a-real. Virtual means PTX JIT on first use; real means architecture-specific machine code. [same pinned CUDA source](https://github.com/ggml-org/whisper.cpp/blob/f24588a272ae8e23280d9c220536437164e6ed28/ggml/src/ggml-cuda/CMakeLists.txt).
- Vulkan compiles shader variants from all .comp files, testing glslc support for cooperative_matrix, cooperative_matrix2, cooperative_matrix_decode_vector, integer_dot_product, bfloat16; no package-specific kernel exclusions. Debug/check/validation/test options default OFF. [pinned Vulkan CMake](https://github.com/ggml-org/whisper.cpp/blob/f24588a272ae8e23280d9c220536437164e6ed28/ggml/src/ggml-vulkan/CMakeLists.txt).

### Inferences
- Above CUDA architecture list and kernel settings are source-derived expected configuration for stable 1.9.1, not cuobjdump proof of the published binaries. Runtime kernel selection still depends on GPU/model/flash_attn setting; compiled availability does not mean always selected.
- CUDA13 package drops pre-Turing support; CUDA12 package is the better candidate if Maxwell/Pascal/Volta are supported by Bono. CUDA12.4 lacks native Blackwell targets but includes PTX for forward JIT; suitability/performance must be tested.
- Switching from Bono combined CUDA+Vulkan static build to these packages is an integration project and trades compile cost for ABI/version/loader management.

### Gaps
- Vulkan extension shader availability requires actual build logs/binary analysis; CMake probe logic alone cannot establish every published extension variant.
- Preview d1be6fd may differ from stable flags: do not apply stable matrix/kernel details blindly to preview.
