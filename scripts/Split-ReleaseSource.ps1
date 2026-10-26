param(
    [Parameter(Mandatory = $true)]
    [string]$SourceArchive,
    [Parameter(Mandatory = $true)]
    [string]$OutputDirectory,
    [string]$ExpectedSha256 = '3647a98eb668736fee85a92e14cc0f379213e7051983040e2260beae8ccec675',
    [long]$ExpectedLength = 2224619090,
    [ValidateRange(1, 1900000000)]
    [long]$PartSizeBytes = 1900000000
)

$ErrorActionPreference = 'Stop'

if ($ExpectedLength -lt 1 -or $ExpectedSha256 -notmatch '^[0-9a-fA-F]{64}$') {
    throw 'ExpectedLength must be positive and ExpectedSha256 must be a 64-character SHA-256 digest.'
}

$source = (Resolve-Path -LiteralPath $SourceArchive -ErrorAction Stop).ProviderPath
$sourceInfo = Get-Item -LiteralPath $source
if ($sourceInfo.PSIsContainer) { throw 'SourceArchive must be a file.' }
if ($sourceInfo.Length -ne $ExpectedLength) {
    throw "Source archive length mismatch: expected $ExpectedLength bytes, found $($sourceInfo.Length)."
}

$actualHash = (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash.ToLowerInvariant()
if ($actualHash -ne $ExpectedSha256.ToLowerInvariant()) {
    throw "Source archive SHA-256 mismatch: expected $ExpectedSha256, found $actualHash."
}

if (-not (Test-Path -LiteralPath $OutputDirectory -PathType Container)) {
    throw 'OutputDirectory must already exist. Create an empty release-artifacts subdirectory first.'
}
$output = (Resolve-Path -LiteralPath $OutputDirectory).ProviderPath
$partCount = [int][math]::Ceiling([double]$ExpectedLength / [double]$PartSizeBytes)
$archiveName = [System.IO.Path]::GetFileName($source)
$parts = @(
    for ($index = 1; $index -le $partCount; $index++) {
        Join-Path $output ('{0}.part{1:D3}' -f $archiveName, $index)
    }
)
$checksumsPath = Join-Path $output ($archiveName + '.parts.sha256')
foreach ($path in @($parts) + @($checksumsPath)) {
    if (Test-Path -LiteralPath $path) { throw "Refusing to overwrite existing output: $path" }
}

$created = New-Object 'System.Collections.Generic.List[string]'
$inputStream = $null
$wholeHash = [System.Security.Cryptography.SHA256]::Create()
try {
    $inputStream = New-Object System.IO.FileStream($source, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::Read)
    $buffer = New-Object byte[] (4 * 1024 * 1024)
    $lines = New-Object 'System.Collections.Generic.List[string]'

    foreach ($partPath in $parts) {
        $remaining = [math]::Min($PartSizeBytes, $inputStream.Length - $inputStream.Position)
        $partStream = $null
        try {
            $partStream = New-Object System.IO.FileStream($partPath, [System.IO.FileMode]::CreateNew, [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
            $created.Add($partPath)
            while ($remaining -gt 0) {
                $toRead = [int][math]::Min($buffer.Length, $remaining)
                $read = $inputStream.Read($buffer, 0, $toRead)
                if ($read -eq 0) { throw 'Source archive ended during splitting.' }
                $partStream.Write($buffer, 0, $read)
                [void]$wholeHash.TransformBlock($buffer, 0, $read, $buffer, 0)
                $remaining -= $read
            }
        }
        finally {
            if ($null -ne $partStream) { $partStream.Dispose() }
        }
        $partHash = (Get-FileHash -LiteralPath $partPath -Algorithm SHA256).Hash.ToLowerInvariant()
        $partName = [System.IO.Path]::GetFileName($partPath)
        $lines.Add("$partHash  $partName")
        Write-Host "Created $partName ($((Get-Item -LiteralPath $partPath).Length) bytes; SHA-256 $partHash)"
    }

    if ($inputStream.Position -ne $ExpectedLength) { throw 'Source archive length changed during splitting.' }
    [void]$wholeHash.TransformFinalBlock([byte[]]@(), 0, 0)
    $splitInputHash = ([System.BitConverter]::ToString($wholeHash.Hash)).Replace('-', '').ToLowerInvariant()
    if ($splitInputHash -ne $actualHash) { throw 'Source archive changed between verification and splitting.' }

    $lines.Add("$actualHash  $archiveName")
    $utf8 = New-Object System.Text.UTF8Encoding($false)
    $checksumStream = New-Object System.IO.FileStream($checksumsPath, [System.IO.FileMode]::CreateNew, [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
    $created.Add($checksumsPath)
    try {
        $writer = New-Object System.IO.StreamWriter($checksumStream, $utf8)
        try { foreach ($line in $lines) { $writer.WriteLine($line) } }
        finally { $writer.Dispose() }
    }
    finally { $checksumStream.Dispose() }
    Write-Host "Checksums: $checksumsPath"
}
catch {
    foreach ($path in $created) {
        if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path -Force }
    }
    throw
}
finally {
    if ($null -ne $inputStream) { $inputStream.Dispose() }
    $wholeHash.Dispose()
}
