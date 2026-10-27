"""Disabled remote downloader with offline fixtures for the earlier prototype.

The CLI and default Python entry point reject requests before DNS or file access.
The injected downloader factory is retained solely for offline regression fixtures.
No live yt-dlp runtime is invoked. See src-tauri/src/download/YOUTUBE_FEASIBILITY.md.
"""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import ipaddress
import json
import os
from pathlib import Path
import re
import socket
import sys
import tempfile
import time
from typing import Any, Callable
from urllib.parse import urlsplit

MAX_BYTES = 2 * 1024**3
MAX_DURATION_SECONDS = 4 * 60 * 60
MAX_SECONDS = 30 * 60
ALLOWED_EXTENSIONS = {"mp4", "mov", "mkv", "webm", "m4v"}
PUBLIC_HOSTS = {"youtube.com", "youtu.be"}
PUBLIC_EXTRACTORS = {"Youtube"}
REMOTE_DOWNLOAD_UNAVAILABLE = (
    "Remote video downloads are unavailable until network and process isolation are verified. "
    "Download your own uploads through YouTube Studio or Google Takeout, then import the local file."
)


class DownloadError(Exception):
    pass


def validate_url(url: str) -> str:
    if len(url) > 2048 or any(ord(char) < 33 for char in url):
        raise DownloadError("Enter one HTTPS video URL without spaces or control characters.")
    parsed = urlsplit(url)
    if parsed.scheme.lower() != "https" or not parsed.hostname or parsed.username or parsed.password:
        raise DownloadError("Enter a public HTTPS video URL without embedded credentials.")
    if parsed.port not in (None, 443):
        raise DownloadError("The video URL must use the standard HTTPS port.")
    hostname = parsed.hostname.rstrip(".").lower()
    if not any(hostname == host or hostname.endswith("." + host) for host in PUBLIC_HOSTS):
        raise DownloadError("This URL source is not enabled. Only YouTube is in the proposed scope.")
    try:
        addresses = {entry[4][0] for entry in socket.getaddrinfo(hostname, 443, type=socket.SOCK_STREAM)}
        if not addresses or not all(ipaddress.ip_address(address).is_global for address in addresses):
            raise DownloadError("The video URL resolves to a private or reserved network address.")
    except socket.gaierror as exc:
        raise DownloadError("The video site's address could not be resolved.") from exc
    return url


def validate_info(info: dict[str, Any]) -> None:
    if info.get("_type") in {"playlist", "multi_video"} or info.get("entries") is not None:
        raise DownloadError("Playlists and multi-video pages are not supported. Open one video URL.")
    if info.get("extractor_key") not in PUBLIC_EXTRACTORS:
        raise DownloadError("This video source is not enabled for direct download.")
    media_id = info.get("id")
    if not isinstance(media_id, str) or not media_id or len(media_id) > 200 or any(ord(char) < 32 for char in media_id):
        raise DownloadError("The site did not provide a valid video identity.")
    webpage_url = info.get("webpage_url")
    if not isinstance(webpage_url, str):
        raise DownloadError("The site did not provide a video source URL.")
    validate_url(webpage_url)
    if info.get("is_live") or info.get("live_status") in {"is_live", "is_upcoming"}:
        raise DownloadError("Live or upcoming streams cannot be imported.")
    duration = info.get("duration")
    if not isinstance(duration, (int, float)) or not 0 < duration <= MAX_DURATION_SECONDS:
        raise DownloadError("The video must have a known duration of four hours or less.")
    sizes = [info.get("filesize"), info.get("filesize_approx")]
    if any(isinstance(size, (int, float)) and size > MAX_BYTES for size in sizes):
        raise DownloadError("The video exceeds the 2 GiB download limit.")


def safe_stem(info: dict[str, Any]) -> str:
    title = str(info.get("title") or "video")
    stem = re.sub(r"[^A-Za-z0-9_-]+", "-", title).strip("-_")[:80]
    return stem or "video"


def download_video(
    url: str,
    output_dir: Path,
    ydl_factory: Callable[[dict[str, Any]], Any] | None = None,
    clock: Callable[[], float] = time.monotonic,
    *,
    rights_confirmed: bool = False,
) -> dict[str, Any]:
    if ydl_factory is None:
        raise DownloadError(REMOTE_DOWNLOAD_UNAVAILABLE)
    if not rights_confirmed:
        raise DownloadError("Confirm that you own this media or have permission to download it.")
    rights_confirmed_at = datetime.now(timezone.utc).isoformat()
    validate_url(url)
    output_dir = output_dir.resolve(strict=True)
    if not output_dir.is_dir():
        raise DownloadError("The download folder is unavailable.")

    started = clock()

    def check_progress(progress: dict[str, Any]) -> None:
        if clock() - started > MAX_SECONDS:
            raise DownloadError("The download exceeded the 30-minute time limit.")
        count = progress.get("downloaded_bytes") or progress.get("total_bytes") or progress.get("total_bytes_estimate") or 0
        if count > MAX_BYTES:
            raise DownloadError("The video exceeds the 2 GiB download limit.")

    with tempfile.TemporaryDirectory(prefix="cortelol-download-", dir=output_dir) as stage_name:
        stage = Path(stage_name)
        options = {
            "noplaylist": True,
            "geo_bypass": False,
            "age_limit": 0,
            "cachedir": False,
            "max_downloads": 1,
            "max_filesize": MAX_BYTES,
            "socket_timeout": 20,
            "retries": 2,
            "fragment_retries": 2,
            "format": "best[ext=mp4]/best",
            "outtmpl": str(stage / "video.%(ext)s"),
            "nopart": False,
            "overwrites": False,
            "continuedl": False,
            "writethumbnail": False,
            "writesubtitles": False,
            "writeautomaticsub": False,
            "writeinfojson": False,
            "progress_hooks": [check_progress],
            "quiet": True,
            "no_warnings": True,
        }
        try:
            with ydl_factory(options) as ydl:
                info = ydl.extract_info(url, download=False)
                if not isinstance(info, dict):
                    raise DownloadError("The site did not provide one readable video.")
                validate_info(info)
                if clock() - started > MAX_SECONDS:
                    raise DownloadError("The video lookup exceeded the 30-minute time limit.")
                downloaded = ydl.extract_info(url, download=True)
                if not isinstance(downloaded, dict):
                    raise DownloadError("The site did not return a completed video.")
                validate_info(downloaded)
                if (downloaded["extractor_key"], downloaded["id"], downloaded["webpage_url"]) != (info["extractor_key"], info["id"], info["webpage_url"]):
                    raise DownloadError("The downloaded video's source identity changed during retrieval.")
        except DownloadError:
            raise
        except Exception as exc:
            raise DownloadError("The site could not provide this public video. Check its availability and try again.") from exc

        files = [path for path in stage.iterdir() if path.is_file()]
        if len(files) != 1 or files[0].suffix.lower().lstrip(".") not in ALLOWED_EXTENSIONS:
            raise DownloadError("The site did not produce one supported video file.")
        source = files[0]
        if source.is_symlink() or source.resolve().parent != stage.resolve():
            raise DownloadError("The downloader produced an invalid file path.")
        size = source.stat().st_size
        if not 0 < size <= MAX_BYTES:
            raise DownloadError("The completed video is empty or exceeds the 2 GiB limit.")
        source_url = downloaded["webpage_url"]
        acquired_at = datetime.now(timezone.utc).isoformat()
        attribution = {
            "sourceUrl": source_url,
            "sourceTitle": str(downloaded.get("title") or "Untitled video")[:300],
            "sourceSite": downloaded["extractor_key"],
            "sourceIdentity": {
                "extractorKey": downloaded["extractor_key"],
                "mediaId": downloaded["id"],
                "canonicalUrl": source_url,
            },
            "rightsConfirmation": {
                "confirmed": True,
                "basis": "ownership-or-permission",
                "confirmedAtUtc": rights_confirmed_at,
            },
            "acquiredAtUtc": acquired_at,
            "acquisitionMethod": "yt-dlp-python-api",
        }
        extension = source.suffix.lower()
        for _ in range(10):
            candidate = output_dir / f"{safe_stem(downloaded)}-{os.urandom(6).hex()}{extension}"
            try:
                os.link(source, candidate)
                break
            except FileExistsError:
                continue
        else:
            raise DownloadError("A unique filename could not be reserved.")
        sidecar = candidate.with_name(candidate.name + ".source.json")
        try:
            staged_sidecar = stage / "source.json"
            with staged_sidecar.open("x", encoding="utf-8") as handle:
                json.dump(attribution, handle, ensure_ascii=False, indent=2)
                handle.write("\n")
            os.link(staged_sidecar, sidecar)
        except Exception:
            candidate.unlink(missing_ok=True)
            raise
        return {"path": str(candidate), "fileSizeBytes": size, **attribution}


def main() -> int:
    if len(sys.argv) == 2 and sys.argv[1] == "--self-test":
        import unittest
        from unittest.mock import patch

        class MockDownloaderTests(unittest.TestCase):
            def setUp(self) -> None:
                self.directory = tempfile.TemporaryDirectory()
                self.addCleanup(self.directory.cleanup)
                self.dns = patch("socket.getaddrinfo", return_value=[(socket.AF_INET, socket.SOCK_STREAM, 6, "", ("142.250.190.14", 443))])
                self.dns.start()
                self.addCleanup(self.dns.stop)

            def factory(self, info: dict[str, Any], payload: bytes = b"video") -> Callable[[dict[str, Any]], Any]:
                class MockYDL:
                    def __init__(self, options: dict[str, Any]):
                        self.options = options

                    def __enter__(self) -> "MockYDL":
                        return self

                    def __exit__(self, *_: Any) -> None:
                        pass

                    def extract_info(self, _url: str, download: bool) -> dict[str, Any]:
                        if download:
                            path = Path(self.options["outtmpl"].replace("%(ext)s", "mp4"))
                            path.write_bytes(payload)
                            self.options["progress_hooks"][0]({"downloaded_bytes": len(payload)})
                        return info

                return MockYDL

            def test_default_entry_point_has_no_dns_filesystem_or_extractor_side_effects(self) -> None:
                media = Path(self.directory.name) / "existing.mp4"
                receipt = Path(self.directory.name) / "existing.mp4.source.json"
                media.write_bytes(b"existing-media")
                receipt.write_bytes(b"existing-receipt")
                for url in ("https://www.youtube.com/watch?v=test", "https://vimeo.com/one", "https://127.0.0.1/secret"):
                    with patch("socket.getaddrinfo") as dns, patch.object(Path, "resolve") as resolve:
                        with self.assertRaisesRegex(DownloadError, "unavailable"):
                            download_video(url, Path(self.directory.name), rights_confirmed=True)
                        dns.assert_not_called()
                        resolve.assert_not_called()
                self.assertEqual(media.read_bytes(), b"existing-media")
                self.assertEqual(receipt.read_bytes(), b"existing-receipt")

            def test_vimeo_is_out_of_scope_before_dns(self) -> None:
                with patch("socket.getaddrinfo") as dns:
                    with self.assertRaisesRegex(DownloadError, "not enabled"):
                        validate_url("https://vimeo.com/one")
                    dns.assert_not_called()

            def test_cli_is_blocked_without_dns_path_resolution_or_child_launch(self) -> None:
                import io
                from contextlib import redirect_stderr
                for url in ("https://www.youtube.com/watch?v=test", "https://vimeo.com/one"):
                    with patch.object(sys, "argv", ["download_video.py", "--i-have-rights", url, self.directory.name]), patch("socket.getaddrinfo") as dns, patch("subprocess.Popen") as child, patch.object(Path, "resolve") as resolve, redirect_stderr(io.StringIO()) as error:
                        self.assertEqual(main(), 1)
                        self.assertIn("unavailable", error.getvalue())
                        self.assertNotIn(url, error.getvalue())
                        dns.assert_not_called()
                        child.assert_not_called()
                        resolve.assert_not_called()

            def test_success_publishes_media_and_attribution(self) -> None:
                info = {"extractor_key": "Youtube", "id": "test", "duration": 60, "title": "My / recording", "webpage_url": "https://www.youtube.com/watch?v=test"}
                result = download_video("https://www.youtube.com/watch?v=test", Path(self.directory.name), self.factory(info), rights_confirmed=True)
                self.assertEqual(Path(result["path"]).read_bytes(), b"video")
                sidecar = json.loads(Path(result["path"] + ".source.json").read_text())
                self.assertEqual(sidecar["sourceUrl"], info["webpage_url"])
                self.assertEqual(sidecar["sourceIdentity"], {"extractorKey": "Youtube", "mediaId": "test", "canonicalUrl": info["webpage_url"]})
                self.assertEqual(sidecar["rightsConfirmation"]["basis"], "ownership-or-permission")
                self.assertIs(sidecar["rightsConfirmation"]["confirmed"], True)
                self.assertLessEqual(datetime.fromisoformat(sidecar["rightsConfirmation"]["confirmedAtUtc"]), datetime.fromisoformat(sidecar["acquiredAtUtc"]))
                self.assertEqual(sidecar["acquisitionMethod"], "yt-dlp-python-api")
                self.assertEqual(len(list(Path(self.directory.name).glob("*.mp4"))), 1)

            def test_playlist_is_rejected_without_published_file(self) -> None:
                info = {"extractor_key": "Youtube", "id": "list", "duration": 60, "entries": [{}], "_type": "playlist"}
                with self.assertRaisesRegex(DownloadError, "Playlists"):
                    download_video("https://www.youtube.com/playlist?list=test", Path(self.directory.name), self.factory(info), rights_confirmed=True)
                self.assertEqual(list(Path(self.directory.name).iterdir()), [])

            def test_size_limit_removes_partial_download(self) -> None:
                info = {"extractor_key": "Youtube", "id": "test", "duration": 60, "webpage_url": "https://www.youtube.com/watch?v=test"}
                class TooLarge:
                    def __init__(self, options: dict[str, Any]): self.options = options
                    def __enter__(self) -> "TooLarge": return self
                    def __exit__(self, *_: Any) -> None: pass
                    def extract_info(self, _url: str, download: bool) -> dict[str, Any]:
                        if download:
                            Path(self.options["outtmpl"].replace("%(ext)s", "mp4")).write_bytes(b"partial")
                            self.options["progress_hooks"][0]({"downloaded_bytes": MAX_BYTES + 1})
                        return info
                with self.assertRaisesRegex(DownloadError, "2 GiB"):
                    download_video("https://www.youtube.com/watch?v=test", Path(self.directory.name), TooLarge, rights_confirmed=True)
                self.assertEqual(list(Path(self.directory.name).iterdir()), [])

            def test_identity_change_is_rejected(self) -> None:
                first = {"extractor_key": "Youtube", "id": "first", "duration": 60, "webpage_url": "https://www.youtube.com/watch?v=first"}
                second = {**first, "id": "second", "webpage_url": "https://www.youtube.com/watch?v=second"}
                class ChangedIdentity:
                    def __init__(self, options: dict[str, Any]): self.options = options
                    def __enter__(self) -> "ChangedIdentity": return self
                    def __exit__(self, *_: Any) -> None: pass
                    def extract_info(self, _url: str, download: bool) -> dict[str, Any]:
                        if download:
                            Path(self.options["outtmpl"].replace("%(ext)s", "mp4")).write_bytes(b"video")
                        return second if download else first
                with self.assertRaisesRegex(DownloadError, "identity changed"):
                    download_video("https://www.youtube.com/watch?v=first", Path(self.directory.name), ChangedIdentity, rights_confirmed=True)
                self.assertEqual(list(Path(self.directory.name).iterdir()), [])

            def test_rights_confirmation_is_required(self) -> None:
                with self.assertRaisesRegex(DownloadError, "Confirm"):
                    download_video("https://www.youtube.com/watch?v=test", Path(self.directory.name), self.factory({}))
                self.assertEqual(list(Path(self.directory.name).iterdir()), [])

            def test_private_dns_and_unlisted_host_are_rejected(self) -> None:
                with self.assertRaisesRegex(DownloadError, "not enabled"):
                    validate_url("https://example.com/video")
                with patch("socket.getaddrinfo", return_value=[(socket.AF_INET, socket.SOCK_STREAM, 6, "", ("127.0.0.1", 443))]):
                    with self.assertRaisesRegex(DownloadError, "private or reserved"):
                        validate_url("https://www.youtube.com/watch?v=test")

        suite = unittest.defaultTestLoader.loadTestsFromTestCase(MockDownloaderTests)
        return 0 if unittest.TextTestRunner().run(suite).wasSuccessful() else 1

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--i-have-rights", action="store_true", help="Confirm you own the media or have permission to download it")
    parser.add_argument("url")
    parser.add_argument("output_dir", type=Path)
    args = parser.parse_args()
    try:
        if not args.i_have_rights:
            raise DownloadError("Confirm permission with --i-have-rights before downloading.")
        print(json.dumps(download_video(args.url, args.output_dir, rights_confirmed=True)), flush=True)
        return 0
    except (DownloadError, OSError, ValueError) as exc:
        print(str(exc), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
