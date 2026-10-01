[CmdletBinding()]
param(
    [ValidateSet('amd64', 'arm64')]
    [string]$Architecture = 'amd64',
    [switch]$Offline
)

$ErrorActionPreference = 'Stop'
$projectDirectory = Split-Path -Parent $PSScriptRoot
$toolDirectory = Join-Path $projectDirectory 'target/cross-tools'
$python = Join-Path $toolDirectory 'Scripts/python.exe'
$zigbuild = Join-Path $toolDirectory 'Scripts/cargo-zigbuild.exe'
$target = if ($Architecture -eq 'amd64') { 'x86_64-unknown-linux-musl' } else { 'aarch64-unknown-linux-musl' }

foreach ($command in @('rustup', 'cargo', 'python')) {
    if (-not (Get-Command $command -ErrorAction SilentlyContinue)) {
        throw "Install $command on the build computer first."
    }
}
Push-Location $projectDirectory
$previousPython = $env:CARGO_ZIGBUILD_PYTHON_PATH
$previousCache = $env:CARGO_ZIGBUILD_CACHE_DIR
try {
    if (-not (Test-Path $python)) {
        if ($Offline) { throw 'Run once without -Offline to install cross-compilation tools.' }
        & python -m venv $toolDirectory
        if ($LASTEXITCODE -ne 0) { throw 'Failed to create build tool environment.' }
    }
    if (-not (Test-Path $zigbuild)) {
        if ($Offline) { throw 'Cross-compilation tools are not installed.' }
        & $python -m pip install cargo-zigbuild==0.23.4 ziglang==0.16.0
        if ($LASTEXITCODE -ne 0) { throw 'Failed to install cross-compilation tools.' }
    }
    if (-not $Offline) {
        & rustup target add $target
        if ($LASTEXITCODE -ne 0) { throw 'Failed to install Rust Linux target.' }
    }
    $env:CARGO_ZIGBUILD_PYTHON_PATH = $python
    $env:CARGO_ZIGBUILD_CACHE_DIR = Join-Path $projectDirectory 'target/zig-cache'
    $buildArguments = @('zigbuild', '--locked', '--release', '--target', $target, '-j', '1')
    if ($Offline) { $buildArguments += '--offline' }
    & $zigbuild @buildArguments
    if ($LASTEXITCODE -ne 0) { throw 'Linux cross-compilation failed.' }
    & $python scripts/package-binary.py $target
    if ($LASTEXITCODE -ne 0) { throw 'Linux package creation failed.' }
} finally {
    $env:CARGO_ZIGBUILD_PYTHON_PATH = $previousPython
    $env:CARGO_ZIGBUILD_CACHE_DIR = $previousCache
    Pop-Location
}
