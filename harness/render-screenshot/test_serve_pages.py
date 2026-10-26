"""`serve_pages.py` の自己テスト（Issue #1281 / TASK-37 / RENDER-5）。

一時ディレクトリを 127.0.0.1 のエフェメラルポートで配る（loopback に閉じ、実ネットワークへは出ない）。
"""

from __future__ import annotations

import http.client
import sys
import tempfile
import threading
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import serve_pages as sp  # noqa: E402


class ServePagesTest(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        base = Path(self._tmp.name)
        self.root = base / "root"
        (self.root / "site").mkdir(parents=True)
        (self.root / "site" / "index.html").write_text("<p>hello</p>", encoding="utf-8")
        (self.root / "empty").mkdir()
        self.outside = base / "outside.txt"
        self.outside.write_text("secret", encoding="utf-8")
        self.symlink_ok = True
        try:
            (self.root / "leak").symlink_to(self.outside)
        except OSError:
            self.symlink_ok = False
        self.server = sp.make_server(self.root, 0)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.addCleanup(self._stop)

    def _stop(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)

    def _request(self, method: str, path: str) -> tuple[int, bytes]:
        conn = http.client.HTTPConnection("127.0.0.1", self.server.server_address[1], timeout=5)
        try:
            conn.request(method, path)
            resp = conn.getresponse()
            return resp.status, resp.read()
        finally:
            conn.close()

    def test_binds_to_loopback_only(self) -> None:
        self.assertEqual(self.server.server_address[0], "127.0.0.1")

    def test_serves_file_and_directory_index(self) -> None:
        self.assertEqual(self._request("GET", "/site/index.html"), (200, b"<p>hello</p>"))
        self.assertEqual(self._request("GET", "/site/"), (200, b"<p>hello</p>"))
        self.assertEqual(self._request("HEAD", "/site/index.html")[0], 200)

    def test_no_directory_listing(self) -> None:
        self.assertEqual(self._request("GET", "/empty/")[0], 404)
        self.assertEqual(self._request("GET", "/")[0], 404)

    def test_path_traversal_and_symlink_escape_are_404(self) -> None:
        self.assertEqual(self._request("GET", "/../outside.txt")[0], 404)
        self.assertEqual(self._request("GET", "/%2e%2e/outside.txt")[0], 404)
        if self.symlink_ok:
            self.assertEqual(self._request("GET", "/leak")[0], 404)

    def test_non_get_methods_are_405(self) -> None:
        for method in ("POST", "PUT", "DELETE"):
            with self.subTest(method=method):
                self.assertEqual(self._request(method, "/site/index.html")[0], 405)

    def test_port_validation(self) -> None:
        for port in (80, 1023, 65536):
            with self.subTest(port=port), self.assertRaises(ValueError):
                sp.make_server(self.root, port)
        self.assertEqual(sp.main(["--root", str(self.root / "missing"), "--port", "8080"]), 2)


if __name__ == "__main__":
    unittest.main()
