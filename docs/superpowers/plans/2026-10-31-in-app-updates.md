# In-app Updates Implementation Plan

**Goal:** Implement the approved two-step updater.
**Architecture:** A dependency-light updater crate owns release selection, validated durable staging, and transactional helper installation. The UI polls background tasks and requests ordinary shutdown after helper acknowledgement.
**Spec:** ../specs/2026-10-31-in-app-updates-design.md

## Constraints
Windows x86-64 official builds only; preserve channel and user data; never block the UI; downloads replace previous staged updates only after validation; installation requires a separate explicit action.

## Tasks
- [x] Add updater library and helper, with tests for ordering, selection, archive validation, staged replacement, rollback and recovery.
- [x] Add explicit build identity, bundle manifest and checksums to packaging/CI. Test the helper independently without native app dependencies.
- [x] Integrate an update menu with background checks, progress, durable downloaded state and helper acknowledgement before shutdown. Add localized labels and usage/recovery documentation.
- [ ] Run formatting, helper tests and `cargo test -p bono --no-default-features` using a short target directory and Ninja. Obtain GPT 6.1 Sol low-effort review, fix substantive findings, push a PR and monitor CI.

## Review focus
Malformed/mixed-channel releases; interrupted downloads retaining the previous generation; traversal and archive size bounds; interrupted installation recovery with missing bono.exe; duplicate instances and unwritable installations.

Implementation will be native in this session as requested; the end-to-end skill supplies the independent review.

## Validation

Application: 257 passed, 18 ignored; updater: 16 unit tests and two real-process integration tests passed (one child fixture is intentionally ignored in the outer harness and invoked by the integration test); packaging: six passed, one environment-dependent test ignored. The helper test covers parent exit, replacement and executable-fixture restart. Stable/nightly CI identity scripts and prerelease rejection were exercised locally. Two GPT 6.1 Sol low-effort reviews approved. The full graphical distribution smoke test is documented but was not run.
