$ErrorActionPreference = 'Stop'
$version = '13.4.2'
$release = $version -replace '^(\d+\.\d+).*', '$1'
$root = Join-Path $env:ProgramFiles "NVIDIA GPU Computing Toolkit/CUDA/v$release"
$nvcc = Join-Path $root 'bin/nvcc.exe'
# Only what ggml-cuda needs to compile and link, and the runtimes we ship. The full toolkit
# (Nsight, samples, every library) takes several times longer to install.
# The workflow's cache key hashes this script, so changing this list rebuilds the cache.
$components = @(
    'nvcc', 'crt', 'nvvm', 'cudart', 'cublas', 'cublas_dev', 'thrust'
) | ForEach-Object { "${_}_$release" }

if (-not (Test-Path -LiteralPath $nvcc -PathType Leaf)) {
    if ($env:ACT) {
        throw "Install CUDA $version locally before running act. Expected toolkit at $root"
    }

    # Install directly because cuda-toolkit@v0.2.30 does not yet support 13.4.
    $installer = Join-Path $env:RUNNER_TEMP "cuda_${version}_windows_x86_64_network.exe"
    Invoke-WebRequest -Uri "https://developer.download.nvidia.com/compute/cuda/$version/network_installers/cuda_${version}_windows_x86_64_network.exe" -OutFile $installer
    $signature = Get-AuthenticodeSignature -LiteralPath $installer
    if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch '(^|,\s*)CN=NVIDIA Corporation(,|$)') {
        throw 'CUDA installer does not have a valid NVIDIA signature'
    }
    $process = Start-Process -FilePath $installer -ArgumentList (@('-s') + $components) -Wait -PassThru -WindowStyle Hidden
    if ($process.ExitCode -notin @(0, 3010)) {
        throw "CUDA installer failed with exit code $($process.ExitCode)"
    }
}

# A partial install may not write version.json, so check the compiler's release instead.
$nvccVersion = & $nvcc --version
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
$nvccVersion
if (-not ($nvccVersion -match "release $([regex]::Escape($release)),")) {
    throw "Expected CUDA $release at $root"
}
foreach ($file in @(
    'bin/x64/cudart64_13.dll', 'bin/x64/cublas64_13.dll', 'bin/x64/cublasLt64_13.dll',
    'lib/x64/cudart.lib', 'lib/x64/cuda.lib', 'lib/x64/cublas.lib', 'lib/x64/cublasLt.lib',
    'include/cuda_runtime.h', 'include/cublas_v2.h'
)) {
    if (-not (Test-Path -LiteralPath (Join-Path $root $file) -PathType Leaf)) {
        throw "Missing CUDA file: $file"
    }
}
"CUDA_PATH=$root" >> $env:GITHUB_ENV
Join-Path $root 'bin' >> $env:GITHUB_PATH
Join-Path $root 'bin/x64' >> $env:GITHUB_PATH
