#!/usr/bin/env python3
"""`chromium_bbox_driver.py` のオフライン自己テスト（Issue #1288・TASK-37・RENDER-5・MEAS-3）。

偽 Chromium（`fixtures/fake_chromium.py`）を `--chromium-cmd` で差し込み、実 Chromium・ネットワーク
なしで CDP の要求列・出力・異常系・信頼境界を検証する。実 Chromium での動作の証跡ではない
（実機は #55 で人間が確認する。REPAIR-3）。
"""

from __future__ import annotations

import contextlib
import hashlib
import io
import json
import os
import re
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))

import capture_screenshots as cs  # noqa: E402
import chromium_bbox_driver as drv  # noqa: E402
import measure_ssim as ms  # noqa: E402
import servo_bbox_driver as svd  # noqa: E402

HERE = Path(__file__).resolve().parent
FAKE = HERE / "fixtures" / "fake_chromium.py"
LAYOUT_RS = HERE.parents[1] / "servo-host" / "src" / "layout.rs"
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


@unittest.skipUnless(os.name == "posix", "pipe CDP (fd 3/4) is POSIX only")
class DriverCase(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmp.name)
        self.out_dir = self.tmp / "chromium"
        self.out_dir.mkdir()
        self.out = self.out_dir / "site-a.png"
        self.record = self.tmp / "record.jsonl"
        self.selectors = write_selectors(self.tmp / "selectors.json")

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def chromium_cmd(self, scenario: str = "ok") -> str:
        return json.dumps([sys.executable, str(FAKE), "--scenario", scenario, "--record", str(self.record)])

    def run_main(self, scenario: str = "ok", *, url: str = URL, extra: list[str] | None = None, isolated: bool = True):
        argv = ["--chromium-cmd", self.chromium_cmd(scenario), "--selectors", str(self.selectors)]
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
        self.assertFalse(svd.bbox_path_for(self.out).exists())

    def fast(self):
        """失敗系のテストを短時間で終えるため、待ち時間を縮める。"""
        stack = contextlib.ExitStack()
        for name, value in (
            ("STARTUP_TIMEOUT_S", 0.5),
            ("FONTS_TIMEOUT_S", 0.5),
            ("SCREENSHOT_TIMEOUT_S", 0.5),
            ("QUERY_TIMEOUT_S", 0.5),
            ("SHUTDOWN_GRACE_S", 0.5),
            ("TIMEOUT_SLACK_S", 0.0),
        ):
            stack.enter_context(mock.patch.object(drv, name, value))
        stack.enter_context(mock.patch.object(svd, "TIMEOUT_SLACK_S", 0.0))
        return stack


class OkScenarioTest(DriverCase):
    """RENDER-5・MEAS-3: 期待どおりの CDP 要求列を送り、矩形と PNG を出力する。"""

    def test_request_sequence_and_outputs(self) -> None:
        code, _ = self.run_main()
        self.assertEqual(code, 0)
        reqs = self.requests()
        self.assertEqual(
            [r["method"] for r in reqs],
            [
                "Browser.getVersion",
                "Target.getTargets",
                "Target.attachToTarget",
                "Page.enable",
                "Page.navigate",
                "Runtime.evaluate",
                "Page.captureScreenshot",
                "Runtime.evaluate",
                "Runtime.evaluate",
                "Target.getTargetInfo",
                "Browser.close",
            ],
        )
        self.assertEqual(reqs[2]["params"], {"targetId": "TARGET-1", "flatten": True})
        self.assertEqual(reqs[4]["params"], {"url": URL})
        self.assertEqual(reqs[5]["params"]["expression"], drv.FONTS_AND_FRAME_EXPR)
        self.assertTrue(reqs[5]["params"]["awaitPromise"])
        self.assertEqual(
            reqs[6]["params"], {"format": "png", "captureBeyondViewport": False, "fromSurface": True}
        )
        # browser レベルには sessionId が無く、page レベルには付く。
        for r in reqs:
            if r["method"] in ("Browser.getVersion", "Target.getTargets", "Target.attachToTarget", "Target.getTargetInfo", "Browser.close"):
                self.assertNotIn("sessionId", r)
            else:
                self.assertEqual(r["sessionId"], "SESSION-1")
        # セレクタは JSON 文字列リテラルとしてだけ埋め込まれる。
        self.assertEqual(reqs[7]["params"]["expression"], drv.BOUNDING_BOX_FN + '("body > header")')
        self.assertEqual(reqs[8]["params"]["expression"], drv.BOUNDING_BOX_FN + '("main")')
        bbox_path = svd.bbox_path_for(self.out)
        self.assertEqual(bbox_path.name, "site-a.bboxes.json")
        loaded = ms.load_bboxes(bbox_path)
        self.assertEqual(loaded, {"header": rect("body > header"), "main": rect("main")})
        self.assertEqual(loaded["header"], {"x": 13.0, "y": 2.5, "width": 113.0, "height": 20.0})
        self.assertIsNone(ms._bbox_png_binding_mismatch(bbox_path, self.out))
        self.assertEqual(cs.read_png_size(self.out), (1280, 800))
        payload = json.loads(bbox_path.read_text(encoding="utf-8"))
        self.assertEqual(payload["png_sha256"], hashlib.sha256(self.out.read_bytes()).hexdigest())
        self.assertEqual(payload["coordinate_space"], "viewport_css_px")
        self.assertEqual(payload["errors"], [])
        self.assertEqual(
            payload["source"],
            {"tool": "chromium_bbox_driver", "chromium_version": "HeadlessChrome/130.0.6723.58", "final_url_matches": True},
        )

    def test_argv_comes_from_saved_pages_template_and_pipe_only(self) -> None:
        self.assertEqual(self.run_main(extra=["--width", "1000", "--height", "700"])[0], 0)
        argv = self.entries()[0]["argv"]
        # prefix（--scenario/--record）の後ろがドライバの付けたフラグ。
        flags = argv[argv.index(str(self.record)) + 1 :]
        user_data = [f for f in flags if f.startswith("--user-data-dir=")]
        self.assertEqual(len(user_data), 1)
        template = [a for a in cs.SAVED_PAGES_CHROMIUM_TEMPLATE if a not in ("--screenshot={out}", "{url}")][1:]
        expected = cs.expand_template(template, {"user_data_dir": user_data[0].split("=", 1)[1], "width": "1000", "height": "700"})
        self.assertEqual(flags, [*expected, "--remote-debugging-pipe", "about:blank"])
        self.assertIn("--window-size=1000,700", flags)
        self.assertIn("--no-sandbox", flags)
        self.assertFalse([f for f in flags if f.startswith("--remote-debugging-port")])
        for banned in ("--user-agent", "--lang", "--accept-lang", "--proxy", "--host-resolver", "--virtual-time-budget", "--screenshot"):
            self.assertFalse([f for f in flags if f.startswith(banned)], banned)
        # プロファイルの一時ディレクトリは終了後に削除される。
        self.assertFalse(Path(user_data[0].split("=", 1)[1]).exists())

    def test_no_identity_override_cdp_methods(self) -> None:
        """SEC-2: UA・ヘッダ・デバイスメトリクス・locale を CDP で上書きしない。"""
        self.assertEqual(self.run_main()[0], 0)
        methods = {r["method"] for r in self.requests()}
        self.assertEqual(
            methods,
            {
                "Browser.getVersion", "Target.getTargets", "Target.attachToTarget", "Page.enable", "Page.navigate",
                "Runtime.evaluate", "Page.captureScreenshot", "Target.getTargetInfo", "Browser.close",
            },
        )
        for m in methods:
            self.assertFalse(m.startswith("Emulation."), m)
            self.assertNotIn("Override", m)
            self.assertNotIn("setExtraHTTPHeaders", m)

    def test_load_timeout_range_and_stale_outputs_replaced(self) -> None:
        svd.bbox_path_for(self.out).write_text("stale", encoding="utf-8")
        self.out.write_bytes(b"stale")
        self.assertEqual(self.run_main()[0], 0)
        self.assertIsNone(ms._bbox_png_binding_mismatch(svd.bbox_path_for(self.out), self.out))
        with self.assertRaises(SystemExit):
            with contextlib.redirect_stderr(io.StringIO()):
                drv.main(["--chromium-bin", "x", "--selectors", "s", "--load-timeout-ms", "0", URL, "o.png"])

    def test_no_entry_for_site_writes_png_only_with_warning(self) -> None:
        write_selectors(self.selectors, [{"site_id": "other", "elements": SELECTORS}])
        code, err = self.run_main()
        self.assertEqual(code, 0)
        self.assertIn("no selectors for site_id 'site-a'", err)
        self.assertTrue(self.out.exists())
        self.assertFalse(svd.bbox_path_for(self.out).exists())

    def test_element_level_failures_are_recorded_and_dropped(self) -> None:
        write_selectors(
            self.selectors,
            [{"site_id": "site-a", "elements": [
                {"id": "ok", "selector": "main"},
                {"id": "gone", "selector": "missing"},
                {"id": "bad", "selector": "bad["},
            ]}],
        )
        code, _ = self.run_main()
        self.assertEqual(code, 0)
        payload = json.loads(svd.bbox_path_for(self.out).read_text(encoding="utf-8"))
        self.assertEqual([e["id"] for e in payload["elements"]], ["ok"])
        self.assertEqual(payload["errors"], [{"id": "gone", "kind": "not_found"}, {"id": "bad", "kind": "invalid_params"}])
        self.assertIsNone(ms._bbox_png_binding_mismatch(svd.bbox_path_for(self.out), self.out))

    def test_negative_zero_is_normalized(self) -> None:
        outcome = drv.interpret_bbox_value({"found": True, "x": -0.0, "y": 0, "width": 1, "height": 2})
        self.assertEqual(str(outcome["box"]["x"]), "0.0")


class FailureScenarioTest(DriverCase):
    """サイト全体の失敗は終了コード 1 で、PNG・bbox のどちらも残さない。"""

    def assertFails(self, scenario: str, needle: str | None = None) -> None:
        with self.fast():
            code, err = self.run_main(scenario)
        self.assertEqual(code, 1, err)
        self.assertNoOutputs()
        if needle:
            self.assertIn(needle, err)

    def test_navigate_error(self) -> None:
        self.assertFails("navigate_error", "navigate failed")

    def test_no_load_event_falls_back_to_ready_state_and_fails(self) -> None:
        self.assertFails("no_load_event", "load event")

    def test_eval_exception(self) -> None:
        self.assertFails("eval_exception", "exception")

    def test_fonts_hang(self) -> None:
        self.assertFails("fonts_hang", "timed out")

    def test_oversized_message_without_terminator(self) -> None:
        with mock.patch.object(drv, "RESPONSE_MESSAGE_MAX", 64 * 1024):
            self.assertFails("oversized_message", "byte limit")

    def test_wrong_response_id(self) -> None:
        self.assertFails("wrong_id", "does not match")

    def test_bad_png_size(self) -> None:
        self.assertFails("bad_png_size", "does not match the requested")

    def test_bad_base64(self) -> None:
        self.assertFails("bad_base64", "base64")

    def test_nonfinite_rect(self) -> None:
        self.assertFails("nonfinite_rect", "must be a number")

    def test_event_flood(self) -> None:
        self.assertFails("event_flood", "unsolicited")

    def test_target_crashed(self) -> None:
        self.assertFails("target_crashed", "crashed")

    def test_close_without_response_is_only_a_warning(self) -> None:
        with self.fast():
            code, err = self.run_main("close_without_response")
        self.assertEqual(code, 0)
        self.assertIn("Browser.close did not complete cleanly", err)
        self.assertIsNone(ms._bbox_png_binding_mismatch(svd.bbox_path_for(self.out), self.out))

    def test_stderr_flood_is_bounded_and_does_not_block(self) -> None:
        code, _ = self.run_main("stderr_flood")
        self.assertEqual(code, 0)

    def test_grandchild_is_killed_with_the_group(self) -> None:
        with self.fast():
            code, _ = self.run_main("spawn_grandchild_and_hang")
        self.assertEqual(code, 1)
        pid = int((Path(str(self.record) + ".grandchild")).read_text(encoding="utf-8"))
        deadline = time.monotonic() + 5
        alive = True
        while alive and time.monotonic() < deadline:
            try:
                os.kill(pid, 0)
                time.sleep(0.1)
            except ProcessLookupError:
                alive = False
        self.assertFalse(alive, "grandchild process survived")

    def test_driver_sigkill_is_cleaned_up_by_watchdog(self) -> None:
        """ドライバーが SIGKILL されても（finally が走らなくても）Chromium の子孫が残らない。"""
        import signal
        import subprocess

        argv = [sys.executable, str(HERE / "chromium_bbox_driver.py"), "--chromium-cmd", self.chromium_cmd("spawn_grandchild_and_hang"),
                "--selectors", str(self.selectors), "--assume-isolated", URL, str(self.out)]
        # capture_screenshots と同様、ドライバーを自分のグループで起動して、そのグループへ SIGKILL を送る。
        proc = subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
        marker = Path(str(self.record) + ".grandchild")
        deadline = time.monotonic() + 10
        while not marker.exists() and time.monotonic() < deadline:
            time.sleep(0.05)
        self.assertTrue(marker.exists(), "fake chromium did not start")
        pid = int(marker.read_text(encoding="utf-8"))
        time.sleep(0.3)
        os.killpg(proc.pid, signal.SIGKILL)
        proc.wait()
        deadline = time.monotonic() + 10
        alive = True
        while alive and time.monotonic() < deadline:
            try:
                os.kill(pid, 0)
                time.sleep(0.1)
            except ProcessLookupError:
                alive = False
        self.assertFalse(alive, "grandchild survived driver SIGKILL")

    def test_launch_failure(self) -> None:
        err = io.StringIO()
        argv = ["--chromium-cmd", json.dumps(["/nonexistent/chromium"]), "--selectors", str(self.selectors),
                "--assume-isolated", URL, str(self.out)]
        with contextlib.redirect_stderr(err):
            self.assertEqual(drv.main(argv), 1)
        self.assertIn("failed to launch chromium", err.getvalue())


class UsageTest(DriverCase):
    """使い方の違反は終了コード 2 で、Chromium を起動しない。"""

    def assertUsage(self, code_err: tuple[int, str], needle: str) -> None:
        code, err = code_err
        self.assertEqual(code, 2, err)
        self.assertIn(needle, err)
        self.assertFalse(self.record.exists())

    def test_requires_assume_isolated(self) -> None:
        self.assertUsage(self.run_main(isolated=False), "--assume-isolated")

    def test_url_rules_are_shared(self) -> None:
        for bad in ("https://pages.test:8080/a", "http://example.com/", "http://127.0.0.1:8080/", "file:///etc/passwd"):
            self.assertEqual(self.run_main(url=bad)[0], 2, bad)
        self.assertFalse(self.record.exists())

    def test_output_rules(self) -> None:
        self.out = self.out_dir / "site-a.jpg"
        self.assertUsage(self.run_main(), ".png")
        self.out = self.tmp / "missing" / "site-a.png"
        self.assertUsage(self.run_main(), "does not exist")
        real = self.tmp / "real"
        real.mkdir()
        link = self.tmp / "link"
        link.symlink_to(real)
        self.out = link / "site-a.png"
        self.assertUsage(self.run_main(), "symlinked")

    def test_chromium_bin_and_cmd_are_exclusive(self) -> None:
        with self.assertRaises(SystemExit), contextlib.redirect_stderr(io.StringIO()):
            drv.main(["--chromium-bin", "x", "--chromium-cmd", "[]", "--selectors", "s", URL, "o.png"])

    def test_chromium_cmd_must_be_string_array(self) -> None:
        for bad in ("not json", "[]", "[1]", '{"a":1}'):
            argv = ["--chromium-cmd", bad, "--selectors", str(self.selectors), "--assume-isolated", URL, str(self.out)]
            with contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(drv.main(argv), 2, bad)

    def test_non_posix_exits_2(self) -> None:
        with mock.patch.object(drv.os, "name", "nt"):
            code, err = self.run_main()
        self.assertEqual(code, 2)
        self.assertIn("unsupported platform", err)

    def test_selector_file_is_shared_with_servo_driver(self) -> None:
        self.assertIs(drv.svd.load_selectors, svd.load_selectors)
        self.assertEqual(svd.load_selectors(self.selectors), {"site-a": SELECTORS})


class ParityTest(unittest.TestCase):
    """Servo ホストと同じ JS 関数式であること（座標の意味論を揃える）。"""

    def test_bounding_box_fn_matches_servo_host_layout_rs(self) -> None:
        text = LAYOUT_RS.read_text(encoding="utf-8")
        prelude = re.search(r'macro_rules! selector_prelude.*?r#"(.*?)"#', text, re.S)
        box = re.search(r'const BOUNDING_BOX_FN: &str = concat!\(\s*selector_prelude!\(\),\s*r#"(.*?)"#', text, re.S)
        self.assertIsNotNone(prelude, "selector_prelude! not found in layout.rs")
        self.assertIsNotNone(box, "BOUNDING_BOX_FN not found in layout.rs")
        self.assertEqual(prelude.group(1) + box.group(1), drv.BOUNDING_BOX_FN)


@unittest.skipUnless(os.name == "posix", "pipe CDP (fd 3/4) is POSIX only")
class SavedPagesIntegrationTest(DriverCase):
    """`capture_screenshots.py --chromium-cmd` として差し込み、measure_ssim の入力契約を満たす。"""

    def test_chromium_cmd_integration(self) -> None:
        sites = {
            "schema_version": 1,
            "viewport": {"width": 1280, "height": 800},
            "sites": [{"id": "site-a", "url": URL, "category": "static", "catalog_id": "z1"}],
        }
        sites_path = self.tmp / "sites.json"
        sites_path.write_text(json.dumps(sites), encoding="utf-8")
        template = [
            sys.executable,
            str(HERE / "chromium_bbox_driver.py"),
            "--chromium-cmd",
            self.chromium_cmd(),
            "--selectors",
            str(self.selectors),
            "--width",
            "{width}",
            "--height",
            "{height}",
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
                    "chromium",
                    "--min-sites",
                    "1",
                    "--sites",
                    str(sites_path),
                    "--out-dir",
                    str(capture_dir),
                    "--chromium-cmd",
                    json.dumps(template),
                ]
            )
        self.assertEqual(code, 0)
        png = capture_dir / "chromium" / "site-a.png"
        bboxes = capture_dir / "chromium" / "site-a.bboxes.json"
        self.assertTrue(png.exists())
        self.assertEqual(sorted(ms.load_bboxes(bboxes)), ["header", "main"])
        self.assertIsNone(ms._bbox_png_binding_mismatch(bboxes, png))


if __name__ == "__main__":
    unittest.main()
