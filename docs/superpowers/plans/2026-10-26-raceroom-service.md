# RaceRoom penalty and pit-service implementation plan

Goal: implement #141 and #142 in one PR using the existing background converter/recorder and nullable generated scoring columns.

Architecture: preserve per-driver penalty types, reason codes and type-specific values in standings; add player-only pit state, menu intent, action mask and durations. Existing header generation supplies Rust/C bindings and SQLite schema. LMU leaves additions empty. No new UI-thread work or repeated countdown announcements.

- [x] Extend include/bono/scoring.h and src/sim/rrre.rs with nullable fields and type-specific reason conversion; preserve existing count semantics.
- [x] Expose detail through src/engineer/tool.rs status/quick and strategy, including simultaneous actions, intent caveat and no countdown clearance.
- [x] Document fields in skill/references/schema.md.
- [x] Test simultaneous penalties, type-scoped reasons, unknown codes, sentinels, pit-state and serving transitions, changing menu intent, zero durations, SQLite persistence/change detection and engineer output/older schemas.
- [x] Run formatting, shared recorder tests and cargo test -p bono --no-default-features with short checkout-specific target directory; review diff.
- [ ] Commit, push and open one closing PR.

Validation constraint: live RaceRoom channel availability requires a running game and remains a separate manual check.

Validation: shared recorder 9 passed; application CPU-only suite 229 passed, 16 ignored. Build required network access for ONNX Runtime. Existing compiler warnings remain. No live game was available for per-car/session validation.

Follow-up: normalize source-specific menu indices and action bits into named nullable selected/active service booleans. Give penalty values explicit units and booleans for presence-only requirements. Retain raw values/reason codes and UI-only details under simulator_metadata.raceroom; future adapters use their own namespace and populate shared fields directly. Validate generic scoring persistence without RaceRoom metadata and rerun CPU-only suites.
