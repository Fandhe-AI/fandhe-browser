"""撮影エンジンのプロキシ迂回（直接接続）を Linux の /proc から検出する best-effort ガード。

役割・責務境界:
    `capture_screenshots.py` が撮影エンジン（Servo 等）を起動した後、そのプロセスツリー
    が検証プロキシ以外へ TCP / UDP で接続していないかを `/proc` のソケット表から
    ポーリングで調べる（RENDER-5 / TASK-37）。Servo の WebSocket は
    `servo-net/websocket_loader.rs` が `TcpStream::connect` で直接接続し、
    `Preferences` のプロキシ設定を通らないため、pref だけでは防げない経路を補う目的。

    検出時は `on_violation`（`capture_screenshots._kill_process_tree`）でツリーを
    終了し、呼び出し側が撮影を `failed` にする。

限界（実装済みを装わない。REPAIR-3）:
    - ポーリング（既定 25 ms）のため、それより短命な接続は取りこぼす
    - 接続先が見えるのは接続済みソケットのみ（未接続 UDP の送信先は見えない）
    - Linux 専用。macOS / Windows と `/proc`（ソケット表・自プロセスの FD）が読めない環境
      では `egress_guard_supported()` が False を返し、呼び出し側が撮影を skipped にする（fail-closed）
    - 監視中に FD・ソケット表を読めなくなった場合（権限等）は `GuardUnavailable` を送出し、
      `EgressGuard.unavailable` に理由を残してツリーを終了する。呼び出し側は撮影を
      `failed` にし、`egress_guard` を `enforced` と記録しない。プロセス終了による
      正常な消失（ENOENT / ESRCH）は監視不能として扱わない
    - プロセス一覧・FD 一覧・ソケット表が上限（MAX_*）を超える、または stat 等の読み取りに
      失敗した場合は、切り捨てて正常扱いにせず `GuardUnavailable`（撮影は failed）とする
    - `/proc/net/tcp6`・`udp6` が無い（IPv6 無効）環境では IPv4 表のみで監視する
    - ソケット表は監視プロセス自身のネットワーク名前空間の `/proc/net/*` を読むため、
      監視対象ツリーのプロセスが別のネットワーク名前空間（`/proc/<pid>/ns/net` の readlink 値が
      監視プロセスと異なる）にいる場合は inode が表に現れず迂回を検出できない。この場合は
      `GuardUnavailable` を送出し、`enforced` と記録しない（fail-closed）
    - 完全な保証ではなく、ネットワーク名前空間による隔離の代わりにはならない
"""

from __future__ import annotations

import ipaddress
import os
import sys
import threading
import time
from pathlib import Path
from typing import Callable

POLL_INTERVAL_SEC = 0.025
# 外部入力（/proc）を走査する際の上限。無制限確保・長時間占有を避ける。
MAX_PROCESSES = 4096
MAX_FDS_PER_PROCESS = 4096
MAX_NET_LINES = 100_000
MAX_LINE_CHARS = 512

_TCP_LISTEN = 0x0A
_PROC = Path("/proc")


class GuardUnavailable(Exception):
    """監視対象の FD・ソケット表を読めず、迂回の有無を判定できないことを表す。"""


def _is_vanished(exc: OSError) -> bool:
    """プロセス終了などによる正常な消失（監視不能ではない）かを返す。"""
    return isinstance(exc, (FileNotFoundError, ProcessLookupError))


def egress_guard_supported() -> bool:
    """このプラットフォームで送信先ソケット監視が使えるかを返す。

    Linux で、`/proc/net/tcp` に加えて自プロセスの FD 一覧とそのリンク先を読めることを要する
    （子プロセスの FD も同じ権限で読む前提の事前確認）。
    """
    if not sys.platform.startswith("linux"):
        return False
    try:
        # IPv4 の tcp / udp は必須。tcp6 / udp6 は IPv6 無効カーネルで存在しないため任意
        # （不在なら IPv6 ソケットも存在し得ないので IPv4 のみの監視で足りる）。
        (_PROC / "net" / "tcp").read_text(encoding="ascii", errors="replace")
        (_PROC / "net" / "udp").read_text(encoding="ascii", errors="replace")
        os.readlink(_PROC / "self" / "ns" / "net")
        fd_dir = _PROC / "self" / "fd"
        names = _retry_transient(lambda: os.listdir(fd_dir))  # type: ignore[assignment]
        if names:
            os.readlink(fd_dir / names[0])
    except OSError:
        return False
    return True


def _parse_addr(text: str) -> tuple[ipaddress.IPv4Address | ipaddress.IPv6Address, int] | None:
    """`/proc/net/tcp*` の `HEXADDR:HEXPORT` を `(ip, port)` に直す。不正な行は None。"""
    host_hex, sep, port_hex = text.partition(":")
    if not sep:
        return None
    try:
        port = int(port_hex, 16)
        if len(host_hex) == 8:
            # IPv4 はホストバイトオーダー（little endian）の 32bit 16 進。
            return ipaddress.IPv4Address(bytes.fromhex(host_hex)[::-1]), port
        if len(host_hex) == 32:
            # IPv6 は 4 バイトワードごとに little endian。
            words = [bytes.fromhex(host_hex[i : i + 8])[::-1] for i in range(0, 32, 8)]
            return ipaddress.IPv6Address(b"".join(words)), port
    except ValueError:
        return None
    return None


def parse_net_line(line: str) -> tuple[str, int, int, int, int] | None:
    """`/proc/net/{tcp,udp}{,6}` の 1 行を `(remote_ip, remote_port, state, inode, local_port)` に直す。

    `remote_ip` は IPv4-mapped IPv6 を IPv4 に畳んだ文字列。ヘッダ行・不正行は None。
    """
    fields = line[:MAX_LINE_CHARS].split()
    if len(fields) < 10:
        return None
    local = _parse_addr(fields[1])
    remote = _parse_addr(fields[2])
    if local is None or remote is None:
        return None
    try:
        state = int(fields[3], 16)
        inode = int(fields[9])
    except ValueError:
        return None
    remote_ip = remote[0]
    if isinstance(remote_ip, ipaddress.IPv6Address) and remote_ip.ipv4_mapped is not None:
        remote_ip = remote_ip.ipv4_mapped
    return str(remote_ip), remote[1], state, inode, local[1]


class _ScanAborted(Exception):
    """停止要求により走査を中断したことを表す内部用例外（監視不能ではない）。"""


def _check_abort(should_abort: Callable[[], bool] | None) -> None:
    if should_abort is not None and should_abort():
        raise _ScanAborted


def _descendant_pids(root_pid: int, should_abort: Callable[[], bool] | None = None) -> set[int]:
    """`root_pid` とその子孫の pid 集合を /proc/<pid>/stat の ppid から求める。"""
    children: dict[int, list[int]] = {}
    try:
        names = os.listdir(_PROC)
    except OSError as exc:
        raise GuardUnavailable(f"cannot list {_PROC}: {exc}") from exc
    # 上限超過は子孫の取りこぼしになるため、切り捨てず監視不能として扱う。
    if len(names) > MAX_PROCESSES:
        raise GuardUnavailable(f"too many entries in {_PROC}: {len(names)} > {MAX_PROCESSES}")
    for name in names:
        _check_abort(should_abort)
        if not name.isdigit():
            continue
        try:
            stat = (_PROC / name / "stat").read_text(encoding="utf-8", errors="replace")
            if not stat:
                continue  # 終了処理中のプロセスは空を返すことがある（正常な消失）
            # comm は括弧内に空白・括弧を含みうるため、最後の ')' 以降を解析する。
            rest = stat[stat.rindex(")") + 2 :].split()
            ppid = int(rest[1])
        except OSError as exc:
            if _is_vanished(exc):
                continue
            raise GuardUnavailable(f"cannot read /proc/{name}/stat: {exc}") from exc
        except (ValueError, IndexError) as exc:
            raise GuardUnavailable(f"cannot parse /proc/{name}/stat: {exc}") from exc
        children.setdefault(ppid, []).append(int(name))
    seen = {root_pid}
    stack = [root_pid]
    while stack:
        for child in children.get(stack.pop(), []):
            if child not in seen:
                seen.add(child)
                stack.append(child)
    return seen


def _retry_transient(fn: Callable[[], object]) -> object:
    """fork 直後・exec 中のプロセスは /proc/<pid>/fd が一瞬 EACCES になるため短く再試行する。

    恒久的な権限不足は再試行後も EACCES のままなので、呼び出し側が監視不能として扱う。
    """
    for _ in range(20):
        try:
            return fn()
        except PermissionError:
            time.sleep(0.005)
    return fn()


def _check_same_netns(pids: set[int], should_abort: Callable[[], bool] | None = None) -> None:
    """監視対象の全プロセスが監視プロセスと同じネットワーク名前空間にいることを確認する。

    `/proc/net/*` は監視プロセス自身の名前空間の表のため、異なる名前空間のソケットは
    表に現れず迂回を取りこぼす。異なる場合・判定できない場合は `GuardUnavailable`。
    """
    try:
        own = os.readlink(_PROC / "self" / "ns" / "net")
    except OSError as exc:
        raise GuardUnavailable(f"cannot read {_PROC}/self/ns/net: {exc}") from exc
    for pid in pids:
        _check_abort(should_abort)
        ns_path = _PROC / str(pid) / "ns" / "net"
        try:
            target = _retry_transient(lambda: os.readlink(ns_path))
        except OSError as exc:
            if _is_vanished(exc):
                continue
            raise GuardUnavailable(f"cannot read {ns_path}: {exc}") from exc
        if target != own:
            raise GuardUnavailable(
                f"pid {pid} is in a different network namespace ({target} != {own}); "
                "/proc/net of the monitor does not list its sockets"
            )


def _socket_inodes(pids: set[int], should_abort: Callable[[], bool] | None = None) -> set[int]:
    inodes: set[int] = set()
    for pid in pids:
        _check_abort(should_abort)
        fd_dir = _PROC / str(pid) / "fd"
        try:
            names = _retry_transient(lambda: os.listdir(fd_dir))  # type: ignore[assignment]
        except OSError as exc:
            if _is_vanished(exc):
                continue
            raise GuardUnavailable(f"cannot list {fd_dir}: {exc}") from exc
        if len(names) > MAX_FDS_PER_PROCESS:
            raise GuardUnavailable(f"too many fds in {fd_dir}: {len(names)} > {MAX_FDS_PER_PROCESS}")
        for name in names:
            _check_abort(should_abort)
            try:
                target = _retry_transient(lambda: os.readlink(fd_dir / name))  # type: ignore[assignment]
            except OSError as exc:
                if _is_vanished(exc):
                    continue
                raise GuardUnavailable(f"cannot read {fd_dir / name}: {exc}") from exc
            if target.startswith("socket:[") and target.endswith("]"):
                try:
                    inodes.add(int(target[8:-1]))
                except ValueError:
                    continue
    return inodes


def find_unproxied_connections(
    root_pid: int,
    proxy_port: int,
    should_abort: Callable[[], bool] | None = None,
) -> list[str]:
    """`root_pid` 以下のプロセスが張る、検証プロキシ以外への接続を `host:port` の一覧で返す。

    許可するのは remote が 127.0.0.1 の `proxy_port` のみ。ツリー内の待受ポートへの
    ループバック接続（プロセス間通信）、LISTEN ソケット、未接続の UDP は除外する。
    FD・ソケット表を読めない場合は `GuardUnavailable` を送出する（接続なしとは扱わない）。
    `should_abort` が真を返した時点で `_ScanAborted` を送出して走査を中断する（`EgressGuard.stop` 用）。
    """
    pids = _descendant_pids(root_pid, should_abort)
    _check_same_netns(pids, should_abort)
    inodes = _socket_inodes(pids, should_abort)
    if not inodes:
        return []
    rows: list[tuple[str, int, int, int, int, bool]] = []
    for table, is_udp in (("tcp", False), ("tcp6", False), ("udp", True), ("udp6", True)):
        try:
            with open(_PROC / "net" / table, encoding="ascii", errors="replace") as fh:
                for index, line in enumerate(fh):
                    if index % 1024 == 0:
                        _check_abort(should_abort)
                    if index >= MAX_NET_LINES:
                        raise GuardUnavailable(f"/proc/net/{table} exceeds {MAX_NET_LINES} lines")
                    parsed = parse_net_line(line)
                    if parsed is not None and parsed[3] in inodes:
                        rows.append((*parsed, is_udp))
        except FileNotFoundError:
            if table in ("tcp6", "udp6"):
                continue  # IPv6 無効カーネル。IPv4 表のみで監視する
            raise GuardUnavailable(f"missing /proc/net/{table}") from None
        except OSError as exc:
            raise GuardUnavailable(f"cannot read /proc/net/{table}: {exc}") from exc
    own_listen_ports = {local for _, _, state, _, local, udp in rows if not udp and state == _TCP_LISTEN}
    found: list[str] = []
    for remote_ip, remote_port, state, _, local_port, is_udp in rows:
        if not is_udp and state == _TCP_LISTEN:
            continue
        if remote_port == 0 and ipaddress.ip_address(remote_ip).is_unspecified:
            continue  # 未接続 UDP
        if remote_ip == "127.0.0.1" and remote_port == proxy_port:
            continue
        if remote_ip == "127.0.0.1" and (remote_port in own_listen_ports or local_port in own_listen_ports):
            continue  # ツリー内のプロセス間通信
        found.append(f"{remote_ip}:{remote_port}")
    return found


class EgressGuard:
    """エンジン実行中に別スレッドで送信先ソケットを監視し、迂回を検出したらツリーを終了する。

    `stop()` 後は結果（`violation` / `unavailable`）が確定し、監視スレッドは以後 `on_violation` を
    呼ばない。スレッドが期限内に終了しなかった場合は `unavailable` を設定する（fail-closed。
    撮影は enforced と記録せず failed になる）。
    """

    def __init__(self, root_pid: int, proxy_port: int, on_violation: Callable[[], None]) -> None:
        self._root_pid = root_pid
        self._proxy_port = proxy_port
        self._on_violation = on_violation
        self._stop = threading.Event()
        # 結果確定と on_violation 呼び出しを直列化する（確定後の遅延 kill = PID 再利用事故を防ぐ）。
        self._lock = threading.Lock()
        self._finalized = False
        self._thread = threading.Thread(target=self._run, name="egress-guard", daemon=True)
        self.violation: str | None = None
        # 監視不能（FD・ソケット表の読み取り失敗等）の理由。設定された撮影は failed にする。
        self.unavailable: str | None = None

    def start(self) -> None:
        self._thread.start()

    def stop(self, join_timeout_sec: float = 5.0) -> None:
        self._stop.set()
        self._thread.join(timeout=join_timeout_sec)
        with self._lock:
            self._finalized = True
            if self._thread.is_alive() and self.violation is None and self.unavailable is None:
                self.unavailable = "monitor thread did not exit within timeout"

    def _report(self, *, violation: str | None = None, unavailable: str | None = None) -> None:
        """結果を記録して `on_violation` を呼ぶ。`stop()` で確定済みなら何もしない。"""
        with self._lock:
            if self._finalized:
                return
            self.violation = violation
            self.unavailable = unavailable
            self._on_violation()

    def _run(self) -> None:
        while not self._stop.is_set():
            try:
                found = find_unproxied_connections(self._root_pid, self._proxy_port, self._stop.is_set)
            except _ScanAborted:
                return
            except Exception as exc:  # noqa: BLE001 - 監視不能を「接続なし」にしない（fail-closed）
                self._report(unavailable=f"{type(exc).__name__}: {exc}"[:300])
                return
            if found:
                self._report(violation=found[0])
                return
            self._stop.wait(POLL_INTERVAL_SEC)
