[CmdletBinding()]
param(
    [string]$Version = "",
    [string]$Glibc = "2.36",
    [switch]$SkipBuild
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
Set-Location $RepoRoot

if ([string]::IsNullOrWhiteSpace($Version)) {
    $cargoToml = Get-Content (Join-Path $RepoRoot "Cargo.toml") -Raw
    if ($cargoToml -match '(?m)^version\s*=\s*"([^"]+)"') {
        $Version = $Matches[1]
    } else {
        throw "Failed to read version from Cargo.toml"
    }
}
$Version = $Version.TrimStart("v")
$Target = "x86_64-unknown-linux-gnu.$Glibc"
$TargetDir = "x86_64-unknown-linux-gnu"

Write-Host "==> Packaging game-accel-server v$Version (Linux x86_64, glibc $Glibc)" -ForegroundColor Cyan

if (-not $SkipBuild) {
    # Refresh PATH in case zig was just installed via winget
    $machinePath = [Environment]::GetEnvironmentVariable("Path", "Machine")
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $env:Path = "$machinePath;$userPath"

    if (-not (Get-Command "zig" -ErrorAction SilentlyContinue)) {
        throw "zig not found in PATH. Install with: winget install zig.zig"
    }
    if (-not (Get-Command "cargo-zigbuild" -ErrorAction SilentlyContinue)) {
        throw "cargo-zigbuild not found in PATH. Install with: cargo install --locked cargo-zigbuild"
    }

    Write-Host "==> Cross-compiling server via cargo zigbuild ($Target)..." -ForegroundColor Cyan
    cargo zigbuild --release -p server --target $Target
    if ($LASTEXITCODE -ne 0) {
        throw "cargo zigbuild failed with exit code $LASTEXITCODE"
    }
}

$BinPath = Join-Path $RepoRoot "target\$TargetDir\release\server"
if (-not (Test-Path $BinPath)) {
    throw "Missing compiled binary: $BinPath"
}

$DistDir = Join-Path $RepoRoot "dist"
$StageDir = Join-Path $DistDir "server-stage"
if (Test-Path $StageDir) {
    Remove-Item -Recurse -Force $StageDir
}
New-Item -ItemType Directory -Force -Path $StageDir | Out-Null

Copy-Item $BinPath (Join-Path $StageDir "server")
if (Test-Path (Join-Path $RepoRoot "scripts\install.sh")) {
    Copy-Item (Join-Path $RepoRoot "scripts\install.sh") $StageDir
}
if (Test-Path (Join-Path $RepoRoot "README.md")) {
    Copy-Item (Join-Path $RepoRoot "README.md") $StageDir
}

$TarName = "game-accel-server-v$Version-linux-x86_64.tar.gz"
$TarPath = Join-Path $DistDir $TarName
if (Test-Path $TarPath) {
    Remove-Item -Force $TarPath
}

tar -C $StageDir -czf $TarPath .
if ($LASTEXITCODE -ne 0) {
    throw "tar failed with exit code $LASTEXITCODE"
}

$Hash = (Get-FileHash -Path $TarPath -Algorithm SHA256).Hash.ToLowerInvariant()
Write-Host "==> Created Linux server archive: $TarPath" -ForegroundColor Green
Write-Host "$Hash  $TarName"
