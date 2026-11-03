$ErrorActionPreference = 'Stop'
$app = Get-Process -Id $env:BONO_UPDATE_PID
# Holding a handle to the app keeps its ID from going to another process before it's waited on
$null = $app.Handle
[Console]::Out.WriteLine('waiting')
[Console]::Out.Flush()
$app.WaitForExit()

$source = $env:BONO_UPDATE_SOURCE
$target = $env:BONO_UPDATE_TARGET
$backup = $env:BONO_UPDATE_BACKUP
$retries = [int]$env:BONO_UPDATE_RETRIES
function Retrying($action) {
    for ($try = 1; ; $try++) {
        try { return & $action }
        catch { if ($try -ge $retries) { throw }; Start-Sleep -Seconds 1 }
    }
}
# A single rename, which fails as a whole. Move-Item would move a folder's contents one by one
# when the folder can't be renamed, and into the destination if it's there
function Move-Retrying($from, $to) {
    Retrying {
        if ([IO.Directory]::Exists($from)) { [IO.Directory]::Move($from, $to) }
        else { [IO.File]::Move($from, $to) }
    }
}

# The app's claim on the installation's updates, its lock file open without sharing, passes to the
# installer, so no Bono opened meanwhile removes what it moves. If another Bono has claimed them
# instead, it's left alone, with the new version still to install
$lock = Retrying { [IO.File]::Open($env:BONO_UPDATE_LOCK, 'OpenOrCreate', 'ReadWrite', 'None') }

$moved = New-Object System.Collections.ArrayList
try {
    if (Test-Path -LiteralPath $backup) { Remove-Item -LiteralPath $backup -Recurse -Force }
    $null = New-Item -ItemType Directory -Path $backup
    # bono.exe last
    foreach ($entry in Get-ChildItem -LiteralPath $source -Force | Sort-Object { $_.Name -eq 'bono.exe' }) {
        $name = $entry.Name
        $old = Join-Path $target $name
        $replaced = Test-Path -LiteralPath $old
        if ($replaced) { Move-Retrying $old (Join-Path $backup $name) }
        $null = $moved.Add(@($name, $replaced))
        Move-Retrying $entry.FullName $old
    }
} catch {
    $failure = $_.Exception.Message
    for ($i = $moved.Count - 1; $i -ge 0; $i--) {
        $name, $replaced = $moved[$i]
        $new = Join-Path $target $name
        $staged = Join-Path $source $name
        try {
            if (-not (Test-Path -LiteralPath $staged)) { Move-Retrying $new $staged }
            if ($replaced) { Move-Retrying (Join-Path $backup $name) $new }
        } catch { $failure += "`n" + $_.Exception.Message }
    }
    try { [IO.File]::WriteAllText($env:BONO_UPDATE_FAILED, $failure) } catch {}
}
# For the version opened to claim
$lock.Dispose()
# Start-Process would take brackets in the paths for wildcards
$start = New-Object Diagnostics.ProcessStartInfo -Property @{
    FileName = Join-Path $target 'bono.exe'; WorkingDirectory = $target; UseShellExecute = $true
}
$null = [Diagnostics.Process]::Start($start)
