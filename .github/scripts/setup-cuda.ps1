$ErrorActionPreference = 'Stop'
$version = '13.4.2'
$root = Join-Path $env:ProgramFiles 'NVIDIA GPU Computing Toolkit/CUDA/v13.4'
$manifest = Join-Path $root 'version.json'

if (-not (Test-Path -LiteralPath $manifest) -or
    (Get-Content -LiteralPath $manifest -Raw | ConvertFrom-Json).cuda.version -ne $version) {
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
    $process = Start-Process -FilePath $installer -ArgumentList '-s' -Wait -PassThru -WindowStyle Hidden
    if ($process.ExitCode -notin @(0, 3010)) {
        throw "CUDA installer failed with exit code $($process.ExitCode)"
    }
}

if ((Get-Content -LiteralPath $manifest -Raw | ConvertFrom-Json).cuda.version -ne $version) {
    throw "Expected CUDA $version after installation"
}
$nvcc = Join-Path $root 'bin/nvcc.exe'
& $nvcc --version
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
foreach ($dll in @('cudart64_13.dll', 'cublas64_13.dll', 'cublasLt64_13.dll')) {
    if (-not (Test-Path -LiteralPath (Join-Path $root "bin/x64/$dll") -PathType Leaf)) {
        throw "Missing CUDA x64 runtime: $dll"
    }
}
"CUDA_PATH=$root" >> $env:GITHUB_ENV
Join-Path $root 'bin' >> $env:GITHUB_PATH
Join-Path $root 'bin/x64' >> $env:GITHUB_PATH
