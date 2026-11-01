"""macOS Seatbelt の拒否ログ（非特権の `log show`）の自己テスト（Issue #1402・TASK-38・`RENDER-3`・`SEC-2`）。

実 macOS は使わない。`measure.run_argv`・`measure.collect_denies` を偽物にして、`log show` の固定 argv・
自己照合の分岐・拒否行のサンプル保持・期限つきリーダーを具体値で確かめる。
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
import judge  # noqa: E402
import measure  # noqa: E402
import seatbelt  # noqa: E402

PORTS = {"redirect": 2001, "img": 2002, "fetch": 2003, "ws": 2004, "internal": 2005, "udp_direct": 2006}
TCP_DEST = {"redirect": "127.0.0.2:2001", "img": "127.0.0.2:2002", "fetch": "127.0.0.2:2003",
            "ws": "127.0.0.2:2004", "internal": "127.0.0.3:2005"}
UDP_DEST = "127.0.0.2:2006"


class ArgvTest(unittest.TestCase):
    def test_log_show_default_is_unchanged(self) -> None:
        self.assertEqual(
            seatbelt.log_show_argv("2026-11-01 00:00:00"),
            ["/usr/bin/log", "show", "--style", "ndjson", "--start", "2026-11-01 00:00:00",
             "--predicate", seatbelt.LOG_PREDICATE])

class RunModeTest(unittest.TestCase):
    def _run(self, *, order: list[str], denies: dict[str, int] | None = None,
             status: dict[str, Any] | None = None, udp_errno: str = "EPERM", mode: str = "seatbelt") -> dict[str, Any]:
        recs = {k: SimpleNamespace(count=0) for k in PORTS}
        with tempfile.TemporaryDirectory() as td:
            out_dir = Path(td)
            (out_dir / f"{mode}.json").write_text(json.dumps({
                "host_pids": [101], "probes": {"udp_direct": udp_errno}, "positive_ok": True,
                "guard": {"unavailable": "n/a"}, "attempts": {"udp_direct": True},
                "attempt_evidence": {"udp_direct": "errno"}}), encoding="utf-8")

            def fake_run(argv: list[str], container: str | None = None) -> tuple[int | None, str]:
                order.append("run_argv")
                return 0, ""

            def fake_collect(start: str, pids: Any = None) -> tuple[dict[str, int], dict[str, Any]]:
                order.append("log_show")
                return dict(denies or {}), dict(status or {"available": True, "deny_lines": 1})

            with mock.patch.object(measure, "run_argv", fake_run), \
                    mock.patch.object(measure, "collect_denies", fake_collect), \
                    mock.patch.object(measure, "build_argv", lambda *a, **k: ["true"]):
                return measure.run_mode(mode, host=Path("h"), ports=PORTS, recs=recs, out_dir=out_dir,
                                        hosts_file=Path("hosts"), image=None)

    def test_self_check_holds_tcp_routes_pass(self) -> None:
        order: list[str] = []
        denies = {UDP_DEST: 1, **{d: 1 for d in TCP_DEST.values()}}
        data = self._run(order=order, denies=denies)
        self.assertEqual(order, ["run_argv", "log_show"])
        self.assertIs(data["deny_log"]["available"], True)
        v = judge.judge_all("seatbelt", data["inner"], {}, {r: 1 for r in PORTS})
        for route in TCP_DEST:
            self.assertEqual(v[route]["verdict"], judge.PASS)
            self.assertEqual(v[route]["evidence"], "deny_log")

    def test_self_check_failure_is_unobservable_with_reason(self) -> None:
        order: list[str] = []
        data = self._run(order=order, denies={d: 1 for d in TCP_DEST.values()})
        dl = data["deny_log"]
        self.assertEqual((dl["available"], dl["reason"]), (False, "self-probe denial missing from deny log"))
        v = judge.judge_all("seatbelt", data["inner"], {}, {r: 1 for r in PORTS}, deny_log_reason=dl["reason"])
        for route in TCP_DEST:
            self.assertEqual(v[route]["verdict"], judge.UNOBSERVABLE)
            self.assertIn("deny log unavailable: self-probe denial missing from deny log", v[route]["reason"])

    def test_self_check_ok_but_no_tcp_denies_is_unobservable_or_fail(self) -> None:
        order: list[str] = []
        data = self._run(order=order, denies={UDP_DEST: 1})
        v = judge.judge_all("seatbelt", data["inner"], {}, {r: 1 for r in PORTS})
        self.assertEqual(v["img"]["verdict"], judge.UNOBSERVABLE)
        self.assertNotIn("deny log unavailable", v["img"]["reason"])
        v = judge.judge_all("seatbelt", data["inner"], {"img": 1}, {r: 1 for r in PORTS})
        self.assertEqual(v["img"]["verdict"], judge.FAIL)

    def test_beacons_alone_are_not_evidence(self) -> None:
        order: list[str] = []
        data = self._run(order=order, denies={UDP_DEST: 1})
        data["inner"]["beacons"] = {"img": 3}
        v = judge.judge_all("seatbelt", data["inner"], {}, {r: 1 for r in PORTS})
        self.assertEqual(v["img"]["verdict"], judge.UNOBSERVABLE)


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

    def test_nonzero_rc_reports_reason(self) -> None:
        class P:
            returncode = 1
            stdout = SimpleNamespace(readline=lambda n=-1: b"", close=lambda: None)

            def wait(self, timeout: float | None = None) -> int:
                return 1

        with mock.patch.object(measure.subprocess, "Popen", lambda *a, **k: P()) as _:
            counts, status = measure.collect_denies("2026-11-01 00:00:00", frozenset({1}))
        self.assertEqual(counts, {})
        self.assertEqual(status, {"available": False, "reason": "log show rc=1"})


class DenySampleTest(unittest.TestCase):
    """拒否メッセージのサンプル保持（Issue #1402。正規表現は変えず、形だけ・対象 pid だけを控える）。"""

    def _scan(self, lines: list[str], pids: frozenset[int] | None = None) -> seatbelt.DenyDiagnostics:
        diag = seatbelt.DenyDiagnostics()
        for ln in lines:
            seatbelt.scan_line(json.dumps({"eventMessage": ln, "processID": 0}), pids, diag)
        return diag

    def test_unmatched_samples_capped_at_16_with_overflow_count(self) -> None:
        diag = self._scan([f"Sandbox: x(1) deny(1) file-op-{i}" for i in range(20)], frozenset({1}))
        self.assertEqual(len(diag.unmatched_deny_samples), 16)
        self.assertEqual(diag.unmatched_deny_samples[0], "Sandbox: <proc>(<pid>) deny(1) file-op-0")
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

    def test_known_operations_kept_and_unknown_become_op(self) -> None:
        for op in ("network-outbound", "network-bind", "file-write-create", "mach-lookup", "process-exec*",
                   "system-fsctl", "sysctl-read", "iokit-open", "ipc-posix-shm-read-data", "signal", "job-creation"):
            with self.subTest(op=op):
                self.assertEqual(seatbelt.redact_shape(f"Sandbox: p(5) deny(1) {op} x"),
                                 f"Sandbox: <proc>(<pid>) deny(1) {op} a")
        for op in ("alice-secret", "op", "file", "networkx", "Signal2", "x" * 41):
            with self.subTest(op=op):
                self.assertEqual(seatbelt.redact_shape(f"Sandbox: p(5) deny(1) {op} x"),
                                 "Sandbox: <proc>(<pid>) deny(1) <op> a")

    def test_unknown_operation_does_not_affect_wildcard_attribution_parse(self) -> None:
        diag = seatbelt.DenyDiagnostics()
        dest = seatbelt.scan_line("Sandbox: x(7) deny(1) network-outbound *:2001", frozenset({7}), diag)
        self.assertEqual(diag.unmatched_deny_samples, [])
        self.assertEqual(diag.wildcard_lines, 1)

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
        self.assertEqual(len(seatbelt.redact_shape("deny(1) file-read-data " + "a" * 100)), len("deny(1) file-read-data ") + 24)
        long = seatbelt.redact_shape("deny(1) file-read-data " + " ".join(["bbbb"] * 200))
        self.assertEqual(len(long), 300)

    def test_control_chars_become_spaces(self) -> None:
        self.assertEqual(seatbelt.sanitize_sample("a\nb\tc\x1bd\re\x7ff\u2028g"), "a b c d e f g")
        self.assertEqual(seatbelt.redact_shape("deny(1) file-read-data a\nb\x1b[0m"), "deny(1) file-read-data a a?[9a")

    def test_samples_in_as_dict_and_summary_block(self) -> None:
        diag = self._scan(["Sandbox: p(1) deny(1) file-read-data weird"], frozenset({1}))
        d = diag.as_dict()
        self.assertEqual(d["unmatched_deny_samples"], ["Sandbox: <proc>(<pid>) deny(1) file-read-data aaaaa"])
        dl = {"available": True, "diagnostics": d}
        block = measure._deny_sample_block(dl)
        self.assertIn("Sandbox: <proc>(<pid>) deny(1) file-read-data aaaaa", block)
        self.assertEqual(block.count("```text"), 1)
        self.assertNotIn("aaaaa", str(measure._without_samples(dl)))


if __name__ == "__main__":
    unittest.main()
