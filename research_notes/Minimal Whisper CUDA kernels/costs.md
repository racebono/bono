# CUDA compilation cost and safe reduction boundaries

## What does the completed cold build spend compiling?

### Takeaway
The 19 MMQ template instances represent 2,934.518 seconds (48.91 minutes) of summed task elapsed duration, 44.81% of the observed CUDA total. Dropping the 14 IQ/K/MXFP4 MMQ instances would remove 2,155.604 seconds (35.93 minutes), or 32.92% of observed CUDA task duration.

### Cited Findings
- Parsed all 126 `.cu.obj` entries in the completed [Ninja log](D:/Projects/bono/target/cold-test/release/build/whisper-rs-sys-6a87b0d7dbab4545/out/build/.ninja_log). Total summed elapsed duration: 6,548.941 seconds / 109.15 minutes. Every duration below is `end_ms - start_ms`; these overlap and include contention.
- MMQ template instances: 19 files / 2,934.518 s. MMF instances: 16 / 855.606 s. All `fattn*` objects: 34 / 801.533 s. Other CUDA objects: 57 / 1,957.284 s. Source: [Ninja log](D:/Projects/bono/target/cold-test/release/build/whisper-rs-sys-6a87b0d7dbab4545/out/build/.ninja_log).
- IQ: 8 MMQ files / 1,200.311 s; K: 5 / 816.364 s; MXFP4: 1 / 138.929 s; classic Q4/Q5/Q8: 5 / 778.914 s. Source: same log.
- Current catalog profile Q5_0 + Q5_1 + Q8_0 retains 3 MMQ instances totaling 474.009 s (7.90 accumulated minutes), removes 16 totaling 2,460.509 s (41.01 accumulated minutes), 83.85% of MMQ duration and 37.57% of all CUDA task duration. Source: same log.
- Keeping only Q5_0 MMQ would retain one 175.831 s instance and remove 18 instances totaling 2,758.687 s (45.98 minutes / 42.12% of CUDA task duration). This is a narrow compatibility policy, not an automatic safe optimization. Source: same log.

| MMQ instance | Task seconds |
| --- | ---: |
| iq3_xxs | 138.289 |
| iq4_nl | 141.804 |
| mxfp4 | 138.929 |
| iq2_xxs | 146.099 |
| iq1_s | 151.704 |
| iq3_s | 148.396 |
| iq4_xs | 145.179 |
| q4_1 | 150.573 |
| iq2_s | 163.276 |
| q4_0 | 154.332 |
| q4_k | 154.164 |
| iq2_xs | 165.564 |
| q3_k | 169.631 |
| q5_0 | 175.831 |
| q5_1 | 180.583 |
| q2_k | 218.399 |
| q8_0 | 117.595 |
| q5_k | 128.855 |
| q6_k | 145.315 |

### Inferences
- IQ/K/MXFP4 removal is the highest-value first compile experiment if Bono supports only classic Whisper weight quantizations. These figures cannot be subtracted from the 10-minute wall time: tasks overlap and change speed under reduced contention. Two-job CI results may differ substantially.
- MMF and MMV are separate families. Keeping Q5_0 weights does not establish that floating-point matrix/vector kernels are unused; preserve them until runtime traces prove otherwise.

### Gaps
- No pruned build or inference benchmark was run. Exact CI wall-time savings and throughput effects remain unmeasured.

## What must change for pruning, and what should remain?

### Takeaway
Pruning requires a coherent CMake and CUDA dispatcher change. Removing source files alone will leave unresolved template symbols; Q8_1 activation quantization must remain even if no Q8 model is offered.

### Cited Findings
- CMake globs all `template-instances/mmq*.cu` and `mmf*.cu`; there is no per-weight-format CMake switch in this pinned tree. [CMakeLists.txt](D:/Projects/bono/target/cold-test/release/build/whisper-rs-sys-6a87b0d7dbab4545/out/whisper.cpp/ggml/src/ggml-cuda/CMakeLists.txt:109).
- `mmq.cu` directly calls `mul_mat_q_case` for all 19 types. Its `ggml_cuda_should_use_mmq` advertises those same types as supported; `FORCE_CUBLAS` returns false from the latter but does not suppress source compilation. [mmq.cu](D:/Projects/bono/target/cold-test/release/build/whisper-rs-sys-6a87b0d7dbab4545/out/whisper.cpp/ggml/src/ggml-cuda/mmq.cu:6).
- There is no Q8_1 weight MMQ instance or MMQ dispatch case. `quantize.cu` implements `quantize_row_q8_1_cuda` and `quantize_mmq_q8_1_cuda`, used as the activation conversion functions by quantized matvec and matrix paths in `ggml-cuda.cu`. [quantize.cu](D:/Projects/bono/target/cold-test/release/build/whisper-rs-sys-6a87b0d7dbab4545/out/whisper.cpp/ggml/src/ggml-cuda/quantize.cu:273); [dispatch](D:/Projects/bono/target/cold-test/release/build/whisper-rs-sys-6a87b0d7dbab4545/out/whisper.cpp/ggml/src/ggml-cuda/ggml-cuda.cu:2259).
- The measured build cache has `GGML_CUDA_FA=OFF`, `GGML_CUDA_FA_ALL_QUANTS=OFF`, architectures `75-virtual;86-real;120-real`. CMake still includes FA source globs. Thus 34 FA compilation tasks do not imply FA was enabled at inference. [CMakeCache](D:/Projects/bono/target/cold-test/release/build/whisper-rs-sys-6a87b0d7dbab4545/out/build/CMakeCache.txt); [CMake source](D:/Projects/bono/target/cold-test/release/build/whisper-rs-sys-6a87b0d7dbab4545/out/whisper.cpp/ggml/src/ggml-cuda/CMakeLists.txt:99).

### Inferences
- Use a central allowed-weight-type definition to gate source inclusion, MMQ switch arms and `should_use_mmq`; audit MMV/dequant/getrows switches too if the goal is to remove these formats throughout CUDA, rather than merely reduce expensive MMQ instantiations.
- Preserve Q8_1 activation quantization and Q5_0 dequantization/matvec pathways in a Q5_0 profile. Removing Q8_0 weight MMQ is distinct from removing Q8_1 internal activations.
- A profile can either reject unsupported model types explicitly at load time or keep correct fallback pathways. Advertising removed operations/types to scheduler risks aborts or misdispatch.

### Gaps
- The safety of fallback for removed MMQ formats was not tested. Do not assume every quantized type automatically reaches a cuBLAS fallback correctly after ad hoc pruning.

## What other kernel families look unused by Whisper?

### Takeaway
Training and non-Transformer model families are plausible secondary removals, but need dispatcher surgery and a graph-operation audit. They provide smaller savings than MMQ format pruning.

### Cited Findings
- Training objects `cross-entropy-loss`, `opt-step-adamw`, `opt-step-sgd` total 84.566 s; `ssm-conv`, `ssm-scan`, `wkv`, `gla` total 171.755 s in this log. [Ninja log](D:/Projects/bono/target/cold-test/release/build/whisper-rs-sys-6a87b0d7dbab4545/out/build/.ninja_log).
- The central backend operation dispatcher directly references SSM, RWKV, gated-linear-attention, loss and optimization implementations; source exclusion alone will therefore require matching dispatcher edits. [ggml-cuda.cu](D:/Projects/bono/target/cold-test/release/build/whisper-rs-sys-6a87b0d7dbab4545/out/whisper.cpp/ggml/src/ggml-cuda/ggml-cuda.cu:2699).
- Slow ordinary objects include `argsort` 136.599 s, `mmvq` 119.494 s, `mmvf` 110.110 s, `top-k` 89.024 s. [Ninja log](D:/Projects/bono/target/cold-test/release/build/whisper-rs-sys-6a87b0d7dbab4545/out/build/.ninja_log).

### Inferences
- Training/SSM/RWKV/GLA likely offer safe functional exclusions for pure Whisper inference once graph and dispatch checks confirm absence. Do not casually drop argsort/top-k because decoding features may use ranking operations; audit actual whisper graph construction first.
- Since FA already is disabled, excluding FA translation units and stubbing/guarding its dispatch is a plausible build overhead reduction (13.36 accumulated minutes observed), but source dependency review is required. This is not additional savings from disabling FA kernels: that switch is already off.

### Gaps
- A full Whisper graph-operation mapping belongs to the use-case investigation; this note only identifies candidates from cost and backend dispatch.

