"""Offline tests for the brokered yt-dlp research worker."""

import base64
import importlib.util
import io
import json
import struct
import subprocess
import unittest
from pathlib import Path
from unittest.mock import Mock


spec = importlib.util.spec_from_file_location(
    "youtube_worker", Path(__file__).resolve().parents[1] / "youtube_worker.py",
)
worker = importlib.util.module_from_spec(spec)
import sys
sys.modules[spec.name] = worker
spec.loader.exec_module(worker)


def frame(message):
    data = json.dumps(message).encode("utf-8")
    return struct.pack("!I", len(data)) + data


def response(**overrides):
    payload = {
        "v": 1, "kind": "http_response", "seq": 1,
        "url": "https://www.youtube.com/watch?v=AbCdEf123_-",
        "status": 200, "headers": {"content-type": "application/json"},
        "body_b64": base64.b64encode(b'{}').decode("ascii"),
    }
    payload.update(overrides)
    return payload


class WorkerTests(unittest.TestCase):
    def test_cli_round_trip_with_fake_broker(self):
        inbound = io.BytesIO(frame(response()))
        outbound = io.BytesIO()

        def fake_extract(video_id, ipc):
            print("dependency diagnostic that must not corrupt IPC")
            _, _, _, body = ipc.send(
                f"https://www.youtube.com/watch?v={video_id}", "GET", {}, b"",
            )
            self.assertEqual(body, b"{}")
            return {"video_id": video_id, "duration_seconds": 42, "format_count": 2}

        self.assertEqual(worker.run_cli(["AbCdEf123_-"], inbound, outbound, fake_extract), 0)
        outbound.seek(0)
        request = worker._read_frame(outbound)
        result = worker._read_frame(outbound)
        self.assertEqual(request["kind"], "http_request")
        self.assertEqual(result, {
            "v": 1, "kind": "metadata_result", "video_id": "AbCdEf123_-",
            "duration_seconds": 42, "format_count": 2,
        })
        self.assertEqual(outbound.read(), b"")

    def test_cli_malformed_broker_frame_returns_sanitized_error(self):
        for raw in (b"", b"\x00\x00\x00\x04bad!", frame({"kind": "wrong"})):
            with self.subTest(raw=raw):
                outbound = io.BytesIO()

                def fake_extract(video_id, ipc):
                    ipc.send(f"https://www.youtube.com/watch?v={video_id}", "GET", {}, b"")

                self.assertEqual(worker.run_cli(["AbCdEf123_-"], io.BytesIO(raw), outbound, fake_extract), 1)
                outbound.seek(0)
                self.assertEqual(worker._read_frame(outbound)["kind"], "http_request")
                error = worker._read_frame(outbound)
                self.assertEqual(error["v"], 1)
                self.assertEqual(error["kind"], "error")
                self.assertIn(error["code"], worker.ERROR_CODES)
                self.assertEqual(outbound.read(), b"")

    def test_cli_rejects_extra_arguments_before_extraction(self):
        outbound = io.BytesIO()
        called = Mock()
        code = worker.run_cli(["AbCdEf123_-", "other"], io.BytesIO(), outbound, called)
        self.assertEqual(code, 1)
        called.assert_not_called()
        outbound.seek(0)
        self.assertEqual(worker._read_frame(outbound), {"v": 1, "kind": "error", "code": "invalid_invocation"})

    def test_subprocess_entrypoint_invalid_id_is_framed_without_network(self):
        result = subprocess.run(
            [sys.executable, str(Path(worker.__file__)), "invalid"],
            input=b"", capture_output=True, timeout=5, check=False,
        )
        self.assertEqual(result.returncode, 1)
        output = io.BytesIO(result.stdout)
        self.assertEqual(worker._read_frame(output), {"v": 1, "kind": "error", "code": "invalid_video_id"})
        self.assertEqual(output.read(), b"")

    def test_video_id_only(self):
        self.assertEqual(worker.canonical_video_id("AbCdEf123_-"), "AbCdEf123_-")
        for value in ("https://youtu.be/AbCdEf123_-", "AbCdEf123_?", "short", "a" * 12, None):
            with self.subTest(value=value), self.assertRaises(worker.WorkerError):
                worker.canonical_video_id(value)

    def test_fake_ipc_round_trip(self):
        outbound = io.BytesIO()
        ipc = worker.BrokerIPC(io.BytesIO(frame(response())), outbound)
        result = ipc.send("https://www.youtube.com/watch?v=AbCdEf123_-", "GET", {"Accept": "*/*"}, b"")
        self.assertEqual(result[3], b"{}")
        outbound.seek(0)
        sent = worker._read_frame(outbound)
        self.assertEqual(sent["kind"], "http_request")
        self.assertEqual(sent["headers"], {"accept": "*/*"})
        self.assertEqual(sent["body_b64"], "")

    def test_rejects_unsafe_requests_before_ipc(self):
        for url, method, headers, body in (
            ("http://www.youtube.com/watch", "GET", {}, b""),
            ("file:///secret", "GET", {}, b""),
            ("https://user:pass@www.youtube.com/", "GET", {}, b""),
            ("https://www.youtube.com:8443/", "GET", {}, b""),
            ("https://www.youtube.com/", "DELETE", {}, b""),
            ("https://www.youtube.com/", "GET", {"Cookie": "secret"}, b""),
            ("https://www.youtube.com/", "POST", {}, b"x" * (worker.MAX_REQUEST_BODY + 1)),
        ):
            with self.subTest(url=url, method=method, headers=headers):
                outbound = io.BytesIO()
                with self.assertRaises(worker.WorkerError):
                    worker.BrokerIPC(io.BytesIO(), outbound).send(url, method, headers, body)
                self.assertEqual(outbound.getvalue(), b"")

    def test_rejects_malformed_broker_responses(self):
        for payload in (
            response(seq=2), response(status=302), response(headers={"set-cookie": "x=y"}),
            response(body_b64="%%%"), response(body_b64=base64.b64encode(b"x" * (worker.MAX_RESPONSE_BODY + 1)).decode()),
        ):
            with self.subTest(payload_keys=list(payload)):
                ipc = worker.BrokerIPC(io.BytesIO(frame(payload)), io.BytesIO())
                with self.assertRaises(worker.WorkerError):
                    ipc.send("https://www.youtube.com/watch", "GET", {}, b"")

    def test_request_limit_and_truncated_frame(self):
        ipc = worker.BrokerIPC(io.BytesIO(), io.BytesIO(), requests=worker.MAX_REQUESTS)
        with self.assertRaisesRegex(worker.WorkerError, "request_limit"):
            ipc.send("https://www.youtube.com/watch", "GET", {}, b"")
        ipc = worker.BrokerIPC(io.BytesIO(b"\x00\x00\x00\x10{}"), io.BytesIO())
        with self.assertRaisesRegex(worker.WorkerError, "ipc_truncated"):
            ipc.send("https://www.youtube.com/watch", "GET", {}, b"")

    def test_actual_yt_dlp_director_has_only_broker_handler(self):
        from yt_dlp.networking.common import Request

        outbound = io.BytesIO()
        ydl = worker.make_yt_dlp(worker.BrokerIPC(io.BytesIO(frame(response())), outbound))
        self.assertEqual(list(ydl._ies), ["Youtube"])
        self.assertEqual(len(ydl._request_director.handlers), 1)
        self.assertEqual(ydl.params["remote_components"], set())
        self.assertEqual(ydl.params["js_runtimes"], {})
        result = ydl._request_director.send(Request("https://www.youtube.com/watch?v=AbCdEf123_-"))
        self.assertEqual(result.read(), b"{}")
        outbound.seek(0)
        self.assertEqual(worker._read_frame(outbound)["kind"], "http_request")

    def test_metadata_uses_fixed_url_and_never_downloads(self):
        ydl = Mock()
        ydl.extract_info.return_value = {"id": "AbCdEf123_-", "duration": 42, "formats": [1, 2]}
        factory = Mock(return_value=ydl)
        result = worker.extract_metadata("AbCdEf123_-", Mock(), factory)
        self.assertEqual(result, {"video_id": "AbCdEf123_-", "duration_seconds": 42, "format_count": 2})
        ydl.extract_info.assert_called_once_with(
            "https://www.youtube.com/watch?v=AbCdEf123_-", download=False, ie_key="Youtube",
        )

    def test_rejects_live_or_wrong_identity(self):
        for info in ({"id": "Other123456", "duration": 42}, {"id": "AbCdEf123_-", "duration": 42, "is_live": True}):
            ydl = Mock()
            ydl.extract_info.return_value = info
            with self.assertRaisesRegex(worker.WorkerError, "unsupported_metadata"):
                worker.extract_metadata("AbCdEf123_-", Mock(), lambda _: ydl)


if __name__ == "__main__":
    unittest.main()
