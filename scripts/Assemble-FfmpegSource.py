"""Assemble the exact pinned BtbN FFmpeg source inputs into a release ZIP.

Run in WSL after the tagged BtbN checkout's download.sh exits successfully.
The resulting archive still requires human review before GPL distribution.
"""

from __future__ import annotations

import argparse
import hashlib
import subprocess
import zipfile
from pathlib import Path


BUILD_COMMIT = "937868360a21d711ec94a33f5591d345ad9b7754"
FFMPEG_COMMIT = "92611900bb112123e9bf997fdcc2d78e4c4922fc"
FFMPEG_TAR_SHA256 = "12c02b1eaf52ec310acae385e76810f4bb71bacc21cacc01d6d9e95ac3b83bbb"
BINARY_ARCHIVE_SHA256 = "0ed7e19d34ca040c0179068ccc6cd6d7f2217d2bcbf7604e2225e271e79bee22"
ZIP_DATE = (2026, 10, 25, 0, 0, 0)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while block := source.read(1024 * 1024):
            digest.update(block)
    return digest.hexdigest()


def zip_add_file(archive: zipfile.ZipFile, path: Path, name: str) -> None:
    entry = zipfile.ZipInfo(name, ZIP_DATE)
    entry.compress_type = zipfile.ZIP_STORED
    entry.create_system = 3
    entry.external_attr = 0o644 << 16
    with path.open("rb") as source, archive.open(entry, "w", force_zip64=True) as target:
        while block := source.read(1024 * 1024):
            target.write(block)


def zip_add_text(archive: zipfile.ZipFile, name: str, content: str) -> None:
    entry = zipfile.ZipInfo(name, ZIP_DATE)
    entry.compress_type = zipfile.ZIP_STORED
    entry.create_system = 3
    entry.external_attr = 0o644 << 16
    archive.writestr(entry, content.encode("utf-8"))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--ffmpeg-source", type=Path, required=True)
    parser.add_argument("--download-complete", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    repo = args.repo.resolve()
    ffmpeg_source = args.ffmpeg_source.resolve()
    output = args.output.resolve()
    completion = args.download_complete.resolve()
    commit = subprocess.check_output(["git", "-C", str(repo), "rev-parse", "HEAD"], text=True).strip()
    if commit != BUILD_COMMIT:
        raise SystemExit(f"Wrong BtbN build commit: {commit}")
    if not completion.is_file() or completion.read_text(encoding="utf-8").strip() != BUILD_COMMIT:
        raise SystemExit("Tagged download.sh completion marker is absent or mismatched")
    if sha256(ffmpeg_source) != FFMPEG_TAR_SHA256:
        raise SystemExit("FFmpeg source archive does not match pinned commit bytes")
    if output.exists():
        raise SystemExit(f"Refusing to overwrite {output}")

    cache = (repo / ".cache" / "downloads").resolve()
    aliases: dict[str, Path] = {}
    for alias in sorted(cache.glob("*.tar.xz")):
        if not alias.is_symlink():
            continue
        target = alias.resolve(strict=True)
        if not target.is_relative_to(cache) or not target.is_file():
            raise SystemExit(f"Unsafe dependency cache link: {alias}")
        aliases[alias.name] = target
    if len(aliases) < 50 or "50-x264.tar.xz" not in aliases:
        raise SystemExit("Tagged download cache is incomplete; run download.sh to completion")

    payloads = sorted(set(aliases.values()), key=lambda item: item.name)
    source_hashes = {path.name: sha256(path) for path in payloads}
    tracked = subprocess.check_output(["git", "-C", str(repo), "ls-files", "-z"]).split(b"\0")
    tracked_paths = sorted(Path(raw.decode("utf-8")) for raw in tracked if raw)
    root = Path(__file__).resolve().parent.parent
    buildconf = root / "legal" / "FFMPEG-BUILDCONF.txt"

    manifest = [
        "# FFmpeg corresponding-source package for Cortelol Windows x64",
        "",
        f"BtbN build scripts commit: {BUILD_COMMIT}",
        f"FFmpeg source commit: {FFMPEG_COMMIT}",
        "BtbN variant: win64 gpl 8.1",
        f"Pinned binary ZIP SHA-256: {BINARY_ARCHIVE_SHA256}",
        f"FFmpeg source tarball SHA-256: {FFMPEG_TAR_SHA256}",
        "",
        "The build scripts and their patches are under BtbN-FFmpeg-Builds/.",
        "The FFmpeg source tarball is under ffmpeg/.",
        "The x264 source tarball is under x264/ and dependency-sources/.",
        "All current download.sh cache aliases and tarball hashes follow.",
        "Rebuild: extract the BtbN tree, place dependency-sources/ tarballs in",
        "its .cache/downloads directory with aliases below, then run",
        "./generate.sh win64 gpl 8.1 and ./build.sh win64 gpl 8.1 using Docker.",
        "The source and linked-license correspondence requires manual review.",
        "",
        "## Dependency cache aliases",
    ]
    manifest.extend(f"{name} -> {target.name}" for name, target in sorted(aliases.items()))
    manifest.extend(["", "## Dependency cache SHA-256"])
    manifest.extend(f"{source_hashes[path.name]}  {path.name}" for path in payloads)
    manifest.append("")

    output.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(output, "w", allowZip64=True) as archive:
        zip_add_text(archive, "SOURCE-MANIFEST.md", "\n".join(manifest))
        zip_add_file(archive, ffmpeg_source, f"ffmpeg/{ffmpeg_source.name}")
        zip_add_file(archive, aliases["50-x264.tar.xz"], "x264/source.tar.xz")
        zip_add_file(archive, buildconf, "FFMPEG-BUILDCONF.txt")
        for path in tracked_paths:
            zip_add_file(archive, repo / path, f"BtbN-FFmpeg-Builds/{path.as_posix()}")
        for path in payloads:
            zip_add_file(archive, path, f"dependency-sources/{path.name}")
    print(f"Source ZIP: {output}")
    print(f"SHA-256: {sha256(output)}")
    print(f"Cache aliases: {len(aliases)}; unique source tarballs: {len(payloads)}")


if __name__ == "__main__":
    main()
