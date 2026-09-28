param(
    [Parameter(Mandatory = $true)]
    [string]$Path
)

$ErrorActionPreference = 'Stop'
$bytes = [IO.File]::ReadAllBytes((Resolve-Path -LiteralPath $Path))

function U16([int]$offset) {
    [BitConverter]::ToUInt16($bytes, $offset)
}

function U32([int]$offset) {
    [BitConverter]::ToUInt32($bytes, $offset)
}

$pe = [int](U32 0x3c)
if ((U32 $pe) -ne 0x00004550) {
    throw 'Not a PE image'
}

$machine = U16 ($pe + 4)
$count = U16 ($pe + 6)
$optionalSize = U16 ($pe + 20)
$optional = $pe + 24
$magic = U16 $optional
$directory = if ($magic -eq 0x20b) {
    $optional + 112
}
elseif ($magic -eq 0x10b) {
    $optional + 96
}
else {
    throw 'Unsupported PE header'
}
$sections = $optional + $optionalSize

function RvaOffset([uint32]$rva) {
    for ($i = 0; $i -lt $count; $i++) {
        $section = $sections + 40 * $i
        $start = U32 ($section + 12)
        $length = [Math]::Max((U32 ($section + 8)), (U32 ($section + 16)))

        if ($rva -ge $start -and $rva -lt $start + $length) {
            return [int]($rva - $start + (U32 ($section + 20)))
        }
    }

    throw "RVA cannot be resolved: $rva"
}

function ReadAscii([int]$offset) {
    $end = $offset
    while ($end -lt $bytes.Length -and $bytes[$end] -ne 0) {
        $end++
    }

    [Text.Encoding]::ASCII.GetString($bytes, $offset, $end - $offset)
}

$imports = [Collections.Generic.List[string]]::new()
$importRva = U32 ($directory + 8)
if ($importRva -ne 0) {
    $entry = RvaOffset $importRva
    while ((U32 ($entry + 12)) -ne 0) {
        $imports.Add((ReadAscii (RvaOffset (U32 ($entry + 12)))))
        $entry += 20
    }
}

$clrRva = U32 ($directory + 14 * 8)
$delayRva = U32 ($directory + 13 * 8)
if ($delayRva -ne 0) {
    throw 'Audit delay-load imports before distribution'
}

$crtImports = @(
    $imports |
        Where-Object {
            $_ -match '^(api-ms-win|ext-ms-win)-crt-' -or
            $_ -match '^(ucrtbase|msvcrt|vcruntime)[^\\]*\.dll$'
        }
)
if ($crtImports.Count) {
    throw "Release must not import the dynamic CRT: $($crtImports -join ', ')"
}

$subsystem = U16 ($optional + 68)
$unexpected = @(
    $imports |
        Where-Object {
            $_ -notmatch '^(api-ms-win-|ext-ms-win-|KERNEL32\.dll$|USER32\.dll$|GDI32\.dll$|ADVAPI32\.dll$|SHELL32\.dll$|SHLWAPI\.dll$|OLE32\.dll$|OLEAUT32\.dll$|COMCTL32\.dll$|COMDLG32\.dll$|UXTHEME\.dll$|WINMM\.dll$|XINPUT1_4\.dll$|BCRYPT\.dll$|BCRYPTPRIMITIVES\.dll$|NTDLL\.dll$|USERENV\.dll$|WS2_32\.dll$)'
        }
)

if ($clrRva -ne 0) {
    throw 'Executable contains a CLR header'
}
if ($machine -ne 0x8664) {
    throw 'Release must target x86_64'
}
if ($subsystem -ne 2) {
    throw 'Release must use the Windows GUI subsystem'
}
if ($unexpected.Count) {
    throw "Unreviewed runtime dependencies: $($unexpected -join ', ')"
}

[pscustomobject]@{
    Machine = 'AMD64';
    Subsystem = 'Windows GUI';
    ClrHeader = $false;
    Imports = @($imports | Sort-Object);
    FileBytes = $bytes.Length;
    Sha256 = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash
}
