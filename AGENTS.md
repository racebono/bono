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
so use `cargo test -p bono --no-default-features` for ordinary application tests.
If other features are needed, enable them explicitly with `--features` while
keeping CUDA and Vulkan disabled.

When changes involve CUDA or Vulkan, also run relevant tests with the affected
feature explicitly enabled, for example
`cargo test -p bono --no-default-features --features vulkan` (or `cuda`, or
`cuda,vulkan` when both are involved). Report any missing SDK or hardware that
prevents the relevant validation.

## Windows build and test paths

Plan build and test commands for Windows path-length limits. Cargo's native
dependencies (including whisper.cpp/ggml and SDL) create deeply nested build
directories; a long checkout or worktree path can cause maximum-path errors,
including the 260-character limit in some native build tools. `cargo check` and
`cargo test` can also trigger these native builds.

- Before building on Windows, choose a short, writable, absolute Cargo target
  directory, unique to this checkout. Use it consistently for build, check, test,
  and packaging commands so they reuse the same artifacts. Respect sandbox write
  permissions when choosing the location.
- For example, in PowerShell, if `D:\bt\bono-t3` is writable and reserved for this
  checkout:

  ```powershell
  $env:CARGO_TARGET_DIR = 'D:\bt\bono-t3'
  cargo build -p bono
  cargo test -p bono --no-default-features
  ```

  These are examples of target-directory setup; select packages and features for
  the task and installed SDKs as described in `README.md`.
- Keep the repository's Ninja configuration in `.cargo/config.toml` and ensure
  Ninja is on `PATH`. It avoids the known MSBuild FileTracker path-length issue
  in Whisper's Vulkan build; do not switch to MSBuild to work around a failure.
- If a path-length error remains, inspect the failing path. A short target
  directory does not shorten dependency source/cache or checkout paths; use a
  shorter checkout or Cargo cache location when those paths are the cause.
  Do not assume Windows long-path support fixes every native tool's limits.
- Retry the affected command with the shorter paths before treating the error
  as a code regression. Report any remaining environment blocker and which
  checks actually completed; do not claim tests passed when compilation failed.
