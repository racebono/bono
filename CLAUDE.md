# Repository instructions

Read and follow [AGENTS.md](AGENTS.md), including its requirements to keep the UI
thread responsive, document unavoidable blocking, run tests without CUDA and
Vulkan unless the changes involve them, and plan Windows build and test commands
around native-tool maximum path-length limits. Prefer short symlinks to the
current worktree whenever possible. Follow its temporary-build-path cleanup
requirements after end-to-end work and before the T3 thread settles: stop using
task-owned temporary build/target/cache paths, remove them and any temporary
symlinks safely without deleting their targets, and report any remaining paths.
Never mention specific results (observed incidents, dates, quoted answers or
pass/fail outcomes) in the skill files under `skill/`.
