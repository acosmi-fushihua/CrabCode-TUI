# SECURITY NOTICE: executing this bootstrap from a mutable URL does not authenticate its source.
# CrabCode TUI fail-closed installer for Windows PowerShell 5.1+.
# Encoding contract: UTF-8 with BOM, required by Windows PowerShell 5.1 over irm | iex.
$ErrorActionPreference = 'Stop'

$Repository = 'acosmi/CrabCode-TUI'
$TempRoot = $null
$Incoming = $null

function Write-Info([string]$Message) { Write-Host "info: $Message" -ForegroundColor Cyan }
function Fail([string]$Message) { throw "CrabCode TUI installer: $Message" }

function Get-PlatformToken {
    try { $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString() }
    catch { $arch = $env:PROCESSOR_ARCHITECTURE }
    switch ($arch.ToUpperInvariant()) {
        'X64' { return 'x64-win32' }
        'AMD64' { return 'x64-win32' }
        default { Fail "unsupported Windows architecture: $arch (x64 required)" }
    }
}

function Assert-Version([string]$Value) {
    if ($Value -notmatch '^v?[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z][0-9A-Za-z.-]*)?$') {
        Fail "release version is not canonical SemVer: $Value"
    }
}

function Get-RelativePortablePath([string]$Root, [string]$Path) {
    $prefix = $Root.TrimEnd('\') + '\'
    if (-not $Path.StartsWith($prefix, [System.StringComparison]::OrdinalIgnoreCase)) {
        Fail "path escapes package root: $Path"
    }
    return $Path.Substring($prefix.Length).Replace('\', '/')
}

function Test-PackageManifest([string]$Root) {
    $manifestPath = Join-Path $Root 'release-manifest.json'
    $digestPath = Join-Path $Root 'release-manifest.digest.json'
    if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf) -or -not (Test-Path -LiteralPath $digestPath -PathType Leaf)) {
        Fail 'release manifest or digest binding is missing'
    }
    $manifest = Get-Content -LiteralPath $manifestPath -Raw -Encoding UTF8 | ConvertFrom-Json
    $digest = Get-Content -LiteralPath $digestPath -Raw -Encoding UTF8 | ConvertFrom-Json
    $manifestHash = (Get-FileHash -LiteralPath $manifestPath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($digest.schemaVersion -ne 1 -or $digest.scheme -ne 'sha256' -or $digest.manifestSha256 -ne $manifestHash) {
        Fail 'release-manifest.digest.json does not bind release-manifest.json'
    }
    $expected = @{}
    foreach ($file in $manifest.files) {
        if ($expected.ContainsKey($file.path) -or $file.path -match '(^|/)\.\.(/|$)' -or $file.path.Contains('\')) {
            Fail "invalid or duplicate manifest path: $($file.path)"
        }
        $expected[$file.path] = $file
    }
    $actualCount = 0
    foreach ($item in Get-ChildItem -LiteralPath $Root -Recurse -Force) {
        if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { Fail "reparse point in package: $($item.FullName)" }
        if ($item.PSIsContainer) { continue }
        if ($item.Length -le 0) { Fail "empty file in package: $($item.FullName)" }
        $relative = Get-RelativePortablePath $Root $item.FullName
        if ($relative -in @('release-manifest.json', 'release-manifest.digest.json')) { continue }
        if (-not $expected.ContainsKey($relative)) { Fail "unmanifested file in package: $relative" }
        $entry = $expected[$relative]
        $actualHash = (Get-FileHash -LiteralPath $item.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        if ([long]$entry.size -ne $item.Length -or $entry.sha256 -ne $actualHash) { Fail "manifest mismatch: $relative" }
        $actualCount++
    }
    if ($actualCount -ne $expected.Count) { Fail 'one or more manifest files are missing' }
}

try {
    $platform = Get-PlatformToken
    if ($env:CRABCODE_ASSET_DIR -and -not $env:CRABCODE_VERSION) {
        Fail 'CRABCODE_ASSET_DIR local mode also requires a fixed CRABCODE_VERSION'
    }
    if ($env:CRABCODE_VERSION) {
        $tag = $env:CRABCODE_VERSION.Trim()
    } else {
        Write-Info 'security notice: this bootstrap source is not authenticated; use the README gh attestation flow for verified installs'
        Write-Info 'querying latest GitHub Release'
        $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repository/releases/latest" -UseBasicParsing
        $tag = [string]$release.tag_name
    }
    Assert-Version $tag
    $version = $tag -replace '^v', ''
    $tag = "v$version"
    $archive = "crabcode-$version-$platform.zip"
    $baseUrl = "https://github.com/$Repository/releases/download/$tag"

    $TempRoot = Join-Path ([IO.Path]::GetTempPath()) ("crabcode-install-" + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $TempRoot | Out-Null
    if ($env:CRABCODE_ASSET_DIR) {
        if (-not [IO.Path]::IsPathRooted($env:CRABCODE_ASSET_DIR)) { Fail 'CRABCODE_ASSET_DIR must be an absolute path' }
        $assetDir = [IO.Path]::GetFullPath($env:CRABCODE_ASSET_DIR)
        if (-not (Test-Path -LiteralPath $assetDir -PathType Container)) { Fail "CRABCODE_ASSET_DIR is not a directory: $assetDir" }
        $archivePath = Join-Path $assetDir $archive
        $checksumPath = Join-Path $assetDir 'checksums-sha256.txt'
        if (-not (Test-Path -LiteralPath $archivePath -PathType Leaf)) { Fail "local asset directory is missing $archive" }
        if (-not (Test-Path -LiteralPath $checksumPath -PathType Leaf)) { Fail 'local asset directory is missing checksums-sha256.txt' }
        Write-Info 'using fixed local assets; the installer will not access the network'
    } else {
        $archivePath = Join-Path $TempRoot $archive
        $checksumPath = Join-Path $TempRoot 'checksums-sha256.txt'
        Write-Info "downloading $archive"
        Invoke-WebRequest -Uri "$baseUrl/$archive" -OutFile $archivePath -UseBasicParsing
        Invoke-WebRequest -Uri "$baseUrl/checksums-sha256.txt" -OutFile $checksumPath -UseBasicParsing
    }

    $matching = @(Get-Content -LiteralPath $checksumPath | Where-Object { $_ -match ('^[a-fA-F0-9]{64}\s+' + [regex]::Escape($archive) + '$') })
    if ($matching.Count -ne 1) { Fail "checksum manifest must contain exactly one $archive record" }
    $expected = ($matching[0] -split '\s+')[0].ToLowerInvariant()
    $actual = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $expected) { Fail 'release archive SHA-256 mismatch' }
    Write-Info 'release-level SHA-256 verified'

    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $packageRootName = "crabcode-$version-$platform"
    $zip = [IO.Compression.ZipFile]::OpenRead($archivePath)
    try {
        if ($zip.Entries.Count -eq 0) { Fail 'release ZIP is empty' }
        foreach ($entry in $zip.Entries) {
            $name = $entry.FullName
            if ($name.Contains('\') -or $name.StartsWith('/') -or $name -match '^[A-Za-z]:' -or $name -match '(^|/)\.\.(/|$)') {
                Fail "unsafe ZIP member: $name"
            }
            if ($name -ne $packageRootName -and $name -ne "$packageRootName/" -and -not $name.StartsWith("$packageRootName/")) {
                Fail "ZIP member escapes package root: $name"
            }
        }
    } finally { $zip.Dispose() }

    $extractRoot = Join-Path $TempRoot 'extracted'
    [IO.Compression.ZipFile]::ExtractToDirectory($archivePath, $extractRoot)
    $source = Join-Path $extractRoot $packageRootName
    if (-not (Test-Path -LiteralPath $source -PathType Container)) { Fail "missing package root $packageRootName" }
    Test-PackageManifest $source
    Write-Info 'per-file package manifest verified'

    $profileRoot = [Environment]::GetFolderPath('UserProfile')
    if (-not $profileRoot) { $profileRoot = $env:USERPROFILE }
    if (-not $profileRoot) { Fail 'cannot resolve user profile' }
    $dataHome = if ($env:XDG_DATA_HOME) { $env:XDG_DATA_HOME } else { Join-Path $profileRoot '.local\share' }
    $binDir = if ($env:CRABCODE_BIN_DIR) { $env:CRABCODE_BIN_DIR } else { Join-Path $profileRoot '.crabcode\bin' }
    $dataHome = [IO.Path]::GetFullPath($dataHome)
    $binDir = [IO.Path]::GetFullPath($binDir)
    $versions = Join-Path $dataHome 'crabcode\versions'
    $destination = Join-Path $versions $version
    New-Item -ItemType Directory -Path $versions -Force | Out-Null
    New-Item -ItemType Directory -Path $binDir -Force | Out-Null

    if (Test-Path -LiteralPath $destination) {
        if (-not (Test-Path -LiteralPath $destination -PathType Container)) { Fail "existing version path is not a directory: $destination" }
        Test-PackageManifest $destination
        Write-Info "reusing verified immutable version $version"
    } else {
        $Incoming = Join-Path $versions ('.install-' + $version + '-' + [Guid]::NewGuid().ToString('N'))
        New-Item -ItemType Directory -Path $Incoming | Out-Null
        Copy-Item -Path (Join-Path $source '*') -Destination $Incoming -Recurse -Force
        Test-PackageManifest $Incoming
        Move-Item -LiteralPath $Incoming -Destination $destination
        $Incoming = $null
    }

    $destination = (Resolve-Path -LiteralPath $destination).Path
    $stable = Join-Path $binDir 'crabcode.exe'
    $stableTemp = Join-Path $binDir ('.crabcode-install-' + [Guid]::NewGuid().ToString('N') + '.exe')
    Copy-Item -LiteralPath (Join-Path $destination 'crabcode.exe') -Destination $stableTemp

    $currentTemp = Join-Path $versions ('.current.tmp.' + [Guid]::NewGuid().ToString('N'))
    # The native generation-marker protocol is byte-stable across platforms and
    # accepts one UTF-8 path terminated by LF. Do not use Environment.NewLine:
    # its Windows CRLF leaves a control character in the Rust parser's value.
    [IO.File]::WriteAllText($currentTemp, $destination + "`n", (New-Object Text.UTF8Encoding($false)))
    Move-Item -LiteralPath $currentTemp -Destination (Join-Path $versions '.current') -Force
    Move-Item -LiteralPath $stableTemp -Destination $stable -Force
    $markerTemp = Join-Path $versions ('.launcher-v1.tmp.' + [Guid]::NewGuid().ToString('N'))
    [IO.File]::WriteAllText($markerTemp, $stable + "`n", (New-Object Text.UTF8Encoding($false)))
    Move-Item -LiteralPath $markerTemp -Destination (Join-Path $versions '.launcher-v1') -Force

    Write-Info "CrabCode TUI $version installed at $destination"
    if (($env:PATH -split ';') -notcontains $binDir) {
        Write-Host "Add this directory to PATH, then run crabcode.exe:`n  $binDir"
    } else {
        Write-Host 'Run: crabcode.exe'
    }
} finally {
    if ($Incoming -and (Test-Path -LiteralPath $Incoming)) { Remove-Item -LiteralPath $Incoming -Recurse -Force }
    if ($TempRoot -and (Test-Path -LiteralPath $TempRoot)) { Remove-Item -LiteralPath $TempRoot -Recurse -Force }
}
