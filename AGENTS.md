# Agent instructions

## Keep the UI thread responsive

Agents must avoid blocking the UI thread at all costs. Move blocking I/O,
network requests, model loading/inference, heavy computation, sleeps, and waits
for worker results off the UI thread. Use background tasks or worker threads
and return results through nonblocking polling, messages, or shared state with
brief lock scopes. An async function does not make blocking work safe to run on
the UI thread.

If blocking the UI thread is unavoidable, document why at the blocking call
site and in the change description. Explain why background execution is not
possible, the expected duration and user-visible impact, and how the blocking
is bounded or mitigated. Convenience alone is not a justification.

## Rely only on documented library behavior

Code must never rely on undocumented behavior of external libraries, crates,
SDKs, or APIs. Depend only on behavior guaranteed by their documentation or
public API contracts, not on behavior observed in testing, implementation
details, or source code that is not part of the documented interface. If the
required behavior is not documented, handle it explicitly in our own code or
choose a documented alternative.

## Test features

Run tests with the `cuda` and `vulkan` features disabled unless the changes
involve those features or their backend behavior. Both are enabled by default,
so use `just test` for ordinary application tests: it builds the phonemizer and
runs `cargo test -p bono --no-default-features`, passing any extra arguments on
(for example `just test synthesizer -- --nocapture`). Prefer it over other
commands, so every worktree builds the same dependencies and the build slots
(below) stay warm.
If other features are needed, enable them explicitly with `--features` while
keeping CUDA and Vulkan disabled.

When changes involve CUDA or Vulkan, also run relevant tests with the affected
feature explicitly enabled, for example
`just slot cargo test -p bono --no-default-features --features vulkan` (or
`cuda`, or `cuda,vulkan` when both are involved). Report any missing SDK or
hardware that prevents the relevant validation.

## Windows build and test paths

Plan build and test commands for Windows path-length limits. Cargo's native
dependencies (including whisper.cpp/ggml and SDL) create deeply nested build
directories; a long checkout or worktree path can cause maximum-path errors,
including the 260-character limit in some native build tools. `cargo check` and
`cargo test` can also trigger these native builds.

- On Windows, run Cargo build, check, test, and packaging commands through a
  build slot: `just test`, or `just slot <command>` for anything else (for
  example `just slot cargo check -p bono --no-default-features`).
  `scripts/slot.ps1` locks a free slot, points its short symlink
  (`<root>\slotN`) at this worktree, and runs the command there with the slot's
  target directory (`<root>\slotN-t`). The root is `BONO_SLOT_ROOT`
  (default `D:\worktrees\bt`). The short paths keep native builds under the
  path-length limits.
- Slots stay between tasks and worktrees, so dependencies are built once and
  reused; when a slot changes worktrees, the script rebuilds only the workspace
  crates. Don't set `CARGO_TARGET_DIR` yourself or create per-task target
  directories, which rebuild every dependency from scratch.
- A command waits, and says so, while every slot is busy. Run long commands in
  the background rather than killing them: a killed command releases its slot,
  but its Cargo processes may keep the target directory locked until they
  finish.
- Select packages and features for the task and installed SDKs as described in
  `README.md`. Building other features in a slot rebuilds the affected
  dependencies there, so keep to `just test` when it covers the change.
- Keep the repository's Ninja configuration in `.cargo/config.toml` and ensure
  Ninja is on `PATH`. It avoids the known MSBuild FileTracker path-length issue
  in Whisper's Vulkan build; do not switch to MSBuild to work around a failure.
- If a path-length error remains, inspect the failing path. A short target
  directory does not shorten dependency source/cache or checkout paths; use a
  shorter checkout or Cargo cache location when the symlink does not help.
  Do not assume Windows long-path support fixes every native tool's limits.
- Retry the affected command with the shorter paths before treating the error
  as a code regression. Report any remaining environment blocker and which
  checks actually completed; do not claim tests passed when compilation failed.

## Clean up temporary build paths

Build slots (`<root>\slotN`, `<root>\slotN-t` and their `.lock` and `.owner`
files) and the shared compiler cache are not task-owned: never delete them as
part of cleanup. Everything below applies to other paths a task creates.

Track temporary build directories, Cargo target/cache directories, and short
symlinks created for the task. Reuse them while the work is active, then remove
them after end-to-end work finishes and before the T3 thread settles (including
when handing back failed or blocked work). If the thread will resume for checks
or follow-up work, retain needed paths until that work finishes, then clean up
immediately before the final handback.

Before cleanup, ensure no build, test, packaging, or background process still
uses those paths. Verify each absolute path and remove only task-owned temporary
artifacts; preserve shared caches, existing user directories, deliverables, and
the worktree itself. Remove symlinks as links without traversing or deleting
their targets. Report any cleanup that could not be completed and the remaining
paths in the final handback.

## Keep specific results out of the skill

Never mention specific results in the skill files under `skill/`, including its
references and evals. Do not cite observed incidents, dates, sessions, quoted
model answers, or pass/fail outcomes from real or test runs. Describe the
behavior to follow and the scenarios to evaluate in general terms; record
specific results in commit messages, pull requests, or issues instead.

## Pull Requests

- Conventional commit titles, plain language: `fix(web): new threads no longer spike CPU`.
- UI changes need before/after images. Motion or timing needs a short video.
- Upload PR evidence to GitHub. Never commit PR-only screenshots or assets such as `.github/pr-assets/`.
