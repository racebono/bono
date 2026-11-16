[windows]
set shell := ["powershell.exe", "-NoLogo", "-Command"]

run:
    cargo build -p bono-phonemizer
    cargo run

# On Windows, tests build in a reusable slot (scripts/slot.ps1): a new worktree only rebuilds
# the workspace crates, not every dependency.

# Test the app without CUDA or Vulkan, e.g. `just test synthesizer`
[windows]
test *args:
    pwsh -NoProfile -File scripts/slot.ps1 just _test {{args}}

[unix]
test *args:
    just _test {{args}}

# The synthesizer tests talk to the phonemizer, so it's built into the same target directory first
_test *args:
    cargo build -p bono-phonemizer
    cargo test -p bono --no-default-features {{args}}

# Run any command in a reusable build slot, e.g. `just slot cargo check -p bono --no-default-features`
[windows]
slot +command:
    pwsh -NoProfile -File scripts/slot.ps1 {{command}}
