[CmdletBinding()]
param(
    [switch]$Interactive,
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

function Write-ThirdPartyNotices {
    param($Metadata, $RootPackage, [string]$Path)

    $packages = @{}
    foreach ($item in $Metadata.packages) { $packages[$item.id] = $item }
    $nodes = @{}
    foreach ($node in $Metadata.resolve.nodes) { $nodes[$node.id] = $node }
    $visited = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    $pending = [Collections.Generic.Stack[string]]::new()
    $pending.Push($RootPackage.id)
    $ordered = @{}
    $keys = [Collections.Generic.List[string]]::new()
    while ($pending.Count) {
        $id = $pending.Pop()
        if (!$visited.Add($id)) { continue }
        $item = $packages[$id]
        # Normal dependencies can be proc macros whose code runs only at build time.
        if (@($item.targets | Where-Object { $_.kind -contains 'proc-macro' }).Count) { continue }
        # Switchmut's own license ships separately as LICENSE.txt.
        if ($id -cne $RootPackage.id) {
            $key = "$($item.name)`n$($item.version)`n$id"
            $ordered[$key] = $item
            $keys.Add($key)
        }
        foreach ($dependency in $nodes[$id].deps) {
            if (@($dependency.dep_kinds | Where-Object { $null -eq $_.kind }).Count) {
                $pending.Push($dependency.pkg)
            }
        }
    }
    $keys.Sort([StringComparer]::Ordinal)

    $text = [Text.StringBuilder]::new()
    [void]$text.Append("Switchmut is AGPL-3.0-or-later.`nCorresponding source: https://github.com/Aeshur/switchmut`n`n")
    foreach ($key in $keys) {
        $item = $ordered[$key]
        $directory = Split-Path -Parent $item.manifest_path
        $files = @{}
        $names = [Collections.Generic.List[string]]::new()
        foreach ($file in Get-ChildItem -LiteralPath $directory -File) {
            if ($file.Name -match '^(LICENSE|LICENCE|COPYING|NOTICE)') {
                $files[$file.Name] = $file.FullName
                $names.Add($file.Name)
            }
        }
        if (!@($names | Where-Object { $_ -match '^(LICENSE|LICENCE|COPYING)' }).Count) {
            throw "No license file for $($item.name) $($item.version) in '$directory'"
        }
        $names.Sort([StringComparer]::Ordinal)
        [void]$text.Append("Name: $($item.name)`nVersion: $($item.version)`nLicense: $($item.license)`nRepository: $($item.repository)`n")
        foreach ($name in $names) {
            [void]$text.Append("`n--- $name ---`n")
            $contents = [IO.File]::ReadAllText($files[$name]).Replace("`r`n", "`n").Replace("`r", "`n")
            [void]$text.Append($contents)
            if (!$contents.EndsWith("`n")) { [void]$text.Append("`n") }
        }
        [void]$text.Append("`n")
    }
    [IO.File]::WriteAllText($Path, $text.ToString(), [Text.UTF8Encoding]::new($false))
}

Push-Location -LiteralPath $project
try {
    Invoke-CargoChecked @('fmt', '--all', '--', '--check') 'Formatting failed'
    Invoke-CargoChecked @('clippy', '--locked', '--all-targets', '--', '-D', 'warnings') 'Clippy failed'
    Invoke-CargoChecked @('test', '--locked', '--all-targets') 'Tests failed'

    if ($Interactive) {
        Invoke-CargoChecked @('test', '--locked', '--test', 'native_windows', 'native_lifecycle', '--', '--ignored', '--test-threads=1', '--nocapture') 'Native integration checks failed'
    }

    if ($LiveGameCheck) {
        Invoke-CargoChecked @('run', '--locked', '--example', 'live_probe') 'Live game probe failed'
    }

    Invoke-CargoChecked @('build', '--locked', '--release', '--bin', 'Switchmut') 'Release build failed'

    $targetTriple = 'x86_64-pc-windows-msvc'
    $metadataJson = & cargo metadata --locked --format-version 1 --filter-platform $targetTriple
    if ($LASTEXITCODE) {
        throw 'Cargo metadata failed'
    }
    $metadata = ($metadataJson -join "`n") | ConvertFrom-Json
    $package = @($metadata.packages | Where-Object { $_.name -eq 'switchmut' })[0]
    $targetDirectory = [string]$metadata.target_directory
    $source = Join-Path $targetDirectory "$targetTriple/release/Switchmut.exe"
    if (!(Test-Path -LiteralPath $source -PathType Leaf)) {
        throw "Cargo did not produce the expected $targetTriple executable at '$source'"
    }
    $sourceCommitOutput = & git rev-parse HEAD
    if ($LASTEXITCODE) { throw 'Could not read the source commit' }
    $sourceCommit = ($sourceCommitOutput -join "`n").Trim()
    if (!$sourceCommit) { throw 'Git returned an empty source commit' }
    $sourceStatus = @(& git status --porcelain=v1)
    if ($LASTEXITCODE) { throw 'Could not read the source status' }
    $stamp = Get-Date -Format yyyyMMdd-HHmmss
    $release = Join-Path $project "release/Switchmut-$($package.version)-$stamp-$PID"
    $stage = Join-Path $release 'package'
    $extracted = Join-Path $release 'extracted'
    New-Item -ItemType Directory -Path $stage, $extracted -Force | Out-Null

    Copy-Item -LiteralPath $source -Destination (Join-Path $stage 'Switchmut.exe')
    Copy-Item -LiteralPath (Join-Path $project 'LICENSE') -Destination (Join-Path $stage 'LICENSE.txt')
    Write-ThirdPartyNotices $metadata $package (Join-Path $stage 'THIRD-PARTY-NOTICES.txt')

    $pe = & (Join-Path $PSScriptRoot 'inspect-pe.ps1') -Path (Join-Path $stage 'Switchmut.exe')
    $versionInfo = [Diagnostics.FileVersionInfo]::GetVersionInfo((Join-Path $stage 'Switchmut.exe'))
    $expectedWindowsVersion = "$($package.version).0"
    if ($versionInfo.FileVersion -ne $expectedWindowsVersion -or $versionInfo.ProductVersion -ne $expectedWindowsVersion) {
        throw "Windows version metadata does not match ${expectedWindowsVersion}: file '$($versionInfo.FileVersion)', product '$($versionInfo.ProductVersion)'"
    }
    if ($versionInfo.OriginalFilename -cne 'Switchmut.exe' -or $versionInfo.InternalName -cne 'Switchmut' -or $versionInfo.ProductName -cne 'Switchmut') {
        throw 'Windows executable metadata must use the Switchmut product name'
    }

    $expectedFiles = @('Switchmut.exe', 'LICENSE.txt', 'THIRD-PARTY-NOTICES.txt')
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

    $interactiveResult = 'not-run'
    if ($Interactive) {
        Invoke-CargoChecked @('run', '--locked', '--example', 'release_smoke', '--', (Join-Path $extracted 'Switchmut.exe')) 'Extracted executable smoke check failed'
        $interactiveResult = 'passed'
    }

    $rustc = (rustc -Vv | Out-String).Trim()
    if ($LASTEXITCODE) { throw 'Could not read the active Rust toolchain' }
    $report = [ordered]@{
        Version = $package.version
        Mode = if ($Interactive) { 'Interactive' } else { 'Headless' }
        TargetTriple = $targetTriple
        Executable = 'package/Switchmut.exe'
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
            NativeChecks = $interactiveResult
            LiveGameCheck = if ($LiveGameCheck) { 'passed' } else { 'not-run' }
            ExtractedApplicationSmoke = $interactiveResult
        }
        Rustc = $rustc
        Pe = $pe
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
