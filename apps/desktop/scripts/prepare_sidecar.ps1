# Stash the built psk-client.exe into the Tauri sidecar directory.
#
# Tauri's externalBin expects a target-triple suffix:
#   src-tauri/binaries/psk-client-x86_64-pc-windows-msvc.exe
#
# Usage:
#   powershell -File apps/desktop/scripts/prepare_sidecar.ps1 -BinDir target\release

[CmdletBinding()]
param(
  [Parameter(Mandatory = $true)][string]$BinDir,
  [string]$Target = "x86_64-pc-windows-msvc"
)

$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $PSScriptRoot
$src = Join-Path $BinDir "psk-client.exe"
if (!(Test-Path $src)) {
  throw "cannot find $src; run cargo build --release at the repo root first"
}
$dstDir = Join-Path $here "src-tauri\binaries"
New-Item -ItemType Directory -Force -Path $dstDir | Out-Null
$dst = Join-Path $dstDir "psk-client-$Target.exe"
Copy-Item $src $dst -Force
Write-Host "sidecar: $dst"