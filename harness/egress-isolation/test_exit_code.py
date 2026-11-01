"""管理者操作の復元・解除の失敗が終了コードに出ること、unobservable に降格した受信の注記の自己テスト。

Issue #1401（Windows のループバック免除）・#1402（macOS の sudo `log config`）の事後監査（P2-1・P2-2・P3-1・P3-3）。
Linux 上で mock だけで動く（実ホスト・管理者操作は行わない）。判定の結果では終了コードを変えない。
"""

from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from typing import Any
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import appcontainer as ac  # noqa: E402
import judge  # noqa: E402
import measure  # noqa: E402

SID = "S-1-15-2-1-2-3-4-5-6-7"
META = {"date_utc": "2026-11-01T00:00:00Z", "kernel": "k", "host_sha256": "x", "host_version": "v",
        "proxy_env_present": False}
ROUTES = ("redirect", "img", "fetch", "ws", "internal", "udp_direct")

SUBS = ("com.apple.sandbox", "com.apple.sandbox.reporting")
CHANGED_RESTORED = {"all_restored": True, "subsystems": {s: {"raise_attempted": True, "restored": True} for s in SUBS}}
CHANGED_NOT_RESTORED = {"all_restored": False,
                        "subsystems": {s: {"raise_attempted": True, "restored": False} for s in SUBS}}
ABORTED = {"all_restored": False, "aborted": "no change made", "subsystems": {s: {} for s in SUBS}}


def _results(*, log_config: Any = None, exempt: Any = None, cleanup: Any = None, verdict: str = "pass") -> dict[str, Any]:
    iso: dict[str, Any] = {"outer_counts": {}, "verdicts": {"fetch": {
        "verdict": verdict, "reason": "r", "outer_count": 0, "control_count": 1, "method": "m"}}, "inner": {}}
    if log_config is not None:
        iso["log_config"] = log_config
    if exempt is not None:
        iso["appcontainer"] = {"loopback_exempt": exempt, "cleanup": cleanup or {}, "preflight": {}}
    return {"meta": META, "modes": {"baseline": {"outer_counts": {}}, "seatbelt": iso}}


class FinalizeExitCodeTest(unittest.TestCase):
    def _run(self, results: dict[str, Any]) -> tuple[int, Path]:
        td = tempfile.TemporaryDirectory()
        self.addCleanup(td.cleanup)
        with mock.patch("builtins.print"), mock.patch.object(sys, "stderr"):
            return measure.finalize(results, Path(td.name)), Path(td.name)

    def test_macos_log_config_not_restored_is_3_after_files_are_written(self) -> None:
        rc, out = self._run(_results(log_config=CHANGED_NOT_RESTORED))
        self.assertEqual(rc, 3)
        self.assertTrue((out / "summary.md").is_file())
        self.assertIn("seatbelt", json.loads((out / "result.json").read_text(encoding="utf-8"))["modes"])

    def test_macos_log_config_restored_is_0(self) -> None:
        self.assertEqual(self._run(_results(log_config=CHANGED_RESTORED))[0], 0)

    def test_aborted_without_change_is_0(self) -> None:
        self.assertEqual(self._run(_results(log_config=ABORTED))[0], 0)

    def test_windows_exempt_removal_failures_are_3(self) -> None:
        req = {"requested": True}
        for cleanup in ({"loopback_exempt_removed": False, "loopback_exempt_absent_after_remove": True},
                        {"loopback_exempt_removed": True, "loopback_exempt_absent_after_remove": False},
                        {"loopback_exempt_removed": True, "loopback_exempt_absent_after_remove": None}):
            self.assertEqual(self._run(_results(exempt=req, cleanup=cleanup))[0], 3, cleanup)

    def test_windows_exempt_removed_is_0(self) -> None:
        ok = {"loopback_exempt_removed": True, "loopback_exempt_absent_after_remove": True}
        self.assertEqual(self._run(_results(exempt={"requested": True}, cleanup=ok))[0], 0)

    def test_not_requested_or_not_attempted_is_0(self) -> None:
        self.assertEqual(self._run(_results(exempt={"requested": False}, cleanup={"staging_removed": True}))[0], 0)
        # 要求はしたが付与前に失敗した（解除キーが無い）場合は戻す対象が無い。
        self.assertEqual(self._run(_results(exempt={"requested": True}, cleanup={"staging_removed": True}))[0], 0)

    def test_verdicts_never_change_exit_code(self) -> None:
        for v in ("fail", "unobservable", "pass"):
            self.assertEqual(self._run(_results(verdict=v))[0], 0, v)


class MainExitCodeTest(unittest.TestCase):
    def _main(self, seatbelt_like: dict[str, Any]) -> int:
        def run_mode(mode: str, **_kw: Any) -> dict[str, Any]:
            base = {"mode": mode, "exit_code": 0, "binary_sha256": "x", "binary_version": "v", "stderr_tail": "",
                    "inner": {}, "outer_counts": {r: 0 for r in ROUTES}}
            return {**base, **(seatbelt_like if mode != "baseline" else {})}
        with tempfile.TemporaryDirectory() as td, \
                mock.patch.object(measure, "start_outer_listeners", return_value=({}, {})), \
                mock.patch.object(measure, "run_mode", side_effect=run_mode), \
                mock.patch.object(measure.subprocess, "run", return_value=SimpleNamespace(stdout="v")), \
                mock.patch("builtins.print"), mock.patch.object(sys, "stderr"):
            return measure.main(["--host", sys.executable, "--modes", "baseline,bwrap-netns", "--out", td])

    def test_main_returns_3_on_restore_failure_and_0_on_success(self) -> None:
        if not sys.platform.startswith("linux"):
            self.skipTest("main() mode selection is exercised on Linux")
        self.assertEqual(self._main({"log_config": CHANGED_NOT_RESTORED, "inner": {}}), 3)
        self.assertEqual(self._main({"log_config": CHANGED_RESTORED, "inner": {}}), 0)


class RunModeLogConfigTest(unittest.TestCase):
    """内側の結果が空でも log_config が結果に残り、終了コードへ届く（Codex P1・Bugbot の指摘）。"""

    def _run_mode(self, runner: Any) -> dict[str, Any]:
        self._td = tempfile.mkdtemp()
        self.addCleanup(__import__("shutil").rmtree, self._td, True)
        with mock.patch.object(measure, "build_argv", return_value=["x"]), \
                mock.patch.object(measure, "run_argv", return_value=(1, "boom")), \
                mock.patch.object(measure, "collect_denies", side_effect=AssertionError("not reached")):
            return measure.run_mode("seatbelt", host=Path("/h"), ports={}, recs={}, out_dir=Path(self._td),
                                    hosts_file=Path("/h"), image=None, resolv_file=Path("/r"), page_port=1,
                                    sudo_log="config", log_runner=runner)

    def test_empty_inner_with_failed_restore_is_3(self) -> None:
        def runner(argv: list[str]) -> tuple[int, str]:
            # status は既定、reset は失敗。
            return (1, "") if "--reset" in argv else (0, "level=DEFAULT persist=DEFAULT")
        data = self._run_mode(runner)
        self.assertEqual(data["inner"], {})
        self.assertIs(data["log_config"]["all_restored"], False)
        self.assertEqual(measure.admin_restore_failures({"modes": {"seatbelt": data}}),
                         ["seatbelt: log config was not restored"])


class DowngradedReceiptTest(unittest.TestCase):
    def test_reason_carries_outer_count_and_summary_warns(self) -> None:
        inner = {"positive_ok": True, "attempts": {"fetch": True}, "guard": {"unavailable": "no monitor"}}
        v = judge.judge_all("appcontainer", inner, {r: (3 if r == "fetch" else 0) for r in ROUTES},
                            {r: 1 for r in ROUTES}, loopback_confound=judge.LOOPBACK_EXEMPT_UNMEASURABLE_REASON)
        self.assertEqual(v["fetch"]["verdict"], judge.UNOBSERVABLE)
        self.assertTrue(v["fetch"]["reason"].startswith("outer receipts (3) are attributable"))
        results = {"meta": META, "modes": {"baseline": {"outer_counts": {}},
                                           "appcontainer": {"inner": inner, "verdicts": v}}}
        text = measure.summarize(results)
        self.assertIn("- WARNING: route fetch is unobservable but has 3 outer receipt(s); "
                      "review for a possible real leak", text)
        self.assertEqual(text.count("WARNING: route"), 1)


class RemoveExemptRobustnessTest(unittest.TestCase):
    def _session(self, sid: str) -> Any:
        sess = ac.AppContainerSession(host=Path("h.exe"), harness_dir=Path("."), loopback_exempt=True)
        sess.sid_string = sid
        return sess

    def test_invalid_sid_value_error_is_recorded_not_raised(self) -> None:
        sess = self._session("not-a-sid")
        with mock.patch.object(ac, "trusted_system_root", return_value="C:\\Windows"):
            self.assertIsNone(sess._exempt_listed())
            sess._remove_loopback_exempt()
        self.assertIs(sess.cleanup["loopback_exempt_removed"], False)
        self.assertIs(sess.cleanup["loopback_exempt_absent_after_remove"], False)

    def test_exit_continues_cleanup_when_sid_invalid(self) -> None:
        sess = self._session("not-a-sid")
        sess._exempt_attempted = True
        with mock.patch.object(ac, "trusted_system_root", return_value="C:\\Windows"), \
                tempfile.TemporaryDirectory() as td:
            sess._root = Path(td) / "staging"
            sess._root.mkdir()
            sess.__exit__(None, None, None)
        self.assertIs(sess.cleanup["staging_removed"], True)


class TrustedSystemRootTest(unittest.TestCase):
    def test_windows_uses_get_system_directory_not_environment(self) -> None:
        def fake(buf: Any, _n: int) -> int:
            buf.value = "C:\\Windows\\System32"
            return len("C:\\Windows\\System32")
        windll = SimpleNamespace(kernel32=SimpleNamespace(GetSystemDirectoryW=fake))
        with mock.patch.object(ac.sys, "platform", "win32"), \
                mock.patch.object(ac.ctypes, "windll", windll, create=True), \
                mock.patch.dict(ac.os.environ, {"SystemRoot": "D:\\evil"}):
            self.assertEqual(ac.trusted_system_root(), "C:\\Windows")

    def test_windows_failure_raises(self) -> None:
        windll = SimpleNamespace(kernel32=SimpleNamespace(GetSystemDirectoryW=lambda _b, _n: 0))
        with mock.patch.object(ac.sys, "platform", "win32"), \
                mock.patch.object(ac.ctypes, "windll", windll, create=True):
            with self.assertRaises(ac.AppContainerError):
                ac.trusted_system_root()


if __name__ == "__main__":
    unittest.main()
