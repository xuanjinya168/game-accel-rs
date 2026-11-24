[CmdletBinding()]
param(
    [string]$Version = "",
    [string]$WintunDll = "",
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
Write-Host "==> Packaging game-accel v$Version (Windows x64)" -ForegroundColor Cyan

if (-not $SkipBuild) {
    Write-Host "==> Building release binaries (game-accel-gui, client)..." -ForegroundColor Cyan
    cargo build --release -p game-accel-gui -p client
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed with exit code $LASTEXITCODE"
    }
}

$GuiExe = Join-Path $RepoRoot "target\release\game-accel.exe"
$CliExe = Join-Path $RepoRoot "target\release\client.exe"
if (-not (Test-Path $GuiExe)) {
    throw "Missing binary: $GuiExe"
}

# Resolve wintun.dll (user arg -> repo root -> cached download -> download official zip)
$WintunCacheDir = Join-Path $RepoRoot "target\wintun"
$CachedWintunDll = Join-Path $WintunCacheDir "wintun\bin\amd64\wintun.dll"

if (-not [string]::IsNullOrWhiteSpace($WintunDll)) {
    if (-not (Test-Path $WintunDll)) {
        throw "Specified wintun.dll not found: $WintunDll"
    }
    $ResolvedWintun = (Resolve-Path $WintunDll).Path
} elseif (Test-Path (Join-Path $RepoRoot "wintun.dll")) {
    $ResolvedWintun = (Join-Path $RepoRoot "wintun.dll")
} elseif (Test-Path $CachedWintunDll) {
    $ResolvedWintun = $CachedWintunDll
} else {
    $WintunUrl = "https://www.wintun.net/builds/wintun-0.14.1.zip"
    $WintunExpectedSha256 = "07C256185D6EE3652E09FA55C0B673E2624B565E02C4B9091C79CA7D2F24EF51"
    New-Item -ItemType Directory -Force -Path $WintunCacheDir | Out-Null
    $WintunZip = Join-Path $WintunCacheDir "wintun-0.14.1.zip"
    Write-Host "==> Downloading official wintun 0.14.1..." -ForegroundColor Cyan
    Invoke-WebRequest -Uri $WintunUrl -OutFile $WintunZip -UseBasicParsing
    $ActualHash = (Get-FileHash -Path $WintunZip -Algorithm SHA256).Hash.ToUpperInvariant()
    if ($ActualHash -ne $WintunExpectedSha256) {
        Remove-Item -Force $WintunZip
        throw "wintun-0.14.1.zip SHA256 mismatch: expected $WintunExpectedSha256, got $ActualHash"
    }
    Expand-Archive -Path $WintunZip -DestinationPath $WintunCacheDir -Force
    if (-not (Test-Path $CachedWintunDll)) {
        throw "wintun.dll not found after extracting $WintunZip"
    }
    $ResolvedWintun = $CachedWintunDll
}
Write-Host "==> Using wintun.dll: $ResolvedWintun"

# Prepare staging directory
$DistDir = Join-Path $RepoRoot "dist"
$StageDir = Join-Path $DistDir "stage"
if (Test-Path $StageDir) {
    Remove-Item -Recurse -Force $StageDir
}
New-Item -ItemType Directory -Force -Path $StageDir | Out-Null

Copy-Item $GuiExe $StageDir
if (Test-Path $CliExe) {
    Copy-Item $CliExe $StageDir
}
Copy-Item $ResolvedWintun (Join-Path $StageDir "wintun.dll")
if (Test-Path (Join-Path $RepoRoot "README.md")) {
    Copy-Item (Join-Path $RepoRoot "README.md") $StageDir
}

# 1. Portable ZIP
$ZipName = "game-accel-v$Version-windows-x64-portable.zip"
$ZipPath = Join-Path $DistDir $ZipName
if (Test-Path $ZipPath) {
    Remove-Item -Force $ZipPath
}
Compress-Archive -Path (Join-Path $StageDir "*") -DestinationPath $ZipPath -CompressionLevel Optimal
Write-Host "==> Created portable archive: $ZipPath" -ForegroundColor Green

# 2. Inno Setup Installer (if ISCC.exe is installed)
$IsccCandidates = @(
    @(
        (Get-Command "ISCC.exe" -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Source -ErrorAction SilentlyContinue),
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "${env:ProgramFiles}\Inno Setup 6\ISCC.exe",
        "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
    ) | Where-Object { -not [string]::IsNullOrWhiteSpace($_) -and (Test-Path $_) }
)

$SetupBaseName = "game-accel-v$Version-windows-x64-setup"
$SetupPath = Join-Path $DistDir "$SetupBaseName.exe"
if ($IsccCandidates.Count -gt 0) {
    $Iscc = $IsccCandidates[0]
    $IssFile = Join-Path $RepoRoot "packaging\windows\installer.iss"
    Write-Host "==> Building Inno Setup installer via $Iscc..." -ForegroundColor Cyan
    & $Iscc `
        "/DAppVersion=$Version" `
        "/DSourceDir=$StageDir" `
        "/DOutputDir=$DistDir" `
        "/DOutputBaseName=$SetupBaseName" `
        $IssFile
    if ($LASTEXITCODE -ne 0) {
        throw "ISCC.exe failed with exit code $LASTEXITCODE"
    }
    Write-Host "==> Created installer: $SetupPath" -ForegroundColor Green
} else {
    Write-Host "==> Inno Setup (ISCC.exe) not found; skipping installer build (portable zip is ready)." -ForegroundColor Yellow
}

# 3. SHA256SUMS.txt
$Artifacts = @($ZipPath)
if (Test-Path $SetupPath) {
    $Artifacts += $SetupPath
}
$SumsPath = Join-Path $DistDir "SHA256SUMS.txt"
$Lines = foreach ($file in $Artifacts) {
    $hash = (Get-FileHash -Path $file -Algorithm SHA256).Hash.ToLowerInvariant()
    $name = Split-Path -Leaf $file
    "$hash  $name"
}
$Lines | Set-Content -Path $SumsPath -Encoding ascii
Write-Host "==> Wrote checksums: $SumsPath" -ForegroundColor Green
Get-Content $SumsPath
