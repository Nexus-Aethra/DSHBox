# Quick iteration deploy for Windows: builds the frontend + dshbox.exe and
# copies the binary over the installed copy at the install directory,
# skipping the NSIS bundle and bundled-runtime/server repacking.
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File scripts\dev-deploy.ps1
#   powershell -ExecutionPolicy Bypass -File scripts\dev-deploy.ps1 -InstallDir D:\dshbox -SkipFrontend
param(
  [string]$InstallDir = "D:\dshbox",
  [switch]$SkipFrontend
)
$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
Set-Location $Root

if (-not $SkipFrontend) {
  Write-Host ">> building frontend (tsc + vite)..." -ForegroundColor Cyan
  & "$env:USERPROFILE\.npm-global\pnpm.cmd" build
  if ($LASTEXITCODE -ne 0) { throw "frontend build failed" }
} else {
  Write-Host ">> skipping frontend build (-SkipFrontend)" -ForegroundColor Cyan
}

Write-Host ">> building dshbox.exe (release, incremental)..." -ForegroundColor Cyan
# --features custom-protocol is REQUIRED: without it tauri treats the binary
# as dev mode and the main window navigates to devUrl (http://localhost:1420)
# instead of the embedded frontend, showing ERR_CONNECTION_REFUSED.
cargo build --release --features custom-protocol --manifest-path src-tauri/Cargo.toml -p dshbox
if ($LASTEXITCODE -ne 0) { throw "dshbox build failed" }

$source = Join-Path $Root "src-tauri\target\release\dshbox.exe"
$target = Join-Path $InstallDir "dshbox.exe"
Copy-Item $source $target -Force
Write-Host ">> deployed to $target" -ForegroundColor Green
Write-Host "   restart DSH Box to test (the bundled runtime/server resources stay in place)."
