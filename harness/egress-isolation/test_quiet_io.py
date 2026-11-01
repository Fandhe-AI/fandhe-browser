"""quiet_io の自己テスト（Issue #1401・TASK-38・`RENDER-3`・`XOS-1`）。

win32 では `subprocess.DEVNULL` / `os.devnull` を使わないこと、それ以外では従来どおり DEVNULL を使うことを、
`sys.platform` と `subprocess.Popen` / `subprocess.run` のモックで OS 非依存に検証する。
"""

from __future__ import annotations

import io
import subprocess
import sys
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import quiet_io  # noqa: E402


class FakeProc:
    """`subprocess.Popen` の最小の偽物。出力は固定のバイト列。"""

    def __init__(self, stdout: bytes = b"", stderr: bytes = b"", rc: int = 0) -> None:
        self.stdin = mock.Mock()
        self.stdout = io.BytesIO(stdout)
        self.stderr = io.BytesIO(stderr)
        self.rc = rc

    def wait(self, timeout: float | None = None) -> int:
        return self.rc


class UseDevnullTest(unittest.TestCase):
    def test_win32_is_false_and_others_true(self) -> None:
        self.assertFalse(quiet_io.use_devnull("win32"))
        self.assertTrue(quiet_io.use_devnull("linux"))
        self.assertTrue(quiet_io.use_devnull("darwin"))


class RunQuietTest(unittest.TestCase):
    def test_win32_never_uses_devnull_and_closes_stdin(self) -> None:
        fake = FakeProc(stdout=b"12 1\n", stderr=b"warn")
        with mock.patch.object(quiet_io.subprocess, "Popen", return_value=fake) as popen, \
                mock.patch.object(quiet_io.subprocess, "run") as run:
            cp = quiet_io.run_quiet(["x"], timeout=5, capture_stdout=True, platform="win32")
        run.assert_not_called()
        kw = popen.call_args.kwargs
        self.assertEqual(kw["stdin"], subprocess.PIPE)
        self.assertEqual(kw["stdout"], subprocess.PIPE)
        self.assertEqual(kw["stderr"], subprocess.PIPE)
        self.assertNotIn(subprocess.DEVNULL, kw.values())
        fake.stdin.close.assert_called_once_with()
        self.assertEqual(cp.stdout, b"12 1\n")
        self.assertEqual(cp.returncode, 0)

    def test_win32_without_capture_returns_empty_stdout(self) -> None:
        fake = FakeProc(stdout=b"noise", rc=3)
        with mock.patch.object(quiet_io.subprocess, "Popen", return_value=fake):
            cp = quiet_io.run_quiet(["x"], timeout=5, platform="win32")
        self.assertEqual((cp.returncode, cp.stdout), (3, b""))

    def test_win32_capture_is_bounded(self) -> None:
        fake = FakeProc(stdout=b"a" * 10000)
        with mock.patch.object(quiet_io.subprocess, "Popen", return_value=fake):
            cp = quiet_io.run_quiet(["x"], timeout=5, capture_stdout=True, keep_max=100, platform="win32")
        self.assertEqual(cp.stdout, b"a" * 100)

    def test_win32_timeout_kills_and_raises(self) -> None:
        fake = FakeProc()
        calls: list[str] = []

        def wait(timeout: float | None = None) -> int:
            calls.append("wait")
            if len(calls) == 1:
                raise subprocess.TimeoutExpired("x", 1)
            return -9

        fake.wait = wait  # type: ignore[method-assign]
        fake.kill = mock.Mock()  # type: ignore[attr-defined]
        with mock.patch.object(quiet_io.subprocess, "Popen", return_value=fake):
            with self.assertRaises(subprocess.TimeoutExpired):
                quiet_io.run_quiet(["x"], timeout=1, platform="win32")
        fake.kill.assert_called_once_with()

    def test_linux_keeps_devnull(self) -> None:
        done = subprocess.CompletedProcess(["x"], 0, b"", b"")
        with mock.patch.object(quiet_io.subprocess, "run", return_value=done) as run, \
                mock.patch.object(quiet_io.subprocess, "Popen") as popen:
            quiet_io.run_quiet(["x"], timeout=7, env={"A": "b"}, platform="linux")
            quiet_io.run_quiet(["x"], timeout=7, capture_stdout=True, platform="darwin")
        popen.assert_not_called()
        first, second = run.call_args_list
        self.assertEqual(first.kwargs["stdin"], subprocess.DEVNULL)
        self.assertEqual(first.kwargs["stdout"], subprocess.DEVNULL)
        self.assertEqual(first.kwargs["stderr"], subprocess.DEVNULL)
        self.assertEqual(first.kwargs["env"], {"A": "b"})
        self.assertEqual(first.kwargs["timeout"], 7)
        self.assertEqual(second.kwargs["stdout"], subprocess.PIPE)

    def test_win32_capture_stderr_is_bounded(self) -> None:
        fake = FakeProc(stderr=b"e" * 10000)
        with mock.patch.object(quiet_io.subprocess, "Popen", return_value=fake):
            cp = quiet_io.run_quiet(["x"], timeout=5, capture_stderr=True, stderr_keep_max=50, platform="win32")
        self.assertEqual(cp.stderr, b"e" * 50)

    def test_stderr_is_empty_unless_requested(self) -> None:
        fake = FakeProc(stderr=b"noise")
        with mock.patch.object(quiet_io.subprocess, "Popen", return_value=fake):
            cp = quiet_io.run_quiet(["x"], timeout=5, platform="win32")
        self.assertEqual(cp.stderr, b"")

    def test_real_process_captures_bounded_stderr_on_this_os(self) -> None:
        code = "import sys; sys.stderr.write('a' * 20000 + 'panicked at x:1'); sys.exit(3)"
        cp = quiet_io.run_quiet([sys.executable, "-c", code], timeout=30, capture_stderr=True, stderr_keep_max=64)
        self.assertEqual(cp.returncode, 3)
        self.assertEqual(len(cp.stderr), 64)
        self.assertTrue(cp.stderr.endswith(b"panicked at x:1"))

    def test_real_process_on_this_os(self) -> None:
        cp = quiet_io.run_quiet([sys.executable, "-c", "print('hi')"], timeout=30, capture_stdout=True)
        self.assertEqual((cp.returncode, cp.stdout.strip()), (0, b"hi"))

    def test_real_process_win32_path_on_this_os(self) -> None:
        # win32 分岐の実プロセス動作（PIPE 経由）。stdin は閉じられ、read が EOF で返る。
        code = "import sys; sys.stdout.write(sys.stdin.read() + 'ok')"
        cp = quiet_io.run_quiet([sys.executable, "-c", code], timeout=30, capture_stdout=True, platform="win32")
        self.assertEqual((cp.returncode, cp.stdout), (0, b"ok"))


class PopenStdioHostTest(unittest.TestCase):
    def test_win32_stderr_is_piped_not_devnull(self) -> None:
        fake = FakeProc(stderr=b"x" * 10)
        with mock.patch.object(quiet_io.subprocess, "Popen", return_value=fake) as popen:
            proc = quiet_io.popen_stdio_host(["h", "--stdio"], env={"A": "b"}, platform="win32")
        self.assertIs(proc, fake)
        kw = popen.call_args.kwargs
        self.assertEqual((kw["stdin"], kw["stdout"], kw["stderr"]), (subprocess.PIPE,) * 3)
        self.assertEqual(kw["env"], {"A": "b"})
        self.assertNotIn(subprocess.DEVNULL, kw.values())

    def test_linux_stderr_is_devnull(self) -> None:
        with mock.patch.object(quiet_io.subprocess, "Popen") as popen:
            quiet_io.popen_stdio_host(["h", "--stdio"], platform="linux")
        kw = popen.call_args.kwargs
        self.assertEqual(kw["stdin"], subprocess.PIPE)
        self.assertEqual(kw["stdout"], subprocess.PIPE)
        self.assertEqual(kw["stderr"], subprocess.DEVNULL)


class TailTextTest(unittest.TestCase):
    def test_keeps_tail_and_strips_control_chars(self) -> None:
        text = quiet_io.tail_text(b"head" + b"\x1b[31mred\x00" + b"\nlast\xff", limit=12)
        self.assertEqual(text, "1mred\nlast\ufffd")

    def test_win32_host_keeps_stderr_drain_and_linux_none(self) -> None:
        fake = FakeProc(stderr=b"boom")
        with mock.patch.object(quiet_io.subprocess, "Popen", return_value=fake):
            proc = quiet_io.popen_stdio_host(["h"], platform="win32")
        self.assertEqual(proc.stderr_drain.join(2.0), b"boom")
        with mock.patch.object(quiet_io.subprocess, "Popen", return_value=FakeProc()):
            self.assertIsNone(quiet_io.popen_stdio_host(["h"], platform="linux").stderr_drain)


class RecordHostExitTest(unittest.TestCase):
    """in_sandbox.record_host_exit（#1413）。shutdown 後の終了コードと stderr の記録。"""

    def setUp(self) -> None:
        import in_sandbox
        self.mod = in_sandbox

    def test_nonzero_exit_records_rc_and_stderr_tail(self) -> None:
        proc = mock.Mock()
        proc.wait.return_value = 3221226505
        proc.stderr_drain.join.return_value = b"panicked at src/x.rs:1"
        result: dict = {}
        self.mod.record_host_exit(proc, result)
        self.assertEqual(result, {"host_exit_rc": 3221226505, "host_stderr_tail": "panicked at src/x.rs:1"})

    def test_zero_exit_records_no_tail(self) -> None:
        proc = mock.Mock()
        proc.wait.return_value = 0
        result: dict = {}
        self.mod.record_host_exit(proc, result)
        self.assertEqual(result, {"host_exit_rc": 0})

    def test_timeout_is_marked(self) -> None:
        proc = mock.Mock()
        proc.wait.side_effect = subprocess.TimeoutExpired("h", 1)
        result: dict = {}
        self.mod.record_host_exit(proc, result)
        self.assertEqual(result, {"host_exit_rc": None, "host_exit_timed_out": True})

    def test_fmt_rc_shows_hex_for_fail_fast(self) -> None:
        import measure
        self.assertEqual(measure.fmt_rc(3221226505),
                         "3221226505 (0xC0000409; fail-fast, includes Rust panic=abort)")
        self.assertEqual(measure.fmt_rc(0), "0")
        self.assertEqual(measure.fmt_rc(-11), "-11")
        self.assertEqual(measure.fmt_rc(None), "None")


class InnerSourceTest(unittest.TestCase):
    def test_in_sandbox_has_no_devnull_references(self) -> None:
        src = (HERE / "in_sandbox.py").read_text(encoding="utf-8")
        for token in ("DEVNULL", "os.devnull"):
            self.assertNotIn(token, src)


if __name__ == "__main__":
    unittest.main()
