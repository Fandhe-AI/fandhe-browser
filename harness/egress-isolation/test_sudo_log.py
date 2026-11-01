"""macOS Seatbelt の opt-in sudo 拒否ログ（`--seatbelt-sudo-log`）の自己テスト（Issue #1402・TASK-38・`RENDER-3`・`SEC-2`）。

実 macOS・sudo は使わない。`measure.run_argv`・`measure.collect_denies` と log config の runner を偽物にして、
opt-in の検証・sudo の argv・log config の復元順序と成否・自己照合の分岐を具体値で確かめる。
"""

from __future__ import annotations

import json
import os
import sys
import tempfile
import unittest
from collections import Counter
from pathlib import Path
from types import SimpleNamespace
from typing import Any
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import judge  # noqa: E402
import measure  # noqa: E402
import seatbelt  # noqa: E402

PORTS = {"redirect": 2001, "img": 2002, "fetch": 2003, "ws": 2004, "internal": 2005, "udp_direct": 2006}
TCP_DEST = {"redirect": "127.0.0.2:2001", "img": "127.0.0.2:2002", "fetch": "127.0.0.2:2003",
            "ws": "127.0.0.2:2004", "internal": "127.0.0.3:2005"}
UDP_DEST = "127.0.0.2:2006"
SUBS = seatbelt.LOG_CONFIG_SUBSYSTEMS
BASE = ["/usr/bin/sudo", "-n", "/usr/bin/log"]


class ArgvTest(unittest.TestCase):
    def test_log_show_default_is_unchanged(self) -> None:
        self.assertEqual(
            seatbelt.log_show_argv("2026-11-01 00:00:00"),
            ["/usr/bin/log", "show", "--style", "ndjson", "--start", "2026-11-01 00:00:00",
             "--predicate", seatbelt.LOG_PREDICATE])

    def test_log_show_sudo(self) -> None:
        self.assertEqual(
            seatbelt.log_show_argv("2026-11-01 00:00:00", sudo=True),
            [*BASE, "show", "--style", "ndjson", "--info", "--debug", "--start", "2026-11-01 00:00:00",
             "--predicate", seatbelt.LOG_PREDICATE])
        with self.assertRaises(ValueError):
            seatbelt.log_show_argv("1; rm -rf /", sudo=True)

    def test_log_config_argv_exact(self) -> None:
        for sub in SUBS:
            self.assertEqual(seatbelt.log_config_argv("status", sub), [*BASE, "config", "--status", "--subsystem", sub])
            self.assertEqual(seatbelt.log_config_argv("raise", sub),
                             [*BASE, "config", "--subsystem", sub, "--mode", "level:debug"])
            self.assertEqual(seatbelt.log_config_argv("restore_default", sub),
                             [*BASE, "config", "--subsystem", sub, "--mode", "level:default"])
            self.assertEqual(seatbelt.log_config_argv("restore_info", sub),
                             [*BASE, "config", "--subsystem", sub, "--mode", "level:info"])

    def test_log_config_rejects_unknown_values(self) -> None:
        with self.assertRaises(ValueError):
            seatbelt.log_config_argv("erase", SUBS[0])
        with self.assertRaises(ValueError):
            seatbelt.log_config_argv("reset", SUBS[0])
        with self.assertRaises(ValueError):
            seatbelt.log_config_argv("restore_default", "com.apple.other")


class ValidateTest(unittest.TestCase):
    def test_accepts_only_darwin_with_seatbelt(self) -> None:
        measure.validate_seatbelt_sudo_log("show", "darwin", ["baseline", "seatbelt"])
        measure.validate_seatbelt_sudo_log("config", "darwin", ["baseline", "seatbelt"])
        measure.validate_seatbelt_sudo_log(None, "linux", ["baseline"])
        for args in (("show", "linux", ["baseline", "seatbelt"]), ("show", "win32", ["baseline", "appcontainer"]),
                     ("show", "darwin", ["baseline"]), ("all", "darwin", ["baseline", "seatbelt"])):
            with self.assertRaises(ValueError):
                measure.validate_seatbelt_sudo_log(*args)

    @unittest.skipUnless(sys.platform.startswith("linux"), "Linux 上の引数検証（非 darwin で exit 2）")
    def test_main_rejects_on_non_darwin(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            host = Path(td) / "host"
            host.write_text("x", encoding="utf-8")
            host.chmod(0o755)
            out = Path(td) / "out"
            out.mkdir()
            self.assertEqual(measure.main(["--host", str(host), "--out", str(out), "--seatbelt-sudo-log", "show"]), 2)


class FakeLog:
    """log config の runner。呼び出し順を共有リストへ記録し、status の出力と各 rc を差し替えられる。"""

    def __init__(self, order: list[str], *, reset_rc: int = 0, after_status: str = "default") -> None:
        self.order, self.reset_rc, self.after_status, self.resetted = order, reset_rc, after_status, False

    def __call__(self, argv: list[str]) -> tuple[int, str]:
        assert "--reset" not in argv
        action = ("status" if "--status" in argv else "restore" if argv[-1] in ("level:default", "level:info")
                  else "raise")
        self.order.append(f"{action}:{argv[argv.index('--subsystem') + 1]}")
        if action == "restore":
            self.resetted = True
            return self.reset_rc, ""
        if action == "status":
            return 0, self.after_status if self.resetted else "default"
        return 0, ""


class RunModeTest(unittest.TestCase):
    def _run(self, *, sudo_log: str | None, order: list[str], log: FakeLog | None = None, run_exc: bool = False,
             denies: dict[str, int] | None = None, status: dict[str, Any] | None = None,
             udp_errno: str = "EPERM", mode: str = "seatbelt") -> dict[str, Any]:
        recs = {k: SimpleNamespace(count=0) for k in PORTS}
        with tempfile.TemporaryDirectory() as td:
            out_dir = Path(td)
            (out_dir / f"{mode}.json").write_text(json.dumps({
                "host_pids": [101], "probes": {"udp_direct": udp_errno}, "positive_ok": True,
                "guard": {"unavailable": "n/a"}, "attempts": {"udp_direct": True},
                "attempt_evidence": {"udp_direct": "errno"}}), encoding="utf-8")

            def fake_run(argv: list[str], container: str | None = None) -> tuple[int | None, str]:
                order.append("run_argv")
                if run_exc:
                    raise RuntimeError("boom")
                return 0, ""

            def fake_collect(start: str, pids: Any = None, *, sudo: bool = False) -> tuple[dict[str, int], dict[str, Any]]:
                order.append("log_show_sudo" if sudo else "log_show")
                if sudo:
                    return dict(denies or {}), dict(status or {"available": True, "deny_lines": 1})
                return {}, {"available": True, "deny_lines": 0, "diagnostics": {"lines": 3}}

            with mock.patch.object(measure, "run_argv", fake_run), \
                    mock.patch.object(measure, "collect_denies", fake_collect), \
                    mock.patch.object(measure, "build_argv", lambda *a, **k: ["true"]):
                if run_exc:
                    with self.assertRaises(RuntimeError):
                        measure.run_mode(mode, host=Path("h"), ports=PORTS, recs=recs, out_dir=out_dir,
                                         hosts_file=Path("hosts"), image=None, sudo_log=sudo_log, log_runner=log)
                    return {}
                return measure.run_mode(mode, host=Path("h"), ports=PORTS, recs=recs, out_dir=out_dir,
                                        hosts_file=Path("hosts"), image=None, sudo_log=sudo_log, log_runner=log)

    def test_without_opt_in_no_sudo_is_used(self) -> None:
        order: list[str] = []
        log = FakeLog(order)
        data = self._run(sudo_log=None, order=order, log=log)
        self.assertEqual(order, ["run_argv", "log_show"])
        self.assertIs(data["deny_log"]["sudo"], False)
        self.assertIsNone(data["deny_log"]["log_config"])

    def test_show_uses_sudo_and_no_log_config(self) -> None:
        order: list[str] = []
        data = self._run(sudo_log="show", order=order, log=FakeLog(order), denies={UDP_DEST: 1})
        self.assertEqual(order, ["run_argv", "log_show_sudo", "log_show"])
        self.assertIs(data["deny_log"]["sudo"], True)
        self.assertEqual(data["deny_log"]["unprivileged"]["deny_lines"], 0)
        self.assertIsNone(data["deny_log"]["log_config"])

    def test_baseline_never_touches_log_config(self) -> None:
        order: list[str] = []
        self._run(sudo_log="config", order=order, log=FakeLog(order), mode="baseline")
        self.assertEqual(order, ["run_argv"])

    def test_config_restore_order(self) -> None:
        order: list[str] = []
        data = self._run(sudo_log="config", order=order, log=FakeLog(order), denies={UDP_DEST: 1})
        a, b = SUBS
        self.assertEqual(order, [f"status:{a}", f"status:{b}", f"raise:{a}", f"raise:{b}", "run_argv",
                                 "log_show_sudo", "log_show", f"restore:{a}", f"restore:{b}", f"status:{a}", f"status:{b}"])
        state = data["deny_log"]["log_config"]
        self.assertIs(state["all_restored"], True)
        self.assertEqual(state["subsystems"][a]["restore_rc"], 0)

    def test_restore_runs_when_run_argv_raises(self) -> None:
        order: list[str] = []
        self._run(sudo_log="config", order=order, log=FakeLog(order), run_exc=True)
        a, b = SUBS
        self.assertEqual(order[-4:], [f"restore:{a}", f"restore:{b}", f"status:{a}", f"status:{b}"])
        self.assertNotIn("log_show_sudo", order)

    def test_restore_runs_when_log_show_fails(self) -> None:
        order: list[str] = []
        data = self._run(sudo_log="config", order=order, log=FakeLog(order),
                         status={"available": False, "reason": "log show rc=1 (sudo)"})
        self.assertIn(f"restore:{SUBS[0]}", order)
        self.assertEqual(data["deny_log"]["reason"], "log show rc=1 (sudo)")
        self.assertIs(data["deny_log"]["log_config"]["all_restored"], True)

    def test_failed_reset_is_recorded_and_not_pass(self) -> None:
        order: list[str] = []
        data = self._run(sudo_log="config", order=order, log=FakeLog(order, reset_rc=1),
                         denies={UDP_DEST: 1, **{d: 1 for d in TCP_DEST.values()}})
        dl = data["deny_log"]
        self.assertIs(dl["log_config"]["all_restored"], False)
        self.assertEqual(dl["log_config"]["subsystems"][SUBS[0]]["restore_rc"], 1)
        self.assertEqual((dl["available"], dl["reason"]), (False, "log config not restored"))
        v = judge.judge_all("seatbelt", data["inner"], {}, {r: 1 for r in PORTS}, deny_log_reason=dl["reason"])
        for route in TCP_DEST:
            self.assertEqual(v[route]["verdict"], judge.UNOBSERVABLE)
            self.assertIn("deny log unavailable: log config not restored", v[route]["reason"])

    def test_status_mismatch_after_reset_is_not_restored(self) -> None:
        order: list[str] = []
        data = self._run(sudo_log="config", order=order, log=FakeLog(order, after_status="debug"),
                         denies={UDP_DEST: 1})
        self.assertIs(data["deny_log"]["log_config"]["all_restored"], False)
        self.assertEqual(data["deny_log"]["reason"], "log config not restored")

    def test_self_check_holds_tcp_routes_pass(self) -> None:
        order: list[str] = []
        denies = {UDP_DEST: 1, **{d: 1 for d in TCP_DEST.values()}}
        data = self._run(sudo_log="config", order=order, log=FakeLog(order), denies=denies)
        self.assertIs(data["deny_log"]["available"], True)
        v = judge.judge_all("seatbelt", data["inner"], {}, {r: 1 for r in PORTS})
        for route in TCP_DEST:
            self.assertEqual(v[route]["verdict"], judge.PASS)
            self.assertEqual(v[route]["evidence"], "deny_log")

    def test_self_check_failure_is_unobservable_with_reason(self) -> None:
        order: list[str] = []
        data = self._run(sudo_log="config", order=order, log=FakeLog(order), denies={d: 1 for d in TCP_DEST.values()})
        dl = data["deny_log"]
        self.assertEqual((dl["available"], dl["reason"]), (False, "self-probe denial missing from deny log"))
        v = judge.judge_all("seatbelt", data["inner"], {}, {r: 1 for r in PORTS}, deny_log_reason=dl["reason"])
        for route in TCP_DEST:
            self.assertEqual(v[route]["verdict"], judge.UNOBSERVABLE)
            self.assertIn("deny log unavailable: self-probe denial missing from deny log", v[route]["reason"])

    def test_self_check_ok_but_no_tcp_denies_is_unobservable_or_fail(self) -> None:
        order: list[str] = []
        data = self._run(sudo_log="show", order=order, log=FakeLog(order), denies={UDP_DEST: 1})
        v = judge.judge_all("seatbelt", data["inner"], {}, {r: 1 for r in PORTS})
        self.assertEqual(v["img"]["verdict"], judge.UNOBSERVABLE)
        self.assertNotIn("deny log unavailable", v["img"]["reason"])
        v = judge.judge_all("seatbelt", data["inner"], {"img": 1}, {r: 1 for r in PORTS})
        self.assertEqual(v["img"]["verdict"], judge.FAIL)

    def test_beacons_alone_are_not_evidence(self) -> None:
        order: list[str] = []
        data = self._run(sudo_log="show", order=order, log=FakeLog(order), denies={UDP_DEST: 1})
        data["inner"]["beacons"] = {"img": 3}
        v = judge.judge_all("seatbelt", data["inner"], {}, {r: 1 for r in PORTS})
        self.assertEqual(v["img"]["verdict"], judge.UNOBSERVABLE)


class SandboxLogConfigTest(unittest.TestCase):
    def test_reset_exception_does_not_stop_remaining_subsystems(self) -> None:
        calls: list[str] = []

        def runner(argv: list[str]) -> tuple[int, str]:
            if argv[-1] == "level:default":
                sub = argv[argv.index("--subsystem") + 1]
                calls.append(sub)
                if sub == SUBS[0]:
                    raise OSError("x")
            return 0, "default"

        with seatbelt.SandboxLogConfig(runner) as cfg:
            pass
        self.assertEqual(calls, list(SUBS))
        self.assertIs(cfg.state["all_restored"], False)
        self.assertEqual(cfg.state["subsystems"][SUBS[0]]["restore_error"], "OSError")
        self.assertIs(cfg.state["subsystems"][SUBS[1]]["restored"], True)


class AbortOnNonDefaultTest(unittest.TestCase):
    def test_custom_before_state_makes_no_change(self) -> None:
        calls: list[str] = []

        def runner(argv: list[str]) -> tuple[int, str]:
            assert "--reset" not in argv
            calls.append("status" if "--status" in argv else "reset" if argv[-1] == "level:default" else "raise")
            return 0, "Mode DEBUG custom"

        with seatbelt.SandboxLogConfig(runner) as cfg:
            pass
        self.assertEqual(set(calls), {"status"})
        self.assertIn("aborted", cfg.state)
        self.assertIs(cfg.state["all_restored"], False)

    def test_status_failure_makes_no_change(self) -> None:
        calls: list[str] = []

        def runner(argv: list[str]) -> tuple[int, str]:
            calls.append("status" if "--status" in argv else "other")
            return 1, ""

        with seatbelt.SandboxLogConfig(runner) as cfg:
            pass
        self.assertEqual(set(calls), {"status"})
        self.assertIn("aborted", cfg.state)


class StatusAllDefaultTest(unittest.TestCase):
    """log config の変更前判定は全項目が既定のときだけ許可する（fail-closed。Issue #1402）。"""

    def _calls(self, status: str) -> tuple[set[str], dict[str, Any]]:
        calls: list[str] = []

        def runner(argv: list[str]) -> tuple[int, str]:
            assert "--reset" not in argv
            calls.append("status" if "--status" in argv else "reset" if argv[-1] == "level:default" else "raise")
            return 0, status

        with seatbelt.SandboxLogConfig(runner) as cfg:
            pass
        return set(calls), cfg.state

    def test_level_default_but_persist_custom_makes_no_change(self) -> None:
        for status in ("level=DEFAULT persist=DEBUG", "Mode for 'x': level: DEFAULT, persist: INFO"):
            calls, state = self._calls(status)
            self.assertEqual(calls, {"status"}, status)
            self.assertIn("aborted", state)

    def test_uninterpretable_status_makes_no_change(self) -> None:
        for status in ("", "something unexpected", "default but not really"):
            calls, state = self._calls(status)
            self.assertEqual(calls, {"status"}, status)
            self.assertIn("aborted", state)

    def test_all_default_forms_are_accepted(self) -> None:
        for status in ("DEFAULT", "Mode for 'com.apple.sandbox' DEFAULT", "level=DEFAULT persist=DEFAULT"):
            self.assertTrue(seatbelt.status_is_all_default(status), status)

    def test_real_macos_token_format_is_info_kind_not_default(self) -> None:
        real = (
            "Mode for 'com.apple.sandbox'  INFO PERSIST_DEFAULT SIGNPOST_PERSISTED_OFF SIGNPOST_ENABLED_ON "
            "SIGNPOST_BACKTRACE_ENABLED_OFF OVERSIZE_ENABLED_OFF\n"
            "Mode for 'com.apple.sandbox.reporting'  INFO PERSIST_DEFAULT SIGNPOST_PERSISTED_OFF "
            "SIGNPOST_ENABLED_ON SIGNPOST_BACKTRACE_ENABLED_OFF OVERSIZE_ENABLED_ON\n"
        )
        self.assertEqual(seatbelt.before_state_kind(real), "info")
        self.assertFalse(seatbelt.status_is_all_default(real))
        self.assertEqual(seatbelt.before_state_kind("Mode for 'x'  DEFAULT PERSIST_DEFAULT SIGNPOST_ENABLED_ON"),
                         "default")
        self.assertIsNone(seatbelt.before_state_kind("Mode for 'a'  INFO PERSIST_DEFAULT\nMode for 'b'  DEFAULT PERSIST_DEFAULT"))

    def test_token_format_non_default_is_rejected(self) -> None:
        for status in ("Mode for 'x'  DEBUG PERSIST_DEFAULT", "Mode for 'x'  INFO PERSIST_DEBUG",
                       "Mode for 'x'  OFF PERSIST_DEFAULT", "", "garbage text here"):
            self.assertFalse(seatbelt.status_is_all_default(status), status)


class RestoreByBeforeKindTest(unittest.TestCase):
    """変更前の種別で戻し方を分ける（INFO はレベルだけ戻し `--reset` を呼ばない。Issue #1402）。"""

    INFO = "Mode for 'x'  INFO PERSIST_DEFAULT SIGNPOST_ENABLED_ON OVERSIZE_ENABLED_ON"
    DEFAULT = "Mode for 'x'  DEFAULT PERSIST_DEFAULT SIGNPOST_ENABLED_ON"

    def _run(self, before: str, after: str) -> tuple[list[list[str]], dict[str, Any]]:
        argvs: list[list[str]] = []
        changed = [False]

        def runner(argv: list[str]) -> tuple[int, str]:
            argvs.append(argv)
            if "--status" in argv:
                return 0, after if changed[0] else before
            changed[0] = True
            return 0, ""

        with seatbelt.SandboxLogConfig(runner) as cfg:
            pass
        return argvs, cfg.state

    def test_info_before_restores_level_only_without_reset(self) -> None:
        argvs, state = self._run(self.INFO, self.INFO)
        changes = [a for a in argvs if "--status" not in a]
        self.assertEqual([a[-2:] for a in changes],
                         [["--mode", "level:debug"]] * 2 + [["--mode", "level:info"]] * 2)
        self.assertFalse(any("--reset" in a for a in argvs))
        self.assertIs(state["all_restored"], True)

    def test_default_before_restores_level_default_without_reset(self) -> None:
        # SIGNPOST_*・OVERSIZE_* がカスタムでも --reset で消さない（Codex P1）。
        argvs, state = self._run(self.DEFAULT + " OVERSIZE_ENABLED_ON", self.DEFAULT + " OVERSIZE_ENABLED_ON")
        changes = [a for a in argvs if "--status" not in a]
        self.assertEqual([a[-1] for a in changes], ["level:debug"] * 2 + ["level:default"] * 2)
        self.assertFalse(any("--reset" in a for a in argvs))
        self.assertIs(state["all_restored"], True)

    def test_status_mismatch_after_restore_is_not_restored(self) -> None:
        _, state = self._run(self.INFO, self.INFO.replace("OVERSIZE_ENABLED_ON", "OVERSIZE_ENABLED_OFF"))
        self.assertIs(state["all_restored"], False)


class _FakePipe:
    """`_TimedLineReader` の `select.select` と `os.read` を差し替える偽のパイプ（OS 非依存）。

    Windows の `select` はパイプの fd に使えない（WinError 10038）ため、実パイプではなくこの偽物で
    期限・行分割の契約を全 OS で検証する。`clock` は `time.monotonic` の代わりで、`select` の待ちで進める。
    """

    def __init__(self, chunks: list[bytes], eof: bool = False) -> None:
        self.chunks = list(chunks)
        self.eof = eof
        self.now = 1000.0
        self.selects = 0

    def clock(self) -> float:
        return self.now

    def select(self, rlist: list[int], _w: list[int], _x: list[int], timeout: float) -> tuple[list[int], list, list]:
        self.selects += 1
        if self.chunks or self.eof:
            return rlist, [], []
        self.now += timeout  # 読めるものが無ければ待ち時間ぶん時計を進める（実時間は待たない）
        return [], [], []

    def read(self, _fd: int, _n: int) -> bytes:
        if self.chunks:
            return self.chunks.pop(0)
        return b""


class TimedReaderTest(unittest.TestCase):
    def _reader(self, fake: _FakePipe, budget: float) -> Any:
        stack = mock.patch.multiple(measure, select=SimpleNamespace(select=fake.select))
        stack.start()
        self.addCleanup(stack.stop)
        for target, repl in (("measure.os.read", fake.read), ("measure.time.monotonic", fake.clock)):
            patcher = mock.patch(target, repl)
            patcher.start()
            self.addCleanup(patcher.stop)
        return measure._TimedLineReader(SimpleNamespace(fileno=lambda: 7), fake.clock() + budget)

    def test_deadline_returns_eof_when_pipe_stays_open(self) -> None:
        fake = _FakePipe([b"a\nbb"])
        reader = self._reader(fake, 0.3)
        self.assertEqual(reader.readline(100), b"a\n")
        self.assertEqual(reader.readline(100), b"")
        self.assertIs(reader.timed_out, True)
        self.assertEqual(reader.readline(100), b"")  # 期限後は残り（b"bb"）を返さず EOF 扱いのまま

    def test_long_line_is_split_at_limit(self) -> None:
        fake = _FakePipe([b"abcdef\n"])
        reader = self._reader(fake, 5)
        self.assertEqual(reader.readline(4), b"abcd")
        self.assertEqual(reader.readline(4), b"ef\n")
        self.assertIs(reader.timed_out, False)

    def test_eof_returns_remaining_then_empty(self) -> None:
        fake = _FakePipe([b"x\ny"], eof=True)
        reader = self._reader(fake, 5)
        self.assertEqual(reader.readline(100), b"x\n")
        self.assertEqual(reader.readline(100), b"y")
        self.assertEqual(reader.readline(100), b"")
        self.assertIs(reader.timed_out, False)


class StopOrderTest(unittest.TestCase):
    def test_terminate_precedes_kill(self) -> None:
        events: list[str] = []

        class P:
            def terminate(self) -> None:
                events.append("terminate")

            def wait(self, timeout: float | None = None) -> None:
                events.append("wait")
                raise measure.subprocess.TimeoutExpired("x", 1)

            def kill(self) -> None:
                events.append("kill")

        measure._stop_log_proc(P())
        self.assertEqual(events, ["terminate", "wait", "kill"])

    def test_sudo_nonzero_rc_reports_reason(self) -> None:
        class P:
            returncode = 1
            stdout = SimpleNamespace(readline=lambda n=-1: b"", close=lambda: None)

            def wait(self, timeout: float | None = None) -> int:
                return 1

        with mock.patch.object(measure.subprocess, "Popen", lambda *a, **k: P()) as _:
            counts, status = measure.collect_denies("2026-11-01 00:00:00", frozenset({1}), sudo=True)
        self.assertEqual(counts, {})
        self.assertEqual(status, {"available": False, "reason": "log show rc=1 (sudo)"})


class DenySampleTest(unittest.TestCase):
    """拒否メッセージのサンプル保持（Issue #1402。正規表現は変えず、形だけ・対象 pid だけを控える）。"""

    def _scan(self, lines: list[str], pids: frozenset[int] | None = None) -> seatbelt.DenyDiagnostics:
        diag = seatbelt.DenyDiagnostics()
        for ln in lines:
            seatbelt.scan_line(json.dumps({"eventMessage": ln, "processID": 0}), pids, diag)
        return diag

    def test_unmatched_samples_capped_at_16_with_overflow_count(self) -> None:
        diag = self._scan([f"Sandbox: x(1) deny(1) other-{i}" for i in range(20)], frozenset({1}))
        self.assertEqual(len(diag.unmatched_deny_samples), 16)
        self.assertEqual(diag.unmatched_deny_samples[0], "Sandbox: <proc>(<pid>) deny(1) other-0")
        self.assertEqual(diag.unmatched_deny_samples_dropped, 4)
        self.assertEqual(diag.sandbox_lines, 20)

    def test_unrelated_pid_unmatched_line_is_only_counted(self) -> None:
        diag = self._scan(["Sandbox: other(999) deny(1) file-read-data /Users/someone/secret.txt",
                           "deny(1) no pid here"], frozenset({101}))
        self.assertEqual(diag.unmatched_deny_samples, [])
        self.assertEqual(diag.unmatched_deny_unattributed, 2)
        self.assertNotIn("secret", str(diag.as_dict()))
        self.assertNotIn("someone", str(diag.as_dict()))

    def test_target_pid_line_is_redacted_to_shape(self) -> None:
        diag = self._scan([
            "Sandbox: servo-host(101) deny(1) file-read-data /private/var/folders/x/y 10.1.2.3:443 "
            "127.0.0.2:65251 ?token=abc localhost:80 ::ffff:127.0.0.1 fe80::1 abc123.x"], frozenset({101}))
        self.assertEqual(diag.unmatched_deny_samples, [
            "Sandbox: <proc>(<pid>) deny(1) file-read-data <path> <ipv4>:443 127.0.0.2:65251 "
            "<redacted> localhost:80 ::ffff:127.0.0.1 <ipv6> aaa999.a"])
        self.assertNotIn("servo-host", str(diag.as_dict()))

    def test_odd_operation_token_becomes_op(self) -> None:
        self.assertEqual(seatbelt.redact_shape("Sandbox: p(5) deny(1) /bad/op x"), "Sandbox: <proc>(<pid>) deny(1) <op> a")

    def test_pid_excluded_samples_are_structured_and_capped_at_8(self) -> None:
        diag = self._scan([f"Sandbox: x({100 + i}) deny(1) network-outbound 10.0.0.{i}:80" for i in range(9)]
                          + ["Sandbox: x(200) deny(1) network-outbound 127.0.0.2:2001"], frozenset({1}))
        self.assertEqual(diag.pid_excluded_samples[0], {"pid": 100, "dest": "<ipv4>:80"})
        self.assertEqual(len(diag.pid_excluded_samples), 8)
        self.assertEqual(diag.pid_excluded_samples_dropped, 2)
        self.assertEqual(diag.deny_excluded_by_pid, 10)
        loop = self._scan(["Sandbox: x(200) deny(1) network-outbound 127.0.0.2:2001"], frozenset({1}))
        self.assertEqual(loop.pid_excluded_samples, [{"pid": 200, "dest": "127.0.0.2:2001"}])

    def test_matched_attributed_line_is_not_sampled(self) -> None:
        diag = seatbelt.DenyDiagnostics()
        dest = seatbelt.scan_line(
            json.dumps({"eventMessage": "Sandbox: x(7) deny(1) network-outbound 127.0.0.2:2001"}),
            frozenset({7}), diag)
        self.assertEqual(dest, "127.0.0.2:2001")
        self.assertEqual((diag.unmatched_deny_samples, diag.pid_excluded_samples), ([], []))

    def test_line_without_deny_is_not_sampled(self) -> None:
        diag = self._scan(["Sandbox: allow network-outbound 127.0.0.2:2001"])
        self.assertEqual((diag.unmatched_deny_samples, diag.unmatched_deny_unattributed, diag.sandbox_lines),
                         ([], 0, 0))

    def test_shape_truncated_to_300_chars_and_24_per_token(self) -> None:
        self.assertEqual(len(seatbelt.redact_shape("deny(1) op " + "a" * 100)), len("deny(1) op ") + 24)
        long = seatbelt.redact_shape("deny(1) op " + " ".join(["bbbb"] * 200))
        self.assertEqual(len(long), 300)

    def test_control_chars_become_spaces(self) -> None:
        self.assertEqual(seatbelt.sanitize_sample("a\nb\tc\x1bd\re\x7ff\u2028g"), "a b c d e f g")
        self.assertEqual(seatbelt.redact_shape("deny(1) op a\nb\x1b[0m"), "deny(1) op a a?[9a")

    def test_samples_in_as_dict_and_summary_block(self) -> None:
        diag = self._scan(["Sandbox: p(1) deny(1) op weird"], frozenset({1}))
        d = diag.as_dict()
        self.assertEqual(d["unmatched_deny_samples"], ["Sandbox: <proc>(<pid>) deny(1) op aaaaa"])
        dl = {"available": True, "diagnostics": d, "unprivileged": {"diagnostics": d}}
        block = measure._deny_sample_block(dl)
        self.assertIn("Sandbox: <proc>(<pid>) deny(1) op aaaaa", block)
        self.assertEqual(block.count("```text"), 2)
        self.assertNotIn("aaaaa", str(measure._without_samples(dl)))


if __name__ == "__main__":
    unittest.main()
