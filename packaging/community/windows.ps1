param([ValidateSet('Tools','Pack','UninstallerRequest','Installer','Verify','Smoke')][string]$Mode)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$repo = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$dist = Join-Path $repo 'dist-community\windows'
$artifacts = Join-Path $repo 'dist-community\artifacts'
$cache = Join-Path $repo 'dist-community\signed-uninstaller'
$compiler = Join-Path $env:RUNNER_TEMP 'community-inno\ISCC.exe'
$recipe = Join-Path $PSScriptRoot 'windows.iss'
$version = (& python (Join-Path $PSScriptRoot 'release.py') version).Trim()
if ($LASTEXITCODE -ne 0) { throw 'Cannot read release version' }

function Assert-Signature([string]$Path, [string]$Subject) {
    $s = Get-AuthenticodeSignature -LiteralPath $Path
    if ($s.Status -ne 'Valid' -or $null -eq $s.SignerCertificate -or $s.SignerCertificate.Subject -notmatch $Subject) {
        throw "Invalid publisher signature on $([IO.Path]::GetFileName($Path))"
    }
}
function Assert-Community([string]$Path, [string]$OriginalName) {
    Assert-Signature $Path '(^|,\s*)CN=Daming Wu(,|$)'
    $signature = Get-AuthenticodeSignature -LiteralPath $Path
    $subject = ($signature.SignerCertificate.Subject -split ',' | ForEach-Object { $_.Trim() }) -join ', '
    if ($subject -cne 'CN=Daming Wu, O=Daming Wu, L=Newberry, S=FL, C=US' -or $null -eq $signature.TimeStamperCertificate) { throw 'Expected exact publisher identity and a trusted signing timestamp' }
    $v = (Get-Item -LiteralPath $Path).VersionInfo
    # Inno Setup pads version-resource strings with spaces. Normalize padding
    # just as the desktop updater does, then compare the complete identity.
    if ($v.ProductName.Trim() -cne 'VocalCode Community' -or $v.OriginalFilename.Trim() -cne $OriginalName -or $v.ProductVersion.Trim() -notin @($version,"$version.0")) {
        throw 'Signed executable product, filename or version does not match the community release'
    }
}

switch ($Mode) {
    'Tools' {
        $setup = Join-Path $env:RUNNER_TEMP 'community-innosetup-6.7.3.exe'
        Invoke-WebRequest -Uri 'https://github.com/jrsoftware/issrc/releases/download/is-6_7_3/innosetup-6.7.3.exe' -OutFile $setup
        if ((Get-FileHash -LiteralPath $setup -Algorithm SHA256).Hash -ne '9c73c3bae7ed48d44112a0f48e66742c00090bdb5bef71d9d3c056c66e97b732') { throw 'Inno Setup installer hash mismatch' }
        # Official verification instructions: https://jrsoftware.org/isdl-verify.php
        # Inno Setup 6.7.3 is signed by its maintainer's company, Pyrsys B.V.
        # Keep both the pinned file digest and the complete publisher identity.
        Assert-Signature $setup '^CN=Pyrsys B\.V\., O=Pyrsys B\.V\., S=Noord-Holland, C=NL$'
        $destination = Split-Path -Parent $compiler
        $p = Start-Process -FilePath $setup -ArgumentList @('/VERYSILENT','/SUPPRESSMSGBOXES','/NORESTART',('/DIR="'+$destination+'"')) -PassThru -Wait -WindowStyle Hidden
        if ($p.ExitCode -ne 0 -or -not (Test-Path -LiteralPath $compiler)) { throw 'Pinned Inno Setup installation failed' }
    }
    'Pack' {
        if (Test-Path -LiteralPath $dist) { throw 'Refusing an existing dist directory' }
        New-Item -ItemType Directory -Path $dist | Out-Null
        $target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $repo 'target' }
        Copy-Item -LiteralPath (Join-Path $target 'release\vocalcode-app.exe') -Destination (Join-Path $dist 'VocalCodeCommunity.exe')
        foreach ($name in @('onnxruntime.dll','onnxruntime_providers_shared.dll','sherpa-onnx-c-api.dll','sherpa-onnx-cxx-api.dll')) {
            Copy-Item -LiteralPath (Join-Path $target "release\$name") -Destination $dist
        }
        $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
        $vs = @(& $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath)
        if ($LASTEXITCODE -ne 0 -or $vs.Count -ne 1) { throw 'Cannot identify the Visual C++ redistributable source' }
        # Visual Studio also creates alias directories such as v145, which do
        # not necessarily contain x64. Inspect only actual versioned layouts.
        $runtime = Get-ChildItem -LiteralPath (Join-Path $vs[0] 'VC\Redist\MSVC') -Directory |
            Where-Object { $_.Name -match '^\d+\.\d+\.\d+(\.\d+)?$' } |
            Sort-Object { [version]$_.Name } -Descending |
            ForEach-Object {
                $x64 = Join-Path $_.FullName 'x64'
                if (Test-Path -LiteralPath $x64 -PathType Container) {
                    Get-ChildItem -LiteralPath $x64 -Filter 'Microsoft.VC*.CRT' -Directory
                }
            } | Select-Object -First 1
        if ($null -eq $runtime) { throw 'Microsoft app-local CRT is missing' }
        foreach ($dll in Get-ChildItem -LiteralPath $runtime.FullName -Filter '*.dll' -File) {
            Assert-Signature $dll.FullName 'Microsoft Corporation'
            Copy-Item -LiteralPath $dll.FullName -Destination $dist
        }
        foreach ($name in @('LICENSE','LICENSING.md','BUILDING.md','THIRD-PARTY-NOTICES.txt','THIRD-PARTY-LICENSES')) {
            Copy-Item -LiteralPath (Join-Path $repo $name) -Destination $dist -Recurse
        }
        New-Item -ItemType Directory -Path (Join-Path $dist 'prerequisites') | Out-Null
        $webview = Join-Path $dist 'prerequisites\MicrosoftEdgeWebview2Setup.exe'
        Invoke-WebRequest -Uri 'https://go.microsoft.com/fwlink/p/?LinkId=2124703' -OutFile $webview
        Assert-Signature $webview 'Microsoft Corporation'
        $info = (& (Join-Path $dist 'VocalCodeCommunity.exe') --build-info | ConvertFrom-Json)
        if ($LASTEXITCODE -ne 0 -or $info.edition -ne 'community' -or $info.version -ne $version) { throw 'Packaged app cannot load or has wrong edition' }
        Get-ChildItem -LiteralPath $dist -Filter '*.dll' -File | ForEach-Object {
            [pscustomobject]@{ name=$_.Name; version=$_.VersionInfo.FileVersion; sha256=(Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant() }
        } | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $dist 'runtime-inventory.json') -Encoding utf8
    }
    'UninstallerRequest' {
        if ($env:AZURE_CLIENT_SECRET -or $env:AZURE_CLIENT_ID -or $env:AZURE_TENANT_ID) { throw 'Credentials leaked into installer compilation' }
        Assert-Community (Join-Path $dist 'VocalCodeCommunity.exe') 'VocalCodeCommunity.exe'
        New-Item -ItemType Directory -Path $cache | Out-Null
        & $compiler '/Qp' ("/DSIGNED_CACHE=$cache") $recipe
        if ($LASTEXITCODE -ne 2) { throw 'Expected the signed-uninstaller request gate' }
        $files = @(Get-ChildItem -LiteralPath $cache -File)
        if ($files.Count -ne 1 -or $files[0].Name -notmatch '^uninst-6\.7\.3-[a-f0-9]{10}\.e32$') { throw 'Unexpected uninstaller request files' }
        if ((Get-AuthenticodeSignature -LiteralPath $files[0].FullName).Status -ne 'NotSigned') { throw 'Expected a fresh unsigned uninstaller' }
        if ($env:GITHUB_OUTPUT) { "path=$($files[0].FullName)" | Add-Content -LiteralPath $env:GITHUB_OUTPUT }
        $global:LASTEXITCODE = 0
    }
    'Installer' {
        if ($env:AZURE_CLIENT_SECRET -or $env:AZURE_CLIENT_ID -or $env:AZURE_TENANT_ID) { throw 'Credentials leaked into installer compilation' }
        $files = @(Get-ChildItem -LiteralPath $cache -File)
        if ($files.Count -ne 1) { throw 'Unexpected signed uninstaller cache' }
        Assert-Signature $files[0].FullName '(^|,\s*)CN=Daming Wu(,|$)'
        & $compiler '/Qp' ("/DSIGNED_CACHE=$cache") $recipe
        if ($LASTEXITCODE -ne 0) { throw 'Community installer compilation failed' }
    }
    'Verify' {
        Assert-Community (Join-Path $artifacts 'VocalCodeCommunitySetup.exe') 'VocalCodeCommunitySetup.exe'
    }
    'Smoke' {
        # Only this disposable GitHub-hosted job may install/uninstall. Never
        # invoke this mode against a maintainer's existing desktop profile.
        if ($env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted') { throw 'Installer smoke requires a disposable hosted runner' }
        $data = Join-Path $env:LOCALAPPDATA 'VocalCode Community'
        if (Test-Path -LiteralPath $data) { throw 'Community data already exists; refusing smoke' }
        $install = Join-Path $env:RUNNER_TEMP 'community-installed'
        $setup = Join-Path $artifacts 'VocalCodeCommunitySetup.exe'
        Assert-Community $setup 'VocalCodeCommunitySetup.exe'
        New-Item -ItemType Directory -Path $data | Out-Null
        $fixture = Join-Path $data 'install-smoke-preserve.txt'
        [IO.File]::WriteAllText($fixture, 'synthetic user data must survive upgrades and uninstall')
        foreach ($attempt in 1..2) {
            $p = Start-Process -FilePath $setup -ArgumentList @('/VERYSILENT','/SUPPRESSMSGBOXES','/NORESTART','/SP-',('/DIR="'+$install+'"')) -PassThru -Wait -WindowStyle Hidden
            if ($p.ExitCode -ne 0) { throw 'Installation/upgrade failed' }
            $exe = Join-Path $install 'VocalCodeCommunity.exe'
            Assert-Community $exe 'VocalCodeCommunity.exe'
            $info = (& $exe --build-info | ConvertFrom-Json)
            if ($LASTEXITCODE -ne 0 -or $info.edition -ne 'community' -or $info.version -ne $version) { throw 'Installed app smoke failed' }
            if ([IO.File]::ReadAllText($fixture) -ne 'synthetic user data must survive upgrades and uninstall') { throw 'Upgrade changed user data' }
        }
        $uninstaller = Join-Path $install 'unins000.exe'
        Assert-Signature $uninstaller '(^|,\s*)CN=Daming Wu(,|$)'
        $p = Start-Process -FilePath $uninstaller -ArgumentList @('/VERYSILENT','/SUPPRESSMSGBOXES','/NORESTART') -PassThru -Wait -WindowStyle Hidden
        if ($p.ExitCode -ne 0 -or -not (Test-Path -LiteralPath $fixture)) { throw 'Uninstall failed or removed user data' }
        Write-Output 'Signed installation, same-version upgrade, native runtime loading and data-preserving uninstall passed.'
    }
}
