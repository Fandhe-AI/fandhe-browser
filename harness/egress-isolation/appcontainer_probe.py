#!/usr/bin/env python3
"""AppContainer の中で動く、Servo を使わない loopback 切り分けの preflight プローブ（純 Python・固定宛先）。

役割・責務境界:
    Issue #1396（TASK-38・`RENDER-3`・`SEC-2`）。AppContainer は既定で loopback に出られないため、外側 listener の
    受信 0 件が「egress 遮断」なのか「loopback 遮断」なのかを区別する対照を取る。`measure.py` が capability なし・
    ネットワーク capability ありの 2 通りで同じ容器から起動し、結果と外側の受信件数を `judge.appcontainer_confound` で比べる。

測るもの（結果 JSON。errno 名と真偽のみ。URL・本文は扱わない）:
    - `loopback_self`: 同一プロセス内で 127.0.0.1 に listen し、別スレッドから connect・accept できるか
      （容器内で許可オリジンを配信してホストに読ませられるかの代理）。
    - `tcp_outer` / `udp_outer`: 外側の 127.0.0.2 の preflight 専用 listener への connect / sendto の結果。
宛先は 127.0.0.0/8 のダミーだけ。NIC アドレス・実在アドレスには向けない。
"""

from __future__ import annotations

import argparse
import errno
import json
import socket
import threading
from pathlib import Path
from typing import Any

IP_OUTER = "127.0.0.2"
TIMEOUT_S = 2.0


def _errno_name(exc: OSError) -> str:
    return errno.errorcode.get(exc.errno or 0, "oserror")


def probe_loopback_self() -> dict[str, Any]:
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        srv.bind(("127.0.0.1", 0))
        srv.listen(1)
        srv.settimeout(TIMEOUT_S)
        port = srv.getsockname()[1]
        accepted: list[bool] = []

        def accept() -> None:
            try:
                conn, _ = srv.accept()
                conn.close()
                accepted.append(True)
            except OSError:
                pass

        t = threading.Thread(target=accept, daemon=True)
        t.start()
        cli = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        cli.settimeout(TIMEOUT_S)
        try:
            cli.connect(("127.0.0.1", port))
        finally:
            cli.close()
        t.join(TIMEOUT_S)
        return {"ok": bool(accepted), "errno": None}
    except OSError as exc:
        return {"ok": False, "errno": _errno_name(exc)}
    finally:
        srv.close()


def probe_tcp(port: int) -> str:
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.settimeout(TIMEOUT_S)
    try:
        sock.connect((IP_OUTER, port))
        return "connected"
    except socket.timeout:
        return "timeout"
    except OSError as exc:
        return _errno_name(exc)
    finally:
        sock.close()


def probe_udp(port: int) -> str:
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        sock.sendto(b"\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\x00\x00\x01\x00\x01", (IP_OUTER, port))
        return "sent"
    except OSError as exc:
        return _errno_name(exc)
    finally:
        sock.close()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="AppContainer loopback preflight probe.")
    parser.add_argument("--tcp-port", required=True, type=int)
    parser.add_argument("--udp-port", required=True, type=int)
    parser.add_argument("--out", required=True, type=Path)
    args = parser.parse_args(argv)
    for p in (args.tcp_port, args.udp_port):
        if not (1024 <= p <= 65535):
            print("error: ports must be in [1024, 65535]")
            return 2
    result: dict[str, Any] = {"loopback_self": probe_loopback_self()}
    result["tcp_outer"] = probe_tcp(args.tcp_port)
    result["udp_outer"] = probe_udp(args.udp_port)
    args.out.write_text(json.dumps(result, sort_keys=True) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
