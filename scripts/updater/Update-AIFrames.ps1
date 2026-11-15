# Installs the newest AIFrames release from GitHub.
#
#   Update-AIFrames.ps1             # update if a newer release exists (skips while AIFrames is open)
#   Update-AIFrames.ps1 -Launch     # update (if possible), then start AIFrames with any extra arguments
#
# Install-AIFrames.ps1 copies this script to %LOCALAPPDATA%\AIFrames-Updater and runs it at logon,
# every 15 minutes, and whenever AIFrames is started from the Start menu.
# PositionalBinding off: extra arguments go to $AppArgs (otherwise the first one would bind to -Repo).
[CmdletBinding(PositionalBinding = $false)]
param(
    [switch]$Launch,
    [string]$Repo = 'RezAIT00/AIFrames',
    [Parameter(ValueFromRemainingArguments = $true)] [string[]]$AppArgs
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$dest = Join-Path $env:LOCALAPPDATA 'Programs\AIFrames'
$exe = Join-Path $dest 'aiframes.exe'
$home_ = Join-Path $env:LOCALAPPDATA 'AIFrames-Updater'
$stateFile = Join-Path $home_ 'installed.json'
$logFile = Join-Path $home_ 'update.log'
New-Item -ItemType Directory -Force $dest, $home_ | Out-Null
$headers = @{ 'User-Agent' = 'AIFrames-Updater'; 'Accept' = 'application/vnd.github+json' }

function Log($msg) {
    try {
        Add-Content -Path $logFile -Value ('{0:yyyy-MM-dd HH:mm:ss}  {1}' -f (Get-Date), $msg)
        # Read the tail completely before rewriting: piping Get-Content straight into Set-Content on the same file fails (file in use).
        if ((Get-Item $logFile).Length -gt 256KB) { (Get-Content $logFile -Tail 500) | Set-Content $logFile }
    } catch { } # logging must never stop an update or a launch
}
function Asset($release, $name) { $release.assets | Where-Object { $_.name -eq $name } | Select-Object -First 1 }
function Download($asset, $path) { Invoke-WebRequest -Uri $asset.browser_download_url -Headers @{ 'User-Agent' = 'AIFrames-Updater' } -OutFile $path -UseBasicParsing }

function Update-App {
    try {
        $r = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -Headers $headers -TimeoutSec 15
    } catch {
        # offline, rate-limited or no release yet: try again next time (on a first install, say why)
        if (-not (Test-Path $exe)) { Log "Could not read the latest release of ${Repo}: $($_.Exception.Message)" }
        return
    }
    $exeAsset = Asset $r 'aiframes.exe'
    $sumAsset = Asset $r 'aiframes.exe.sha256'
    if (-not $exeAsset -or -not $sumAsset) { Log "Release $($r.tag_name) has no aiframes.exe / aiframes.exe.sha256; skipped."; return }

    $sumFile = Join-Path $home_ 'latest.sha256'
    Download $sumAsset $sumFile
    $expected = ((Get-Content $sumFile -Raw).Trim() -split '\s+')[0].ToUpper()
    # A damaged state file means "reinstall", not an error on every run from now on.
    $installed = if (Test-Path $stateFile) { try { Get-Content $stateFile -Raw | ConvertFrom-Json } catch { $null } } else { $null }
    if ($installed -and $installed.sha256 -eq $expected -and (Test-Path $exe)) { return }

    if (Get-Process aiframes -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe }) {
        Log "Update $($r.tag_name) available but AIFrames is open; will retry later."
        return
    }
    $tmp = "$exe.new"
    Download $exeAsset $tmp
    if ((Get-FileHash $tmp -Algorithm SHA256).Hash -ne $expected) { Remove-Item $tmp -Force; Log "Download of $($r.tag_name) failed verification."; return }
    Move-Item $tmp $exe -Force
    [pscustomobject]@{ version = $r.tag_name; sha256 = $expected; installed = (Get-Date).ToString('s') } | ConvertTo-Json | Set-Content $stateFile -Encoding UTF8

    # First install (or a missing association): register the .aifr file type.
    if (-not (Test-Path 'HKCU:\Software\Classes\AIFrames.Project')) {
        Start-Process $exe -ArgumentList '--register', '--quiet' -Wait
    }
    Log "Installed AIFrames $($r.tag_name)."

    # Refresh the updater itself from the same release (takes effect next run).
    foreach ($f in 'Update-AIFrames.ps1', 'run-hidden.vbs') {
        $a = Asset $r $f
        if ($a) { try { Download $a (Join-Path $home_ "$f.new"); Move-Item (Join-Path $home_ "$f.new") (Join-Path $home_ $f) -Force } catch { Log "Could not refresh ${f}: $($_.Exception.Message)" } }
    }
}

# One updater at a time: the scheduled task (wscript.exe returns at once, so the task's IgnoreNew setting
# doesn't apply), its logon and missed-run triggers, and Start menu launches can overlap, and all of them
# download to the same aiframes.exe.new. If another run is busy, skip the update (and still launch).
$mutex = New-Object System.Threading.Mutex($false, 'Local\AIFrames-Updater')
$owned = $false
try { $owned = $mutex.WaitOne(0) } catch { $owned = $true } # AbandonedMutexException: a previous run died; it is ours now
if ($owned) {
    try { Update-App } catch { Log "Update error: $($_.Exception.Message)" } finally { $mutex.ReleaseMutex() }
}

if ($Launch -and (Test-Path $exe)) {
    # Windows PowerShell joins -ArgumentList with spaces, so quote each argument (doubling trailing backslashes).
    if ($AppArgs) { Start-Process $exe -ArgumentList ($AppArgs | ForEach-Object { '"{0}"' -f ($_ -replace '(\\+)$', '$1$1') }) } else { Start-Process $exe }
}
