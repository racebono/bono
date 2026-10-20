# Bono Whisper model and quantization scope

## Which model formats does Bono actually expose?

### Takeaway
Bono's recommended setup uses Q5_0, but preserving the complete current download menu requires Q5_0, Q5_1, Q8_0 and floating-point support. The menu is a live upstream catalog rather than a pinned allowlist.

### Cited Findings
- The recommended model is `ggml-large-v3-turbo-q5_0.bin`; setup selects that exact filename. — [app constant](D:/Projects/bono/src/app/topbar/audio/whisper.rs:51), [setup lookup](D:/Projects/bono/src/app/topbar/audio/whisper.rs:411).
- The catalog endpoint is the root of `ggerganov/whisper.cpp` on Hugging Face; the app accepts every root-level filename matching `ggml-*.bin`, extracts an optional `-q` suffix only for display, and does not restrict quantizations. — [catalog/parser](D:/Projects/bono/src/app/topbar/audio/whisper.rs:735), [filter](D:/Projects/bono/src/app/topbar/audio/whisper.rs:818).
- The current catalog, inspected 2026-10-20, contains tiny/base/small (including English variants) in unquantized, Q5_1 and Q8_0; medium (including English) in unquantized, Q5_0 and Q8_0; large-v1 unquantized; large-v2 in unquantized, Q5_0 and Q8_0; large-v3 in unquantized and Q5_0; large-v3-turbo in unquantized, Q5_0 and Q8_0. No Q4, K-quant, IQ, BF16 or MXFP4 model is offered there. — [primary model catalog](https://huggingface.co/ggerganov/whisper.cpp/tree/main).
- The UI loops over the full parsed catalog and offers use/download controls. — [menu](D:/Projects/bono/src/app/topbar/audio/whisper.rs:157).
- Config stores an arbitrary optional UTF-8 model path and passes it directly to `WhisperContext::new_with_params` with default context parameters. The inspected Whisper UI has no custom file picker, but a manually set persisted path is not restricted by the catalog. — [config](D:/Projects/bono/src/config.rs:264), [load](D:/Projects/bono/src/app/topbar/audio/whisper.rs:442).

### Inferences
- A Q5_0-only CUDA specialization preserves recommended setup but narrows acceleration for existing menu choices. Q5_0/Q5_1/Q8_0 is the better initial compatibility target.
- A reduced build must either enforce a supported-type allowlist or safely fall back for additional future catalog entries/custom paths; the current dynamic menu does not provide that contract.

### Gaps
- No inference compatibility or numerical accuracy test was performed. Catalog filenames and converter/loader source establish intended types, not an inspection of each multi-gigabyte model payload.

## Are quantized models purely quantized, and what custom formats exist?

### Takeaway
Quantized Whisper models still require floating-point tensors and operations. The pinned quantizer supports several formats absent from Bono's current menu; that broader source support is not evidence that Bono users need all of them.

### Cited Findings
- Cargo pins whisper-rs `d2787715d43086c12fedbefb781cf0cefcca4a00`; its checked-out whisper.cpp submodule is `2eeeba56e9edd762b4b38467bab96c2517163158`. — [lock](D:/Projects/bono/Cargo.lock:5179), [pinned upstream tree](https://github.com/ggml-org/whisper.cpp/tree/2eeeba56e9edd762b4b38467bab96c2517163158).
- Its quantizer accepts Q4_0/Q4_1/Q5_0/Q5_1/Q8_0 and Q2_K/Q3_K/Q4_K/Q5_K/Q6_K. The conversion switch rejects IQ, BF16 and MXFP4 quantization even though GGML's broader ftype enum contains them. — [quantizer helper lines 6–16 and 48–78](https://github.com/ggml-org/whisper.cpp/blob/2eeeba56e9edd762b4b38467bab96c2517163158/examples/common-ggml.cpp#L6).
- The quantizer only converts 2D tensors; it explicitly skips convolution biases and encoder/decoder positional embeddings, preserving other non-2D tensors as their source floating type. — [quantizer lines 159–168](https://github.com/ggml-org/whisper.cpp/blob/2eeeba56e9edd762b4b38467bab96c2517163158/examples/quantize/quantize.cpp#L159), [2D filter lines 121–164](https://github.com/ggml-org/whisper.cpp/blob/2eeeba56e9edd762b4b38467bab96c2517163158/examples/common-ggml.cpp#L121).
- The converter defaults to F16 but preserves small tensors and positional embeddings/biases in F32. — [converter lines 261 and 296–321](https://github.com/ggml-org/whisper.cpp/blob/2eeeba56e9edd762b4b38467bab96c2517163158/models/convert-pt-to-ggml.py#L296).
- The loader maps model ftype to matrix weight type, uses F16 for convolution weights except all-F32 models, and creates F32 positional embeddings, norms and biases. Attention/MLP matrices and decoder token embedding use the selected weight type. — [loader lines 1555, 1677–1678 and 1758–1834](https://github.com/ggml-org/whisper.cpp/blob/2eeeba56e9edd762b4b38467bab96c2517163158/src/whisper.cpp#L1677).
- Bono performs ordinary Whisper language detection and beam-search transcription (beam size 5); no app-level quantization conversion or special matrix implementation is selected. — [inference](D:/Projects/bono/src/app/topbar/audio/whisper.rs:565).

### Inferences
- Default model weight type is Q5_0 alongside F16/F32 tensors; full menu weight types add Q5_1 and Q8_0. Floating-point matmul kernels cannot be removed merely because the default filename is quantized.
- K-quant kernels are unnecessary for today's built-in menu but can matter for manually supplied models made with the bundled quantizer. Q4 variants likewise are optional custom-model scope. IQ/MXFP4/BF16 are even less justified by the current app catalog; their actual loader viability is a separate question.
- Q8_1 used internally as an activation/intermediate representation must not be confused with Q8_0 model weights; retaining only model-weight names is insufficient to derive the full CUDA dependency closure.

### Gaps
- Need CUDA dispatch analysis from the other research strand to determine the activation formats, conversion kernels, matrix-vector kernels, and matrix-matrix kernels required by these weight types.
- No evidence of actual installed user model selections or production telemetry was examined; “needed” here means preserving app features/current catalog, not observed usage statistics.
