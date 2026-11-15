# Builds AIFrames in release mode, installs it for the current user and registers the .aifr file type.
#
#   scripts\install.ps1                      # build + install + register
#   scripts\install.ps1 -OpenSettings        # ...and open Settings > Default apps to confirm the default
#   scripts\install.ps1 -Uninstall           # remove registration, shortcut and installed files
param(
    [switch]$OpenSettings,
    [switch]$Uninstall,
    [switch]$NoBuild
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$dest = Join-Path $env:LOCALAPPDATA 'Programs\AIFrames'
$exe = Join-Path $dest 'aiframes.exe'
$lnk = Join-Path ([Environment]::GetFolderPath('Programs')) 'AIFrames.lnk'
Get-Process aiframes -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe } | Stop-Process -Force

if ($Uninstall) {
    if (Test-Path $exe) { Start-Process $exe -ArgumentList '--unregister' -Wait }
    Remove-Item $lnk -ErrorAction SilentlyContinue
    Remove-Item $dest -Recurse -Force -ErrorAction SilentlyContinue
    Write-Output 'AIFrames uninstalled.'
    return
}

if (-not $NoBuild) {
    Push-Location $root
    # cargo reports progress on stderr, which Windows PowerShell treats as an error record; go through cmd.
    try { cmd /c 'cargo build --release -p aiframes-app 2>&1'; if ($LASTEXITCODE) { throw 'cargo build failed' } } finally { Pop-Location }
}
New-Item -ItemType Directory -Force $dest | Out-Null
Copy-Item (Join-Path $root 'target\release\aiframes.exe') $exe -Force

$ws = New-Object -ComObject WScript.Shell
$s = $ws.CreateShortcut($lnk)
$s.TargetPath = $exe
$s.Save()

$regArgs = @('--register')
if (-not $OpenSettings) { $regArgs += '--quiet' }
Start-Process $exe -ArgumentList $regArgs -Wait
$cmd = (Get-ItemProperty 'HKCU:\Software\Classes\AIFrames.Project\shell\open\command' -ErrorAction SilentlyContinue).'(default)'
if (-not $cmd) { throw 'AIFrames registration did not complete' }

Write-Output "Installed AIFrames: $exe"
Write-Output "  registered for .aifr: $cmd"
Write-Output "  Claude Code: claude mcp add aiframes -- `"$exe`" mcp"
Write-Output ''
Write-Output 'To make AIFrames the default for .aifr: right-click a file > Open with > Choose another app > AIFrames > Always.'
