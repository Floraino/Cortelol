param(
    [Parameter(Mandatory = $true)][string]$FfmpegArchive,
    [Parameter(Mandatory = $true)][string]$SourceArchive,
    [Parameter(Mandatory = $true)][string]$YtDlpExe,
    [Parameter(Mandatory = $true)][string]$DenoArchive,
    [ValidatePattern('^[A-Za-z0-9][A-Za-z0-9-]{0,39}$')][string]$CandidateLabel,
    [switch]$CheckOnly,
    [switch]$StageOnly
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($CheckOnly -and $StageOnly) { throw 'Choose either -CheckOnly or -StageOnly.' }

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
$downloader = Get-Content -LiteralPath (Join-Path $root 'legal/downloader-win64.json') -Raw | ConvertFrom-Json
if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) { throw 'Build on Windows x64.' }
if ([Runtime.InteropServices.RuntimeInformation]::OSArchitecture -ne [Runtime.InteropServices.Architecture]::X64) { throw 'The pinned build targets Windows x64 only.' }
if ($manifest.targetTriple -ne 'x86_64-pc-windows-msvc') { throw 'Unexpected target triple.' }
if ($downloader.targetTriple -ne $manifest.targetTriple) { throw 'Unexpected downloader target triple.' }
foreach ($path in @($FfmpegArchive, $SourceArchive, $YtDlpExe, $DenoArchive)) {
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Required archive is missing: $path" }
}
$ffmpegPath = (Resolve-Path -LiteralPath $FfmpegArchive).Path
$sourcePath = (Resolve-Path -LiteralPath $SourceArchive).Path
$ytDlpPath = (Resolve-Path -LiteralPath $YtDlpExe).Path
$denoPath = (Resolve-Path -LiteralPath $DenoArchive).Path
if ([IO.Path]::GetFileName($ffmpegPath) -ne $manifest.archiveName) { throw 'The FFmpeg archive filename does not match the pinned release.' }
if (-not $manifest.sourceArchiveName -or -not $manifest.sourceArchiveSha256) {
    throw 'Corresponding-source archive name and SHA-256 must be pinned in legal/ffmpeg-win64.json before packaging.'
}
if ([IO.Path]::GetFileName($sourcePath) -ne $manifest.sourceArchiveName) { throw 'Corresponding-source archive filename differs from the manifest.' }
Assert-Sha256 $ffmpegPath $manifest.archiveSha256 'FFmpeg binary archive'
Assert-Sha256 $sourcePath $manifest.sourceArchiveSha256 'Corresponding-source archive'
if ([IO.Path]::GetFileName($ytDlpPath) -ne $downloader.ytDlpName) { throw 'yt-dlp filename differs from the pinned release.' }
if ([IO.Path]::GetFileName($denoPath) -ne $downloader.denoArchiveName) { throw 'Deno archive filename differs from the pinned release.' }
Assert-Sha256 $ytDlpPath $downloader.ytDlpSha256 'yt-dlp executable'
Assert-Sha256 $denoPath $downloader.denoArchiveSha256 'Deno archive'
foreach ($notice in @(
    @('legal/YT-DLP-LICENSE.txt', $downloader.ytDlpLicenseSha256),
    @('legal/YT-DLP-THIRD-PARTY-LICENSES.txt', $downloader.ytDlpThirdPartySha256),
    @('legal/DENO-LICENSE.md', $downloader.denoLicenseSha256)
)) {
    Assert-Sha256 (Join-Path $root $notice[0]) $notice[1] $notice[0]
}
if ((Get-Item -LiteralPath $sourcePath).Length -lt 1MB) { throw 'Corresponding-source archive is unexpectedly small; review its contents.' }

Add-Type -AssemblyName System.IO.Compression.FileSystem

$denoZip = [System.IO.Compression.ZipFile]::OpenRead($denoPath)
try {
    $denoEntries = @($denoZip.Entries | Where-Object { -not $_.FullName.EndsWith('/') })
    if ($denoEntries.Count -ne 1 -or $denoEntries[0].FullName -ne 'deno.exe') {
        throw 'The pinned Deno archive must contain exactly one deno.exe entry.'
    }
    Assert-EntryHash $denoEntries[0] $downloader.denoExeSha256 'deno.exe'
} finally { $denoZip.Dispose() }
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
        Write-Output 'FFmpeg, source, yt-dlp, Deno, and downloader notices passed pinned hash checks. Review source completeness manually.'
        return
    }

    $binaries = Join-Path $root 'src-tauri/binaries'
    $legalStage = Join-Path $root 'src-tauri/release-legal'
    $ffmpegStage = Join-Path $binaries 'ffmpeg-x86_64-pc-windows-msvc.exe'
    $ffprobeStage = Join-Path $binaries 'ffprobe-x86_64-pc-windows-msvc.exe'
    $ytDlpStage = Join-Path $binaries 'yt-dlp-x86_64-pc-windows-msvc.exe'
    $denoStage = Join-Path $binaries 'deno-x86_64-pc-windows-msvc.exe'
    $licenseStage = Join-Path $legalStage 'FFMPEG-GPL.txt'
    New-Item -ItemType Directory -Force -Path $binaries, $legalStage | Out-Null
    $created = @()
    $keepStage = $false
    try {
        if (-not (Test-Path -LiteralPath $ffmpegStage)) { $created += $ffmpegStage; Copy-Entry $ffmpeg $ffmpegStage }
        if (-not (Test-Path -LiteralPath $ffprobeStage)) { $created += $ffprobeStage; Copy-Entry $ffprobe $ffprobeStage }
        if (-not (Test-Path -LiteralPath $ytDlpStage)) { $created += $ytDlpStage; Copy-Item -LiteralPath $ytDlpPath -Destination $ytDlpStage }
        if (-not (Test-Path -LiteralPath $denoStage)) {
            $denoZip = [System.IO.Compression.ZipFile]::OpenRead($denoPath)
            try { $created += $denoStage; Copy-Entry ($denoZip.GetEntry('deno.exe')) $denoStage }
            finally { $denoZip.Dispose() }
        }
        if (-not (Test-Path -LiteralPath $licenseStage)) { $created += $licenseStage; Copy-Entry $license $licenseStage }
        Assert-Sha256 $ffmpegStage $manifest.ffmpegSha256 'Staged ffmpeg executable'
        Assert-Sha256 $ffprobeStage $manifest.ffprobeSha256 'Staged ffprobe executable'
        Assert-Sha256 $ytDlpStage $downloader.ytDlpSha256 'Staged yt-dlp executable'
        Assert-Sha256 $denoStage $downloader.denoExeSha256 'Staged Deno executable'
        Assert-Sha256 $licenseStage $manifest.licenseSha256 'Staged FFmpeg GPL license'
        $ytDlpVersion = & $ytDlpStage --version 2>&1
        if ($LASTEXITCODE -ne 0 -or ($ytDlpVersion -join '').Trim() -ne $downloader.ytDlpVersion) { throw 'Pinned yt-dlp version check failed.' }
        $denoVersion = & $denoStage --version 2>&1
        if ($LASTEXITCODE -ne 0 -or $denoVersion[0] -ne "deno $($downloader.denoVersion) (stable, release, x86_64-pc-windows-msvc)") { throw 'Pinned Deno version check failed.' }
        $version = & $ffmpegStage -version 2>&1
        if ($LASTEXITCODE -ne 0 -or $version[0] -notlike "ffmpeg version $($manifest.version)*" -or ($version -join ' ') -notmatch '--enable-gpl' -or ($version -join ' ') -notmatch '--enable-libx264') {
            throw 'Pinned ffmpeg executable failed version/configuration verification.'
        }
        $encoders = & $ffmpegStage -hide_banner -encoders 2>&1
        if ($LASTEXITCODE -ne 0 -or ($encoders -join ' ') -notmatch 'libx264') { throw 'The pinned FFmpeg does not expose libx264.' }
        if ($StageOnly) {
            $keepStage = $true
            Write-Output 'Pinned Windows x64 sidecars and FFmpeg license staged. No npm, Tauri build, or installer was run.'
            return
        }
        Push-Location $root
        try {
            & npm ci
            if ($LASTEXITCODE -ne 0) { throw 'npm ci failed.' }
            & npm test
            if ($LASTEXITCODE -ne 0) { throw 'npm test failed.' }
            $buildStartedUtc = [DateTime]::UtcNow
            & npm run tauri -- build --config src-tauri/release.windows.conf.json
            if ($LASTEXITCODE -ne 0) { throw 'Tauri NSIS build failed.' }
        } finally { Pop-Location }
        $installers = @(Get-ChildItem -LiteralPath (Join-Path $root 'src-tauri/target/release/bundle/nsis') -Filter '*.exe' -File)
        if ($installers.Count -ne 1) { throw 'Expected exactly one NSIS installer output.' }
        if ($installers[0].LastWriteTimeUtc -lt $buildStartedUtc) {
            throw 'NSIS installer predates this build; refusing to stage a stale output.'
        }
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
        Copy-Item -LiteralPath (Join-Path $root 'legal/DOWNLOADER-WINDOWS.md') -Destination $output
        Copy-Item -LiteralPath (Join-Path $root 'legal/downloader-win64.json') -Destination $output
        Copy-Item -LiteralPath (Join-Path $root 'legal/YT-DLP-LICENSE.txt') -Destination $output
        Copy-Item -LiteralPath (Join-Path $root 'legal/YT-DLP-THIRD-PARTY-LICENSES.txt') -Destination $output
        Copy-Item -LiteralPath (Join-Path $root 'legal/DENO-LICENSE.md') -Destination $output
        Get-ChildItem -LiteralPath $output -File | Get-FileHash -Algorithm SHA256 |
            ForEach-Object { '{0}  {1}' -f $_.Hash.ToLowerInvariant(), (Split-Path $_.Path -Leaf) } |
            Set-Content -LiteralPath (Join-Path $output 'SHA256SUMS.txt') -Encoding ascii
        Write-Output "NSIS candidate and source package staged at $output. Publish only after clean Windows VM acceptance, dependency notice review, and source-compliance sign-off."
    } finally {
        if (-not $keepStage) { foreach ($stage in $created) { Remove-Item -LiteralPath $stage -Force } }
    }
} finally {
    $zip.Dispose()
}
