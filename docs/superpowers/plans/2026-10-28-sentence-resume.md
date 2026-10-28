# Sentence Resume Implementation Plan

**Goal:** Resume engineer audio at the start of the first unfinished synthesized sentence after a spotter interruption.

**Architecture:** Retain sentence PCM until SDL consumes it. Pause before observing the input queue, discard fully consumed sentences, clear the stream, and submit retained sentences again after release. SDL reports consumption, not physical speaker position; device buffering remains an accuracy limit. Keep replay incremental on the UI thread, without synthesis, sleeps, or worker waits.

**Tech Stack:** Rust, SDL3, existing background synthesizer.

## Tasks
- [x] Add a sentence queue with explicit submission and consumption accounting; test partial sentences, exact boundaries, repeated interrupts, empty pieces, and unknown queue positions.
- [x] Integrate hold/release and incremental replay into engineer playback, preserving cancellation, device removal, draining, and failure tones.
- [x] Run formatting and application tests with CUDA/Vulkan disabled using a short target path, then obtain independent review and fix substantive findings.
- [ ] Commit, push, open a PR, and follow CI. Leave merging to the user.

## Validation
- Application suite: 257 passed, 18 ignored, 0 failed with CUDA/Vulkan disabled.
- Isolated SDL dummy-device test: 1 passed; exercises completed-output replay, repeated holds, and cancellation.
- Independent code review: no concrete bugs found; added the recommended SDL regression test.
- Changed-file rustfmt and git diff --check passed. Workspace-wide rustfmt reports pre-existing differences in unrelated files.
- SDL reference: https://wiki.libsdl.org/SDL3/SDL_GetAudioStreamQueued (input consumption) and https://wiki.libsdl.org/SDL3/SDL_ClearAudioStream (discard queued data).

