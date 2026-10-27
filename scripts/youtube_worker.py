"""Offline research worker for brokered YouTube metadata extraction.

The CLI accepts exactly one video ID. Standard input/output carry framed IPC;
the parent must first supply OS isolation and a network-policy broker. No real
media is downloaded here. This is not registered as a desktop command.
"""

from __future__ import annotations

import base64
import contextlib
import io
import json
import re
import struct
import sys
from dataclasses import dataclass
from typing import BinaryIO, Callable, Mapping
from urllib.parse import urlsplit


VIDEO_ID = re.compile(r"[A-Za-z0-9_-]{11}\Z", re.ASCII)
MAX_FRAME = 12 * 1024 * 1024
MAX_REQUEST_BODY = 256 * 1024
MAX_RESPONSE_BODY = 8 * 1024 * 1024
MAX_TOTAL_RESPONSE_BYTES = 32 * 1024 * 1024
MAX_REQUESTS = 64
ERROR_CODES = frozenset({
    "invalid_video_id", "invalid_invocation", "ipc_truncated", "ipc_frame_limit",
    "ipc_invalid_json", "ipc_invalid_shape", "ipc_invalid_response",
    "request_limit", "response_limit", "invalid_url", "unsupported_url",
    "unsupported_request", "invalid_headers", "unsupported_header",
    "plugins_already_loaded", "handler_configuration", "unsupported_metadata",
})
ALLOWED_REQUEST_HEADERS = frozenset({
    "accept", "accept-language", "content-type", "range", "referer",
    "user-agent", "origin", "x-youtube-client-name", "x-youtube-client-version",
})
ALLOWED_RESPONSE_HEADERS = frozenset({
    "content-type", "content-length", "content-range", "accept-ranges", "date",
    "etag", "last-modified", "cache-control",
})


class WorkerError(Exception):
    """A sanitized local worker or IPC failure."""


def canonical_video_id(value: object) -> str:
    if not isinstance(value, str) or not VIDEO_ID.fullmatch(value):
        raise WorkerError("invalid_video_id")
    return value


def _headers(value: Mapping[str, str], allowed: frozenset[str]) -> dict[str, str]:
    if not isinstance(value, Mapping) or len(value) > 32:
        raise WorkerError("invalid_headers")
    result: dict[str, str] = {}
    for key, item in value.items():
        if not isinstance(key, str) or not isinstance(item, str):
            raise WorkerError("invalid_headers")
        name = key.lower()
        if name not in allowed or name in result or len(item) > 2048 or any(
            ord(char) < 32 or ord(char) == 127 for char in item
        ):
            raise WorkerError("unsupported_header")
        result[name] = item
    return result


def _read_exact(stream: BinaryIO, size: int) -> bytes:
    parts = []
    remaining = size
    while remaining:
        part = stream.read(remaining)
        if not isinstance(part, bytes) or not part:
            raise WorkerError("ipc_truncated")
        parts.append(part)
        remaining -= len(part)
    return b"".join(parts)


def _read_frame(stream: BinaryIO) -> dict:
    size = struct.unpack("!I", _read_exact(stream, 4))[0]
    if size == 0 or size > MAX_FRAME:
        raise WorkerError("ipc_frame_limit")
    try:
        message = json.loads(_read_exact(stream, size))
    except (UnicodeError, ValueError) as exc:
        raise WorkerError("ipc_invalid_json") from exc
    if not isinstance(message, dict):
        raise WorkerError("ipc_invalid_shape")
    return message


def _write_frame(stream: BinaryIO, message: dict) -> None:
    data = json.dumps(message, separators=(",", ":"), ensure_ascii=True).encode("ascii")
    if not data or len(data) > MAX_FRAME:
        raise WorkerError("ipc_frame_limit")
    stream.write(struct.pack("!I", len(data)) + data)
    stream.flush()


@dataclass
class BrokerIPC:
    reader: BinaryIO
    writer: BinaryIO
    requests: int = 0
    response_bytes: int = 0

    def send(self, url: str, method: str, headers: Mapping[str, str], body: bytes) -> tuple[str, int, dict[str, str], bytes]:
        if self.requests >= MAX_REQUESTS:
            raise WorkerError("request_limit")
        if not isinstance(url, str) or len(url) > 4096:
            raise WorkerError("invalid_url")
        try:
            parsed = urlsplit(url)
            valid_url = (parsed.scheme == "https" and bool(parsed.hostname) and
                         not parsed.username and not parsed.password and parsed.port in (None, 443))
        except ValueError:
            valid_url = False
        if not valid_url:
            raise WorkerError("unsupported_url")
        if method not in ("GET", "HEAD", "POST") or not isinstance(body, bytes) or len(body) > MAX_REQUEST_BODY:
            raise WorkerError("unsupported_request")
        if method != "POST" and body:
            raise WorkerError("unsupported_request")
        clean_headers = _headers(headers, ALLOWED_REQUEST_HEADERS)
        self.requests += 1
        _write_frame(self.writer, {
            "v": 1, "kind": "http_request", "seq": self.requests,
            "url": url, "method": method, "headers": clean_headers,
            "body_b64": base64.b64encode(body).decode("ascii"),
        })
        response = _read_frame(self.reader)
        if set(response) != {"v", "kind", "seq", "url", "status", "headers", "body_b64"} or (
            response["v"] != 1 or response["kind"] != "http_response" or response["seq"] != self.requests
        ):
            raise WorkerError("ipc_invalid_response")
        final_url, status = response["url"], response["status"]
        if not isinstance(final_url, str) or len(final_url) > 4096:
            raise WorkerError("ipc_invalid_response")
        try:
            final = urlsplit(final_url)
            valid_final = (final.scheme == "https" and bool(final.hostname) and
                           not final.username and not final.password and final.port in (None, 443))
        except ValueError:
            valid_final = False
        if not valid_final:
            raise WorkerError("ipc_invalid_response")
        if type(status) is not int or not 200 <= status <= 599 or 300 <= status <= 399:
            raise WorkerError("ipc_invalid_response")
        response_headers = _headers(response["headers"], ALLOWED_RESPONSE_HEADERS)
        encoded = response["body_b64"]
        if not isinstance(encoded, str) or len(encoded) > (MAX_RESPONSE_BODY * 4 // 3 + 8):
            raise WorkerError("response_limit")
        try:
            data = base64.b64decode(encoded, validate=True)
        except ValueError as exc:
            raise WorkerError("ipc_invalid_response") from exc
        if len(data) > MAX_RESPONSE_BODY:
            raise WorkerError("response_limit")
        self.response_bytes += len(data)
        if self.response_bytes > MAX_TOTAL_RESPONSE_BYTES:
            raise WorkerError("response_limit")
        return final_url, status, response_headers, data


def make_yt_dlp(ipc: BrokerIPC):
    """Create one yt-dlp director with exactly one brokered RequestHandler.

    yt-dlp is imported only here. The caller must set up OS isolation before
    importing it; Python-level settings cannot deny direct network by children.
    """
    from yt_dlp import YoutubeDL
    from yt_dlp.globals import all_plugins_loaded, plugin_dirs
    from yt_dlp.networking.common import RequestDirector, RequestHandler, RequestError, Response

    if all_plugins_loaded.value:
        raise WorkerError("plugins_already_loaded")
    plugin_dirs.value = []

    class BrokerRH(RequestHandler):
        _SUPPORTED_URL_SCHEMES = ("https",)
        _SUPPORTED_PROXY_SCHEMES = ()
        _SUPPORTED_FEATURES = ()

        def _check_extensions(self, extensions):
            # No cookies, impersonation, per-request proxies, or TLS overrides.
            if extensions:
                raise RequestError("unsupported_extension")

        def _send(self, request):
            try:
                url, status, headers, data = ipc.send(
                    request.url, request.method, self._get_headers(request), request.data or b"",
                )
            except WorkerError as exc:
                raise RequestError(str(exc)) from exc
            return Response(io.BytesIO(data), url, headers, status=status)

    class BrokeredYoutubeDL(YoutubeDL):
        @property
        def _request_director(self):
            return self._broker_director

    ydl = BrokeredYoutubeDL({
        "allowed_extractors": ["youtube"], "quiet": True, "no_warnings": True,
        "skip_download": True, "noplaylist": True, "ignoreconfig": True,
        "cachedir": False, "remote_components": set(), "js_runtimes": {},
        "proxy": "", "http_headers": {}, "extract_flat": False,
    })
    director = RequestDirector(logger=ydl, verbose=False)
    director.add_handler(BrokerRH(logger=ydl, headers={}, proxies={}))
    if len(director.handlers) != 1:
        raise WorkerError("handler_configuration")
    ydl._broker_director = director
    return ydl


def extract_metadata(video_id: str, ipc: BrokerIPC, factory: Callable[[BrokerIPC], object] = make_yt_dlp) -> dict:
    """Offline prototype: metadata only, with every yt-dlp HTTP request brokered."""
    video_id = canonical_video_id(video_id)
    ydl = factory(ipc)
    info = ydl.extract_info(
        f"https://www.youtube.com/watch?v={video_id}",
        download=False, ie_key="Youtube",
    )
    if not isinstance(info, dict) or info.get("id") != video_id or info.get("is_live"):
        raise WorkerError("unsupported_metadata")
    duration = info.get("duration")
    if type(duration) not in (int, float) or not 0 < duration <= 6 * 3600:
        raise WorkerError("unsupported_metadata")
    return {"video_id": video_id, "duration_seconds": duration, "format_count": min(len(info.get("formats") or []), 100)}


class _DiscardText:
    """Keep dependency text output out of the binary IPC stream."""

    def write(self, value: str) -> int:
        return len(value)

    def flush(self) -> None:
        pass


def run_cli(
    argv: list[str], reader: BinaryIO, writer: BinaryIO,
    extractor: Callable[[str, BrokerIPC], dict] = extract_metadata,
) -> int:
    """Run one metadata job and emit exactly one terminal result frame."""
    try:
        if len(argv) != 1:
            raise WorkerError("invalid_invocation")
        video_id = canonical_video_id(argv[0])
        with contextlib.redirect_stdout(_DiscardText()), contextlib.redirect_stderr(_DiscardText()):
            metadata = extractor(video_id, BrokerIPC(reader, writer))
        if not isinstance(metadata, dict) or set(metadata) != {"video_id", "duration_seconds", "format_count"}:
            raise WorkerError("unsupported_metadata")
        duration, count = metadata["duration_seconds"], metadata["format_count"]
        if (metadata["video_id"] != video_id or type(duration) not in (int, float)
                or not 0 < duration <= 6 * 3600 or type(count) is not int or not 0 <= count <= 100):
            raise WorkerError("unsupported_metadata")
        _write_frame(writer, {"v": 1, "kind": "metadata_result", **metadata})
        return 0
    except WorkerError as exc:
        code = str(exc) if str(exc) in ERROR_CODES else "worker_failed"
    except Exception:
        # yt-dlp errors may contain URLs, response bodies, or filesystem paths.
        code = "extraction_failed"
    _write_frame(writer, {"v": 1, "kind": "error", "code": code})
    return 1


if __name__ == "__main__":
    raise SystemExit(run_cli(sys.argv[1:], sys.stdin.buffer, sys.stdout.buffer))
