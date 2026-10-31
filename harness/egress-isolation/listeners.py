#!/usr/bin/env python3
"""egress 隔離の実測で使うダミー listener（TCP・UDP）。

役割・責務境界:
    Issue #1377（TASK-38・`RENDER-3`・`SEC-2`）。Servo ホストが許可外の宛先へ接続を試みたかを
    「受信記録」で観測する。bind は 127.0.0.0/8 の 1024 以上のポートに限り（それ以外は ValueError）、
    実在の外部サイト・内部ネットワークには一切向けない。

呼び出し文脈:
    - `measure.py`（外側の netns）が許可外の宛先役として起動し、隔離をすり抜けた接続を数える。
    - `in_sandbox.py`（隔離された netns の内側）が同じ宛先の写しを起動し、ページが実際に接続を
      試みたこと（試行の観測）を確かめる。

記録する内容（漏えい防止）:
    経路ラベル・件数・相手 IP と、HTTP のメソッド・クエリ/フラグメントを除いたパスだけ。
    本文・クエリ・ヘッダは保持しない。受信は先頭 `MAX_REQUEST_BYTES` まで、記録は `MAX_RECORDS` 件まで、
    同時接続は `MAX_CONCURRENT` までに制限する。
Python 標準ライブラリのみ。
"""

from __future__ import annotations

import ipaddress
import socket
import threading
from typing import Any

MIN_PORT = 1024
MAX_REQUEST_BYTES = 1024
MAX_RECORDS = 1000
MAX_CONCURRENT = 16
CONN_TIMEOUT_S = 1.0
MAX_PATH_CHARS = 128
_HTTP_204 = b"HTTP/1.1 204 No Content\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"


def validate_bind(ip: str, port: int) -> None:
    """bind 先を検証する。127.0.0.0/8 以外・1024 未満（0 は空きポート指定として許可）は ValueError。"""
    try:
        addr = ipaddress.IPv4Address(ip)
    except ValueError as exc:
        raise ValueError(f"listener ip must be an IPv4 address in 127.0.0.0/8: {ip!r}") from exc
    if addr not in ipaddress.IPv4Network("127.0.0.0/8"):
        raise ValueError(f"listener ip must be in 127.0.0.0/8: {ip}")
    if isinstance(port, bool) or not isinstance(port, int) or not (port == 0 or MIN_PORT <= port <= 65535):
        raise ValueError(f"listener port must be 0 or in [{MIN_PORT}, 65535]: {port!r}")


def sanitize_path(raw: str) -> str:
    """要求行のパスからクエリ・フラグメントを除き、長さを制限する。"""
    return raw.split("?", 1)[0].split("#", 1)[0][:MAX_PATH_CHARS]


def parse_request_line(data: bytes) -> tuple[str | None, str | None]:
    """HTTP 要求行（`METHOD PATH VERSION`）から (method, クエリなしのパス) を取る。不正なら (None, None)。"""
    line = data.split(b"\r\n", 1)[0][:MAX_REQUEST_BYTES].decode("ascii", errors="replace")
    parts = line.split(" ")
    if len(parts) != 3 or not parts[2].startswith("HTTP/") or not parts[0].isalpha():
        return None, None
    return parts[0][:16], sanitize_path(parts[1])


class TcpRecorder:
    """TCP の接続を受け付けて記録する listener。HTTP らしければ 204 を返し、それ以外は閉じる。"""

    def __init__(self, label: str, ip: str, port: int = 0) -> None:
        validate_bind(ip, port)
        self.label = label
        self._sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self._sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self._sock.bind((ip, port))
        self._sock.listen(MAX_CONCURRENT)
        self._sock.settimeout(0.2)
        self.ip, self.port = self._sock.getsockname()
        self._records: list[dict[str, Any]] = []
        self._count = 0
        self._lock = threading.Lock()
        self._slots = threading.BoundedSemaphore(MAX_CONCURRENT)
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._run, name=f"rec-{label}", daemon=True)

    def start(self) -> "TcpRecorder":
        self._thread.start()
        return self

    @property
    def count(self) -> int:
        with self._lock:
            return self._count

    def records(self) -> list[dict[str, Any]]:
        with self._lock:
            return list(self._records)

    def _run(self) -> None:
        while not self._stop.is_set():
            try:
                conn, peer = self._sock.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            if not self._slots.acquire(blocking=False):
                conn.close()
                continue
            threading.Thread(target=self._handle, args=(conn, peer[0]), daemon=True).start()

    def _handle(self, conn: socket.socket, peer_ip: str) -> None:
        try:
            conn.settimeout(CONN_TIMEOUT_S)
            try:
                data = conn.recv(MAX_REQUEST_BYTES)
            except OSError:
                data = b""
            method, path = parse_request_line(data)
            with self._lock:
                self._count += 1
                if len(self._records) < MAX_RECORDS:
                    self._records.append({"peer": peer_ip, "method": method, "path": path})
            if method is not None:
                try:
                    conn.sendall(_HTTP_204)
                except OSError:
                    pass
        finally:
            try:
                conn.close()
            except OSError:
                pass
            self._slots.release()

    def stop(self) -> None:
        self._stop.set()
        try:
            self._sock.close()
        except OSError:
            pass
        self._thread.join(timeout=2)


class UdpRecorder:
    """UDP のデータグラムを受信して件数・相手 IP・長さだけを記録する listener（内容は保持しない）。"""

    def __init__(self, label: str, ip: str, port: int = 0) -> None:
        validate_bind(ip, port)
        self.label = label
        self._sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self._sock.bind((ip, port))
        self._sock.settimeout(0.2)
        self.ip, self.port = self._sock.getsockname()
        self._records: list[dict[str, Any]] = []
        self._count = 0
        self._lock = threading.Lock()
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._run, name=f"rec-{label}", daemon=True)

    def start(self) -> "UdpRecorder":
        self._thread.start()
        return self

    @property
    def count(self) -> int:
        with self._lock:
            return self._count

    def records(self) -> list[dict[str, Any]]:
        with self._lock:
            return list(self._records)

    def _run(self) -> None:
        while not self._stop.is_set():
            try:
                data, peer = self._sock.recvfrom(MAX_REQUEST_BYTES)
            except socket.timeout:
                continue
            except OSError:
                return
            with self._lock:
                self._count += 1
                if len(self._records) < MAX_RECORDS:
                    self._records.append({"peer": peer[0], "length": len(data)})

    def stop(self) -> None:
        self._stop.set()
        try:
            self._sock.close()
        except OSError:
            pass
        self._thread.join(timeout=2)
