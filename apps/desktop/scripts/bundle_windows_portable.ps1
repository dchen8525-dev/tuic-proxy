# Pack the tauri-built tuic-proxy-client-gui.exe (+ sidecar) into a single-file portable exe.
#
#   [tuic-proxy-launcher.exe][payload.zip][64-byte footer]
#
# On first run the launcher extracts the payload to
# %LOCALAPPDATA%\TUICProxy\bin\<first-8 of SHA-256>\ and launches the cached
# tuic-proxy-client-gui.exe; new versions get their own directory by content hash and old
# unused directories are cleaned after 7 days. User settings live in the
# WebView localStorage (keyed by com.tuic.proxy).
#
# Usage:
#   powershell -File apps/desktop/scripts/bundle_windows_portable.ps1 `
#     -ExeDir apps\desktop\src-tauri\target\release [-Version 0.0.1] [-OutDir dist]

[CmdletBinding()]
param(
  [Parameter(Mandatory = $true)][string]$ExeDir,
  [string]$Version,
  [string]$OutDir
)

$ErrorActionPreference = "Stop"
$repo = Resolve-Path (Join-Path $PSScriptRoot "..\..\..")
if (-not $Version) {
  $cargoToml = Get-Content (Join-Path $repo "Cargo.toml") -Raw
  if ($cargoToml -match '(?ms)^\[package\].*?^version\s*=\s*"([^"]+)"') {
    $Version = $Matches[1]
  } else {
    throw "cannot read [package].version from Cargo.toml; pass -Version"
  }
}
if (-not $OutDir) { $OutDir = Join-Path $repo "dist" }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

$app = Join-Path $ExeDir "tuic-proxy-client-gui.exe"
if (!(Test-Path $app)) { throw "cannot find $app; run npx tauri build first" }
$sidecar = Get-ChildItem $ExeDir -Filter "tuic-proxy-client*.exe" |
           Where-Object { $_.Name -ne "tuic-proxy-client-gui.exe" } | Select-Object -First 1
if (!$sidecar) { throw "cannot find sidecar tuic-proxy-client*.exe" }

if (-not [BitConverter]::IsLittleEndian) {
  throw "this script assumes a little-endian host"
}

$stage = Join-Path ([System.IO.Path]::GetTempPath()) ("tuic-proxy-launcher-stage-" + [guid]::NewGuid())
New-Item -ItemType Directory -Force -Path $stage | Out-Null
try {
  $payloadDir = Join-Path $stage "payload"
  New-Item -ItemType Directory -Force -Path $payloadDir | Out-Null
  Copy-Item $app (Join-Path $payloadDir "tuic-proxy-client-gui.exe")
  Copy-Item $sidecar.FullName (Join-Path $payloadDir "tuic-proxy-client.exe")
  Set-Content -Path (Join-Path $payloadDir "version.txt") -Encoding utf8 -Value "tuic-proxy-client-gui $Version"

  # 1) build the launcher
  $crateDir = Join-Path $repo "apps\desktop\launcher"
  $targetDir = Join-Path $crateDir "target"
  Push-Location $crateDir
  try {
    cargo build --release --target-dir $targetDir
    if ($LASTEXITCODE -ne 0) { throw "cargo build (launcher) failed" }
  } finally { Pop-Location }
  $launcher = Join-Path $targetDir "release\tuic-proxy-launcher.exe"
  if (!(Test-Path $launcher)) { throw "cannot find $launcher" }

  # 2) payload zip
  try {
    Add-Type -AssemblyName System.IO.Compression.FileSystem -ErrorAction Stop
  } catch {}
  $payload = Join-Path $stage "payload.zip"
  if (Test-Path $payload) { Remove-Item -Force $payload }
  [System.IO.Compression.ZipFile]::CreateFromDirectory(
    $payloadDir, $payload,
    [System.IO.Compression.CompressionLevel]::Optimal, $false)

  # 3) 64-byte footer: magic + format version + offset/length + SHA-256
  $sha = (Get-FileHash -Algorithm SHA256 -LiteralPath $payload).Hash
  $hash = [byte[]]::new(32)
  for ($i = 0; $i -lt 32; $i++) {
    $hash[$i] = [Convert]::ToByte($sha.Substring($i * 2, 2), 16)
  }
  $offset = (Get-Item $launcher).Length
  $length = (Get-Item $payload).Length

  $footer = [byte[]]::new(64)
  [Text.Encoding]::ASCII.GetBytes("TUICBOOT").CopyTo($footer, 0)
  [BitConverter]::GetBytes([uint32]1).CopyTo($footer, 8)   # format version
  [BitConverter]::GetBytes([uint64]$offset).CopyTo($footer, 16)
  [BitConverter]::GetBytes([uint64]$length).CopyTo($footer, 24)
  $hash.CopyTo($footer, 32)

  # 4) concatenate: launcher + payload + footer
  $out = Join-Path $OutDir "tuic-proxy-client-gui-$Version-windows-amd64.exe"
  $fs = [System.IO.File]::Create($out)
  try {
    foreach ($part in @($launcher, $payload)) {
      $bytes = [System.IO.File]::ReadAllBytes($part)
      $fs.Write($bytes, 0, $bytes.Length)
    }
    $fs.Write($footer, 0, $footer.Length)
  } finally { $fs.Close() }

  Write-Host ("portable exe: {0} ({1:N1} MiB; payload {2:N1} MiB)" -f `
    $out, ((Get-Item $out).Length / 1MB), ($length / 1MB))
} finally {
  Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
}