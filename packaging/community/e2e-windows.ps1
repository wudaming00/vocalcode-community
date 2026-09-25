# End-to-end checks of VocalCodeSetup.exe against real earlier installations,
# on a disposable GitHub-hosted Windows runner only. Never run this on a
# computer with a VocalCode installation or data folder you care about: it
# installs, updates and uninstalls per-user software and changes HKCU.
#
#   Paid   A real paid VocalCode 1.2.1 (its signed installer, sha-pinned) is
#          installed, given realistic paid-format data, and updated exactly as
#          its own in-app updater does it: the helper script below is that
#          release's WINDOWS_UPDATE_HELPER, byte for byte, run the same way.
#          First, the same updater running from a copy that no installer
#          registered (as Scoop unpacks it) must install nothing.
#   Early  The early free build, VocalCode Community 1.4.0 (its signed
#          installer, sha-pinned), is replaced by VocalCodeSetup.exe.
#
# The data fixtures under packaging/community/e2e were written by those
# releases' own code (paid: vocalcode-core/vocalcode-meeting at v1.2.1,
# Config::default() with a few choices, toml::to_string_pretty; MeetingStore).
# Credential-looking files seeded here are random bytes made up by this script.
param(
    [Parameter(Mandatory = $true)][ValidateSet('Paid', 'Early')][string]$Scenario,
    [Parameter(Mandatory = $true)][string]$Installer,
    [Parameter(Mandatory = $true)][string]$TestExe,
    [Parameter(Mandatory = $true)][string]$Downloads
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
# ProcessStartInfo.ArgumentList: the helper itself runs in Windows PowerShell,
# as the paid app runs it, but this script needs PowerShell 7.
if ($PSVersionTable.PSVersion.Major -lt 7) { throw 'Run this script with pwsh' }

if ($env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted') {
    throw 'The end-to-end checks require a disposable GitHub-hosted runner'
}

$repo = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$fixtures = Join-Path $PSScriptRoot 'e2e'
$version = (& python (Join-Path $PSScriptRoot 'release.py') version).Trim()
if ($LASTEXITCODE -ne 0) { throw 'Cannot read the release version' }
$local = [Environment]::GetFolderPath('LocalApplicationData')
$programs = Join-Path $local 'Programs'
$uninstallRoot = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall'
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$work = Join-Path $env:RUNNER_TEMP ("vocalcode-e2e-" + $Scenario.ToLowerInvariant())
New-Item -ItemType Directory -Force -Path $work | Out-Null

# Write-Host, not output: a function's output is its return value.
function Step([string]$Text) { Write-Host "==> $Text" }

# The browser-like agent is only for vocalcode.app, which refuses bare HTTP
# clients. The file is accepted only by its pinned SHA-256.
function Get-PinnedFile([string]$Url, [string]$Sha256, [string]$Name) {
    $path = Join-Path $Downloads $Name
    New-Item -ItemType Directory -Force -Path $Downloads | Out-Null
    if (Test-Path -LiteralPath $path) {
        if ((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash -eq $Sha256) {
            Step "$Name from the cache (sha256 verified)"
            return $path
        }
        Remove-Item -LiteralPath $path -Force
    }
    $agent = 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36'
    foreach ($attempt in 1..4) {
        try {
            Invoke-WebRequest -Uri $Url -OutFile $path -UserAgent $agent -UseBasicParsing -MaximumRedirection 5
            break
        } catch {
            if ($attempt -eq 4) { throw "Could not download $Name from $Url after 4 attempts: $($_.Exception.Message)" }
            Start-Sleep -Seconds (5 * $attempt)
        }
    }
    $actual = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash
    if ($actual -ne $Sha256) { throw "$Name does not have the pinned SHA-256 (got $actual)" }
    Step "$Name downloaded (sha256 verified)"
    return $path
}

# Start-Process -Wait also waits for every descendant; an installer may leave
# one behind. Wait for the process itself.
function Invoke-Exe([string]$File, [string[]]$Arguments, [int]$TimeoutSeconds = 900) {
    $p = Start-Process -FilePath $File -ArgumentList $Arguments -PassThru
    if (-not $p.WaitForExit($TimeoutSeconds * 1000)) { throw "$([IO.Path]::GetFileName($File)) did not finish within $TimeoutSeconds s" }
    return $p.ExitCode
}

function Wait-Until([scriptblock]$Condition, [int]$Seconds, [string]$What) {
    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        if (& $Condition) { return }
        Start-Sleep -Milliseconds 500
    }
    throw "Timed out after $Seconds s waiting for: $What"
}

function Get-Uninstall([string]$Key) {
    $path = Join-Path $uninstallRoot $Key
    if (Test-Path -LiteralPath $path) { return Get-ItemProperty -LiteralPath $path }
    return $null
}

function Get-RunValue([string]$Name) {
    $item = Get-ItemProperty -LiteralPath $runKey -ErrorAction SilentlyContinue
    if ($null -ne $item -and $item.PSObject.Properties.Name -contains $Name) { return [string]$item.$Name }
    return $null
}

# Relative path -> sha256/size/last write, for "nothing here changed".
function Get-Snapshot([string]$Root) {
    $map = @{}
    foreach ($file in @(Get-ChildItem -LiteralPath $Root -Recurse -File -Force)) {
        $relative = $file.FullName.Substring($Root.Length).TrimStart('\')
        $map[$relative] = '{0}|{1}|{2}' -f (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash, $file.Length, $file.LastWriteTimeUtc.Ticks
    }
    return $map
}

function Assert-Unchanged([hashtable]$Before, [string]$Root, [string[]]$Allowed = @(), [switch]$NothingAdded) {
    foreach ($name in $Before.Keys) {
        if ($Allowed -contains $name) { continue }
        $path = Join-Path $Root $name
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Removed: $name" }
        $file = Get-Item -LiteralPath $path -Force
        $now = '{0}|{1}|{2}' -f (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash, $file.Length, $file.LastWriteTimeUtc.Ticks
        if ($now -ne $Before[$name]) { throw "Changed: $name" }
    }
    if ($NothingAdded) {
        foreach ($name in (Get-Snapshot $Root).Keys) {
            if (-not $Before.ContainsKey($name)) { throw "Added: $name" }
        }
    }
}

# Every per-user uninstall entry that names VocalCode, whatever its key.
function Get-VocalCodeEntries {
    return @(Get-ChildItem -LiteralPath $uninstallRoot | ForEach-Object { Get-ItemProperty -LiteralPath $_.PSPath } |
        Where-Object { $_.PSObject.Properties.Name -contains 'DisplayName' -and [string]$_.DisplayName -like 'VocalCode*' })
}

# A stand-in VocalCode.exe for a copy no installer registered, as Scoop
# leaves it (scoop\apps\vocalcode\current). It only records the arguments it
# was started with, beside itself in started.txt. Built with the C# compiler
# of the .NET Framework that Windows includes.
function New-StandInApp([string]$Folder) {
    $csc = Join-Path $env:WINDIR 'Microsoft.NET\Framework64\v4.0.30319\csc.exe'
    if (-not (Test-Path -LiteralPath $csc)) { throw "The .NET Framework C# compiler is missing: $csc" }
    New-Item -ItemType Directory -Force -Path $Folder | Out-Null
    $source = Join-Path $work 'stand-in.cs'
    $code = 'using System; using System.IO; static class StandIn { static int Main(string[] args) { ' +
            'File.WriteAllText(Path.Combine(AppDomain.CurrentDomain.BaseDirectory, "started.txt"), string.Join("|", args)); return 0; } }'
    [IO.File]::WriteAllText($source, $code)
    $exe = Join-Path $Folder 'VocalCode.exe'
    $out = & $csc /nologo /target:winexe ('/out:' + $exe) $source 2>&1 | Out-String
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $exe)) { throw "Could not build the stand-in app: $out" }
    return $exe
}

# A 0.1 s, 16 kHz mono PCM chunk of silence, written here rather than
# committed as generated audio.
function Write-SilentWav([string]$Path) {
    $samples = 1600
    $stream = New-Object IO.MemoryStream
    $w = New-Object IO.BinaryWriter($stream)
    $w.Write([Text.Encoding]::ASCII.GetBytes('RIFF')); $w.Write([int](36 + 2 * $samples))
    $w.Write([Text.Encoding]::ASCII.GetBytes('WAVEfmt ')); $w.Write([int]16); $w.Write([int16]1); $w.Write([int16]1)
    $w.Write([int]16000); $w.Write([int]32000); $w.Write([int16]2); $w.Write([int16]16)
    $w.Write([Text.Encoding]::ASCII.GetBytes('data')); $w.Write([int](2 * $samples)); $w.Write((New-Object byte[] (2 * $samples)))
    $w.Flush()
    [IO.File]::WriteAllBytes($Path, $stream.ToArray())
}

function Copy-Fixture([string]$Name, [string]$Destination) {
    New-Item -ItemType Directory -Force -Path $Destination | Out-Null
    Copy-Item -Path (Join-Path (Join-Path $fixtures $Name) '*') -Destination $Destination -Recurse -Force
    foreach ($meeting in @(Get-ChildItem -LiteralPath (Join-Path $Destination 'meetings') -Directory)) {
        $audio = Join-Path $meeting.FullName 'audio'
        New-Item -ItemType Directory -Force -Path $audio | Out-Null
        Write-SilentWav (Join-Path $audio 'microphone-000000.wav')
    }
}

# Made-up bytes under the names the paid releases gave their licence, trial
# and time-anchor files. They only prove those names are never touched.
function Write-Decoy([string]$Path) {
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $Path) | Out-Null
    $bytes = New-Object byte[] 96
    [Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
    [IO.File]::WriteAllBytes($Path, $bytes)
}

# The data-loading checks, run from the unit-test binary built on the same
# commit, against a copy of the given folder with the user data only.
function Invoke-LoadCheck([string]$Variable, [string]$Source, [string]$Test) {
    $copy = Join-Path $work ("copy-" + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $copy | Out-Null
    foreach ($name in @('vocalcode.toml', 'replacements.txt', 'totals.json', 'activity.json', 'personalization', 'meetings')) {
        $item = Join-Path $Source $name
        if (Test-Path -LiteralPath $item) { Copy-Item -LiteralPath $item -Destination $copy -Recurse }
    }
    Set-Item -Path "Env:$Variable" -Value $copy
    try {
        $out = & $TestExe --exact $Test --nocapture --test-threads=1 2>&1 | Out-String
        Write-Host $out
        if ($LASTEXITCODE -ne 0) { throw "$Test failed against the installed data" }
        # A misspelt name would run nothing and still exit 0.
        if ($out -notmatch 'test result: ok\. 1 passed') { throw "$Test did not run" }
    } finally {
        Remove-Item -Path "Env:$Variable"
    }
}

# Exactly how a paid release (v1.0.13 through v1.2.1, all identical) runs its
# installer: WINDOWS_UPDATE_HELPER in vocalcode-app/src/webui.rs, launched as
# powershell.exe -NoProfile -NonInteractive -Command <this>, with these five
# environment values, after the helper acknowledges through VC_UPDATE_READY.
$PaidUpdateHelper = @'
$ErrorActionPreference = 'Stop'; $cleanupWorkspace = $true; function Restart-Normal { try { Start-Process -FilePath $env:VC_UPDATE_EXE -ErrorAction Stop | Out-Null } catch { [System.Diagnostics.Process]::Start($env:VC_UPDATE_EXE) | Out-Null } }; function Restart-Failed([string]$reason) { try { Start-Process -FilePath $env:VC_UPDATE_EXE -ArgumentList @('--update-failed', $reason) -ErrorAction Stop | Out-Null } catch { [System.Diagnostics.Process]::Start($env:VC_UPDATE_EXE, ('--update-failed ' + $reason)) | Out-Null } }; function Preserve-Diagnostic([string]$reason) { $script:cleanupWorkspace = $false; try { [System.IO.File]::WriteAllText((Join-Path $env:VC_UPDATE_DIR 'failure.txt'), $reason) } catch {} }; $waitId = [int]$env:VC_UPDATE_PID; $parent = Get-Process -Id $waitId -ErrorAction SilentlyContinue; try { [System.IO.File]::WriteAllText($env:VC_UPDATE_READY, 'ready'); if ($null -ne $parent -and -not $parent.WaitForExit(300000)) { Preserve-Diagnostic 'parent-exit-timeout'; return }; $p = Start-Process -FilePath $env:VC_UPDATE_INSTALLER -ArgumentList @('/VERYSILENT','/SUPPRESSMSGBOXES','/NOCANCEL','/NORESTART') -PassThru -ErrorAction Stop; if (-not $p.WaitForExit(900000)) { Preserve-Diagnostic 'installer-timeout'; $treeStopped = $false; try { $taskkill = Join-Path ([System.Environment]::SystemDirectory) 'taskkill.exe'; $killer = Start-Process -FilePath $taskkill -ArgumentList @('/PID', ([string]$p.Id), '/T', '/F') -PassThru -ErrorAction Stop; $killerDone = $killer.WaitForExit(10000); if (-not $killerDone) { try { $killer.Kill(); $null = $killer.WaitForExit(5000) } catch {} } $treeStopped = $killerDone -and $killer.ExitCode -eq 0 -and $p.WaitForExit(10000); } catch {}; if ($treeStopped) { Restart-Failed 'installer-timeout' } else { Preserve-Diagnostic 'installer-timeout-process-tree-alive' }; return }; if ($p.ExitCode -eq 0) { Restart-Normal } else { Restart-Failed ([string]$p.ExitCode) } } catch { Restart-Failed 'helper-launch-error' } finally { if ($cleanupWorkspace) { Remove-Item -LiteralPath $env:VC_UPDATE_INSTALLER -Force -ErrorAction SilentlyContinue; Remove-Item -LiteralPath $env:VC_UPDATE_DIR -Recurse -Force -ErrorAction SilentlyContinue } }
'@
# SHA-256 of the text above as the paid binaries embed it (UTF-8, 2186 bytes).
$PaidUpdateHelperSha256 = '54001A33BAD75F146CC3EB937794C6AF134AB3C43054D9AFD7EF1362BCAD5ECD'

# One in-app update of a paid release, with this VocalCodeSetup.exe as the
# downloaded installer and $RestartExe as the app that is updating (the
# helper restarts it afterwards). Returns the finished helper process.
function Invoke-PaidUpdate([string]$RestartExe) {
    $updateDir = Join-Path $env:TEMP ('vocalcode-update-' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $updateDir | Out-Null
    $staged = Join-Path $updateDir 'VocalCodeSetup.exe'
    Copy-Item -LiteralPath $Installer -Destination $staged
    $ready = Join-Path $updateDir 'helper.ready'
    # Stands in for the paid app: it owns Local\VocalCode.Desktop, as that
    # app does, and exits a few seconds after the helper is ready.
    $parentScript = '$m = New-Object System.Threading.Mutex($false, ''Local\VocalCode.Desktop''); $null = $m.WaitOne(); Start-Sleep -Seconds 6; $m.ReleaseMutex(); $m.Dispose()'
    $parent = Start-Process -FilePath 'powershell.exe' -ArgumentList @('-NoProfile', '-NonInteractive', '-Command', $parentScript) -PassThru -WindowStyle Hidden
    Start-Sleep -Seconds 1
    $powershell = Join-Path ([Environment]::SystemDirectory) 'WindowsPowerShell\v1.0\powershell.exe'
    $psi = New-Object Diagnostics.ProcessStartInfo($powershell)
    foreach ($argument in @('-NoProfile', '-NonInteractive', '-Command', $PaidUpdateHelper.Trim())) { $psi.ArgumentList.Add($argument) }
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.EnvironmentVariables['VC_UPDATE_INSTALLER'] = $staged
    $psi.EnvironmentVariables['VC_UPDATE_EXE'] = $RestartExe
    $psi.EnvironmentVariables['VC_UPDATE_DIR'] = $updateDir
    $psi.EnvironmentVariables['VC_UPDATE_PID'] = [string]$parent.Id
    $psi.EnvironmentVariables['VC_UPDATE_READY'] = $ready
    $helper = [Diagnostics.Process]::Start($psi)
    Wait-Until { Test-Path -LiteralPath $ready } 20 'the helper acknowledgement'
    if (-not $parent.WaitForExit(60000)) { throw 'The stand-in paid app did not exit' }
    if (-not $helper.WaitForExit(16 * 60 * 1000)) { throw 'The paid update helper did not finish' }
    if (Test-Path -LiteralPath (Join-Path $updateDir 'failure.txt')) { throw "The helper recorded a failure: $(Get-Content -LiteralPath (Join-Path $updateDir 'failure.txt'))" }
    if (Test-Path -LiteralPath $updateDir) { throw 'The helper kept its workspace, which it does only after a failure' }
    return $helper
}

function Assert-VocalCodeInstalled([string]$Location) {
    $entry = Get-Uninstall 'VocalCode_is1'
    if ($null -eq $entry) { throw 'VocalCode_is1 is not registered' }
    # AppVersion is the executable's FileVersion string, which may carry .0.
    if ($entry.DisplayVersion -notin @($version, "$version.0")) { throw "VocalCode_is1 shows version $($entry.DisplayVersion), expected $version" }
    if ($entry.DisplayName -ne "VocalCode version $($entry.DisplayVersion)") { throw "VocalCode_is1 shows name '$($entry.DisplayName)'" }
    if ($entry.'Inno Setup: App Path' -ne $Location) { throw "VocalCode_is1 moved to $($entry.'Inno Setup: App Path')" }
    $exe = Join-Path $Location 'VocalCode.exe'
    $info = (Get-Item -LiteralPath $exe).VersionInfo
    if ($info.ProductName.Trim() -ne 'VocalCode' -or $info.ProductVersion.Trim() -notin @($version, "$version.0")) {
        throw "$exe is not this build ($($info.ProductName) $($info.ProductVersion))"
    }
    $build = & $exe --build-info | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0 -or $build.version -ne $version -or $build.edition -ne 'community' -or $build.identity -ne 'release' -or $build.data_directory -ne 'VocalCode') {
        throw "$exe --build-info does not describe this free build"
    }
}

switch ($Scenario) {
'Paid' {
    $data = Join-Path $local 'VocalCode'
    $app = Join-Path $programs 'VocalCode'
    if ((Get-Uninstall 'VocalCode_is1') -or (Test-Path -LiteralPath $data) -or (Test-Path -LiteralPath $app)) {
        throw 'This runner already has VocalCode; refusing'
    }
    $helperHash = (Get-FileHash -InputStream ([IO.MemoryStream]::new([Text.Encoding]::UTF8.GetBytes($PaidUpdateHelper.Trim())))).Hash
    if ($helperHash -ne $PaidUpdateHelperSha256) { throw 'The replayed helper is not the paid releases'' text' }

    # Scoop's bucket (wudaming00/vocalcode-docs, "innosetup": true) unpacks
    # the paid installer into scoop\apps\vocalcode and registers nothing.
    Step 'An update from a copy no installer registered (as Scoop unpacks it) installs nothing'
    $standIn = New-StandInApp (Join-Path $work 'scoop\apps\vocalcode\current')
    $standInLog = Join-Path (Split-Path -Parent $standIn) 'started.txt'
    $null = Invoke-PaidUpdate $standIn
    Wait-Until { Test-Path -LiteralPath $standInLog } 60 'the helper to restart the unregistered copy'
    $restartedWith = [IO.File]::ReadAllText($standInLog)
    if ($restartedWith -ne '--update-failed|7') { throw "The unregistered copy was restarted with '$restartedWith', not '--update-failed 7'" }
    if ((Get-VocalCodeEntries).Count -ne 0) { throw 'An update of an unregistered copy registered an app' }
    if (Test-Path -LiteralPath $app) { throw "An update of an unregistered copy installed into $app" }
    if (Test-Path -LiteralPath $data) { throw "An update of an unregistered copy created $data" }
    Remove-Item -LiteralPath $standInLog

    $paidSetup = Get-PinnedFile 'https://vocalcode.app/VocalCodeSetup-1.2.1.exe' '2BA1B7B36BE74AE3819B5EEC2E8A217DBBFBAC2A44E0232D01F325E91028CB1A' 'VocalCodeSetup-1.2.1.exe'
    $signature = Get-AuthenticodeSignature -LiteralPath $paidSetup
    if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -ne 'CN=Daming Wu, O=Daming Wu, L=Newberry, S=FL, C=US') { throw 'The paid 1.2.1 installer is not the signed release' }

    Step 'Install the paid VocalCode 1.2.1 as its users did (silently; its [Run] entry is skipped)'
    $code = Invoke-Exe $paidSetup @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/SP-', ('/LOG="' + (Join-Path $work 'paid-install.log') + '"'))
    if ($code -ne 0) { throw "Paid 1.2.1 installation failed ($code)" }
    $paid = Get-Uninstall 'VocalCode_is1'
    if ($null -eq $paid -or $paid.DisplayVersion -ne '1.2.1') { throw 'Paid 1.2.1 is not registered as VocalCode_is1' }
    $app = $paid.'Inno Setup: App Path'
    $exe = Join-Path $app 'VocalCode.exe'
    if ((Get-Item -LiteralPath $exe).VersionInfo.FileVersion -notlike '1.2.1*') { throw 'Paid VocalCode.exe is not 1.2.1' }
    $paidUninstaller = (Get-FileHash -LiteralPath (Join-Path $app 'unins000.exe') -Algorithm SHA256).Hash
    if (-not (Test-Path -LiteralPath (Join-Path $data 'vocalcode.toml'))) { throw 'Paid Setup did not create its default vocalcode.toml (the uninstall-log case this test is about)' }
    # The paid app is never started: its trial check would contact the
    # production licence service.

    Step 'With paid 1.2.1 registered, an updater that would restart another copy is refused too'
    $appBefore = Get-Snapshot $app
    $refusedLog = Join-Path $work 'refused-install.log'
    $env:VC_UPDATE_EXE = $standIn
    try {
        $code = Invoke-Exe $Installer @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NOCANCEL', '/NORESTART', ('/LOG="' + $refusedLog + '"'))
    } finally {
        Remove-Item -Path 'Env:VC_UPDATE_EXE'
    }
    if ($code -ne 7) { throw "VocalCodeSetup.exe exited with $code for an updater restarting another copy; expected 7" }
    if (-not (Select-String -LiteralPath $refusedLog -SimpleMatch 'nothing is installed' -Quiet)) { throw 'The installer log does not say why it refused' }
    $entries = @(Get-VocalCodeEntries)
    if ($entries.Count -ne 1 -or $entries[0].PSChildName -ne 'VocalCode_is1' -or $entries[0].DisplayVersion -ne '1.2.1') { throw 'The refused update changed the registration' }
    Assert-Unchanged $appBefore $app -NothingAdded
    if (Test-Path -LiteralPath $standInLog) { throw 'A refused installer started an app' }

    Step 'Seed paid-1.2.1-format data, credential decoys and the paid login item'
    Copy-Fixture 'paid-1.2.1' $data
    $decoys = @('vocalcode-license.json', 'vocalcode-license.legacy.json', 'vocalcode-trial.dat', 'vocalcode-time-anchor.bin',
                'vocalcode-time-anchor.json', '.vocalcode-license.lock', 'secure\diagnostics.key')
    foreach ($name in $decoys) { Write-Decoy (Join-Path $data $name) }
    # Where releases before 0.5.2 kept the trial file: in the program folder.
    Write-Decoy (Join-Path $app 'vocalcode-trial.dat')
    $appDecoy = (Get-FileHash -LiteralPath (Join-Path $app 'vocalcode-trial.dat') -Algorithm SHA256).Hash
    $login = '"' + $exe + '"'
    if (-not (Test-Path -LiteralPath $runKey)) { New-Item -Path $runKey | Out-Null }
    New-ItemProperty -LiteralPath $runKey -Name 'VocalCode' -Value $login -PropertyType String -Force | Out-Null
    $before = Get-Snapshot $data

    # Paid 1.0 and 1.1 installed cargs.dll and THIRD-PARTY-LICENSES\Cargs-
    # LICENSE.txt (their pack.ps1 and vocalcode.iss); 1.2 removes the DLL but
    # leaves the notice. The 1.2.1 installed above has neither, so they are
    # put where a person updating from 1.0 or 1.1 still has them.
    $stale = @('cargs.dll', 'THIRD-PARTY-LICENSES\Cargs-LICENSE.txt')
    foreach ($name in $stale) { Write-Decoy (Join-Path $app $name) }

    Step 'Replay the paid in-app update with this VocalCodeSetup.exe'
    $started = Get-Date
    $helper = Invoke-PaidUpdate $exe

    Step 'The helper started the new VocalCode.exe'
    $running = $null
    Wait-Until {
        $script:running = Get-CimInstance Win32_Process -Filter "Name = 'VocalCode.exe'" | Where-Object { $_.ExecutablePath -eq $exe } | Select-Object -First 1
        $null -ne $script:running
    } 60 'VocalCode.exe to be running'
    if ($running.ParentProcessId -ne $helper.Id) { throw "VocalCode.exe was started by process $($running.ParentProcessId), not the helper $($helper.Id)" }
    if ($running.CommandLine -match '--update-failed') { throw 'The helper reported a failed update to the app' }
    $log = Join-Path $data 'vocalcode.log'
    $logText = {
        if (-not (Test-Path -LiteralPath $log)) { return '' }
        $stream = [IO.File]::Open($log, 'Open', 'Read', 'ReadWrite, Delete')
        try { (New-Object IO.StreamReader($stream)).ReadToEnd() } finally { $stream.Dispose() }
    }
    Wait-Until { (& $logText) -match "VocalCode $([regex]::Escape($version)) starting" -and (& $logText) -match 'loaded config from' } 120 'the new build to start and load its settings'
    # A few more seconds for the engine, then say what the log shows.
    Start-Sleep -Seconds 10
    foreach ($line in @((& $logText) -split "`n" | Where-Object { $_ -match 'starting|loaded config|replacement rule|migrated config|ERROR' })) { Step "log: $($line.Trim())" }
    if (-not (Get-Process -Id $running.ProcessId -ErrorAction SilentlyContinue)) { throw 'The new VocalCode.exe exited on its own' }
    Stop-Process -Id $running.ProcessId -Force
    Wait-Until { -not (Get-Process -Id $running.ProcessId -ErrorAction SilentlyContinue) } 30 'VocalCode.exe to stop'
    # Its WebView2 helpers follow the host out; nothing below depends on it.
    try {
        Wait-Until { -not (Get-CimInstance Win32_Process -Filter "Name = 'msedgewebview2.exe'" | Where-Object { $_.CommandLine -like "*$data*" }) } 60 'its WebView2 processes to exit'
    } catch {
        Step 'WebView2 processes of the stopped app are still exiting; continuing'
    }

    Step 'The same registration and folder now hold this build'
    Assert-VocalCodeInstalled $app
    if (Get-Uninstall 'VocalCode.Community_is1') { throw 'A second app was registered' }
    if ((Get-FileHash -LiteralPath (Join-Path $app 'unins000.exe') -Algorithm SHA256).Hash -eq $paidUninstaller) { throw 'The paid uninstaller was not replaced' }
    foreach ($name in $stale) {
        if (Test-Path -LiteralPath (Join-Path $app $name)) { throw "A paid-only program file is left: $name" }
    }
    $notice = Join-Path $app 'THIRD-PARTY-LICENSES\Microsoft-Visual-Cpp-Runtime-NOTICE.txt'
    if (-not (Select-String -LiteralPath $notice -SimpleMatch "VocalCode's Windows package" -Quiet)) { throw 'The installed licence notices are not this build''s' }
    if ((Get-RunValue 'VocalCode') -ne $login) { throw 'The login item changed' }
    if ((Get-FileHash -LiteralPath (Join-Path $app 'vocalcode-trial.dat') -Algorithm SHA256).Hash -ne $appDecoy) { throw 'The program-folder trial decoy changed' }

    Step 'Every seeded file, decoy included, is byte-for-byte untouched'
    Assert-Unchanged $before $data

    Step 'This build reads that data (settings, dictionary, totals, meetings)'
    Invoke-LoadCheck 'VC_E2E_PAID_DATA' $data 'tests::a_paid_data_folder_loads_as_it_was'

    Step "Uninstall keeps the data, including the paid Setup's first vocalcode.toml"
    $code = Invoke-Exe (Join-Path $app 'unins000.exe') @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', ('/LOG="' + (Join-Path $work 'uninstall.log') + '"'))
    if ($code -ne 0) { throw "Uninstall failed ($code)" }
    Wait-Until { -not (Get-Uninstall 'VocalCode_is1') -and -not (Test-Path -LiteralPath $exe) } 90 'the uninstall to finish'
    Assert-Unchanged $before $data
    if ($null -ne (Get-RunValue 'VocalCode')) { throw 'The uninstaller left the login item' }
    Write-Output "Paid 1.2.1 -> $version through the paid updater: passed (helper started at $($started.ToString('u')))."
}
'Early' {
    $early = Join-Path $local 'VocalCode Community'
    if ((Get-Uninstall 'VocalCode.Community_is1') -or (Test-Path -LiteralPath $early)) { throw 'This runner already has VocalCode Community; refusing' }
    $earlySetup = Get-PinnedFile 'https://github.com/wudaming00/vocalcode-community/releases/download/v1.4.0/VocalCodeCommunitySetup.exe' '55B49F79AE74C96A5BE04FBE6A351BF2068F75B40DE36056CBD02B9AACD1C6CD' 'VocalCodeCommunitySetup-1.4.0.exe'

    function Install-Early {
        $code = Invoke-Exe $earlySetup @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/SP-')
        if ($code -ne 0) { throw "VocalCode Community 1.4.0 installation failed ($code)" }
        $entry = Get-Uninstall 'VocalCode.Community_is1'
        if ($null -eq $entry -or $entry.DisplayVersion -notin @('1.4.0', '1.4.0.0')) { throw 'VocalCode Community 1.4.0 is not registered' }
        return $entry.'Inno Setup: App Path'
    }

    Step 'Install VocalCode Community 1.4.0 and give it data and its login item'
    $earlyApp = Install-Early
    $earlyExe = Join-Path $earlyApp 'VocalCodeCommunity.exe'
    Copy-Fixture 'community-1.4.0' $early
    if (-not (Test-Path -LiteralPath $runKey)) { New-Item -Path $runKey | Out-Null }
    New-ItemProperty -LiteralPath $runKey -Name 'VocalCodeCommunity' -Value ('"' + $earlyExe + '"') -PropertyType String -Force | Out-Null
    $before = Get-Snapshot $early

    Step 'While it runs, a silent install leaves it in place and still succeeds'
    $holder = Start-Process -FilePath 'powershell.exe' -ArgumentList @('-NoProfile', '-NonInteractive', '-Command',
        '$m = New-Object System.Threading.Mutex($false, ''Local\VocalCode.Community.Desktop''); $null = $m.WaitOne(); Start-Sleep -Seconds 45; $m.ReleaseMutex()') -PassThru -WindowStyle Hidden
    Start-Sleep -Seconds 2
    $code = Invoke-Exe $Installer @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', ('/LOG="' + (Join-Path $work 'install-while-running.log') + '"'))
    if ($code -ne 0) { throw "VocalCodeSetup.exe failed while the early build ran ($code)" }
    if (-not (Get-Uninstall 'VocalCode.Community_is1')) { throw 'A running early build was uninstalled' }
    $holder.Kill(); $null = $holder.WaitForExit(10000)

    Step 'Installed again with it closed, VocalCodeSetup.exe removes it and keeps its data'
    $code = Invoke-Exe $Installer @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', ('/LOG="' + (Join-Path $work 'install.log') + '"'))
    if ($code -ne 0) { throw "VocalCodeSetup.exe failed ($code)" }
    Assert-VocalCodeInstalled (Join-Path $programs 'VocalCode')
    if (Get-Uninstall 'VocalCode.Community_is1') { throw 'VocalCode Community is still registered' }
    if (Test-Path -LiteralPath $earlyExe) { throw 'VocalCodeCommunity.exe is still installed' }
    if ($null -ne (Get-RunValue 'VocalCodeCommunity')) { throw "The early build's login item is left" }
    $menu = Join-Path ([Environment]::GetFolderPath('Programs')) 'VocalCode Community'
    if (Test-Path -LiteralPath $menu) { throw 'Its Start menu folder is left' }
    Assert-Unchanged $before $early

    Step 'Its data is importable in this build'
    Invoke-LoadCheck 'VC_E2E_EARLY_DATA' $early 'legacy_import::tests::the_early_free_builds_folder_imports_whole'

    $code = Invoke-Exe (Join-Path (Join-Path $programs 'VocalCode') 'unins000.exe') @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART')
    if ($code -ne 0) { throw "Uninstall failed ($code)" }
    Wait-Until { -not (Get-Uninstall 'VocalCode_is1') } 90 'the uninstall to finish'
    Assert-Unchanged $before $early
    Write-Output "VocalCode Community 1.4.0 -> ${version}: passed."
}
}
