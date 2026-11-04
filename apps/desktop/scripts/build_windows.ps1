# Windows one-click build: sidecar prep -> tauri build (--no-bundle) -> single-file launcher pack.
#
# Usage (with Rust and Node installed):
#   powershell -File apps/desktop/scripts/build_windows.ps1 -ClientBin target\release\psk-client.exe
#
# Params:
#   -ClientBin   built psk-client.exe (auto-detected if omitted)
#   -TargetDir   tauri target dir (default apps\desktop\src-tauri\target\release)

[CmdletBinding()]
param(
  [string]$ClientBin,
  [string]$TargetDir
)

$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $PSScriptRoot
$repo = Resolve-Path (Join-Path $here "..\..")

if (-not $ClientBin) {
  $candidates = @(
    (Join-Path $repo "target\release\psk-client.exe")
  )
  $ClientBin = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
  if (-not $ClientBin) {
    throw "cannot find psk-client.exe; pass -ClientBin or build with cargo build --release first"
  }
}
Write-Host "== sidecar: $ClientBin"
& (Join-Path $PSScriptRoot "prepare_sidecar.ps1") -BinDir (Split-Path -Parent $ClientBin)

Push-Location $here
try {
  if (!(Test-Path "node_modules")) { npm install --no-audit --no-fund }
  npx tauri build --no-bundle
  if ($LASTEXITCODE -ne 0) { throw "tauri build failed" }
} finally { Pop-Location }

if (-not $TargetDir) { $TargetDir = Join-Path $here "src-tauri\target\release" }
& (Join-Path $PSScriptRoot "bundle_windows_portable.ps1") -ExeDir $TargetDir