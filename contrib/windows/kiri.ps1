# Kiri on Windows: installs the Linux engine into a WSL2 distro, and
# kiri-win.exe, which serves the Docker API on \\.\pipe\kiri for the Windows
# docker CLI (as the `kiri` context) and carries it into WSL. It brings the
# docker CLI, Compose and buildx too, as Kiri.app does, so no other Docker
# is needed. Per user: no admin, no service, no change to WSL's or Windows'
# network settings.
#   powershell -ExecutionPolicy Bypass -File kiri.ps1 [-Distro Ubuntu] [-Bundle C:\path\to\kiri-windows-x86_64 (or -aarch64)] [-Default]
#   powershell -ExecutionPolicy Bypass -File kiri.ps1 -Uninstall [-Distro Ubuntu]
#   powershell -ExecutionPolicy Bypass -File kiri.ps1 -Update -Msi <staged .msi> [-WaitFor <pid>]
# The MSI (installer/windows) lays the same files in the chosen install
# folder (default %LOCALAPPDATA%\Kiri) and runs `-Uninstall -IfSetUp` when
# removed: nothing happens unless first-run setup completed, and the distro
# setup chose is taken from setup.json. kiri-win.exe runs `-Update`
# (update.rs) when it quits for an update it downloaded: once that process
# is gone the staged MSI upgrades Kiri silently and the new Kiri opens.
# Kiri becomes docker's default engine (the current context, and the builder
# `docker build` uses) unless Docker Desktop is installed; -Default makes it
# the default anyway.
param(
  [string]$Distro = "Ubuntu",
  [string]$Bundle = $PSScriptRoot,
  [switch]$Default,
  [switch]$Uninstall,
  [switch]$IfSetUp,
  [switch]$Update,
  [string]$Msi,
  [int]$WaitFor
)
$ErrorActionPreference = "Stop"

# -Update: waits for the kiri-win that downloaded the MSI to quit (its files
# are in use until then), upgrades silently (per user, no prompt; 3010 is
# "done, restart Windows later"), clears the stage when that went through,
# and opens the new Kiri. Each step goes to helper.log beside the MSI, which
# stays, with msiexec's install.log, when the upgrade failed.
if ($Update) {
  if (-not $Msi) { throw "-Update needs -Msi" }
  $stage = Split-Path $Msi -Parent
  $helperLog = Join-Path $stage "helper.log"
  function Say($m) { Add-Content -LiteralPath $helperLog -Value "$(Get-Date -Format s) $m" }
  try {
    Say "waiting for kiri-win $WaitFor"
    while ($WaitFor -and (Get-Process -Id $WaitFor -ErrorAction SilentlyContinue)) { Start-Sleep -Milliseconds 200 }
    Say "installing $Msi"
    $p = Start-Process -FilePath (Join-Path $env:SystemRoot "System32\msiexec.exe") -ArgumentList "/i", "`"$Msi`"", "/qn", "/norestart", "/L*V", "`"$(Join-Path $stage 'install.log')`"" -Wait -PassThru
    Say "msiexec exit $($p.ExitCode)"
    if ($p.ExitCode -eq 0 -or $p.ExitCode -eq 3010) { Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue }
    $regDir = (Get-ItemProperty -Path "HKCU:\Software\Kiri" -ErrorAction SilentlyContinue).InstallDir
    $launch = if ($regDir) { Join-Path $regDir.TrimEnd('\') "kiri-win.exe" } elseif (Test-Path (Join-Path $PSScriptRoot "kiri-win.exe")) { Join-Path $PSScriptRoot "kiri-win.exe" } else { Join-Path $env:LOCALAPPDATA "Kiri\kiri-win.exe" }
    Start-Process -FilePath $launch
  } catch { Say "failed: $_" }
  return
}

# Install root: this script's folder (MSI/portable), else registry, else LocalAppData.
$regDir = (Get-ItemProperty -Path "HKCU:\Software\Kiri" -ErrorAction SilentlyContinue).InstallDir
if (Test-Path (Join-Path $PSScriptRoot "kiri-win.exe")) { $dir = $PSScriptRoot }
elseif ($regDir) { $dir = $regDir.TrimEnd('\') }
else { $dir = Join-Path $env:LOCALAPPDATA "Kiri" }
$setupJson = Join-Path $dir "setup.json"
if ($IfSetUp -and -not (Test-Path $setupJson)) { return }
if (-not $PSBoundParameters.ContainsKey("Distro") -and (Test-Path $setupJson)) {
  $saved = (Get-Content $setupJson -Raw | ConvertFrom-Json).distro
  if ($saved) { $Distro = $saved }
}
if (-not (Get-Command wsl.exe -ErrorAction SilentlyContinue)) { throw "WSL is not installed: run 'wsl --install' first" }
$exe = Join-Path $dir "kiri-win.exe"
$bin = Join-Path $dir "bin"
$plugins = Join-Path $bin "cli-plugins"
$link = Join-Path ([Environment]::GetFolderPath("Startup")) "Kiri.lnk"
$dockerConfig = Join-Path $env:USERPROFILE ".docker\config.json"
$elevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)

# Kiri's bin on this user's PATH: last, so a docker CLI already installed
# (Docker Desktop's is on the system PATH, which comes first) keeps winning.
# Through the registry, not [Environment]::SetEnvironmentVariable, which
# would write REG_SZ and turn %USERPROFILE% and the like in the user's PATH
# into fixed paths; the value keeps its kind (REG_EXPAND_SZ) and its text.
function Set-KiriPath([bool]$On) {
  $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey("Environment", $true)
  try {
    $kind = [Microsoft.Win32.RegistryValueKind]::ExpandString
    $raw = ""
    if ($key.GetValueNames() -contains "Path") {
      $kind = $key.GetValueKind("Path")
      $raw = [string]$key.GetValue("Path", "", [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    }
    $parts = @(($raw -split ";") | Where-Object { $_ -and $_ -ne $bin -and ([Environment]::ExpandEnvironmentVariables($_) -ne $bin) })
    if ($On) { $parts += $bin }
    if ($parts.Count) { $key.SetValue("Path", ($parts -join ";"), $kind) } else { $key.DeleteValue("Path", $false) }
  } finally { $key.Close() }
  # What SetEnvironmentVariable would have done: tell Explorer and the rest
  # that the environment changed, so new terminals see the new PATH.
  if (-not ("Kiri.Env" -as [type])) {
    Add-Type -Namespace Kiri -Name Env -MemberDefinition '[DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr SendMessageTimeout(IntPtr hWnd, uint msg, UIntPtr wParam, string lParam, uint flags, uint timeout, out UIntPtr result);'
  }
  $r = [UIntPtr]::Zero
  [Kiri.Env]::SendMessageTimeout([IntPtr]0xffff, 0x1A, [UIntPtr]::Zero, "Environment", 2, 5000, [ref]$r) | Out-Null
  $env:Path = (@(($env:Path -split ";") | Where-Object { $_ -and $_ -ne $bin }) + $(if ($On) { $bin } else { @() })) -join ";"
}

# Compose and buildx through the docker config's cliPluginsExtraDirs. The
# config also holds registry logins: it is merged, never replaced, and
# backed up once before Kiri first changes it.
function Set-KiriPlugins([bool]$On) {
  $cfg = if (Test-Path $dockerConfig) { Get-Content $dockerConfig -Raw | ConvertFrom-Json } else { [pscustomobject]@{} }
  $dirs = @($cfg.cliPluginsExtraDirs | Where-Object { $_ -and $_ -ne $plugins })
  if ($On) { $dirs += $plugins }
  if ($null -eq $cfg.cliPluginsExtraDirs -and -not $On) { return }
  if ((Test-Path $dockerConfig) -and -not (Test-Path "$dockerConfig.before-kiri")) { Copy-Item $dockerConfig "$dockerConfig.before-kiri" }
  if ($dirs.Count) { $cfg | Add-Member -NotePropertyName cliPluginsExtraDirs -NotePropertyValue $dirs -Force }
  else { $cfg.PSObject.Properties.Remove("cliPluginsExtraDirs") }
  New-Item -ItemType Directory -Force (Split-Path $dockerConfig) | Out-Null
  # UTF-8 without a BOM: Set-Content -Encoding utf8 writes one in Windows
  # PowerShell, and docker then rejects the whole file, logins included.
  [IO.File]::WriteAllText($dockerConfig, ($cfg | ConvertTo-Json -Depth 32), (New-Object Text.UTF8Encoding $false))
  $dockerExe = Join-Path $bin "docker.exe"
  if (-not (Test-Path $dockerExe)) { $dockerExe = (Get-Command docker.exe -ErrorAction SilentlyContinue).Source }
  if ($dockerExe) {
    $prev = $ErrorActionPreference; $ErrorActionPreference = "Continue"
    $out = (& $dockerExe context ls 2>&1 | ForEach-Object { "$_" }) -join "`n"
    $ErrorActionPreference = $prev
    if ($out -match "Error parsing config") { throw "docker cannot read $dockerConfig after Kiri changed it (the backup is $dockerConfig.before-kiri): $out" }
  }
}

# A WSL distro's folder (the registry has what `wsl --list` prints as UTF-16).
function Get-WslBasePath([string]$Name) {
  foreach ($k in Get-ChildItem "HKCU:\Software\Microsoft\Windows\CurrentVersion\Lxss" -ErrorAction SilentlyContinue) {
    $p = Get-ItemProperty $k.PSPath -ErrorAction SilentlyContinue
    if ($p.DistributionName -eq $Name) { return ([string]$p.BasePath) -replace '^\\\\\?\\', '' }
  }
}

Get-Process kiri-win -ErrorAction SilentlyContinue | Stop-Process -Force
if ($Uninstall) {
  if ($IfSetUp) {
    # From the MSI: %LOCALAPPDATA%\Kiri is about to go, so the log lives in %TEMP%.
    Start-Transcript -Path (Join-Path $env:TEMP "kiri-uninstall.log") -Force | Out-Null
    "PowerShell $PSHOME ($([IntPtr]::Size * 8)-bit), elevated: $elevated, interactive: $([Environment]::UserInteractive), distro: $Distro"
    if ($elevated) {
      Write-Host "Kiri: not undoing setup from an elevated session (it would act on the administrator's WSL and docker settings, not yours): uninstall from a non-elevated session"
      Stop-Transcript | Out-Null
      exit 1
    }
  }
  # Each step on its own: one that fails (a docker CLI that is gone, WSL not
  # answering) must not keep the next from running.
  $ErrorActionPreference = "Continue"
  $problems = @()
  function Step([string]$What, [scriptblock]$Do) {
    try { & $Do; Write-Host "- $What" } catch { Write-Host "- $What : FAILED: $_"; $script:problems += $What }
  }
  # Uninstalling stops everything Kiri runs: kiri-win is gone (above); any
  # kiri or docker command still running from Kiri's folder (`kiri share`, a
  # docker compose up) goes too, before the engine in WSL stops (below).
  Step "Kiri commands still running stopped" {
    Get-Process kiri, docker, docker-compose, docker-buildx -ErrorAction SilentlyContinue |
      Where-Object { $_.Path -and $_.Path.StartsWith($dir, [StringComparison]::OrdinalIgnoreCase) } |
      Stop-Process -Force
  }
  # Before the builder goes: docker build's own builder back as it was
  # before Kiri changed it (crates/kiri/src/buildwith.rs), from kiri-win.
  Step "docker build given back its own builder" {
    if (Test-Path $exe) { $p = Start-Process $exe -ArgumentList "--uninstall-builds" -Wait -PassThru; if ($p.ExitCode -ne 0) { throw "kiri-win exited $($p.ExitCode)" } }
  }
  # The cluster's `kiri` kubectl context out of ~\.kube\config, the rest
  # of the file kept (crates/kiri/src/kubeconfig.rs), from kiri-win.
  Step "kubectl context removed" {
    if (Test-Path $exe) { $p = Start-Process $exe -ArgumentList "--uninstall-kubeconfig" -Wait -PassThru; if ($p.ExitCode -ne 0) { throw "kiri-win exited $($p.ExitCode)" } }
  }
  Step "docker context and builder removed" {
    $docker = Join-Path $bin "docker.exe"
    if (-not (Test-Path $docker)) { $docker = (Get-Command docker.exe -ErrorAction SilentlyContinue).Source }
    if ($docker) {
      if ((& $docker context show 2>$null) -eq "kiri") { & $docker context use default 2>$null | Out-Null }
      & $docker buildx rm kiri-build 2>$null | Out-Null
      & $docker context rm -f kiri 2>$null | Out-Null
    }
    if ([Environment]::GetEnvironmentVariable("BUILDX_BUILDER", "User") -eq "kiri-build") { [Environment]::SetEnvironmentVariable("BUILDX_BUILDER", $null, "User") }
  }
  Step "Compose and buildx taken out of $dockerConfig" { Set-KiriPlugins $false }
  Step "$bin taken off PATH" { Set-KiriPath $false }
  # Both ways it was ever set: the Startup-folder shortcut, and the Run
  # value setup's "Start Kiri when I sign in" writes (setup.rs).
  Step "start at sign-in removed" {
    Remove-Item $link -Force -ErrorAction SilentlyContinue
    Remove-ItemProperty "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" -Name "Kiri" -ErrorAction SilentlyContinue
  }
  # The engine in WSL. A distro setup imported under %LOCALAPPDATA%\Kiri\wsl
  # (Kiri Linux as `kiri`, or Ubuntu when `wsl --install` was not available)
  # keeps its disk, with the images and containers, in the folder the MSI
  # deletes next: it is unregistered here, so WSL does not keep a distro
  # whose disk is gone. A distro of the user's own only loses the engine.
  $base = Get-WslBasePath $Distro
  $kiriOwned = $base -and $base.TrimEnd("\").StartsWith($dir.TrimEnd("\") + "\", [StringComparison]::OrdinalIgnoreCase)
  if ($Distro -eq "kiri" -or $kiriOwned) {
    Step "WSL distro '$Distro' stopped" { wsl.exe --terminate $Distro 2>$null | Out-Null }
    if ($base) {
      Write-Host "The WSL distro '$Distro' is Kiri's (its disk is $base\ext4.vhdx): it is unregistered, and the images, containers and volumes on that disk are deleted with it."
      Step "WSL distro '$Distro' unregistered" { wsl.exe --unregister $Distro 2>$null | Out-Null; if ($LASTEXITCODE -ne 0) { throw "wsl --unregister exited $LASTEXITCODE" } }
    } else {
      Write-Host "The WSL distro '$Distro' is not registered (already gone)."
    }
  } else {
    Step "engine removed from $Distro (systemd unit and /usr/local/bin/kiri*)" {
      wsl.exe -d $Distro -u root sh -c "systemctl disable --now kiri 2>/dev/null; rm -f /etc/systemd/system/kiri.service /usr/local/bin/kiri /usr/local/bin/kiri-init"
      if ($LASTEXITCODE -ne 0) { throw "wsl exited $LASTEXITCODE" }
    }
    Write-Host "Its images, containers and volumes stay in /var/lib/kiri and /var/lib/kiri-daemon in $Distro."
  }
  Step "$dir removed" { Remove-Item $dir -Recurse -Force -ErrorAction SilentlyContinue }
  # Last, since it asks for approval (Windows shows a UAC prompt): the DNS
  # rule for <name>.kiri.local, if it was turned on. Declining it, or a
  # session with no desktop to ask on, leaves only this rule behind.
  $nrpt = Get-DnsClientNrptRule -ErrorAction SilentlyContinue | Where-Object { $_.Namespace -contains ".kiri.local" }
  if ($nrpt) {
    if ([Environment]::UserInteractive) {
      Step "DNS rule for *.kiri.local removed (asked for approval)" {
        $p = Start-Process "$PSHOME\powershell.exe" -Verb RunAs -Wait -PassThru -WindowStyle Hidden -ArgumentList "-NoProfile", "-Command", "Get-DnsClientNrptRule | Where-Object { `$_.Namespace -contains '.kiri.local' } | Remove-DnsClientNrptRule -Force; Clear-DnsClientCache"
        if ($p.ExitCode -ne 0) { throw "exit code $($p.ExitCode)" }
      }
    } else {
      Write-Host "- DNS rule for *.kiri.local left in place (no desktop to ask for approval on)"
    }
    if (Get-DnsClientNrptRule -ErrorAction SilentlyContinue | Where-Object { $_.Namespace -contains ".kiri.local" }) {
      Write-Host "  To remove it, in an elevated PowerShell:  Get-DnsClientNrptRule | Where-Object { `$_.Namespace -contains '.kiri.local' } | Remove-DnsClientNrptRule -Force"
    }
  }
  if ($problems.Count) { Write-Host "Kiri is removed, with $($problems.Count) step(s) that failed (above)." } else { Write-Host "Kiri is removed." }
  if ($IfSetUp) { Stop-Transcript | Out-Null }
  if ($problems.Count) { exit 1 }
  return
}

$wslPath = (wsl.exe -d $Distro wslpath -a ($Bundle -replace '\\', '/')).Trim()
wsl.exe -d $Distro -u root sh "$wslPath/install.sh"
if ($LASTEXITCODE -ne 0) { throw "install.sh failed in $Distro" }

New-Item -ItemType Directory -Force $dir, $plugins | Out-Null
Copy-Item (Join-Path $Bundle "kiri-win.exe") $exe -Force
Copy-Item (Join-Path $Bundle "bin\docker.exe"), (Join-Path $Bundle "bin\kiri.exe") $bin -Force
Copy-Item (Join-Path $Bundle "bin\cli-plugins\*.exe") $plugins -Force
Set-KiriPath $true
Set-KiriPlugins $true

# Started at login, for this user only, in the tray.
$shell = New-Object -ComObject WScript.Shell
$s = $shell.CreateShortcut($link)
$s.TargetPath = $exe
$s.Arguments = "--background --distro $Distro"
$s.WorkingDirectory = $dir
$s.Save()
Start-Process $exe -ArgumentList "--distro", $Distro

$docker = (Get-Command docker.exe).Source
& $docker context rm -f kiri 2>$null | Out-Null
& $docker context create kiri --description "Kiri (WSL2)" --docker "host=npipe:////./pipe/kiri" | Out-Null
# docker build: BuildKit in a Kiri container, loading what it builds into
# Kiri (kiri-win makes it too, if this finds the engine not up yet).
& $docker buildx inspect kiri-build 2>$null | Out-Null
if ($LASTEXITCODE -ne 0) { & $docker buildx create --name kiri-build --driver docker-container --driver-opt default-load=true kiri 2>$null | Out-Null }

# The default engine. `docker build` picks its builder by context name:
# kiri-win makes the `kiri` one a copy of Kiri's, and which builder plain
# `docker build` uses beside Docker Desktop is asked in its window
# (crates/kiri/src/buildwith.rs).
$desktop = Test-Path (Join-Path $env:ProgramFiles "Docker\Docker\Docker Desktop.exe")
if ($Default -or -not $desktop) {
  & $docker context use kiri | Out-Null
  Write-Host "Kiri is docker's default engine: docker run --rm alpine echo hello"
} else {
  Write-Host "Docker Desktop is installed and stays the default: docker --context kiri run --rm alpine echo hello"
  Write-Host "(to make Kiri the default, run kiri.ps1 -Default; docker buildx build --builder kiri-build builds on Kiri)"
}
Write-Host "(docker: $docker; open a new terminal for PATH to apply)"
