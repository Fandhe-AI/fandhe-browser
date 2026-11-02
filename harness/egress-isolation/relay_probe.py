#!/usr/bin/env python3
"""netns の中から UDS の中継経由で CONNECT プロキシに届くかを測る、Servo を使わないプローブ（Linux 専用）。

役割・責務境界:
    Issue #1431（TASK-38・`RENDER-3`・`SEC-2`・`XOS-1`）。設計 `docs/design/servo-host-egress-isolation.md` §8.1
    （#1003 の 2026-11-02 の見直し）は Linux の方式を B（bwrap `--unshare-net`）とし、外への通信は本体の許可リスト付き
    CONNECT プロキシ（§7 の承認事項。未実装）だけを通す想定にした。`--unshare-net` の中からは親の loopback に届かないため、
    「netns 内のクライアント → netns 内 `127.0.0.1:R`（中継）→ bind マウントした UDS → 親の CONNECT プロキシ → 宛先」
    の構成が成り立つかを、本番に組み込む前にここで確かめる。`measure.py`（Servo を駆動）とは独立したエントリポイント
    （`appcontainer_probe.py`・`platform_probe.py` と同じ位置づけ）。結果 JSON の `mode` は `proxy-relay`。

測る 5 項目（判定は `judge_relay`。pass / fail / unobservable の fail-closed）:
    - `lo`: `--unshare-net` の中で `lo` が使えるか（127.0.0.1 に bind・listen し、同じ netns から往復できるか）。
    - `uds`: 親で listen するファイルシステム上の UDS を `--bind`（rw）/ `--ro-bind`（ro）で見せたとき、中から connect できるか。
    - `relay`: 中継を通して親のプロキシ経由で外側の listener に届くか（陽性対照。200 応答と nonce の往復まで）。
    - `direct`: 中継を通さない直接 connect が netns で失敗するか（陰性対照。ベースラインでは届くことが前提）。
    - `ip_recheck`: プロキシが接続先 IP を再検査し、内部アドレス（127.0.0.2）への CONNECT を拒否できるか。

テスト用プロキシのポリシーは測定用のフィクスチャで、本番のプロキシではない。本番のポリシーは loopback を含む内部アドレスを
すべて拒否するはずで、ここでは 127.0.0.4 を「許可先」の代役、127.0.0.2 を「許可リストに載ったが内部 IP に解決された名前」
の代役として使う。本番のプロキシの実装・servo-host への注入は §7 の承認事項で、このファイルでは行わない。

安全上の規則:
    - 宛先は 127.0.0.0/8 のダミーだけ。プロキシは 127.0.0.0/8 以外へは（許可リストに載っていても）connect しない。
    - プロキシは UDS（0700 のディレクトリ内）でだけ待ち受ける。TCP の listen は loopback の echo listener と netns 内の中継のみ。
    - authority は IPv4 リテラルだけを受け付け、名前解決はしない。ヘッダ長・同時接続数・転送量・アイドル時間に上限を置く。
    - 結果に残すのは件数・バイト数・errno 名・ステータスコード・判定コードだけ（nonce の値・要求の本文は残さない）。
    - bwrap の argv は定数のリスト（シェル不使用）。依存・`unsafe`・ctypes は使わない。

終了コード: 0（完走。合否は result.json の verdicts を見る）／ 1（計測の失敗）／ 2（引数の不正・Linux 以外・bwrap 無しなど前提の不成立。
可能な範囲で unobservable の結果 JSON を書く）。

使い方:
    python3 -I -B harness/egress-isolation/relay_probe.py --out "$(mktemp -d)"
"""

from __future__ import annotations

import argparse
import errno
import ipaddress
import json
import os
import platform
import re
import secrets
import select
import shutil
import socket
import sys
import tempfile
import threading
import time
from pathlib import Path
from typing import Any

_HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(_HERE))
import judge  # noqa: E402
import listeners  # noqa: E402
import measure  # noqa: E402
import platform_probe  # noqa: E402

MODE = "proxy-relay"
PASS, FAIL, UNOBS = judge.PASS, judge.FAIL, judge.UNOBSERVABLE
ITEMS = ("lo", "uds", "relay", "direct", "ip_recheck")

IP_TARGET = "127.0.0.4"  # プロキシの許可先の代役（既存の 127.0.0.1〜.3 と役割を分ける）
IP_INTERNAL = judge.IP_BLOCKED  # 127.0.0.2。内部アドレスの代役
INTERNAL_IPS = frozenset({"127.0.0.2", "127.0.0.3"})  # プロキシが「内部アドレス」とみなす集合（フィクスチャ）
MOUNT_RW = "/run/fandhe-relay-rw"
MOUNT_RO = "/run/fandhe-relay-ro"
SOCK_NAME = "p.sock"
SUN_PATH_MAX = 100  # sun_path は 108 バイト。余裕を見る。
MAX_HEADER = 8192
MAX_ECHO_BYTES = 4096
MAX_RELAY_BYTES = 65536
IDLE_S = 2.0
CONN_TIMEOUT_S = 2.0
SETTLE_S = 0.4  # 内側の終了後、外側スレッドの受信集計が追いつくのを待つ。
RESPONSE_REASONS = {200: "Connection established", 400: "Bad Request", 403: "Forbidden",
                    405: "Method Not Allowed", 502: "Bad Gateway"}
AUTHORITY_RE = re.compile(r"^(\d{1,3}(?:\.\d{1,3}){3}):(\d{1,5})$")


# ---------------------------------------------------------------------------
# 純粋関数（I/O なし）
# ---------------------------------------------------------------------------

def parse_connect_request(data: bytes) -> tuple[int, str | None, int | None]:
    """CONNECT 要求のヘッダ塊を解析する。(200, ip, port)・(400, None, None)・(405, None, None) を返す。

    authority は IPv4 リテラル:ポートだけ（名前解決は親で行わない）。ヘッダは 8 KiB まで、終端 CRLFCRLF が必要。
    """
    if len(data) > MAX_HEADER or b"\r\n\r\n" not in data:
        return 400, None, None
    try:
        line = data.split(b"\r\n", 1)[0].decode("ascii")
    except UnicodeDecodeError:
        return 400, None, None
    parts = line.split(" ")
    if len(parts) != 3 or parts[2] not in ("HTTP/1.0", "HTTP/1.1"):
        return 400, None, None
    if parts[0] != "CONNECT":
        return 405, None, None
    m = AUTHORITY_RE.match(parts[1])
    if m is None:
        return 400, None, None
    try:
        ip = str(ipaddress.IPv4Address(m.group(1)))
    except ValueError:
        return 400, None, None
    port = int(m.group(2))
    if not 1 <= port <= 65535:
        return 400, None, None
    return 200, ip, port


def evaluate_policy(ip: str, port: int, allow: frozenset[tuple[str, int]]) -> str:
    """フィクスチャのポリシー。評価順: 安全柵（127/8 外）→ IP の再検査 → 許可リスト。

    再検査を許可リストより先に置くのは、許可リストに載った名前が内部 IP に解決された状況で拒否できるのが
    IP の再検査だけであることを測るため（`ip_recheck`）。
    """
    if ipaddress.IPv4Address(ip) not in ipaddress.IPv4Network("127.0.0.0/8"):
        return "out_of_range"
    if ip in INTERNAL_IPS:
        return "ip_denied"
    if (ip, port) not in allow:
        return "not_allowlisted"
    return "allowed"


def build_relay_argv(*, netns: bool, sockdir: str, inner_out: str, ports: dict[str, int], phase: str) -> list[str]:
    """bwrap の固定 argv。`--tmpfs /run` の後に UDS のディレクトリを rw と ro で明示的に見せる。"""
    argv = ["bwrap", "--die-with-parent"]
    if netns:
        argv.append("--unshare-net")
    argv += ["--dev-bind", "/", "/", "--tmpfs", "/run",
             "--bind", sockdir, MOUNT_RW, "--ro-bind", sockdir, MOUNT_RO, "--",
             sys.executable, "-I", "-B", str(Path(__file__).resolve()),
             "--inner", "--phase", phase, "--ports", json.dumps(ports, sort_keys=True), "--out", inner_out]
    return argv


def _unobs(reason: str, method: str = "") -> dict[str, str]:
    return {"verdict": UNOBS, "reason": reason, "method": method}


def _v(verdict: str, reason: str, method: str) -> dict[str, str]:
    return {"verdict": verdict, "reason": reason, "method": method}


def _ok(d: Any) -> bool:
    return isinstance(d, dict) and d.get("ok") is True


def _n(d: Any, *keys: str) -> int | None:
    for k in keys:
        if not isinstance(d, dict):
            return None
        d = d.get(k)
    return d if isinstance(d, int) and not isinstance(d, bool) and d >= 0 else None


def judge_relay(*, bwrap_found: bool, base_rc: int | None, net_rc: int | None, base: Any, net: Any,
                outer_base: Any, outer_net: Any, proxy: Any) -> dict[str, dict[str, str]]:
    """5 項目の判定（純粋関数）。データが無いことから pass を導かない（fail-closed）。

    `outer_*` は {"target": 受信件数, "internal": 受信件数}、`proxy` は netns の実行中の {"codes": {...}, "upstream_ok": n}。
    """
    if not bwrap_found:
        return {i: _unobs("bwrap not found") for i in ITEMS}
    if net_rc != 0:
        why = "timed out" if net_rc is None else f"netns run exited with rc={net_rc}"
        return {i: _unobs(why) for i in ITEMS}
    if not isinstance(net, dict) or not isinstance(net.get("precheck"), dict):
        return {i: _unobs("netns inner result missing or malformed") for i in ITEMS}
    if net["precheck"].get("ok") is not True:
        return {i: _unobs("netns precheck failed: " + str(net["precheck"].get("reason"))[:80]) for i in ITEMS}
    codes = proxy.get("codes") if isinstance(proxy, dict) else None
    upstream_ok = _n(proxy, "upstream_ok")
    out_t, out_i = _n(outer_net, "target"), _n(outer_net, "internal")
    if not isinstance(codes, dict) or upstream_ok is None or out_t is None or out_i is None:
        return {i: _unobs("outer counters missing") for i in ITEMS}

    def code(name: str) -> int:
        v = codes.get(name, 0)
        return v if isinstance(v, int) and not isinstance(v, bool) else 0

    res: dict[str, dict[str, str]] = {}
    # 1. lo
    lo = net.get("lo")
    res["lo"] = (_v(PASS, "bind, listen and round-trip on 127.0.0.1 inside --unshare-net", "in-netns echo")
                 if _ok(lo) else _v(FAIL, f"loopback unusable in netns (errno={_errname(lo)})", "in-netns echo"))
    # 2. uds
    uds = net.get("uds") if isinstance(net.get("uds"), dict) else {}
    failed = [k for k in ("rw", "ro") if not _ok(uds.get(k))]
    if failed:
        res["uds"] = _v(FAIL, "connect failed for: " + ",".join(failed)
                        + " (" + ",".join(f"{k}={_errname(uds.get(k))}" for k in failed) + ")",
                        "AF_UNIX connect to --bind (rw) and --ro-bind (ro) paths")
    elif code("closed_early") < 2:
        res["uds"] = _unobs("connect succeeded but the proxy did not count both connections",
                            "AF_UNIX connect to --bind (rw) and --ro-bind (ro) paths")
    else:
        res["uds"] = _v(PASS, "rw and ro mounts both reachable; proxy counted both", "AF_UNIX connect")
    # 3. relay
    rel = net.get("relay") if isinstance(net.get("relay"), dict) else {}
    if rel.get("status") != 200 or rel.get("echo_ok") is not True:
        res["relay"] = _v(FAIL, f"status={rel.get('status')} echo_ok={rel.get('echo_ok')} errno={rel.get('errno')}",
                          "CONNECT via relay + nonce round-trip")
    elif out_t < 1 or upstream_ok < 1 or code("allowed") < 1:
        res["relay"] = _unobs("200 and echo ok but outer receipts or proxy counters are missing",
                              "CONNECT via relay + nonce round-trip")
    else:
        res["relay"] = _v(PASS, f"200, nonce echoed, outer receipts={out_t}, proxy allowed={code('allowed')}",
                          "CONNECT via relay + nonce round-trip")
    # 4. direct
    base_ok = (base_rc == 0 and isinstance(base, dict) and _ok(_get(base, "direct", "target"))
               and (_n(outer_base, "target") or 0) >= 1)
    direct = net.get("direct") if isinstance(net.get("direct"), dict) else {}
    leaked = out_t - upstream_ok  # プロキシ経由でない外側の受信
    if not base_ok:
        res["direct"] = _unobs("baseline did not reach the outer listener (no positive control)",
                               "direct connect inside netns vs baseline")
    elif _ok(direct.get("target")) or leaked > 0:
        res["direct"] = _v(FAIL, f"direct connect reached outside (connect_ok={_ok(direct.get('target'))}, "
                           f"unproxied receipts={leaked})", "direct connect inside netns vs baseline")
    elif not isinstance(direct.get("target"), dict) or direct["target"].get("ok") is not False or leaked != 0:
        # 試行結果が欠落・不整合（ok が明示的な False でない、受信差が負など）のときは pass を導かない
        res["direct"] = _unobs("direct connect result missing or inconsistent (need ok=false and unproxied receipts=0)",
                               "direct connect inside netns vs baseline")
    else:
        res["direct"] = _v(PASS, f"direct connect failed ({_errname(direct.get('target'))}); unproxied receipts=0",
                           "direct connect inside netns vs baseline")
    # 5. ip_recheck
    rc_ = net.get("ip_recheck") if isinstance(net.get("ip_recheck"), dict) else {}
    base_int = (base_rc == 0 and isinstance(base, dict) and _ok(_get(base, "direct", "internal"))
                and (_n(outer_base, "internal") or 0) >= 1)
    m5 = "CONNECT to internal IP via relay"
    if rc_.get("status") == 200 or out_i > 0:
        res["ip_recheck"] = _v(FAIL, f"internal address reached (status={rc_.get('status')}, outer receipts={out_i})", m5)
    elif not base_int:
        res["ip_recheck"] = _unobs("baseline did not reach the internal listener (no positive control)", m5)
    elif rc_.get("status") == 403 and code("ip_denied") >= 1 and code("not_allowlisted") == 0:
        res["ip_recheck"] = _v(PASS, "403 with reason ip_denied; internal listener received 0", m5)
    else:
        res["ip_recheck"] = _unobs(f"rejected for another reason (status={rc_.get('status')}, "
                                   f"ip_denied={code('ip_denied')}, not_allowlisted={code('not_allowlisted')})", m5)
    return res


def _get(d: Any, *keys: str) -> Any:
    for k in keys:
        if not isinstance(d, dict):
            return None
        d = d.get(k)
    return d


def _errname(d: Any) -> str:
    return str(d.get("errno")) if isinstance(d, dict) and d.get("errno") else "none"


# ---------------------------------------------------------------------------
# 外側: echo listener・CONNECT プロキシ
# ---------------------------------------------------------------------------

def pump(a: socket.socket, b: socket.socket, limit: int = MAX_RELAY_BYTES, idle: float = IDLE_S) -> None:
    """a と b の間でバイトを双方向にコピーする（総量・アイドル時間に上限。どちらかが閉じたら終了）。"""
    total = 0
    try:
        while True:
            ready, _, _ = select.select([a, b], [], [], idle)
            if not ready:
                return
            for s in ready:
                data = s.recv(4096)
                if not data:
                    return
                total += len(data)
                if total > limit:
                    return
                (b if s is a else a).sendall(data)
    except OSError:
        return


class EchoListener:
    """受けたバイトをそのまま返す loopback listener。件数とバイト数だけ記録する（本文は保持しない）。"""

    def __init__(self, ip: str) -> None:
        listeners.validate_bind(ip, 0)
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.sock.bind((ip, 0))
        self.sock.listen(16)
        self.sock.settimeout(0.2)
        self.port = self.sock.getsockname()[1]
        self.connections = 0
        self.bytes = 0
        self._lock = threading.Lock()
        self._sem = threading.BoundedSemaphore(listeners.MAX_CONCURRENT)
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._loop, daemon=True)

    def start(self) -> None:
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()
        self._thread.join(2)
        self.sock.close()

    def _loop(self) -> None:
        while not self._stop.is_set():
            try:
                conn, _ = self.sock.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            with self._lock:
                self.connections += 1
            if not self._sem.acquire(blocking=False):
                conn.close()
                continue
            threading.Thread(target=self._serve, args=(conn,), daemon=True).start()

    def _serve(self, conn: socket.socket) -> None:
        try:
            conn.settimeout(CONN_TIMEOUT_S)
            seen = 0
            while seen < MAX_ECHO_BYTES:
                data = conn.recv(1024)
                if not data:
                    break
                seen += len(data)
                with self._lock:
                    self.bytes += len(data)
                conn.sendall(data)
        except OSError:
            pass
        finally:
            conn.close()
            self._sem.release()


class ConnectProxy:
    """UDS で待ち受けるテスト用の最小 CONNECT プロキシ（本番のプロキシではない）。"""

    def __init__(self, sock_path: str, allow: frozenset[tuple[str, int]]) -> None:
        self.allow = allow
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.bind(sock_path)
        os.chmod(sock_path, 0o600)
        self.sock.listen(16)
        self.sock.settimeout(0.2)
        self.codes: dict[str, int] = {}
        self.upstream_ok = 0
        self._lock = threading.Lock()
        self._sem = threading.BoundedSemaphore(listeners.MAX_CONCURRENT)
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._loop, daemon=True)

    def start(self) -> None:
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()
        self._thread.join(2)
        self.sock.close()

    def snapshot(self) -> dict[str, Any]:
        with self._lock:
            return {"codes": dict(self.codes), "upstream_ok": self.upstream_ok}

    def _count(self, name: str) -> None:
        with self._lock:
            self.codes[name] = self.codes.get(name, 0) + 1

    def _loop(self) -> None:
        while not self._stop.is_set():
            try:
                conn, _ = self.sock.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            if not self._sem.acquire(blocking=False):
                self._count("busy")
                conn.close()
                continue
            threading.Thread(target=self._handle, args=(conn,), daemon=True).start()

    @staticmethod
    def _reply(conn: socket.socket, status: int) -> None:
        conn.sendall(f"HTTP/1.1 {status} {RESPONSE_REASONS[status]}\r\n\r\n".encode("ascii"))

    def _handle(self, conn: socket.socket) -> None:
        try:
            conn.settimeout(CONN_TIMEOUT_S)
            buf = b""
            while b"\r\n\r\n" not in buf and len(buf) < MAX_HEADER:
                chunk = conn.recv(1024)
                if not chunk:
                    break
                buf += chunk
            if not buf:
                self._count("closed_early")
                return
            status, ip, port = parse_connect_request(buf)
            if status != 200 or ip is None or port is None:
                self._count("bad_request" if status == 400 else "method_not_allowed")
                self._reply(conn, status)
                return
            verdict = evaluate_policy(ip, port, self.allow)
            self._count(verdict)
            if verdict != "allowed":
                self._reply(conn, 403)
                return
            try:
                up = socket.create_connection((ip, port), timeout=CONN_TIMEOUT_S)
            except OSError:
                self._count("upstream_error")
                self._reply(conn, 502)
                return
            with up:
                with self._lock:
                    self.upstream_ok += 1
                self._reply(conn, 200)
                pump(conn, up)
        except OSError:
            pass
        finally:
            conn.close()
            self._sem.release()


# ---------------------------------------------------------------------------
# 内側（bwrap の中）
# ---------------------------------------------------------------------------

def _errno_of(exc: OSError) -> str:
    if isinstance(exc, socket.timeout):
        return "ETIMEDOUT"
    return errno.errorcode.get(exc.errno or 0, "oserror")


def _tcp_echo(addr: tuple[str, int]) -> dict[str, Any]:
    """addr へ直接 connect し、4 バイトの往復まで確かめる。結果は真偽と errno 名だけ。"""
    try:
        with socket.create_connection(addr, timeout=CONN_TIMEOUT_S) as s:
            s.settimeout(CONN_TIMEOUT_S)
            s.sendall(b"ping")
            return {"ok": s.recv(4) == b"ping", "errno": None}
    except OSError as exc:
        return {"ok": False, "errno": _errno_of(exc)}


def probe_lo() -> dict[str, Any]:
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        srv.bind(("127.0.0.1", 0))
        srv.listen(1)
        srv.settimeout(CONN_TIMEOUT_S)
        port = srv.getsockname()[1]

        def serve() -> None:
            try:
                c, _ = srv.accept()
                with c:
                    c.settimeout(CONN_TIMEOUT_S)
                    c.sendall(c.recv(16))
            except OSError:
                pass

        t = threading.Thread(target=serve, daemon=True)
        t.start()
        res = _tcp_echo(("127.0.0.1", port))
        t.join(CONN_TIMEOUT_S)
        return res
    except OSError as exc:
        return {"ok": False, "errno": _errno_of(exc)}
    finally:
        srv.close()


def probe_uds(path: str) -> dict[str, Any]:
    """UDS へ connect して、要求を送らずに閉じる（プロキシ側は closed_early と数える）。"""
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        s.settimeout(CONN_TIMEOUT_S)
        s.connect(path)
        return {"ok": True, "errno": None}
    except OSError as exc:
        return {"ok": False, "errno": _errno_of(exc)}
    finally:
        s.close()


class Relay:
    """netns 内の 127.0.0.1:R で待ち受け、受けた接続を UDS（rw マウント）へ中継する。"""

    def __init__(self, uds_path: str) -> None:
        self.uds_path = uds_path
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.sock.bind(("127.0.0.1", 0))
        self.sock.listen(16)
        self.sock.settimeout(0.2)
        self.port = self.sock.getsockname()[1]
        self._stop = threading.Event()
        self._sem = threading.BoundedSemaphore(listeners.MAX_CONCURRENT)
        threading.Thread(target=self._loop, daemon=True).start()

    def stop(self) -> None:
        self._stop.set()
        self.sock.close()

    def _loop(self) -> None:
        while not self._stop.is_set():
            try:
                conn, _ = self.sock.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            if not self._sem.acquire(blocking=False):
                conn.close()
                continue
            threading.Thread(target=self._serve, args=(conn,), daemon=True).start()

    def _serve(self, conn: socket.socket) -> None:
        up = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            up.settimeout(CONN_TIMEOUT_S)
            up.connect(self.uds_path)
            pump(conn, up)
        except OSError:
            pass
        finally:
            up.close()
            conn.close()
            self._sem.release()


def via_relay(relay_port: int, authority: str, nonce: bytes | None = None) -> dict[str, Any]:
    """中継へ CONNECT を送る。ステータスと（nonce を送ったときは）往復の一致だけを返す。"""
    out: dict[str, Any] = {"status": None, "echo_ok": False, "bytes": 0, "errno": None}
    try:
        with socket.create_connection(("127.0.0.1", relay_port), timeout=CONN_TIMEOUT_S) as s:
            s.settimeout(CONN_TIMEOUT_S)
            s.sendall(f"CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n".encode("ascii"))
            head = b""
            while b"\r\n\r\n" not in head and len(head) < 1024:
                chunk = s.recv(256)
                if not chunk:
                    break
                head += chunk
            parts = head.split(b" ", 2)
            out["status"] = int(parts[1]) if len(parts) >= 2 and parts[1].isdigit() else None
            if nonce is not None and out["status"] == 200:
                s.sendall(nonce)
                got = b""
                while len(got) < len(nonce):
                    chunk = s.recv(len(nonce) - len(got))
                    if not chunk:
                        break
                    got += chunk
                out["bytes"] = len(got)
                out["echo_ok"] = got == nonce
    except OSError as exc:
        out["errno"] = _errno_of(exc)
    return out


def run_inner(phase: str, ports: dict[str, int], out_path: Path) -> int:
    """bwrap の中で動く本体。phase=baseline は直接 connect だけ、phase=netns は 5 項目すべてを試す。"""
    target, internal = (IP_TARGET, ports["target"]), (IP_INTERNAL, ports["internal"])
    result: dict[str, Any] = {"phase": phase}
    if phase == "netns":
        def read(p: str) -> str | None:
            try:
                return Path(p).read_text(encoding="ascii", errors="replace")
            except OSError:
                return None
        ok, why = judge.check_isolation(read("/proc/net/dev"), read("/proc/net/route"), read("/proc/net/ipv6_route"))
        result["precheck"] = {"ok": ok, "reason": why}
        if not ok:
            out_path.write_text(json.dumps(result), encoding="utf-8")
            return 2
        result["lo"] = probe_lo()
        result["uds"] = {"rw": probe_uds(f"{MOUNT_RW}/{SOCK_NAME}"), "ro": probe_uds(f"{MOUNT_RO}/{SOCK_NAME}")}
    # direct は中継より前に行う（外側の受信を中継経由の分と分けて数えるため）。
    result["direct"] = {"target": _tcp_echo(target), "internal": _tcp_echo(internal)}
    if phase == "netns":
        relay = Relay(f"{MOUNT_RW}/{SOCK_NAME}")
        try:
            # 中継（netns 内）と外側リスナーは別 netns のため、ポート番号の一致は衝突ではない。
            result["relay"] = via_relay(relay.port, f"{IP_TARGET}:{ports['target']}", secrets.token_hex(8).encode())
            r = via_relay(relay.port, f"{IP_INTERNAL}:{ports['internal']}")
            result["ip_recheck"] = {"status": r["status"], "errno": r["errno"]}
        finally:
            relay.stop()
    out_path.write_text(json.dumps(result, sort_keys=True), encoding="utf-8")
    return 0


# ---------------------------------------------------------------------------
# 外側: 実行・記録
# ---------------------------------------------------------------------------

def _read_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None


def _diff(after: dict[str, Any], before: dict[str, Any]) -> dict[str, Any]:
    codes = {k: v - before["codes"].get(k, 0) for k, v in after["codes"].items() if v - before["codes"].get(k, 0)}
    return {"codes": codes, "upstream_ok": after["upstream_ok"] - before["upstream_ok"]}


def environment() -> dict[str, Any]:
    return {"kernel": platform.release(), "machine": platform.machine(),
            "bwrap": platform_probe.tool_versions().get("bwrap", ""), "sysctl": platform_probe.read_sysctls()}


def render_summary(result: dict[str, Any]) -> str:
    env = result.get("environment", {})
    lines = ["# proxy-relay (Issue #1431)", "",
             f"- kernel: {env.get('kernel')}", f"- bwrap: {env.get('bwrap')}",
             f"- apparmor_restrict_unprivileged_userns: {env.get('sysctl', {}).get('kernel.apparmor_restrict_unprivileged_userns')}",
             "", "| item | verdict | reason |", "| ---- | ------- | ------ |"]
    for i in ITEMS:
        v = result["verdicts"][i]
        lines.append(f"| {i} | {v['verdict']} | {v['reason']} |")
    return "\n".join(lines) + "\n"


def write_result(out: Path, result: dict[str, Any]) -> None:
    (out / "result.json").write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    (out / "summary.md").write_text(render_summary(result), encoding="utf-8")


def run_outer(out: Path) -> int:
    result: dict[str, Any] = {"mode": MODE, "environment": environment()}
    if not sys.platform.startswith("linux") or shutil.which("bwrap") is None:
        why = "linux only" if not sys.platform.startswith("linux") else "bwrap not found"
        result["verdicts"] = {i: _unobs(why) for i in ITEMS}
        write_result(out, result)
        print(f"relay_probe: prerequisite not met ({why})", file=sys.stderr)
        return 2
    sockdir = tempfile.mkdtemp(prefix="fer-", dir="/tmp")
    os.chmod(sockdir, 0o700)
    echo_t, echo_i = EchoListener(IP_TARGET), EchoListener(IP_INTERNAL)
    proxy: ConnectProxy | None = None
    try:
        sock_path = os.path.join(sockdir, SOCK_NAME)
        if len(sock_path.encode()) > SUN_PATH_MAX:
            result["verdicts"] = {i: _unobs("UDS path too long for sun_path") for i in ITEMS}
            write_result(out, result)
            return 2
        proxy = ConnectProxy(sock_path, frozenset({(IP_TARGET, echo_t.port), (IP_INTERNAL, echo_i.port)}))
        for srv in (echo_t, echo_i, proxy):
            srv.start()
        ports = {"target": echo_t.port, "internal": echo_i.port}
        runs: dict[str, Any] = {}
        counts: dict[str, Any] = {}
        inner: dict[str, Any] = {}
        proxy_net: dict[str, Any] = {}
        for phase, netns in (("baseline", False), ("netns", True)):
            before = (echo_t.connections, echo_i.connections, proxy.snapshot())
            inner_out = out / f"{phase}.json"
            rc, err = measure.run_argv(build_relay_argv(netns=netns, sockdir=sockdir, inner_out=str(inner_out),
                                                       ports=ports, phase=phase))
            time.sleep(SETTLE_S)
            counts[phase] = {"target": echo_t.connections - before[0], "internal": echo_i.connections - before[1]}
            if phase == "netns":
                proxy_net = _diff(proxy.snapshot(), before[2])
            inner[phase] = _read_json(inner_out)
            runs[phase] = {"rc": rc, "stderr_tail": err[-200:], "inner": inner[phase]}
        result.update(runs=runs, outer_counts=counts, proxy_counts=proxy_net)
        result["verdicts"] = judge_relay(
            bwrap_found=True, base_rc=runs["baseline"]["rc"], net_rc=runs["netns"]["rc"], base=inner["baseline"],
            net=inner["netns"], outer_base=counts["baseline"], outer_net=counts["netns"], proxy=proxy_net)
        write_result(out, result)
        return 0
    finally:
        for srv in (echo_t, echo_i, proxy):
            if srv is not None:
                srv.stop()
        shutil.rmtree(sockdir, ignore_errors=True)


def valid_out_dir(raw: str) -> Path:
    p = Path(raw)
    if not p.is_absolute() or not p.is_dir() or any(p.iterdir()):
        raise ValueError("--out must be an absolute path to an existing empty directory")
    return p


def valid_ports(raw: str) -> dict[str, int]:
    d = json.loads(raw)
    if (not isinstance(d, dict) or set(d) != {"target", "internal"}
            or any(isinstance(v, bool) or not isinstance(v, int) or not 1024 <= v <= 65535 for v in d.values())):
        raise ValueError("--ports must be {target, internal} with ports in [1024, 65535]")
    return d


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--out", required=True)
    ap.add_argument("--inner", action="store_true", help="internal use: run inside bwrap")
    ap.add_argument("--phase", choices=("baseline", "netns"))
    ap.add_argument("--ports")
    args = ap.parse_args(argv)
    try:
        if args.inner:
            if args.phase is None or args.ports is None:
                raise ValueError("--inner needs --phase and --ports")
            return run_inner(args.phase, valid_ports(args.ports), Path(args.out))
        return run_outer(valid_out_dir(args.out))
    except (ValueError, KeyError) as exc:
        print(f"relay_probe: {exc}", file=sys.stderr)
        return 2
    except Exception as exc:  # noqa: BLE001  計測の失敗は exit 1 で区別する。
        print(f"relay_probe: measurement failed: {type(exc).__name__}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
