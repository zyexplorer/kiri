# One-click Windows portable zip from an existing Linux/engine half.
#
# Reuses target/kiri-windows-x86_64 (kiri, kiri-init, kiri-linux.tar.gz, docker
# CLI, …), rebuilds bin\kiri.exe and kiri-win.exe with KIRI_BUNDLE_SHA256
# embedded, and writes dist\Kiri-<version>-windows-x86_64-portable.zip.
#
# Does NOT rebuild the Linux engine. For a full from-scratch bundle you need
# Ubuntu (+ Rust) as the default WSL distro and: cargo xtask windows
#
# Usage (from repo root, or double-click pack-windows-portable.cmd):
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\pack-windows-portable.ps1
#   scripts\pack-windows-portable.ps1 -OutDir D:\Kiri\dist
param(
  [string]$OutDir = "",
  [switch]$SkipKiriCli  # only rebuild kiri-win.exe (faster; keep existing bin\kiri.exe)
)

$ErrorActionPreference = "Stop"
$Root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$Out = Join-Path $Root "target\kiri-windows-x86_64"
$VsCandidates = @(
  "${env:ProgramFiles(x86)}\Microsoft Visual Studio\2022\BuildTools\Common7\Tools\VsDevCmd.bat",
  "${env:ProgramFiles(x86)}\Microsoft Visual Studio\2022\Community\Common7\Tools\VsDevCmd.bat",
  "${env:ProgramFiles}\Microsoft Visual Studio\2022\BuildTools\Common7\Tools\VsDevCmd.bat",
  "${env:ProgramFiles}\Microsoft Visual Studio\2022\Community\Common7\Tools\VsDevCmd.bat"
)
$Vs = $VsCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1

function Fail([string]$msg) {
  Write-Host ""
  Write-Host "ERROR: $msg" -ForegroundColor Red
  exit 1
}

# cargo cannot overwrite target\release\kiri-win.exe while that process is live.
function Stop-LockedKiriWin {
  $hits = @(Get-Process -Name kiri-win -ErrorAction SilentlyContinue)
  if (-not $hits.Count) { return }
  Write-Host ("Stopping {0} kiri-win process(es) locking the release build..." -f $hits.Count) -ForegroundColor Yellow
  $hits | Stop-Process -Force -ErrorAction SilentlyContinue
  Start-Sleep -Seconds 1
  $left = @(Get-Process -Name kiri-win -ErrorAction SilentlyContinue)
  if ($left.Count) {
    $pids = ($left | ForEach-Object { $_.Id }) -join ", "
    Fail ("kiri-win.exe is still running (PID {0}); quit Kiri from the tray, then re-run this script" -f $pids)
  }
}

function Get-Sha256([string]$path) {
  (Get-FileHash -Algorithm SHA256 -Path $path).Hash.ToLowerInvariant()
}

function Invoke-VsCargo([string]$cargoArgs) {
  if (-not $Vs) { Fail "Visual Studio Build Tools / VsDevCmd.bat not found" }
  $cmd = "call `"$Vs`" -arch=amd64 -host_arch=amd64 && set HOME= && cargo $cargoArgs"
  cmd /c $cmd
  if ($LASTEXITCODE -ne 0) { Fail "cargo failed: $cargoArgs (exit $LASTEXITCODE)" }
}

Write-Host "Repo:  $Root"
Write-Host "Bundle: $Out"

if (-not (Test-Path $Out)) {
  Fail "missing $Out`nRestore from a previous portable zip, or run: cargo xtask windows"
}

$required = @(
  "kiri", "kiri-init", "kiri-linux.tar.gz",
  "bin\docker.exe",
  "bin\cli-plugins\docker-compose.exe",
  "bin\cli-plugins\docker-buildx.exe"
)
foreach ($rel in $required) {
  $p = Join-Path $Out $rel
  if (-not (Test-Path $p)) { Fail "bundle incomplete, missing: $rel`nNeed a full target\kiri-windows-x86_64 (or unzip an old portable zip there)." }
}

# Workspace version (first version = line in root Cargo.toml)
$verLine = Select-String -Path (Join-Path $Root "Cargo.toml") -Pattern '^\s*version\s*=\s*"([^"]+)"' | Select-Object -First 1
if (-not $verLine) { Fail "could not read version from Cargo.toml" }
$Version = $verLine.Matches[0].Groups[1].Value
if (-not $OutDir) { $OutDir = Join-Path $Root "dist" }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

# Scripts beside kiri-win must match the hashes we embed
Copy-Item (Join-Path $Root "contrib\windows\kiri.ps1") $Out -Force
Copy-Item (Join-Path $Root "contrib\windows\setup-tools.ps1") $Out -Force
Copy-Item (Join-Path $Root "contrib\windows\kiri-admin.ps1") $Out -Force
Copy-Item (Join-Path $Root "contrib\wsl\install.sh") (Join-Path $Out "install.sh") -Force
Copy-Item (Join-Path $Root "LICENSE") (Join-Path $Out "LICENSE") -Force -ErrorAction SilentlyContinue
Copy-Item (Join-Path $Root "NOTICE") (Join-Path $Out "NOTICE") -Force -ErrorAction SilentlyContinue

Push-Location $Root
try {
  if (-not $SkipKiriCli) {
    Write-Host "`n=== build bin\kiri.exe ===" -ForegroundColor Cyan
    Invoke-VsCargo "build -p kiri --release --bin kiri"
    Copy-Item (Join-Path $Root "target\release\kiri.exe") (Join-Path $Out "bin\kiri.exe") -Force
  } elseif (-not (Test-Path (Join-Path $Out "bin\kiri.exe"))) {
    Fail "bin\kiri.exe missing; omit -SkipKiriCli to build it"
  }

  $hashed = @(
    "kiri", "kiri-init", "kiri-linux.tar.gz", "install.sh",
    "kiri.ps1", "setup-tools.ps1", "kiri-admin.ps1",
    "bin/docker.exe", "bin/kiri.exe",
    "bin/cli-plugins/docker-compose.exe", "bin/cli-plugins/docker-buildx.exe"
  )
  $parts = foreach ($f in $hashed) {
    $p = Join-Path $Out ($f -replace "/", "\")
    if (-not (Test-Path $p)) { Fail "missing hashed file: $f" }
    "{0}={1}" -f $f, (Get-Sha256 $p)
  }
  $manifest = $parts -join ";"
  $manifestPath = Join-Path $Root "target\bundle-manifest.txt"
  New-Item -ItemType Directory -Force -Path (Join-Path $Root "target") | Out-Null
  Set-Content -Path $manifestPath -Value $manifest -NoNewline -Encoding ascii
  Write-Host "Bundle manifest: $($parts.Count) files -> $manifestPath"

  Write-Host "`n=== build kiri-win.exe (with KIRI_BUNDLE_SHA256) ===" -ForegroundColor Cyan
  Stop-LockedKiriWin
  Invoke-VsCargo "clean -p kiri-win"
  # Quote the set value: manifest contains ';' which cmd treats as a separator.
  $buildCmd = "call `"$Vs`" -arch=amd64 -host_arch=amd64 && set HOME= && set `"KIRI_BUNDLE_SHA256=$manifest`" && cargo build -p kiri-win --release"
  cmd /c $buildCmd
  if ($LASTEXITCODE -ne 0) { Fail "kiri-win build failed (exit $LASTEXITCODE)" }

  Copy-Item (Join-Path $Root "target\release\kiri-win.exe") (Join-Path $Out "kiri-win.exe") -Force

  $adminHash = Get-Sha256 (Join-Path $Out "kiri-admin.ps1")
  $exeText = [Text.Encoding]::UTF8.GetString([IO.File]::ReadAllBytes((Join-Path $Out "kiri-win.exe")))
  if (-not $exeText.Contains("kiri-admin.ps1=$adminHash")) {
    Fail "kiri-admin.ps1 checksum was NOT embedded in kiri-win.exe (do not ship this build)"
  }
  Write-Host "Checksum embed OK (kiri-admin.ps1)" -ForegroundColor Green
} finally {
  Pop-Location
}

$readme = @"
Kiri $Version Windows 便携安装包（x86_64）
=====================================

这不是「解压即跑、不写系统」的纯绿色软件，而是离线安装包：
解压后运行一次，会安装到当前用户目录，并在 WSL2 里装上 Linux 引擎。

安装步骤
--------
1. 解压本 zip 到任意目录
2. 双击运行 kiri-win.exe（必须从解压目录运行，不要只拷单个 exe）
3. 按窗口里的首次设置向导完成

验证分发时请用本 zip 解压出的全新目录，不要用 %LOCALAPPDATA%\Kiri 里可能残留的旧 exe。
"@
[System.IO.File]::WriteAllText((Join-Path $Out "使用说明.txt"), $readme, [Text.UTF8Encoding]::new($false))

$zipName = "Kiri-$Version-windows-x86_64-portable.zip"
$zip = Join-Path $OutDir $zipName
if (Test-Path $zip) { Remove-Item $zip -Force }

Write-Host "`n=== zip ===" -ForegroundColor Cyan
Push-Location $Out
try {
  Compress-Archive -Path * -DestinationPath $zip -CompressionLevel Optimal -Force
} finally {
  Pop-Location
}

$i = Get-Item $zip
$sha = (Get-Sha256 $zip)
Write-Host ""
Write-Host "OK: $($i.FullName)" -ForegroundColor Green
Write-Host ("     {0:N0} bytes ({1:N1} MB)" -f $i.Length, ($i.Length / 1MB))
Write-Host "     SHA256 $sha"
Write-Host "     $($i.LastWriteTime)"
