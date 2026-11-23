# Runs a .cmd/.bat for Kiri's project terminal: strips a UTF-8 BOM (cmd would
# otherwise glue EF BB BF onto `@echo off`), keeps the temp copy beside the
# original so `cd /d "%~dp0"` still finds sibling scripts, then invokes via
# ComSpec on the same console (needed for `docker compose up` TTY).
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [string] $Script,
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]] $ScriptArgs
)

$ErrorActionPreference = "Stop"
$p = (Resolve-Path -LiteralPath $Script).Path
$leaf = Split-Path -Leaf $p
$argLine = if ($ScriptArgs -and $ScriptArgs.Count) { " " + ($ScriptArgs -join " ") } else { "" }
# Visible short label in the project terminal (not the long wrapper).
Write-Host ("{0}{1}" -f $leaf, $argLine) -ForegroundColor Cyan

$b = [IO.File]::ReadAllBytes($p)
if ($b.Length -ge 3 -and $b[0] -eq 0xEF -and $b[1] -eq 0xBB -and $b[2] -eq 0xBF) {
    $b = $b[3..($b.Length - 1)]
}
$dir = Split-Path -Parent $p
$t = Join-Path $dir (".kiri-" + [Guid]::NewGuid().ToString("n") + [IO.Path]::GetExtension($p))
[IO.File]::WriteAllBytes($t, $b)
try {
    if ($argLine) {
        & $env:ComSpec /c ("chcp 65001>nul & call `"" + $t + "`"" + $argLine)
    } else {
        & $env:ComSpec /c ("chcp 65001>nul & call `"" + $t + "`"")
    }
    exit $LASTEXITCODE
} finally {
    Remove-Item -LiteralPath $t -Force -ErrorAction SilentlyContinue
}
