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
# 異常終了の診断用に保持する stderr の末尾の最大バイト数（#1413。panic メッセージ等を残すための小さな上限）。
STDERR_KEEP_MAX = 8 * 1024


def use_devnull(platform: str | None = None) -> bool:
    """DEVNULL を使ってよいか。win32（AppContainer が `nul` を拒否する）のときだけ False。"""
    return (platform if platform is not None else sys.platform) != "win32"


class BoundedDrain:
    """パイプを最後まで読み、`keep_max` バイトだけ保持して残りを捨てるスレッド。

    既定は先頭保持。`keep_tail=True` は末尾保持（直近 `keep_max` バイトを残す。#1413 の stderr 診断用で、
    最後の段階マーカーと panic メッセージを確実に残すため）。
    """

    def __init__(self, stream: Any, keep_max: int = KEEP_MAX, keep_tail: bool = False) -> None:
        self._stream = stream
        self._keep_max = keep_max
        self._keep_tail = keep_tail
        self.data = bytearray()
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()

    def _run(self) -> None:
        try:
            while True:
                chunk = self._stream.read(DRAIN_CHUNK)
                if not chunk:
                    return
                if self._keep_tail:
                    if self._keep_max > 0:
                        self.data += chunk
                        if len(self.data) > self._keep_max:
                            del self.data[:len(self.data) - self._keep_max]
                    continue
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
              capture_stderr: bool = False, stderr_keep_max: int = STDERR_KEEP_MAX,
              platform: str | None = None) -> subprocess.CompletedProcess[bytes]:
    """`subprocess.run` の代わり。stdin 無し・stdout / stderr は `capture_*` のときだけ（上限つき）返す。

    `capture_stderr` は #1413 の診断用（selftest が異常終了した理由を残す）。末尾 `stderr_keep_max` バイトだけ保持する。

    タイムアウトは `subprocess.TimeoutExpired`（`subprocess.run` と同じ）。
    """
    devnull = use_devnull(platform)
    if devnull and not capture_stderr:
        return subprocess.run(  # noqa: S603  (argv は呼び出し側で固定)
            argv, env=env, stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE if capture_stdout else subprocess.DEVNULL,
            stderr=subprocess.DEVNULL, timeout=timeout, check=False)
    # stderr を上限つきで取るため（`subprocess.run` の PIPE は無制限）Popen と BoundedDrain を使う。
    proc = subprocess.Popen(  # noqa: S603
        argv, env=env, stdin=subprocess.DEVNULL if devnull else subprocess.PIPE,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        if proc.stdin is not None:
            proc.stdin.close()
    except OSError:
        pass
    out = BoundedDrain(proc.stdout, keep_max if capture_stdout else 0)
    err = BoundedDrain(proc.stderr, stderr_keep_max if capture_stderr else 0, keep_tail=True)
    try:
        rc = proc.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()
        raise
    data = out.join(5.0)
    err_data = err.join(5.0)
    return subprocess.CompletedProcess(argv, rc, data if capture_stdout else b"",
                                       err_data if capture_stderr else b"")


def tail_text(data: bytes, limit: int = 2048) -> str:
    """診断用に stderr の末尾 `limit` バイトを UTF-8 replace で文字列化し、制御文字（改行・タブ以外）を除く。"""
    text = data[-limit:].decode("utf-8", errors="replace")
    return "".join(c for c in text if c in "\n\t" or c.isprintable())


def popen_stdio_host(argv: list[str], *, env: dict[str, str] | None = None,
                     platform: str | None = None) -> subprocess.Popen[bytes]:
    """stdin / stdout を PIPE（フレーム通信用）、stderr を捨てる形でホストを起動する。

    win32 では stderr を PIPE にして上限つきで読み捨てるスレッドを起こす（読まないとパイプが満杯で
    ホストが止まるため）。診断用に末尾 `STDERR_KEEP_MAX` バイトを保持し、戻り値の属性 `stderr_drain`
    （`BoundedDrain`）から後で読める（#1413）。非 win32 は従来どおり捨てる（`stderr_drain` は None）。
    """
    if use_devnull(platform):
        proc = subprocess.Popen(  # noqa: S603
            argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=env)
        proc.stderr_drain = None  # type: ignore[attr-defined]
        return proc
    proc = subprocess.Popen(  # noqa: S603
        argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
    proc.stderr_drain = BoundedDrain(proc.stderr, STDERR_KEEP_MAX, keep_tail=True)  # type: ignore[attr-defined]
    return proc
