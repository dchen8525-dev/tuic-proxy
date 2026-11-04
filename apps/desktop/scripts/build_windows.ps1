# Windows one-click build: sidecar prep -> tauri build (--no-bundle) -> single-file launcher pack.
#
# Usage (with Rust and Node installed):
#   powershell -File apps/desktop/scripts/build_windows.ps1 -ClientBin target\release\tuic-proxy-client.exe
#
# Params:
#   -ClientBin   built tuic-proxy-client.exe (auto-detected if omitted)
#   -TargetDir   tauri target dir (default apps\desktop\src-tauri\target\release)
#   -Version     version stamped into the portable exe (default: Cargo.toml)
#   -OutDir      output directory for the portable exe (default: <repo>\dist)

[CmdletBinding()]
param(
  [string]$ClientBin,
  [string]$TargetDir,
  [string]$Version,
  [string]$OutDir
)

$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $PSScriptRoot
$repo = Resolve-Path (Join-Path $here "..\..")

if (-not $ClientBin) {
  $candidates = @(
    (Join-Path $repo "target\release\tuic-proxy-client.exe")
  )
  $ClientBin = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
  if (-not $ClientBin) {
    throw "cannot find tuic-proxy-client.exe; pass -ClientBin or build with cargo build --release first"
  }
}
Write-Host "== sidecar: $ClientBin"
& (Join-Path $PSScriptRoot "prepare_sidecar.ps1") -BinDir (Split-Path -Parent $ClientBin)

Push-Location $here
try {
  if (!(Test-Path "node_modules")) { npm ci --no-audit --no-fund }
  npx tauri build --no-bundle
  if ($LASTEXITCODE -ne 0) { throw "tauri build failed" }
} finally { Pop-Location }

if (-not $TargetDir) { $TargetDir = Join-Path $here "src-tauri\target\release" }
$bundleArgs = @{ ExeDir = $TargetDir }
if ($Version) { $bundleArgs.Version = $Version }
if ($OutDir) { $bundleArgs.OutDir = $OutDir }
& (Join-Path $PSScriptRoot "bundle_windows_portable.ps1") @bundleArgs