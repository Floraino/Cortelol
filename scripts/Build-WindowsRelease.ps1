param(
    [Parameter(Mandatory = $true)][string]$FfmpegArchive,
    [Parameter(Mandatory = $true)][string]$SourceArchive,
    [ValidatePattern('^[A-Za-z0-9][A-Za-z0-9-]{0,39}$')][string]$CandidateLabel,
    [switch]$CheckOnly
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Assert-Sha256([string]$Path, [string]$Expected, [string]$Label) {
    if ($Expected -notmatch '^[0-9a-fA-F]{64}$') {
        throw "$Label has no pinned SHA-256 in legal/ffmpeg-win64.json."
    }
    $actual = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $Expected.ToLowerInvariant()) {
        throw "$Label SHA-256 mismatch: expected $Expected, got $actual."
    }
}

function Get-Entry([System.IO.Compression.ZipArchive]$Zip, [string]$RelativePath) {
    $matches = @($Zip.Entries | Where-Object { $_.FullName.EndsWith("/$RelativePath", [StringComparison]::Ordinal) })
    if ($matches.Count -ne 1) {
        throw "Expected exactly one $RelativePath in the pinned FFmpeg archive."
    }
    return $matches[0]
}

function Assert-EntryHash([System.IO.Compression.ZipArchiveEntry]$Entry, [string]$Expected, [string]$Label) {
    $sha = [System.Security.Cryptography.SHA256]::Create()
    $stream = $Entry.Open()
    try {
        $actual = ([BitConverter]::ToString($sha.ComputeHash($stream))).Replace('-', '').ToLowerInvariant()
        if ($actual -ne $Expected) {
            throw "$Label SHA-256 mismatch: expected $Expected, got $actual."
        }
    } finally {
        $stream.Dispose()
        $sha.Dispose()
    }
}

function Copy-Entry([System.IO.Compression.ZipArchiveEntry]$Entry, [string]$Destination) {
    $inputStream = $Entry.Open()
    $outputStream = [System.IO.File]::Create($Destination)
    try { $inputStream.CopyTo($outputStream) }
    finally {
        $outputStream.Dispose()
        $inputStream.Dispose()
    }
}

$root = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$manifest = Get-Content -LiteralPath (Join-Path $root 'legal/ffmpeg-win64.json') -Raw | ConvertFrom-Json
if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) { throw 'Build on Windows x64.' }
if ([Runtime.InteropServices.RuntimeInformation]::OSArchitecture -ne [Runtime.InteropServices.Architecture]::X64) { throw 'The pinned build targets Windows x64 only.' }
if ($manifest.targetTriple -ne 'x86_64-pc-windows-msvc') { throw 'Unexpected target triple.' }
foreach ($path in @($FfmpegArchive, $SourceArchive)) {
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Required archive is missing: $path" }
}
$ffmpegPath = (Resolve-Path -LiteralPath $FfmpegArchive).Path
$sourcePath = (Resolve-Path -LiteralPath $SourceArchive).Path
if ([IO.Path]::GetFileName($ffmpegPath) -ne $manifest.archiveName) { throw 'The FFmpeg archive filename does not match the pinned release.' }
if (-not $manifest.sourceArchiveName -or -not $manifest.sourceArchiveSha256) {
    throw 'Corresponding-source archive name and SHA-256 must be pinned in legal/ffmpeg-win64.json before packaging.'
}
if ([IO.Path]::GetFileName($sourcePath) -ne $manifest.sourceArchiveName) { throw 'Corresponding-source archive filename differs from the manifest.' }
Assert-Sha256 $ffmpegPath $manifest.archiveSha256 'FFmpeg binary archive'
Assert-Sha256 $sourcePath $manifest.sourceArchiveSha256 'Corresponding-source archive'
if ((Get-Item -LiteralPath $sourcePath).Length -lt 1MB) { throw 'Corresponding-source archive is unexpectedly small; review its contents.' }

Add-Type -AssemblyName System.IO.Compression.FileSystem
$sourceZip = [System.IO.Compression.ZipFile]::OpenRead($sourcePath)
try {
    $sourceNames = @($sourceZip.Entries | ForEach-Object { $_.FullName })
    foreach ($required in @(
        'SOURCE-MANIFEST.md',
        'FFMPEG-BUILDCONF.txt',
        "ffmpeg/ffmpeg-$($manifest.ffmpegSourceCommit).tar.gz",
        'x264/source.tar.xz',
        'BtbN-FFmpeg-Builds/build.sh',
        'BtbN-FFmpeg-Builds/download.sh',
        'BtbN-FFmpeg-Builds/scripts.d/50-x264.sh'
    )) {
        if ($sourceNames -notcontains $required) {
            throw "Corresponding-source archive lacks required path: $required"
        }
    }
    if (@($sourceNames | Where-Object { $_.StartsWith('dependency-sources/', [StringComparison]::Ordinal) }).Count -lt 50) {
        throw 'Corresponding-source archive has too few dependency source tarballs.'
    }
    $sourceManifestEntry = $sourceZip.GetEntry('SOURCE-MANIFEST.md')
    $reader = New-Object System.IO.StreamReader($sourceManifestEntry.Open())
    try { $sourceText = $reader.ReadToEnd() } finally { $reader.Dispose() }
    foreach ($expected in @($manifest.archiveSha256, $manifest.buildScriptsCommit, $manifest.ffmpegSourceCommit)) {
        if (-not $sourceText.Contains($expected)) { throw "Corresponding-source manifest does not identify $expected" }
    }
    Assert-EntryHash ($sourceZip.GetEntry("ffmpeg/ffmpeg-$($manifest.ffmpegSourceCommit).tar.gz")) $manifest.ffmpegSourceSha256 'FFmpeg source tarball'
} finally { $sourceZip.Dispose() }

$zip = [System.IO.Compression.ZipFile]::OpenRead($ffmpegPath)
try {
    $ffmpeg = Get-Entry $zip 'bin/ffmpeg.exe'
    $ffprobe = Get-Entry $zip 'bin/ffprobe.exe'
    $license = Get-Entry $zip 'LICENSE.txt'
    Assert-EntryHash $ffmpeg $manifest.ffmpegSha256 'ffmpeg.exe'
    Assert-EntryHash $ffprobe $manifest.ffprobeSha256 'ffprobe.exe'
    Assert-EntryHash $license $manifest.licenseSha256 'FFmpeg GPL license'
    if ($CheckOnly) {
        Write-Output 'Release media inputs and corresponding-source archive passed pinned hash checks. Review source completeness manually.'
        return
    }

    $binaries = Join-Path $root 'src-tauri/binaries'
    $legalStage = Join-Path $root 'src-tauri/release-legal'
    $ffmpegStage = Join-Path $binaries 'ffmpeg-x86_64-pc-windows-msvc.exe'
    $ffprobeStage = Join-Path $binaries 'ffprobe-x86_64-pc-windows-msvc.exe'
    $licenseStage = Join-Path $legalStage 'FFMPEG-GPL.txt'
    foreach ($stage in @($ffmpegStage, $ffprobeStage, $licenseStage)) {
        if (Test-Path -LiteralPath $stage) { throw "Refusing to replace existing staged file: $stage" }
    }
    New-Item -ItemType Directory -Force -Path $binaries, $legalStage | Out-Null
    $created = @()
    try {
        $created += $ffmpegStage; Copy-Entry $ffmpeg $ffmpegStage
        $created += $ffprobeStage; Copy-Entry $ffprobe $ffprobeStage
        $created += $licenseStage; Copy-Entry $license $licenseStage
        $version = & $ffmpegStage -version 2>&1
        if ($LASTEXITCODE -ne 0 -or $version[0] -notlike "ffmpeg version $($manifest.version)*" -or ($version -join ' ') -notmatch '--enable-gpl' -or ($version -join ' ') -notmatch '--enable-libx264') {
            throw 'Pinned ffmpeg executable failed version/configuration verification.'
        }
        $encoders = & $ffmpegStage -hide_banner -encoders 2>&1
        if ($LASTEXITCODE -ne 0 -or ($encoders -join ' ') -notmatch 'libx264') { throw 'The pinned FFmpeg does not expose libx264.' }
        Push-Location $root
        try {
            & npm ci
            if ($LASTEXITCODE -ne 0) { throw 'npm ci failed.' }
            & npm test
            if ($LASTEXITCODE -ne 0) { throw 'npm test failed.' }
            & npm run tauri -- build --config src-tauri/release.windows.conf.json
            if ($LASTEXITCODE -ne 0) { throw 'Tauri NSIS build failed.' }
        } finally { Pop-Location }
        $installers = @(Get-ChildItem -LiteralPath (Join-Path $root 'src-tauri/target/release/bundle/nsis') -Filter '*.exe' -File)
        if ($installers.Count -ne 1) { throw 'Expected exactly one NSIS installer output.' }
        $candidateDirectory = if ($CandidateLabel) { "$($manifest.version)-$CandidateLabel" } else { $manifest.version }
        $output = Join-Path $root "release-artifacts/$candidateDirectory"
        if (Test-Path -LiteralPath $output) { throw "Refusing to overwrite release staging directory: $output" }
        New-Item -ItemType Directory -Path $output -Force | Out-Null
        Copy-Item -LiteralPath $installers[0].FullName -Destination $output
        Copy-Item -LiteralPath $sourcePath -Destination $output
        Copy-Item -LiteralPath (Join-Path $root 'LICENSE') -Destination $output
        Copy-Item -LiteralPath (Join-Path $root 'legal/FFMPEG-WINDOWS.md') -Destination $output
        Copy-Item -LiteralPath (Join-Path $root 'legal/ffmpeg-win64.json') -Destination $output
        Copy-Item -LiteralPath (Join-Path $root 'legal/FFMPEG-BUILDCONF.txt') -Destination $output
        Copy-Item -LiteralPath $licenseStage -Destination $output
        Get-ChildItem -LiteralPath $output -File | Get-FileHash -Algorithm SHA256 |
            ForEach-Object { '{0}  {1}' -f $_.Hash.ToLowerInvariant(), (Split-Path $_.Path -Leaf) } |
            Set-Content -LiteralPath (Join-Path $output 'SHA256SUMS.txt') -Encoding ascii
        Write-Output "NSIS candidate and source package staged at $output. Publish only after clean Windows VM acceptance, dependency notice review, and source-compliance sign-off."
    } finally {
        foreach ($stage in $created) { Remove-Item -LiteralPath $stage -Force }
    }
} finally {
    $zip.Dispose()
}
