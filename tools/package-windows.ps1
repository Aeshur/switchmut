[CmdletBinding()]
param(
    [ValidateSet('Headless', 'Interactive')]
    [string]$Mode = 'Headless',
    [switch]$NativeChecks,
    [switch]$LiveGameCheck
)

$ErrorActionPreference = 'Stop'
$project = Split-Path -Parent $PSScriptRoot

function Invoke-CargoChecked {
    param(
        [Parameter(Mandatory = $true)]
        [string[]]$Arguments,
        [Parameter(Mandatory = $true)]
        [string]$Failure
    )
    & cargo @Arguments
    if ($LASTEXITCODE) {
        throw $Failure
    }
}

Push-Location -LiteralPath $project
try {
    Invoke-CargoChecked @('fmt', '--all', '--', '--check') 'Formatting failed'
    Invoke-CargoChecked @('clippy', '--locked', '--all-targets', '--', '-D', 'warnings') 'Clippy failed'
    Invoke-CargoChecked @('test', '--locked', '--all-targets') 'Tests failed'

    if ($NativeChecks) {
        Invoke-CargoChecked @('test', '--locked', '--test', 'native_windows', 'native_lifecycle', '--', '--ignored', '--test-threads=1', '--nocapture') 'Native integration checks failed'
    }

    if ($LiveGameCheck) {
        Invoke-CargoChecked @('run', '--locked', '--example', 'live_probe') 'Live game probe failed'
    }

    Invoke-CargoChecked @('build', '--locked', '--release', '--bin', 'Switchmut') 'Release build failed'

    $metadata = cargo metadata --locked --format-version 1 | ConvertFrom-Json
    if ($LASTEXITCODE) {
        throw 'Cargo metadata failed'
    }
    $package = @($metadata.packages | Where-Object { $_.name -eq 'switchmut' })[0]
    $targetTriple = 'x86_64-pc-windows-msvc'
    $targetDirectory = [string]$metadata.target_directory
    $source = Join-Path $targetDirectory "$targetTriple/release/Switchmut.exe"
    if (!(Test-Path -LiteralPath $source -PathType Leaf)) {
        throw "Cargo did not produce the expected $targetTriple executable at '$source'"
    }
    $sourceCommit = (& git rev-parse HEAD).Trim()
    if ($LASTEXITCODE) { throw 'Could not read the source commit' }
    $sourceStatus = @(& git status --porcelain=v1)
    if ($LASTEXITCODE) { throw 'Could not read the source status' }
    $stamp = Get-Date -Format yyyyMMdd-HHmmss
    $release = Join-Path $project "release/Switchmut-$($package.version)-$stamp-$PID"
    $stage = Join-Path $release 'package'
    $extracted = Join-Path $release 'extracted'
    New-Item -ItemType Directory -Path $stage, $extracted -Force | Out-Null

    Copy-Item -LiteralPath $source -Destination (Join-Path $stage 'Switchmut.exe')

    $pe = & (Join-Path $PSScriptRoot 'inspect-pe.ps1') -Path (Join-Path $stage 'Switchmut.exe')
    $pe | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $release 'pe-report.json') -Encoding utf8
    $versionInfo = [Diagnostics.FileVersionInfo]::GetVersionInfo((Join-Path $stage 'Switchmut.exe'))
    $expectedWindowsVersion = "$($package.version).0"
    if ($versionInfo.FileVersion -ne $expectedWindowsVersion -or $versionInfo.ProductVersion -ne $expectedWindowsVersion) {
        throw "Windows version metadata does not match ${expectedWindowsVersion}: file '$($versionInfo.FileVersion)', product '$($versionInfo.ProductVersion)'"
    }
    if ($versionInfo.OriginalFilename -cne 'Switchmut.exe' -or $versionInfo.InternalName -cne 'Switchmut' -or $versionInfo.ProductName -cne 'Switchmut') {
        throw 'Windows executable metadata must use the Switchmut product name'
    }

    $expectedFiles = @('Switchmut.exe')
    $actualFiles = @(
        Get-ChildItem -LiteralPath $stage -Recurse -File | ForEach-Object {
            $_.FullName.Substring($stage.Length + 1).Replace('\', '/')
        }
    ) | Sort-Object
    if (Compare-Object ($expectedFiles | Sort-Object) $actualFiles -CaseSensitive) {
        throw "Package inventory mismatch: expected $($expectedFiles -join ', '), found $($actualFiles -join ', ')"
    }

    $archive = Join-Path $release 'Switchmut-windows.zip'
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    [IO.Compression.ZipFile]::CreateFromDirectory(
        $stage,
        $archive,
        [IO.Compression.CompressionLevel]::Optimal,
        $false
    )
    $archiveItem = Get-Item -LiteralPath $archive
    if ($archiveItem.Length -ge 50000000) {
        throw 'ZIP must be below 50,000,000 bytes'
    }

    [IO.Compression.ZipFile]::ExtractToDirectory($archive, $extracted)
    $checks = @(
        Get-ChildItem -LiteralPath $stage -Recurse -File | Sort-Object FullName |
            ForEach-Object {
                $relative = $_.FullName.Substring($stage.Length).TrimStart([IO.Path]::DirectorySeparatorChar)
                $testFile = Join-Path $extracted $relative
                $expected = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash
                $actual = (Get-FileHash -LiteralPath $testFile -Algorithm SHA256).Hash
                if ($expected -ne $actual) {
                    throw "Extracted file differs: $relative"
                }
                [ordered]@{ Path = $relative; Bytes = $_.Length; Sha256 = $actual }
            }
    )
    if (@(Get-ChildItem -LiteralPath $extracted -Recurse -File).Count -ne $checks.Count) {
        throw 'Unexpected extracted files'
    }

    $interactiveResult = 'not-run'
    if ($Mode -eq 'Interactive') {
        Invoke-CargoChecked @('run', '--locked', '--example', 'release_smoke', '--', (Join-Path $extracted 'Switchmut.exe')) 'Extracted executable smoke check failed'
        $interactiveResult = 'passed'
    }

    $rustc = (rustc -Vv | Out-String).Trim()
    if ($LASTEXITCODE) { throw 'Could not read the active Rust toolchain' }
    $report = [ordered]@{
        Version = $package.version
        Mode = $Mode
        TargetTriple = $targetTriple
        Executable = 'extracted/Switchmut.exe'
        ExecutableBytes = $pe.FileBytes
        ExecutableSha256 = $pe.Sha256
        FileVersion = $versionInfo.FileVersion
        ProductVersion = $versionInfo.ProductVersion
        Archive = (Split-Path -Leaf $archive)
        ArchiveBytes = $archiveItem.Length
        ArchiveSha256 = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash
        Source = [ordered]@{
            Commit = $sourceCommit
            Dirty = $sourceStatus.Count -gt 0
        }
        Validation = [ordered]@{
            Formatting = 'passed'
            Clippy = 'passed'
            UnitTests = 'passed'
            NativeChecks = if ($NativeChecks) { 'passed' } else { 'not-run' }
            LiveGameCheck = if ($LiveGameCheck) { 'passed' } else { 'not-run' }
            ExtractionHashes = 'passed'
            ExtractedApplicationSmoke = $interactiveResult
        }
        Rustc = $rustc
        Pe = $pe
        Files = $checks
    }
    $manifest = Join-Path $release 'Switchmut-release-manifest.json'
    $report | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $manifest -Encoding utf8

    if ($env:GITHUB_OUTPUT) {
        "archive=$archive" | Add-Content -LiteralPath $env:GITHUB_OUTPUT -Encoding utf8
        "manifest=$manifest" | Add-Content -LiteralPath $env:GITHUB_OUTPUT -Encoding utf8
    }

    [pscustomobject]$report |
        Select-Object Version, Mode, Executable, ExecutableBytes, ExecutableSha256, Archive, ArchiveBytes, ArchiveSha256, Validation, Pe |
        ConvertTo-Json -Depth 6
}
finally {
    Pop-Location
}
