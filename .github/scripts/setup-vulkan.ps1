$ErrorActionPreference = 'Stop'
$version = '1.4.304.1'
$root = Join-Path $env:RUNNER_TEMP "VulkanSDK/$version"
$installer = Join-Path $env:RUNNER_TEMP "VulkanSDK-$version-Installer.exe"

# The full prebuilt SDK includes glslc, which Whisper's Vulkan backend requires.
Invoke-WebRequest -Uri "https://sdk.lunarg.com/sdk/download/$version/windows/VulkanSDK-$version-Installer.exe" -OutFile $installer
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
& (Join-Path $root 'Bin/glslc.exe') --version
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

"VULKAN_SDK=$root" >> $env:GITHUB_ENV
Join-Path $root 'Bin' >> $env:GITHUB_PATH
