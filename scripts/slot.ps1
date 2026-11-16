# Runs a command in a reusable build slot, so a new worktree doesn't rebuild every dependency.
#
#   pwsh -NoProfile -File scripts/slot.ps1 cargo test -p bono --no-default-features
#
# A slot is a short symlink (<root>\slotN) pointing at the calling worktree, plus a target
# directory (<root>\slotN-t) that stays between runs. Every worktree builds through the same
# slot paths, so the dependencies built for one worktree are fresh for the next and only the
# workspace crates are rebuilt. The short paths also keep native builds (whisper.cpp, SDL)
# under Windows' path-length limits.
#
# Slots are shared and long-lived: don't delete them as a task's temporary build paths.
#
# Environment:
#   BONO_SLOT_ROOT  folder holding the slots (default: D:\worktrees\bt)
#   BONO_SLOTS      how many slots may build at once (default: 3)
#
# Each slot is locked for the whole command, and a command waits when every slot is busy.
# The lock is an open file handle, so it is released even if the command is killed.
# Commands run inside a slot (BONO_SLOT is set) run directly in it.

$ErrorActionPreference = 'Stop'

if ($args.Count -eq 0) {
    Write-Error 'usage: slot.ps1 <command> [args...]'
    exit 2
}
$command = $args[0]
$commandArgs = @($args | Select-Object -Skip 1)

if ($env:BONO_SLOT) {
    & $command @commandArgs
    exit $LASTEXITCODE
}

function Get-NormalPath([string]$path) {
    [IO.Path]::GetFullPath($path).TrimEnd('\', '/')
}

$root = if ($env:BONO_SLOT_ROOT) { $env:BONO_SLOT_ROOT } else { 'D:\worktrees\bt' }
if (-not (Test-Path -LiteralPath ([IO.Path]::GetPathRoot([IO.Path]::GetFullPath($root))))) {
    Write-Error "Slot root '$root' is on a drive that doesn't exist; set BONO_SLOT_ROOT to a short, writable folder"
    exit 2
}
$root = Get-NormalPath $root
New-Item -ItemType Directory -Force -Path $root | Out-Null
$count = if ($env:BONO_SLOTS) { [int]$env:BONO_SLOTS } else { 3 }

$top = git rev-parse --show-toplevel
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
$top = Get-NormalPath $top
$topItem = Get-Item -LiteralPath $top
if ($topItem.LinkType) { $top = Get-NormalPath $topItem.ResolvedTarget }
$relative = [IO.Path]::GetRelativePath($top, (Get-NormalPath (Get-Location).ProviderPath))
if ($relative.StartsWith('..')) { $relative = '.' }

function Get-Owner([string]$name) {
    $file = Join-Path $root "$name.owner"
    if (Test-Path -LiteralPath $file) { (Get-Content -LiteralPath $file -Raw).Trim() } else { '' }
}

function Get-SlotOrder {
    $slots = 1..$count | ForEach-Object {
        $name = "slot$_"
        $ownerFile = Join-Path $root "$name.owner"
        $owner = Get-Owner $name
        [pscustomobject]@{
            Name = $name
            # This worktree's own slot, then one warm from another worktree, then an empty one
            Rank = if ($owner -ieq $top) { 0 } elseif ($owner) { 1 } else { 2 }
            # Among slots warm for other worktrees, reuse the least recently used one first
            Used = if (Test-Path -LiteralPath $ownerFile) { (Get-Item -LiteralPath $ownerFile).LastWriteTimeUtc } else { [datetime]::MinValue }
        }
    }
    $slots | Sort-Object Rank, Used | ForEach-Object Name
}

# Take the first free slot, preferring the one this worktree used last
$lock = $null
$slot = $null
$waiting = $false
while (-not $lock) {
    foreach ($name in Get-SlotOrder) {
        try {
            $lock = [IO.File]::Open((Join-Path $root "$name.lock"), 'OpenOrCreate', 'ReadWrite', 'None')
            $slot = $name
            break
        } catch [IO.IOException] {
            # Another command holds this slot
        }
    }
    if (-not $lock) {
        if (-not $waiting) {
            Write-Host "slot: all $count slots in $root are busy; waiting for one to free up"
            $waiting = $true
        }
        Start-Sleep -Seconds 5
    }
}

try {
    $link = Join-Path $root $slot
    $target = Join-Path $root "$slot-t"

    # Point the slot's link at this worktree, removing only an old link, never a real folder
    $linkItem = Get-Item -LiteralPath $link -Force -ErrorAction SilentlyContinue
    if ($linkItem -and -not $linkItem.LinkType) {
        throw "$link is a real folder, not a slot link; move it out of the way"
    }
    if (-not $linkItem -or (Get-NormalPath $linkItem.ResolvedTarget) -ine $top) {
        # Deleting a directory link removes the link itself, not the folder it points to
        if ($linkItem) { [IO.Directory]::Delete($link) }
        try {
            New-Item -ItemType SymbolicLink -Path $link -Target $top | Out-Null
        } catch {
            # Symbolic links need Developer Mode or admin rights; junctions don't
            New-Item -ItemType Junction -Path $link -Target $top | Out-Null
        }
    }

    $env:CARGO_TARGET_DIR = $target
    $env:BONO_SLOT = $slot
    Set-Location -LiteralPath (Join-Path $link $relative)

    # Cargo decides freshness by file timestamps, so another worktree's older files could look
    # up to date. When the slot changes worktrees, rebuild the workspace crates from scratch;
    # dependencies come from the registry and git caches, so they stay valid.
    if ((Get-Owner $slot) -ine $top) {
        if (Test-Path -LiteralPath $target) {
            $metadata = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json
            if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
            $packages = $metadata.packages | ForEach-Object { '-p'; $_.name }
            Write-Host "slot: $slot last built another worktree; cleaning the workspace crates"
            cargo clean @packages
            if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
        }
        Set-Content -LiteralPath (Join-Path $root "$slot.owner") -Value $top
    } else {
        # Mark the slot as recently used
        (Get-Item -LiteralPath (Join-Path $root "$slot.owner")).LastWriteTimeUtc = [datetime]::UtcNow
    }

    Write-Host "slot: $slot ($link -> $top), CARGO_TARGET_DIR=$target"
    & $command @commandArgs
    exit $LASTEXITCODE
} finally {
    $lock.Dispose()
}
