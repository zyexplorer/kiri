# Build the Windows MSI + update feed for a GitHub Release on zyexplorer/kiri.
#
# Prerequisites: target\kiri-windows-x86_64 bundle (cargo xtask windows), WiX.
#
# Then create a GitHub Release tagged v<version> (e.g. v0.3.0-zy) and upload:
#   - target\Kiri-<version>-x86_64.msi
#   - target\latest-windows-x86_64.json
#
# Clients (personal build) read:
#   https://github.com/zyexplorer/kiri/releases/latest/download/latest-windows-x86_64.json
#
# Usage (from repo root):
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\release-github-windows.ps1
param(
  [switch]$SkipWindowsBundle  # reuse existing target\kiri-windows-x86_64
)

$ErrorActionPreference = "Stop"
$Root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
Set-Location $Root

$verLine = Select-String -Path (Join-Path $Root "Cargo.toml") -Pattern '^\s*version\s*=\s*"([^"]+)"' | Select-Object -First 1
if (-not $verLine) { throw "could not read version from Cargo.toml" }
$Version = $verLine.Matches[0].Groups[1].Value
$Tag = "v$Version"
$Base = "https://github.com/zyexplorer/kiri/releases/download/$Tag"

Write-Host "Version: $Version"
Write-Host "Tag:     $Tag"
Write-Host "Assets:  $Base/..."

if (-not $SkipWindowsBundle) {
  Write-Host "`n=== cargo xtask windows ===" -ForegroundColor Cyan
  cargo xtask windows
  if ($LASTEXITCODE -ne 0) { throw "cargo xtask windows failed" }
} elseif (-not (Test-Path (Join-Path $Root "target\kiri-windows-x86_64\kiri-win.exe"))) {
  throw "missing target\kiri-windows-x86_64; omit -SkipWindowsBundle or run cargo xtask windows"
}

# msi-pack reuses target\kiri-windows-* (avoids rebuilding the Linux engine in WSL).
Write-Host "`n=== cargo xtask msi-pack ===" -ForegroundColor Cyan
$env:KIRI_RELEASE_BASE = $Base
if (Test-Path (Join-Path $Root "docs\releases\$Version.md")) {
  $env:KIRI_RELEASE_NOTES = (Resolve-Path (Join-Path $Root "docs\releases\$Version.md")).Path
  Write-Host "Notes: $env:KIRI_RELEASE_NOTES"
}
cargo xtask msi-pack
if ($LASTEXITCODE -ne 0) { throw "cargo xtask msi-pack failed" }

$msi = Join-Path $Root "target\Kiri-$Version-x86_64.msi"
$feed = Join-Path $Root "target\latest-windows-x86_64.json"
if (-not (Test-Path $msi)) { throw "missing $msi" }
if (-not (Test-Path $feed)) { throw "missing $feed" }

Write-Host ""
Write-Host "OK. Create GitHub Release $Tag on zyexplorer/kiri and upload:" -ForegroundColor Green
Write-Host "  $msi"
Write-Host "  $feed"
Write-Host ""
Write-Host "Example:"
Write-Host "  git tag $Tag && git push origin $Tag"
Write-Host "  gh release create $Tag `"$msi`" `"$feed`" --title `"Kiri $Version`" --notes `"Personal Windows build.`""
