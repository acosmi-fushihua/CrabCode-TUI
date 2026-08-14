param(
    [Parameter(Mandatory = $true)]
    [string]$InstallerPath
)

$savedPreference = $ErrorActionPreference
$ErrorActionPreference = 'Stop'
$installer = (Resolve-Path -LiteralPath $InstallerPath).Path
$server = $null
$hadVersion = Test-Path Env:CRABCODE_VERSION
$savedVersion = $env:CRABCODE_VERSION
$hadAssetDirectory = Test-Path Env:CRABCODE_ASSET_DIR
$savedAssetDirectory = $env:CRABCODE_ASSET_DIR
$scratch = Join-Path ([IO.Path]::GetTempPath()) ("crabcode-bootstrap-transport-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $scratch | Out-Null

try {
    $source = [System.IO.File]::ReadAllText($installer)
    $tokens = $null
    $parseErrors = $null
    $ast = [System.Management.Automation.Language.Parser]::ParseInput($source, [ref]$tokens, [ref]$parseErrors)
    if ($parseErrors.Count -ne 0) { throw "installer parse failed: $($parseErrors[0].Message)" }

    foreach ($functionName in @('Fail', 'Assert-Version', 'Get-ReleaseRecordFromChecksum')) {
        $definitions = @($ast.FindAll({
            param($candidate)
            $candidate -is [System.Management.Automation.Language.FunctionDefinitionAst] -and
                $candidate.Name -eq $functionName
        }, $false))
        if ($definitions.Count -ne 1) {
            throw "installer must contain exactly one top-level function $functionName; found $($definitions.Count)"
        }
        Invoke-Expression $definitions[0].Extent.Text
    }

    $checksum = Join-Path $scratch 'checksums-sha256.txt'
    $validHash = -join ('a' * 64)

    function Assert-ChecksumFailure([string[]]$Lines, [string]$Label) {
        [IO.File]::WriteAllLines($checksum, $Lines, (New-Object Text.UTF8Encoding($false)))
        try {
            [void](Get-ReleaseRecordFromChecksum $checksum 'x64-win32')
        } catch {
            if ($_.Exception.Message -notlike '*checksum manifest must contain exactly one canonical x64-win32 archive*') { throw }
            return
        }
        throw "checksum parser accepted invalid vector: $Label"
    }

    $validLine = "$validHash  crabcode-1.2.3-x64-win32.zip"
    $unrelatedLine = "$validHash  crabcode-9.8.7-arm64-darwin.tar.gz"
    [IO.File]::WriteAllLines($checksum, @($unrelatedLine, $validLine), (New-Object Text.UTF8Encoding($false)))
    $record = Get-ReleaseRecordFromChecksum $checksum 'x64-win32'
    if ($record.Hash -ne $validHash -or $record.Archive -ne 'crabcode-1.2.3-x64-win32.zip' -or $record.Version -ne '1.2.3') {
        throw 'checksum parser changed the canonical record'
    }

    $tabLine = "$validHash`tcrabcode-1.2.4-x64-win32.zip"
    [IO.File]::WriteAllLines($checksum, @($tabLine), (New-Object Text.UTF8Encoding($false)))
    $tabRecord = Get-ReleaseRecordFromChecksum $checksum 'x64-win32'
    if ($tabRecord.Hash -ne $validHash -or $tabRecord.Archive -ne 'crabcode-1.2.4-x64-win32.zip' -or $tabRecord.Version -ne '1.2.4') {
        throw 'checksum parser rejected a canonical tab-separated record'
    }

    Assert-ChecksumFailure -Lines ([string[]]@()) -Label 'zero records'
    Assert-ChecksumFailure -Lines @($validLine, $validLine) -Label 'duplicate records'
    Assert-ChecksumFailure -Lines @('bad  crabcode-1.2.3-x64-win32.zip') -Label 'malformed hash'
    Assert-ChecksumFailure -Lines @("$validHash  crabcode-1.2.3-arm64-darwin.zip") -Label 'wrong platform'
    Assert-ChecksumFailure -Lines @("$validHash  ../crabcode-1.2.3-x64-win32.zip") -Label 'path traversal'
    Assert-ChecksumFailure -Lines @("$validHash  crabcode-1.2.3-x64-win32.zip.exe") -Label 'suffix pollution'

    $ready = Join-Path $scratch 'ready'
    $server = Start-Job -ArgumentList $installer, $ready -ScriptBlock {
        param($Installer, $Ready)
        $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
        $readyTemporary = $null
        try {
            $listener.Start()
            $port = ([Net.IPEndPoint]$listener.LocalEndpoint).Port
            $baseUrl = "http://127.0.0.1:$port"
            $readyTemporary = "$Ready.$([Guid]::NewGuid().ToString('N')).tmp"
            [IO.File]::WriteAllText($readyTemporary, $baseUrl, [Text.Encoding]::ASCII)
            [IO.File]::Move($readyTemporary, $Ready)
            $readyTemporary = $null

            for ($index = 0; $index -lt 2; $index++) {
                $client = $listener.AcceptTcpClient()
                try {
                    $stream = $client.GetStream()
                    $reader = New-Object IO.StreamReader($stream, [Text.Encoding]::ASCII, $false, 1024, $true)
                    $requestLine = $reader.ReadLine()
                    do { $line = $reader.ReadLine() } while ($null -ne $line -and $line -ne '')
                    if ($requestLine -match '^GET /latest/download/install\.ps1 HTTP/') {
                        $header = "HTTP/1.1 302 Found`r`nLocation: $baseUrl/asset/install.ps1`r`nContent-Length: 0`r`nConnection: close`r`n`r`n"
                        $headerBytes = [Text.Encoding]::ASCII.GetBytes($header)
                        $stream.Write($headerBytes, 0, $headerBytes.Length)
                    } elseif ($requestLine -match '^GET /asset/install\.ps1 HTTP/') {
                        $body = [IO.File]::ReadAllBytes($Installer)
                        $header = "HTTP/1.1 200 OK`r`nContent-Type: application/octet-stream`r`nContent-Length: $($body.Length)`r`nConnection: close`r`n`r`n"
                        $headerBytes = [Text.Encoding]::ASCII.GetBytes($header)
                        $stream.Write($headerBytes, 0, $headerBytes.Length)
                        $stream.Write($body, 0, $body.Length)
                    } else {
                        throw "unexpected request: $requestLine"
                    }
                    $stream.Flush()
                } finally {
                    $client.Dispose()
                }
            }
        } finally {
            if ($readyTemporary -and (Test-Path -LiteralPath $readyTemporary)) {
                Remove-Item -LiteralPath $readyTemporary -Force
            }
            $listener.Stop()
        }
    }

    for ($attempt = 0; $attempt -lt 100 -and -not (Test-Path -LiteralPath $ready); $attempt++) {
        Start-Sleep -Milliseconds 50
    }
    if (-not (Test-Path -LiteralPath $ready)) { throw 'loopback bootstrap server did not start' }

    $baseUrl = [IO.File]::ReadAllText($ready, [Text.Encoding]::ASCII)
    if ($baseUrl -notmatch '^http://127\.0\.0\.1:[1-9][0-9]*$') {
        throw "loopback bootstrap server published an invalid base URL: $baseUrl"
    }
    $repositoryRoot = Split-Path -Parent (Split-Path -Parent $installer)
    $packageVersion = [string](Get-Content -LiteralPath (Join-Path $repositoryRoot 'package.json') -Raw | ConvertFrom-Json).version
    Assert-Version $packageVersion
    $env:CRABCODE_VERSION = $packageVersion
    $env:CRABCODE_ASSET_DIR = Join-Path $scratch 'deliberately-missing-assets'
    $url = "$baseUrl/latest/download/install.ps1"
    $Error.Clear()
    $expectedFailure = $null
    try {
        $ErrorActionPreference = 'Continue'
        irm $url | iex
    } catch {
        $expectedFailure = $_
    } finally {
        $transportErrors = @($Error)
        $ErrorActionPreference = 'Stop'
    }
    if (-not $expectedFailure -or $expectedFailure.Exception.Message -notlike '*CRABCODE_ASSET_DIR is not a directory*') {
        throw "wire bootstrap did not reach the expected post-parse guard: $expectedFailure"
    }
    $commandErrors = @($transportErrors | Where-Object {
        $_.FullyQualifiedErrorId -like '*CommandNotFoundException*' -or
            $_.Exception.Message -like '*not recognized as the name of a cmdlet*'
    })
    if ($commandErrors.Count -ne 0) { throw "wire bootstrap exposed an encoding token: $($commandErrors[0])" }

    $completed = Wait-Job -Job $server -Timeout 10
    if (-not $completed -or $server.State -ne 'Completed') { throw "loopback bootstrap server did not complete: $($server.State)" }
    Receive-Job -Job $server -ErrorAction Stop
} finally {
    $ErrorActionPreference = $savedPreference
    if ($hadVersion) { $env:CRABCODE_VERSION = $savedVersion }
    else { Remove-Item Env:CRABCODE_VERSION -ErrorAction SilentlyContinue }
    if ($hadAssetDirectory) { $env:CRABCODE_ASSET_DIR = $savedAssetDirectory }
    else { Remove-Item Env:CRABCODE_ASSET_DIR -ErrorAction SilentlyContinue }
    if ($server) {
        Stop-Job -Job $server -ErrorAction SilentlyContinue
        Remove-Job -Job $server -Force -ErrorAction SilentlyContinue
    }
    if (Test-Path -LiteralPath $scratch) { Remove-Item -LiteralPath $scratch -Recurse -Force }
}
