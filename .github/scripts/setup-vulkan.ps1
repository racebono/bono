$ErrorActionPreference = 'Stop'
$version = '1.4.341.1'
$root = Join-Path $env:RUNNER_TEMP "VulkanSDK/$version"
$name = "vulkansdk-windows-X64-$version.exe"
$installer = Join-Path $env:RUNNER_TEMP $name

# The full prebuilt SDK includes glslc, which Whisper's Vulkan backend requires.
Invoke-WebRequest -Uri "https://sdk.lunarg.com/sdk/download/$version/windows/$name" -OutFile $installer
$signature = Get-AuthenticodeSignature -LiteralPath $installer
if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch '(^|,\s*)CN="?LunarG, Inc\."?(,|$)') {
    throw 'Vulkan SDK installer does not have a valid LunarG signature'
}
# Copy-only installation leaves system registration and the system PATH alone.
$arguments = "--root `"$root`" --accept-licenses --default-answer --confirm-command install copy_only=1"
$process = Start-Process -FilePath $installer -ArgumentList $arguments -Wait -PassThru -WindowStyle Hidden
if ($process.ExitCode -ne 0) {
    throw "Vulkan SDK installer failed with exit code $($process.ExitCode)"
}

foreach ($file in @('Include/vulkan/vulkan.h', 'Lib/vulkan-1.lib', 'Bin/glslc.exe')) {
    if (-not (Test-Path -LiteralPath (Join-Path $root $file) -PathType Leaf)) {
        throw "Missing Vulkan SDK file: $file"
    }
}
$glslc = Join-Path $root 'Bin/glslc.exe'
& $glslc --version
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Without it, ggml leaves out what makes flash attention fast on RTX cards, and only says so in its
# build's output, which Cargo hides.
$shader = Join-Path $env:RUNNER_TEMP 'coopmat2.comp'
"#version 460`n#extension GL_NV_cooperative_matrix2 : require`nvoid main() {}" | Set-Content -LiteralPath $shader
& $glslc --target-env=vulkan1.3 -o "$shader.spv" $shader
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

"VULKAN_SDK=$root" >> $env:GITHUB_ENV
Join-Path $root 'Bin' >> $env:GITHUB_PATH
