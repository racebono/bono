# CUDA quantized dispatch and safe pruning design

## Which quantized formats and operations are needed?

### Takeaway
For Q5_0 weights keep Q5_0 MMQ, Q5_0 MMVQ, Q5_0 conversion/getrows, and Q8_1 activation quantization. To preserve every format emitted by this pinned Whisper quantizer, keep ten MMQ formats rather than the current nineteen. Do not assume Whisper only supports the five classic formats: the pinned quantizer explicitly supports five K formats too.

### Cited Findings
- Native submodule revision is `2eeeba56e9edd762b4b38467bab96c2517163158`, verified with local git rev-parse. All source references below were read from the corresponding copied native tree under `target/cold-test/release/build/whisper-rs-sys-6a87b0d7dbab4545/out/whisper.cpp` (GitHub fetch returned cache miss; source findings are local inspection, not successful remote retrieval).
- Quantizer accepted names and qtype switch are Q4_0, Q4_1, Q5_0, Q5_1, Q8_0, Q2_K, Q3_K, Q4_K, Q5_K, Q6_K. IQ and MXFP4 types are explicitly invalid quantizer destinations. [Pinned common-ggml.cpp lines 6–19, 50–80](https://github.com/ggml-org/whisper.cpp/blob/2eeeba56e9edd762b4b38467bab96c2517163158/examples/common-ggml.cpp#L6).
- CUDA MMQ instantiates nineteen formats: the ten above plus MXFP4 and IQ1_S, IQ2_XXS, IQ2_XS, IQ2_S, IQ3_XXS, IQ3_S, IQ4_XS, IQ4_NL. [Pinned MMQ switch](https://github.com/ggml-org/whisper.cpp/blob/2eeeba56e9edd762b4b38467bab96c2517163158/ggml/src/ggml-cuda/mmq.cu#L6).
- Q5_0 MMQ means quantized weights multiplied by Q8_1-derived quantized activations; MMVQ uses vec_dot_q5_0_q8_1. Thus removing Q8_0 *weight* instances does not justify removing Q8_1 activation quantization. [MMQ](https://github.com/ggml-org/whisper.cpp/blob/2eeeba56e9edd762b4b38467bab96c2517163158/ggml/src/ggml-cuda/mmq.cu); [MMVQ](https://github.com/ggml-org/whisper.cpp/blob/2eeeba56e9edd762b4b38467bab96c2517163158/ggml/src/ggml-cuda/mmvq.cu#L14).
- Whisper allocates linear projection and token embedding weights in the model wtype, but convolution weights are F16 except F32 models. Therefore a Q5_0-only weight profile still needs floating point matrix kernels and ordinary CUDA operations. [whisper.cpp lines 1677–1841](https://github.com/ggml-org/whisper.cpp/blob/2eeeba56e9edd762b4b38467bab96c2517163158/src/whisper.cpp#L1677).

### Inferences
- A conservative first profile preserves all ten quantizer destinations and drops nine MMQ instances (47% of MMQ translation units, not 47% wall time). A product-specific Q5_0 profile drops eighteen (95% of MMQ units), provided catalog/custom model policy permits it.
- If the catalog audit establishes Q5_0 + Q5_1 + Q8_0, the practical catalog profile keeps three MMQ instances and removes sixteen. Adding Q4_0/Q4_1 keeps five and removes fourteen (the K, IQ, MXFP4 instances). This preserves all five classic formats, not all ten destinations of the pinned quantizer. With MMVQ and conversion retained, removed MMQ types can still work through other CUDA paths; this is a fast-path profile, not a format ban.
- Retain convert.cu and getrows.cu untouched initially: they provide Q5_0 dequantization/conversion and embedding lookup, and are not the nineteen expensive separate MMQ translation units.

### Gaps
- K-format block divisibility and model-specific quantizer behavior need validation before claiming every Whisper model can use every accepted K format. Accepted quantizer destinations do not establish that every model geometry is compatible.
- No GPU inference or pruned build was executed in this investigation.

## When does Q5_0 use MMQ, and can cuBLAS replace it?

### Takeaway
Q5_0 MMQ is a real runtime path on modern NVIDIA hardware, especially encoder and prompt/batched projection work. Token decoding with up to eight columns selects MMVQ first. FORCE_CUBLAS does not eliminate custom quantized vector kernels or their compilation.

### Cited Findings
- Quantized F32-input/F32-output matmul with no unsafe temporary-view padding selects MMVQ for `src1->ne[1] <= 8`; MMVQ precedes MMQ in dispatch. Larger work selects MMQ when should_use_mmq permits it, otherwise cuBLAS. [ggml-cuda.cu 2181–2264](https://github.com/ggml-org/whisper.cpp/blob/2eeeba56e9edd762b4b38467bab96c2517163158/ggml/src/ggml-cuda/ggml-cuda.cu#L2181); [MMVQ threshold](https://github.com/ggml-org/whisper.cpp/blob/2eeeba56e9edd762b4b38467bab96c2517163158/ggml/src/ggml-cuda/mmvq.cuh#L3).
- should_use_mmq returns true for supported formats when turing_mma_available(cc); that helper requires NVIDIA and highest compiled arch >= Turing (75). For older NVIDIA: compiled arch below DP4A rejects MMQ; otherwise it permits MMQ if no FP16 MMA hardware or column count <64. FORCE_MMQ overrides this latter heuristic, not earlier unsupported-format/arch checks. [MMQ lines 262–314](https://github.com/ggml-org/whisper.cpp/blob/2eeeba56e9edd762b4b38467bab96c2517163158/ggml/src/ggml-cuda/mmq.cu#L262); [common.cuh 315](https://github.com/ggml-org/whisper.cpp/blob/2eeeba56e9edd762b4b38467bab96c2517163158/ggml/src/ggml-cuda/common.cuh#L315).
- FORCE_CUBLAS is an early false return in should_use_mmq. CMake still globs all mmq instance sources; MMVQ selection is independent. [MMQ](https://github.com/ggml-org/whisper.cpp/blob/2eeeba56e9edd762b4b38467bab96c2517163158/ggml/src/ggml-cuda/mmq.cu#L262); [CMake](https://github.com/ggml-org/whisper.cpp/blob/2eeeba56e9edd762b4b38467bab96c2517163158/ggml/src/ggml-cuda/CMakeLists.txt).
- cuBLAS fallback converts quantized weights to F16 on fast-FP16 hardware or F32 otherwise using ggml_get_to_fp16_cuda/get_to_fp32_cuda, then invokes GEMM. Q5_0 converters exist in convert.cu. [ggml-cuda.cu 1286–1365](https://github.com/ggml-org/whisper.cpp/blob/2eeeba56e9edd762b4b38467bab96c2517163158/ggml/src/ggml-cuda/ggml-cuda.cu#L1286); [convert.cu](https://github.com/ggml-org/whisper.cpp/blob/2eeeba56e9edd762b4b38467bab96c2517163158/ggml/src/ggml-cuda/convert.cu).

### Inferences
- Removing all MMQ is technically possible with dispatch adjusted, retaining MMVQ and conversion/cuBLAS. It increases weight dequantization and temporary floating point storage and may reduce encoder throughput. Benchmark before choosing this over retaining Q5_0 MMQ.
- Source pruning for unused weight formats is preferable to disabling Q5_0 MMQ because it preserves existing fast paths for Bono models.

### Gaps
- Exact Bono runtime tensor dispatch counts are not instrumented; encoder-vs-decoder attribution follows model graph shapes and dispatch logic, not profiling.

## What must a safe patch change?

### Takeaway
Filtering `.cu` files alone is insufficient. Use one format allowlist to generate both the CMake source list and dispatch definitions; preserve fallback conversion and explicit support decisions.

### Cited Findings
- mmq.cuh declares extern explicit instantiations; each instance file defines one through DECL_MMQ_CASE. mmq.cu directly calls all nineteen in a runtime switch. Removing instance files while keeping switch calls leaves references to absent definitions. [mmq.cuh 4051–4072](https://github.com/ggml-org/whisper.cpp/blob/2eeeba56e9edd762b4b38467bab96c2517163158/ggml/src/ggml-cuda/mmq.cuh#L4051); [Q5_0 instance](https://github.com/ggml-org/whisper.cpp/blob/2eeeba56e9edd762b4b38467bab96c2517163158/ggml/src/ggml-cuda/template-instances/mmq-instance-q5_0.cu); [switch](https://github.com/ggml-org/whisper.cpp/blob/2eeeba56e9edd762b4b38467bab96c2517163158/ggml/src/ggml-cuda/mmq.cu#L6).

### Inferences
1. Add a fork-owned option e.g. `GGML_CUDA_MMQ_TYPES` default ALL; WHISPER profile includes the ten types; BONO profile Q5_0 only. These option names are proposed, not existing upstream flags.
2. Replace unconditional mmq source glob with selected instance list. Generate macros per enabled MMQ type.
3. Guard cases in both `ggml_cuda_mul_mat_q_switch_type` and the support switch in `ggml_cuda_should_use_mmq` with those same macros. Disabled MMQ formats then fall back to existing MMVQ/cuBLAS paths rather than falsely selecting missing MMQ kernels. Extern declarations may be left present because declarations alone produce no reference, though guarding them improves consistency.
   No backend supports_op restriction is needed for MMQ-only pruning with existing MMVQ/converters retained: those formats remain supported by CUDA. Introducing a format ban here would unnecessarily lose working cuBLAS fallback.
4. Retain all MMVQ initially: it is one source file, and preserving all formats there allows small-column work for a broader model set. For further compile reduction guard the instantiating cases in `mul_mat_vec_q_switch_type` (mmvq.cu 479 onward); also add type-aware checks to ordinary, fused, and mul_mat_id MMVQ selectors. Simply deleting cases otherwise reaches fatal default. Audit all call sites before pruning MMVQ.
5. Keep MMF F16/F32 (and BF16 until separately audited), cuBLAS, convert/getrows, activation quantization, attention, normalization, softmax, convolution, copy/elementwise kernels. Quantized weight pruning does not imply those are unused.
6. Validate build/link; CPU/CUDA/Vulkan model load; GPU inference against baseline for representative Q5_0 sizes and all retained formats; nonquantized FP16; short and long audio; beam/prompt settings; GPU memory; encoder/decoder timing; and clean native CI build timing. These checks distinguish compile savings from runtime regressions.

### Gaps
- Link-failure reasoning is source-derived, not an intentionally broken link experiment.
- CPU fallback for entirely unsupported formats should be explicit product policy. Restricting MMQ alone preserves CUDA conversion fallback; restricting all CUDA support requires auditing backend supports_op to avoid scheduler accepting operations whose launchers abort.
