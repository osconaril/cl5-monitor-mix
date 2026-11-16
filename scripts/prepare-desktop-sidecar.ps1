#!/usr/bin/env pwsh
$ErrorActionPreference = "Stop"

$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
$TargetLine = (rustc -vV | Select-String "^host: ").ToString()
$Target = $TargetLine -replace "^host:\s*", ""

$SidecarDir = Join-Path $RepoRoot "crates\desktop\binaries"
$WebSourceDir = Join-Path $RepoRoot "crates\web"
$WebDistDir = Join-Path $WebSourceDir "dist"
$PackagedWebDir = Join-Path $RepoRoot "crates\desktop\resources\web"
$LauncherSource = Join-Path $RepoRoot "crates\desktop\launcher.html"

# 1. Build sidecar binaries
cargo build --manifest-path (Join-Path $RepoRoot "Cargo.toml") --release -p cl5-server -p fake-console
if ($LASTEXITCODE -ne 0) { throw "Cargo build failed with exit code $LASTEXITCODE" }

# 2. Build Trunk web frontend
Push-Location $WebSourceDir
try {
    $env:NO_COLOR = "true"
    trunk build --release --dist dist
    if ($LASTEXITCODE -ne 0) { throw "Trunk build failed with exit code $LASTEXITCODE" }
}
finally {
    Pop-Location
}

# 3. Verify and copy web assets
$IndexFile = Join-Path $WebDistDir "index.html"
if (-not (Test-Path $IndexFile -PathType Leaf)) {
    throw "Missing built index.html at $IndexFile"
}
if (-not (Get-ChildItem -Path $WebDistDir -Filter "*.wasm" -Recurse -File | Select-Object -First 1)) {
    throw "No .wasm file found in $WebDistDir"
}

if (Test-Path $PackagedWebDir) {
    Remove-Item -Path $PackagedWebDir -Recurse -Force
}
New-Item -ItemType Directory -Path $PackagedWebDir -Force | Out-Null

Copy-Item -Path (Join-Path $WebDistDir "*") -Destination $PackagedWebDir -Recurse -Force
Copy-Item -Path $LauncherSource -Destination (Join-Path $PackagedWebDir "launcher.html") -Force

if (-not (Test-Path (Join-Path $PackagedWebDir "index.html") -PathType Leaf)) {
    throw "Verification failed: index.html missing in $PackagedWebDir"
}
if (-not (Get-ChildItem -Path $PackagedWebDir -Filter "*.wasm" -Recurse -File | Select-Object -First 1)) {
    throw "Verification failed: no .wasm file in $PackagedWebDir"
}
Write-Host "Prepared bundled web assets: $PackagedWebDir"

# 4. Copy sidecar executables with target triple + .exe suffix
$Ext = if ($Target -like "*windows*") { ".exe" } else { "" }
New-Item -ItemType Directory -Path $SidecarDir -Force | Out-Null

foreach ($Binary in @("cl5-server", "fake-console")) {
    $SourceBin = Join-Path $RepoRoot "target\release\$Binary$Ext"
    $DestSidecar = Join-Path $SidecarDir "$Binary-$Target$Ext"
    Copy-Item -Path $SourceBin -Destination $DestSidecar -Force
    Write-Host "Prepared desktop sidecar: $DestSidecar"
}