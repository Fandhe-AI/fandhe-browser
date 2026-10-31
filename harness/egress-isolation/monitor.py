#!/usr/bin/env python3
"""ハーネスが意図的に行う接続を除外して監視を続ける送信先モニタ（`egress_guard` の走査関数を流用）。

役割・責務境界:
    Issue #1377（TASK-38・`RENDER-3`・`SEC-2`）。`render-screenshot/egress_guard.py` の `EgressGuard` は
    最初の迂回を検知した時点で監視スレッドを終了する。本ハーネスは試行そのものとして
    127.0.0.2 / 127.0.0.3 のダミー宛先へ接続させるため、`EgressGuard` を使うと最初の試行で監視が止まり、
    後続の経路が観測されないまま合格になりうる。そこで走査関数 `find_unproxied_connections` だけを使い、
    既知のダミー宛先（`expected`）は「想定された試行」として数え、それ以外の宛先だけを違反として記録して
    監視を継続する。

呼び出し文脈:
    `in_sandbox.py` が Servo ホストの起動直後に開始し、経路ごとに `polls`（完了した走査回数）を控えて
    `judge.judge_all` へ渡す。走査が 1 回も完了していない経路は合格にしない（観測不能を合格にしない契約）。
    `egress_guard` 自体（`render-screenshot` の撮影用）の挙動は変えない。
"""

from __future__ import annotations

import sys
import threading
from pathlib import Path
from typing import Callable

_HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(_HERE.parent / "render-screenshot"))
import egress_guard  # noqa: E402

Scanner = Callable[[int, int, Callable[[], bool]], "list[str]"]


class DestinationMonitor:
    """想定内のダミー宛先を除外しつつ、走査を止めずに送信先を監視する。

    `violation`: 想定外の宛先への接続（最初の 1 件。監視は継続）。
    `unavailable`: 監視不能の理由（走査の失敗）。走査失敗でスレッドが終了した場合は必ず設定される。
    `polls`: 完了した走査の回数。`expected_seen`: 観測された想定内の宛先。
    """

    def __init__(self, root_pid: int, page_port: int, expected: set[str],
                 scanner: Scanner | None = None, interval_s: float = egress_guard.POLL_INTERVAL_SEC) -> None:
        self._root_pid = root_pid
        self._page_port = page_port
        self._expected = expected
        self._scan = scanner or egress_guard.find_unproxied_connections
        self._interval = interval_s
        self._stop = threading.Event()
        self._lock = threading.Lock()
        self._thread = threading.Thread(target=self._run, name="egress-monitor", daemon=True)
        self.violation: str | None = None
        self.unavailable: str | None = None
        self.polls = 0
        self.expected_seen: set[str] = set()

    def start(self) -> None:
        self._thread.start()

    def stop(self, join_timeout_s: float = 5.0) -> None:
        self._stop.set()
        self._thread.join(timeout=join_timeout_s)
        with self._lock:
            if self._thread.is_alive() and self.unavailable is None:
                self.unavailable = "monitor thread did not exit within timeout"

    def snapshot_polls(self) -> int:
        with self._lock:
            return self.polls

    def _run(self) -> None:
        while not self._stop.is_set():
            try:
                found = self._scan(self._root_pid, self._page_port, self._stop.is_set)
            except egress_guard._ScanAborted:  # noqa: SLF001 - 停止要求による中断（監視不能ではない）
                return
            except Exception as exc:  # noqa: BLE001 - 監視不能を「接続なし」にしない（fail-closed）
                with self._lock:
                    self.unavailable = f"{type(exc).__name__}: {exc}"[:300]
                return
            with self._lock:
                self.polls += 1
                for dest in found:
                    if dest in self._expected:
                        self.expected_seen.add(dest)
                    elif self.violation is None:
                        self.violation = dest
            self._stop.wait(self._interval)
