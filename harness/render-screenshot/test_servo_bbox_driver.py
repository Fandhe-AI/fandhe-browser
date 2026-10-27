#!/usr/bin/env python3
"""`servo_bbox_driver.py` のオフライン自己テスト（Issue #1282・TASK-37・RENDER-5・MEAS-3）。

偽ホスト（`fixtures/fake_render_host.py`）を `--host-cmd` で差し込み、実ホスト・ネットワーク
なしでメッセージ列・出力・異常系・信頼境界を検証する。実 Servo ホストでの動作の証跡ではない。
"""

from __future__ import annotations

import contextlib
import hashlib
import io
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))

import capture_screenshots as cs  # noqa: E402
import measure_ssim as ms  # noqa: E402
import servo_bbox_driver as drv  # noqa: E402

HERE = Path(__file__).resolve().parent
FAKE_HOST = HERE / "fixtures" / "fake_render_host.py"
URL = "http://pages.test:8080/site-a/index.html"
SELECTORS = [
    {"id": "header", "selector": "body > header"},
    {"id": "main", "selector": "main"},
]


def rect(selector: str) -> dict:
    return {"x": float(len(selector)), "y": 2.5, "width": 100.0 + len(selector), "height": 20.0}


def write_selectors(path: Path, sites: list[dict] | None = None) -> Path:
    payload = {"schema_version": 1, "sites": sites if sites is not None else [{"site_id": "site-a", "elements": SELECTORS}]}
    path.write_text(json.dumps(payload), encoding="utf-8")
    return path


class DriverCase(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmp.name)
        self.out_dir = self.tmp / "servo"
        self.out_dir.mkdir()
        self.out = self.out_dir / "site-a.png"
        self.record = self.tmp / "record.jsonl"
        self.selectors = write_selectors(self.tmp / "selectors.json")

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def host_cmd(self, scenario: str = "ok") -> str:
        return json.dumps([sys.executable, str(FAKE_HOST), "--scenario", scenario, "--record", str(self.record)])

    def run_main(self, scenario: str = "ok", *, url: str = URL, extra: list[str] | None = None, isolated: bool = True):
        argv = ["--host-cmd", self.host_cmd(scenario), "--selectors", str(self.selectors)]
        if isolated:
            argv.append("--assume-isolated")
        argv += extra or []
        argv += [url, str(self.out)]
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            code = drv.main(argv)
        return code, err.getvalue()

    def entries(self) -> list[dict]:
        return [json.loads(line) for line in self.record.read_text(encoding="utf-8").splitlines()]

    def requests(self) -> list[dict]:
        return [e["message"] for e in self.entries() if e["kind"] == "request"]

    def assertNoOutputs(self) -> None:
        self.assertFalse(self.out.exists())
        self.assertFalse(drv.bbox_path_for(self.out).exists())


class OkScenarioTest(DriverCase):
    """RENDER-5・RENDER-3: 期待どおりのメッセージ列を送り、矩形と PNG を出力する。"""

    def test_request_sequence_and_outputs(self) -> None:
        code, _ = self.run_main()
        self.assertEqual(code, 0)
        self.assertEqual(
            self.requests(),
            [
                {"v": 1, "id": 1, "method": "load", "params": {"url": URL}},
                {"v": 1, "id": 2, "method": "screenshot", "params": {"full_page": False}},
                {"v": 1, "id": 3, "method": "bounding_box", "params": {"selector": "body > header"}},
                {"v": 1, "id": 4, "method": "bounding_box", "params": {"selector": "main"}},
                {"v": 1, "id": 5, "method": "shutdown"},
            ],
        )
        bbox_path = drv.bbox_path_for(self.out)
        self.assertEqual(bbox_path.name, "site-a.bboxes.json")
        loaded = ms.load_bboxes(bbox_path)
        self.assertEqual(
            loaded,
            {"header": rect("body > header"), "main": rect("main")},
        )
        self.assertEqual(loaded["header"], {"x": 13.0, "y": 2.5, "width": 113.0, "height": 20.0})
        self.assertIsNone(ms._bbox_png_binding_mismatch(bbox_path, self.out))
        self.assertEqual(cs.read_png_size(self.out), (1280, 800))
        payload = json.loads(bbox_path.read_text(encoding="utf-8"))
        self.assertEqual(payload["png_sha256"], hashlib.sha256(self.out.read_bytes()).hexdigest())
        self.assertEqual(payload["coordinate_space"], "viewport_css_px")
        self.assertEqual(payload["errors"], [])
        self.assertEqual(
            payload["source"],
            {"tool": "servo_bbox_driver", "host_version": "0.0.0", "servo_version": "0.6.0", "final_url_matches": True},
        )

    def test_load_timeout_is_forwarded(self) -> None:
        code, _ = self.run_main(extra=["--load-timeout-ms", "1234"])
        self.assertEqual(code, 0)
        self.assertEqual(self.requests()[0]["params"], {"url": URL, "timeout_ms": 1234})

    def test_stale_outputs_are_replaced(self) -> None:
        drv.bbox_path_for(self.out).write_text("stale", encoding="utf-8")
        self.out.write_bytes(b"stale")
        self.assertEqual(self.run_main()[0], 0)
        self.assertIsNone(ms._bbox_png_binding_mismatch(drv.bbox_path_for(self.out), self.out))

    def test_no_entry_for_site_writes_png_only_with_warning(self) -> None:
        write_selectors(self.selectors, [{"site_id": "other", "elements": SELECTORS}])
        code, err = self.run_main()
        self.assertEqual(code, 0)
        self.assertIn("no selectors for site_id 'site-a'", err)
        self.assertTrue(self.out.exists())
        self.assertEqual([r["method"] for r in self.requests()], ["load", "screenshot", "shutdown"])
        self.assertFalse(drv.bbox_path_for(self.out).exists())

    def test_explicit_site_id(self) -> None:
        write_selectors(self.selectors, [{"site_id": "x", "elements": SELECTORS[:1]}])
        code, _ = self.run_main(extra=["--site-id", "x"])
        self.assertEqual(code, 0)
        self.assertEqual(list(ms.load_bboxes(drv.bbox_path_for(self.out))), ["header"])


class ResourceLimitTest(DriverCase):
    """受信キュー・stderr・送信の資源上限（有界であること）。"""

    def test_unsolicited_frames_overflow_fails_and_reader_exits(self) -> None:
        code = (
            "import struct,sys\n"
            "b=b'{}'\n"
            "for _ in range(50):\n"
            "    sys.stdout.buffer.write(struct.pack('<I',len(b))+b)\n"
            "sys.stdout.buffer.flush()\n"
            "import time; time.sleep(30)\n"
        )
        proc = subprocess.Popen(
            [sys.executable, "-c", code], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL
        )
        try:
            chan = drv.HostChannel(proc)
            chan._thread.join(timeout=10)
            self.assertFalse(chan._thread.is_alive())
            messages = []
            with self.assertRaises(drv.HostError) as ctx:
                for _ in range(60):
                    chan.recv(1.0)
                    messages.append(1)
            self.assertEqual(len(messages), drv.MAX_PENDING_FRAMES)
            self.assertIn("unsolicited", str(ctx.exception))
        finally:
            proc.kill()
            proc.wait()

    def test_stderr_flood_is_bounded_and_does_not_block(self) -> None:
        code, _ = self.run_main("stderr_flood")
        self.assertEqual(code, 0)

    def test_stderr_tail_keeps_only_the_end(self) -> None:
        data = b"a" * 100_000 + b"END"
        tail = drv.StderrTail(io.BytesIO(data))
        text = tail.text()
        self.assertEqual(len(text), drv.STDERR_TAIL_BYTES)
        self.assertTrue(text.endswith("END"))

    def test_send_timeout_kills_host_that_does_not_read(self) -> None:
        proc = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(60)"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
        try:
            chan = drv.HostChannel(proc)
            chan.request_max = 4 * 1024 * 1024
            big = {"v": 1, "id": 1, "method": "load", "params": {"url": "x" * (2 * 1024 * 1024)}}
            with self.assertRaises(drv.HostError) as ctx:
                chan.send(big, 0.5)
            self.assertIn("timed out writing", str(ctx.exception))
            drv._stop_host(proc, False)
            self.assertIsNotNone(proc.poll())
        finally:
            if proc.poll() is None:
                proc.kill()
            proc.wait()


class ElementErrorTest(DriverCase):
    def test_not_found_is_recorded_and_excluded(self) -> None:
        write_selectors(
            self.selectors,
            [{"site_id": "site-a", "elements": [{"id": "gone", "selector": "#missing"}, SELECTORS[1]]}],
        )
        code, _ = self.run_main("not_found_one")
        self.assertEqual(code, 0)
        payload = json.loads(drv.bbox_path_for(self.out).read_text(encoding="utf-8"))
        self.assertEqual([e["id"] for e in payload["elements"]], ["main"])
        self.assertEqual(payload["errors"], [{"id": "gone", "kind": "not_found"}])


class FailureTest(DriverCase):
    """ホスト・プロトコルの失敗は終了コード 1 で、PNG・bbox を残さない。"""

    def _assert_fails(self, scenario: str, message: str) -> None:
        drv.bbox_path_for(self.out).write_text("stale", encoding="utf-8")
        self.out.write_bytes(b"stale")
        code, err = self.run_main(scenario)
        self.assertEqual(code, 1, err)
        self.assertIn(message, err)
        self.assertNoOutputs()

    def test_missing_capability(self) -> None:
        self._assert_fails("missing_capability", "bounding_box")

    def test_bad_protocol_version(self) -> None:
        self._assert_fails("bad_protocol_version", "protocol 1")

    def test_oversized_frame(self) -> None:
        self._assert_fails("oversized_frame", "exceeds")

    def test_result_and_error(self) -> None:
        self._assert_fails("result_and_error", "exactly one of result and error")

    def test_wrong_id(self) -> None:
        self._assert_fails("wrong_id", "does not match")

    def test_nonfinite_bbox(self) -> None:
        self._assert_fails("nonfinite_bbox", "width/height must be >= 0")

    def test_load_failed(self) -> None:
        self._assert_fails("load_failed", "load failed: load_failed")

    def test_close_without_response(self) -> None:
        self._assert_fails("close_without_response", "closed stdout")

    def test_bad_png_size(self) -> None:
        self._assert_fails("bad_png", "does not match the reported")

    def test_query_timeout_fails_whole_site(self) -> None:
        self._assert_fails("query_timeout", "bounding_box failed for header: timeout")

    def test_handshake_timeout_kills_host(self) -> None:
        with mock.patch.object(drv, "HANDSHAKE_TIMEOUT_S", 0.5):
            self._assert_fails("no_hello", "timed out")

    def test_missing_host_binary(self) -> None:
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            code = drv.main(
                ["--host-bin", str(self.tmp / "nope"), "--selectors", str(self.selectors), "--assume-isolated", URL, str(self.out)]
            )
        self.assertEqual(code, 1)
        self.assertIn("failed to launch the host", err.getvalue())


class UsageTest(DriverCase):
    def test_non_saved_pages_urls_rejected(self) -> None:
        for url in (
            "https://pages.test:8080/a",
            "http://example.com:8080/a",
            "http://pages.test/a",
            "http://user:pw@pages.test:8080/a",
            "http://pages.test:80/a",
        ):
            code, _ = self.run_main(url=url)
            self.assertEqual(code, 2, url)
        self.assertFalse(self.record.exists())

    def test_requires_assume_isolated(self) -> None:
        code, err = self.run_main(isolated=False)
        self.assertEqual(code, 2)
        self.assertIn("--assume-isolated", err)
        self.assertFalse(self.record.exists())

    def test_out_must_be_png(self) -> None:
        self.out = self.out_dir / "site-a.txt"
        self.assertEqual(self.run_main()[0], 2)

    def test_symlinked_output_dir_rejected(self) -> None:
        link = self.tmp / "link"
        try:
            link.symlink_to(self.out_dir, target_is_directory=True)
        except OSError:
            self.skipTest("symlink unsupported")
        self.out = link / "site-a.png"
        self.assertEqual(self.run_main()[0], 2)


class SelectorsFileTest(DriverCase):
    def _load(self, payload: object) -> dict:
        self.selectors.write_text(json.dumps(payload), encoding="utf-8")
        return drv.load_selectors(self.selectors)

    def _bad(self, payload: object) -> None:
        with self.assertRaises(drv.UsageError):
            self._load(payload)

    def site(self, elements: list[dict], site_id: str = "s") -> dict:
        return {"schema_version": 1, "sites": [{"site_id": site_id, "elements": elements}]}

    def test_valid(self) -> None:
        self.assertEqual(self._load(self.site(SELECTORS)), {"s": SELECTORS})

    def test_violations(self) -> None:
        self._bad(self.site([{"id": "-bad", "selector": "a"}]))
        self._bad(self.site([{"id": "a", "selector": "a"}, {"id": "a", "selector": "b"}]))
        self._bad(self.site([{"id": "a", "selector": ""}]))
        self._bad(self.site([{"id": "a", "selector": "a" * 1025}]))
        for bad in ("a\n", "a\x00", "a\x7f", "a\x85", "a\u2028", "a\u2029"):
            self._bad(self.site([{"id": "a", "selector": bad}]))
        self._bad(self.site([{"id": "a", "selector": "a", "extra": 1}]))
        self._bad(self.site([{"id": f"e{i}", "selector": "a"} for i in range(ms.MAX_BBOX_ELEMENTS + 1)]))
        self._bad({"schema_version": 2, "sites": []})
        self._bad({"schema_version": 1, "sites": [], "extra": 1})
        self._bad({"schema_version": 1, "sites": [{"site_id": "s", "elements": []}, {"site_id": "s", "elements": []}]})
        self._bad({"schema_version": 1, "sites": [{"site_id": f"s{i}", "elements": []} for i in range(drv.MAX_SITES + 1)]})

    def test_boundary_selector_lengths(self) -> None:
        self.assertIn("s", self._load(self.site([{"id": "a", "selector": "a" * 1024}])))

    def test_file_size_limit(self) -> None:
        self.selectors.write_bytes(b" " * (drv.MAX_SELECTORS_FILE_BYTES + 1))
        with self.assertRaises(drv.UsageError):
            drv.load_selectors(self.selectors)


class HostEnvTest(DriverCase):
    def test_env_is_allowlist_only(self) -> None:
        with mock.patch.dict("os.environ", {"HTTP_PROXY": "http://x", "LANG": "ja_JP.UTF-8", "TZ": "Asia/Tokyo"}):
            self.assertEqual(self.run_main()[0], 0)
        env = self.entries()[0]["env"]
        for banned in ("HTTP_PROXY", "http_proxy", "LANG", "TZ", "PATH", "LD_PRELOAD", "DYLD_INSERT_LIBRARIES"):
            self.assertNotIn(banned, env)
        if sys.platform.startswith("win"):
            self.assertIn("TMP", env)
        else:
            self.assertTrue(env["HOME"].startswith(str(Path(env["TMPDIR"]).parent)))
            self.assertNotEqual(env["HOME"], str(Path.home()))
        # 一時ディレクトリは終了後に削除される。
        self.assertFalse(Path(env.get("HOME", env.get("TMP", ""))).exists())

    def test_expected_names_per_platform(self) -> None:
        wd = Path("/w")
        self.assertEqual(
            sorted(drv.build_host_env(wd, "linux")),
            sorted(["HOME", "TMPDIR", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME"]),
        )
        self.assertEqual(sorted(drv.build_host_env(wd, "darwin")), ["HOME", "TMPDIR"])
        with mock.patch.dict("os.environ", {"SystemRoot": "C:\\Windows"}):
            self.assertEqual(sorted(drv.build_host_env(wd, "win32")), ["SystemRoot", "TEMP", "TMP"])


class SavedPagesIntegrationTest(DriverCase):
    """`capture_screenshots.py --servo-cmd` として差し込み、measure_ssim の入力契約を満たす。"""

    def test_servo_cmd_integration(self) -> None:
        sites = {
            "schema_version": 1,
            "viewport": {"width": 1280, "height": 800},
            "sites": [{"id": "site-a", "url": URL, "category": "static", "catalog_id": "z1"}],
        }
        sites_path = self.tmp / "sites.json"
        sites_path.write_text(json.dumps(sites), encoding="utf-8")
        template = [
            sys.executable,
            str(HERE / "servo_bbox_driver.py"),
            "--host-cmd",
            self.host_cmd(),
            "--selectors",
            str(self.selectors),
            "--assume-isolated",
            "{url}",
            "{out}",
        ]
        capture_dir = self.tmp / "cap"
        report = {"schema_version": 1, "isolated": True, "checks": [{"name": "x", "ok": True, "detail": "d"}]}
        with mock.patch.object(cs.isolation_check, "run_isolation_check", return_value=report):
            code = cs.main(
                [
                    "--saved-pages",
                    "--allow-unproxied-engine",
                    "--engines",
                    "servo",
                    "--min-sites",
                    "1",
                    "--sites",
                    str(sites_path),
                    "--out-dir",
                    str(capture_dir),
                    "--servo-cmd",
                    json.dumps(template),
                ]
            )
        self.assertEqual(code, 0)
        png = capture_dir / "servo" / "site-a.png"
        bboxes = capture_dir / "servo" / "site-a.bboxes.json"
        self.assertTrue(png.exists())
        self.assertEqual(sorted(ms.load_bboxes(bboxes)), ["header", "main"])
        self.assertIsNone(ms._bbox_png_binding_mismatch(bboxes, png))


if __name__ == "__main__":
    unittest.main()
