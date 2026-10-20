# Bono compatibility with versioned Whisper native libraries

## What exact native dependency does Bono need?

### Takeaway
Bono uses a custom whisper-rs fork, not an interchangeable system Whisper package. The lockfile pins wrapper revision d2787715d43086c12fedbefb781cf0cefcca4a00; its checked-out submodule is whisper.cpp 2eeeba56e9edd762b4b38467bab96c2517163158, reporting version 1.8.3. Use exact commit identity rather than only the 1.8.3 label when selecting artifacts.

### Cited Findings
- Local Cargo.lock lines 5179–5191 pin both whisper-rs and whisper-rs-sys to d2787715d43086c12fedbefb781cf0cefcca4a00 in Aandreba/whisper-rs. [Local lockfile](D:/Projects/bono/Cargo.lock:5179)
- `git submodule status` in the cached wrapper checkout reports 2eeeba56e9edd762b4b38467bab96c2517163158; its sys/whisper.cpp/CMakeLists.txt declares VERSION 1.8.3. [Cached native source](C:/Users/Alex/.cargo/git/checkouts/whisper-rs-8c1dccfa435c963c/d278771/sys/whisper.cpp/CMakeLists.txt:3)
- The source at that upstream commit exposes backend choices and fine-grained CUDA options; CUDA and Vulkan are separate build switches. [Upstream native CMake](https://raw.githubusercontent.com/ggml-org/whisper.cpp/2eeeba56e9edd762b4b38467bab96c2517163158/ggml/CMakeLists.txt)

### Inferences
- Downloaded DLLs built from another whisper.cpp commit require header/ABI review before use. The exact pinned commit plus the same compiler/runtime and backend configuration is the straightforward compatible choice.

### Gaps
- No binary compatibility test was performed. Cached checkout is clean except untracked Cargo marker files; no source modifications were made by this research.

## Can the current wrapper consume an existing library?

### Takeaway
There is no prebuilt-library bypass in the pinned build.rs. An integration change is necessary even if a suitable download exists.

### Cited Findings
- Pinned build.rs copies bundled native sources, generates bindings, creates cmake::Config, and unconditionally calls config.build(). Its only early return is DOCS_RS. WHISPER_DONT_GENERATE_BINDINGS skips binding generation only. [Pinned local build.rs](C:/Users/Alex/.cargo/git/checkouts/whisper-rs-8c1dccfa435c963c/d278771/sys/build.rs:100)
- It sets BUILD_SHARED_LIBS=OFF. For ordinary CUDA/Vulkan builds it explicitly links static whisper, ggml, ggml-base, ggml-cpu, ggml-cuda, and ggml-vulkan. It separately emits CUDA import-library links cublas, cudart, cublasLt, cuda and Vulkan's vulkan-1. [Pinned linking code](C:/Users/Alex/.cargo/git/checkouts/whisper-rs-8c1dccfa435c963c/d278771/sys/build.rs:301)
- Upstream whisper-rs documentation describes WHISPER_DONT_GENERATE_BINDINGS as a workaround for bindgen, not a native compilation bypass. Upstream moved from GitHub to Codeberg. [Official upstream README](https://github.com/tazz4843/whisper-rs)
- Published whisper-rs-sys documentation currently resolves to 0.15.0 and lists bindgen and cmake as build dependencies. This documents a crate, not a downloadable native binary SDK. [Official crate documentation](https://docs.rs/whisper-rs-sys/latest/whisper_rs_sys/)

### Inferences
- A ready-made Windows DLL ZIP is not a drop-in replacement for this static build script. Either add an explicit static prebuilt path preserving library names and dependencies, or implement shared-library linkage and runtime packaging as a separate change.
- Best fit for current behavior: publish an immutable bundle of the six static libraries, generated matching bindings/headers, metadata, hashes, and link requirements from the exact pinned native commit. Key it by MSVC target/runtime, CUDA version, GPU architectures, backend flags, and native revision. Consume it through an explicit prebuilt branch in the fork's sys/build.rs.
- Such a bundle avoids nvcc and Vulkan shader compilation in routine Bono builds. CUDA link import libraries/runtime distribution still need handling; downloading native Whisper alone does not automatically remove those requirements.

### Gaps
- The Aandreba GitHub page/releases/API could not be fetched through the web tool (cache miss/inaccessible); cannot claim its releases are empty.
- Codeberg current build.rs was inaccessible through the web tool, so no claim that newer upstream has no prebuilt support. The no-bypass conclusion is proven only for Bono's pinned fork.

## Which features must a replacement preserve?

### Takeaway
Bono's current Windows distribution combines CPU + CUDA + Vulkan, with CUDA FlashAttention disabled. Match that combination explicitly; a generic CUDA package may omit Vulkan or enable different kernels/architectures.

### Cited Findings
- Cargo.toml defaults to cuda and vulkan; Apple additionally enables metal. HIP, SYCL and OpenBLAS are opt-in, absent from the default Windows configuration. [Bono features](D:/Projects/bono/Cargo.toml:97)
- .cargo/config.toml sets GGML_CUDA_FA=OFF and comments that Bono leaves Whisper flash_attn off. [Bono build configuration](D:/Projects/bono/.cargo/config.toml:19)
- Windows workflow sets GGML_NATIVE=OFF, empty CMAKE_CUDA_FLAGS, CUDA architectures 75-virtual;86-real;120-real. The pinned CUDA CMake rewrites 120 to 120a. [Bono workflow](D:/Projects/bono/.github/workflows/windows.yml:67), [native CUDA CMake](https://raw.githubusercontent.com/ggml-org/whisper.cpp/2eeeba56e9edd762b4b38467bab96c2517163158/ggml/src/ggml-cuda/CMakeLists.txt)
- The prior local cold-build CMakeCache records CUDA_FA=OFF, CUDA_FA_ALL_QUANTS=OFF, FORCE_CUBLAS=OFF, FORCE_MMQ=OFF, Vulkan=ON, NATIVE=OFF, OpenMP=OFF, AVX/AVX2=ON, AVX512=OFF. [Local cache](D:/Projects/bono/target/cold-test/release/build/whisper-rs-sys-6a87b0d7dbab4545/out/build/CMakeCache.txt:637)
- The pinned CUDA CMake globs MMQ/MMF template instances irrespective of FORCE_CUBLAS. FA_ALL_QUANTS changes vector FA instances to either all formats or only q4_0, q8_0, f16 pairs. FA=OFF defines GGML_CUDA_NO_FA; kernel source guards remove implementations. [Native CUDA CMake](https://raw.githubusercontent.com/ggml-org/whisper.cpp/2eeeba56e9edd762b4b38467bab96c2517163158/ggml/src/ggml-cuda/CMakeLists.txt)

### Inferences
- Changing to an FA-enabled prebuilt can add unused binary content but does not automatically activate FA in Bono. Runtime flash_attn and compiled kernel availability are distinct.
- Prebuilt vendor descriptions should include native commit, CPU baseline, backend DLLs/libraries, CUDA architectures and toolkit, FA flags, and quantized matrix multiplication coverage. A backend name alone cannot prove kernel equivalence.

### Gaps
- No runtime GPU/model-format regression test was run; the local cold cache is evidence of that completed build, not proof that every future CI run has identical effective CMake values.
