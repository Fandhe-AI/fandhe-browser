"""内側（サンドボックス内）の子プロセス起動で `nul` デバイスを開かないための共通ヘルパー。

役割・責務境界:
    Issue #1401（TASK-38・`RENDER-3`・`XOS-1`）。`in_sandbox.py` が `ps`・ホストバイナリ等を起動するときの
    stdin / stdout / stderr の扱いを閉じ込める。Windows の AppContainer のトークンは `nul` を開けず、
    `subprocess.DEVNULL`（内部で `os.open(os.devnull)`）が `PermissionError` で拒否されたため、
    win32 では DEVNULL を使わず stdin を PIPE にして直後に close し、stdout / stderr は PIPE を
    上限つきで読み捨てる（ログファイルを作らないので肥大化せず、ディスク書込もない）。
    Linux・macOS は従来どおり DEVNULL を使い、挙動を変えない。

呼び出し文脈: `in_sandbox.py` の内側コードだけが使う。外側（`measure.py`・`appcontainer.py` のホスト側）は対象外。
"""

from __future__ import annotations

import subprocess
import sys
import threading
from typing import Any

DRAIN_CHUNK = 4096
# 読み捨てる際に保持する先頭の最大バイト数（呼び出し側が結果を使う場合の上限。メモリ肥大を防ぐ）。
KEEP_MAX = 64 * 1024


def use_devnull(platform: str | None = None) -> bool:
    """DEVNULL を使ってよいか。win32（AppContainer が `nul` を拒否する）のときだけ False。"""
    return (platform if platform is not None else sys.platform) != "win32"


class BoundedDrain:
    """パイプを最後まで読み、先頭 `keep_max` バイトだけ保持して残りを捨てるスレッド。"""

    def __init__(self, stream: Any, keep_max: int = KEEP_MAX) -> None:
        self._stream = stream
        self._keep_max = keep_max
        self.data = bytearray()
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()

    def _run(self) -> None:
        try:
            while True:
                chunk = self._stream.read(DRAIN_CHUNK)
                if not chunk:
                    return
                room = self._keep_max - len(self.data)
                if room > 0:
                    self.data += chunk[:room]
        except (OSError, ValueError):
            return

    def join(self, timeout: float | None = None) -> bytes:
        """読み切りを待ち、保持したバイト列（上限以内）を返す。"""
        self._thread.join(timeout)
        return bytes(self.data)


def run_quiet(argv: list[str], *, timeout: float, env: dict[str, str] | None = None,
              capture_stdout: bool = False, keep_max: int = KEEP_MAX,
              platform: str | None = None) -> subprocess.CompletedProcess[bytes]:
    """`subprocess.run` の代わり。stdin 無し・stderr 破棄・stdout は `capture_stdout` のときだけ（上限つき）返す。

    タイムアウトは `subprocess.TimeoutExpired`（`subprocess.run` と同じ）。
    """
    if use_devnull(platform):
        return subprocess.run(  # noqa: S603  (argv は呼び出し側で固定)
            argv, env=env, stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE if capture_stdout else subprocess.DEVNULL,
            stderr=subprocess.DEVNULL, timeout=timeout, check=False)
    proc = subprocess.Popen(  # noqa: S603
        argv, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        if proc.stdin is not None:
            proc.stdin.close()
    except OSError:
        pass
    out = BoundedDrain(proc.stdout, keep_max if capture_stdout else 0)
    err = BoundedDrain(proc.stderr, 0)
    try:
        rc = proc.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()
        raise
    data = out.join(5.0)
    err.join(5.0)
    return subprocess.CompletedProcess(argv, rc, data if capture_stdout else b"", b"")


def popen_stdio_host(argv: list[str], *, env: dict[str, str] | None = None,
                     platform: str | None = None) -> subprocess.Popen[bytes]:
    """stdin / stdout を PIPE（フレーム通信用）、stderr を捨てる形でホストを起動する。

    win32 では stderr を PIPE にして上限つきで読み捨てるスレッドを起こす（読まないとパイプが満杯で
    ホストが止まるため）。
    """
    if use_devnull(platform):
        return subprocess.Popen(  # noqa: S603
            argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=env)
    proc = subprocess.Popen(  # noqa: S603
        argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
    BoundedDrain(proc.stderr, 0)
    return proc
