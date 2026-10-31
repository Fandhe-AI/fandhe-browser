"""listeners.py の自己テスト（Issue #1377・TASK-38・`SEC-2`）。loopback のみ使用。"""

from __future__ import annotations

import socket
import sys
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import listeners  # noqa: E402


def wait_count(rec: object, n: int) -> None:
    deadline = time.monotonic() + 3
    while time.monotonic() < deadline and rec.count < n:  # type: ignore[attr-defined]
        time.sleep(0.02)


class BindRestrictionTest(unittest.TestCase):
    def test_non_loopback_and_low_ports_rejected(self) -> None:
        for ip, port in (("0.0.0.0", 0), ("10.0.0.1", 0), ("192.168.1.1", 2000), ("not-an-ip", 0),
                         ("127.0.0.2", 80), ("127.0.0.2", 70000), ("127.0.0.2", True)):
            with self.assertRaises(ValueError, msg=f"{ip}:{port}"):
                listeners.validate_bind(ip, port)

    def test_loopback_alias_accepted(self) -> None:
        listeners.validate_bind("127.0.0.2", 0)
        listeners.validate_bind("127.9.9.9", 1024)


class SanitizeTest(unittest.TestCase):
    def test_query_and_fragment_removed(self) -> None:
        self.assertEqual(listeners.sanitize_path("/a/b?token=abc#frag"), "/a/b")

    def test_path_length_capped(self) -> None:
        self.assertEqual(len(listeners.sanitize_path("/" + "a" * 1000)), listeners.MAX_PATH_CHARS)

    def test_request_line_parsing(self) -> None:
        self.assertEqual(listeners.parse_request_line(b"GET /x?q=1 HTTP/1.1\r\nHost: a\r\n\r\n"), ("GET", "/x"))
        self.assertEqual(listeners.parse_request_line(b"\x16\x03\x01garbage"), (None, None))


class TcpRecorderTest(unittest.TestCase):
    def test_records_http_request_without_query_or_body(self) -> None:
        rec = listeners.TcpRecorder("t", "127.0.0.2").start()
        try:
            with socket.create_connection(("127.0.0.2", rec.port), timeout=2) as c:
                c.sendall(b"POST /p?secret=1 HTTP/1.1\r\nHost: x\r\n\r\nbody-secret")
                self.assertTrue(c.recv(100).startswith(b"HTTP/1.1 204"))
            wait_count(rec, 1)
            self.assertEqual(rec.count, 1)
            self.assertEqual(rec.records(), [{"peer": "127.0.0.1", "method": "POST", "path": "/p"}])
            self.assertNotIn("secret", repr(rec.records()))
        finally:
            rec.stop()

    def test_connect_only_is_counted(self) -> None:
        rec = listeners.TcpRecorder("t", "127.0.0.3").start()
        try:
            socket.create_connection(("127.0.0.3", rec.port), timeout=2).close()
            wait_count(rec, 1)
            self.assertEqual(rec.count, 1)
            self.assertIsNone(rec.records()[0]["method"])
        finally:
            rec.stop()

    def test_record_cap(self) -> None:
        old = listeners.MAX_RECORDS
        listeners.MAX_RECORDS = 2
        rec = listeners.TcpRecorder("t", "127.0.0.2").start()
        try:
            for _ in range(4):
                socket.create_connection(("127.0.0.2", rec.port), timeout=2).close()
            wait_count(rec, 4)
            self.assertEqual(rec.count, 4)
            self.assertEqual(len(rec.records()), 2)
        finally:
            rec.stop()
            listeners.MAX_RECORDS = old


class UdpRecorderTest(unittest.TestCase):
    def test_records_length_only(self) -> None:
        rec = listeners.UdpRecorder("u", "127.0.0.2").start()
        try:
            s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            s.sendto(b"abcdef", ("127.0.0.2", rec.port))
            s.close()
            wait_count(rec, 1)
            self.assertEqual(rec.records(), [{"peer": "127.0.0.1", "length": 6}])
        finally:
            rec.stop()


if __name__ == "__main__":
    unittest.main()
