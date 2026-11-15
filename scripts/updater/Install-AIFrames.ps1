# Installs AIFrames from its latest GitHub release and keeps it updated automatically.
#
# One line in PowerShell (no admin needed):
#   irm https://github.com/RezAIT00/AIFrames/releases/latest/download/Install-AIFrames.ps1 | iex
#
# Or download this file and run:   powershell -ExecutionPolicy Bypass -File Install-AIFrames.ps1 [-Uninstall]
param([switch]$Uninstall, [string]$Repo = 'RezAIT00/AIFrames')
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$home_ = Join-Path $env:LOCALAPPDATA 'AIFrames-Updater'
$dest = Join-Path $env:LOCALAPPDATA 'Programs\AIFrames'
$exe = Join-Path $dest 'aiframes.exe'
$lnk = Join-Path ([Environment]::GetFolderPath('Programs')) 'AIFrames.lnk'
$taskName = 'AIFrames Auto-Update'

if ($Uninstall) {
    Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
    Get-Process aiframes -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe } | Stop-Process -Force
    if (Test-Path $exe) { Start-Process $exe -ArgumentList '--unregister' -Wait }
    Remove-Item $lnk, $home_, $dest -Recurse -Force -ErrorAction SilentlyContinue
    Write-Host 'AIFrames and its updater were removed.'
    return
}

New-Item -ItemType Directory -Force $home_ | Out-Null
foreach ($f in 'Update-AIFrames.ps1', 'run-hidden.vbs') {
    Invoke-WebRequest -Uri "https://github.com/$Repo/releases/latest/download/$f" -Headers @{ 'User-Agent' = 'AIFrames-Updater' } -OutFile (Join-Path $home_ $f) -UseBasicParsing
}
$updater = Join-Path $home_ 'Update-AIFrames.ps1'
$vbs = Join-Path $home_ 'run-hidden.vbs'

Write-Host "Installing AIFrames from github.com/$Repo ..."
& powershell -NoProfile -ExecutionPolicy Bypass -File $updater -Repo $Repo
if (-not (Test-Path $exe)) { throw "Install failed; see $home_\update.log" }

# Start menu shortcut goes through the updater so launching always picks up the newest release.
$ws = New-Object -ComObject WScript.Shell
$s = $ws.CreateShortcut($lnk)
$s.TargetPath = 'wscript.exe'
$s.Arguments = "`"$vbs`" `"$updater`" -Launch"
$s.IconLocation = "$exe,0"
$s.Save()

# Background check at logon and every 15 minutes.
$action = New-ScheduledTaskAction -Execute 'wscript.exe' -Argument "`"$vbs`" `"$updater`""
$triggers = @(
    (New-ScheduledTaskTrigger -AtLogOn -User "$env:USERDOMAIN\$env:USERNAME"),
    (New-ScheduledTaskTrigger -Once -At (Get-Date).AddMinutes(1) -RepetitionInterval (New-TimeSpan -Minutes 15))
)
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable -MultipleInstances IgnoreNew -ExecutionTimeLimit (New-TimeSpan -Minutes 10)
Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $triggers -Settings $settings -Description "Keeps AIFrames updated from github.com/$Repo releases." -Force | Out-Null

if (-not (Get-Command ffmpeg -ErrorAction SilentlyContinue)) {
    Write-Host 'AIFrames needs ffmpeg; installing it with winget...'
    try { winget install --id Gyan.FFmpeg -e --accept-source-agreements --accept-package-agreements } catch { Write-Host 'Could not install ffmpeg automatically; install it manually (winget install Gyan.FFmpeg).' }
}

# No installed.json when an existing aiframes.exe was kept (e.g. it was open, so the updater waits).
$v = try { (Get-Content (Join-Path $home_ 'installed.json') -Raw | ConvertFrom-Json).version + ' ' } catch { '' }
Write-Host ''
Write-Host "AIFrames ${v}installed. It updates itself from GitHub releases at logon, every 15 minutes, and when started from the Start menu."
