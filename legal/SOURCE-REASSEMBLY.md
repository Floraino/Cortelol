# Reassembling the Windows FFmpeg corresponding source

The Windows installer bundles a pinned GPL build of FFmpeg. Its exact corresponding-source ZIP is 2,224,619,090 bytes, so a release distributes it as numbered parts, each at most 1,900,000,000 bytes. Download **every** `ffmpeg-corresponding-source.zip.partNNN` file and `ffmpeg-corresponding-source.zip.parts.sha256` from the same release. Keep them in one directory. Do not extract individual parts.

The complete archive must have SHA-256:

```text
3647a98eb668736fee85a92e14cc0f379213e7051983040e2260beae8ccec675
```

## Windows PowerShell

Run this in the directory containing the downloaded parts. The script verifies each part against the release checksums, refuses to overwrite an existing complete archive, joins the numbered parts in binary mode, and verifies the resulting archive:

```powershell
$name = 'ffmpeg-corresponding-source.zip'
$expected = @{}
Get-Content -LiteralPath "$name.parts.sha256" | ForEach-Object {
    if ($_ -notmatch '^([0-9a-fA-F]{64})  (.+)$') { throw "Invalid checksum line: $_" }
    $expected[$Matches[2]] = $Matches[1].ToLowerInvariant()
}
$parts = @(Get-ChildItem -File -Filter "$name.part*" | Where-Object Name -match '\.part[0-9]{3}$' | Sort-Object Name)
if ($parts.Count -lt 2) { throw 'Missing source archive parts.' }
for ($i = 0; $i -lt $parts.Count; $i++) {
    $number = '{0:D3}' -f ($i + 1)
    if ($parts[$i].Name -ne "$name.part$number") { throw "Missing or unexpected part at $number" }
    if (-not $expected.ContainsKey($parts[$i].Name)) { throw "Missing checksum for $($parts[$i].Name)" }
    $hash = (Get-FileHash -LiteralPath $parts[$i].FullName -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($hash -ne $expected[$parts[$i].Name]) { throw "Checksum mismatch: $($parts[$i].Name)" }
}
if (Test-Path -LiteralPath $name) { throw "Refusing to overwrite $name" }
$target = [System.IO.File]::Open($name, [System.IO.FileMode]::CreateNew)
try {
    foreach ($part in $parts) {
        $input = [System.IO.File]::OpenRead($part.FullName)
        try { $input.CopyTo($target) } finally { $input.Dispose() }
    }
} finally { $target.Dispose() }
$hash = (Get-FileHash -LiteralPath $name -Algorithm SHA256).Hash.ToLowerInvariant()
if ($hash -ne $expected[$name] -or $hash -ne '3647a98eb668736fee85a92e14cc0f379213e7051983040e2260beae8ccec675') {
    throw 'Reassembled archive checksum mismatch.'
}
Write-Host "Verified $name ($hash)"
```

## macOS or Linux

Run this in the download directory. The checksum list includes the complete archive, so check the parts first, join them, then check the archive:

```sh
set -e
set -C
sha256sum -c ffmpeg-corresponding-source.zip.parts.sha256 --ignore-missing
cat ffmpeg-corresponding-source.zip.part001 ffmpeg-corresponding-source.zip.part002 > ffmpeg-corresponding-source.zip
sha256sum -c ffmpeg-corresponding-source.zip.parts.sha256 --ignore-missing
```

On macOS, use `shasum -a 256` if GNU `sha256sum` is unavailable; compare each digest with the `.parts.sha256` file. The split currently produces two parts; if a future release includes more, concatenate all numbered parts in order. The full ZIP should contain `SOURCE-MANIFEST.md`, `ffmpeg/`, `x264/`, `BtbN-FFmpeg-Builds/`, and `dependency-sources/`. The archive and the binary build provenance are documented in [FFMPEG-WINDOWS.md](FFMPEG-WINDOWS.md) and [ffmpeg-win64.json](ffmpeg-win64.json).
