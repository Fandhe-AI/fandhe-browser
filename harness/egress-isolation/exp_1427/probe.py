#!/usr/bin/env python3
"""AppContainer の中（または対照の通常プロセス）で動く、宛先を計画ファイルで受ける connect / sendto プローブ。

役割・責務境界:
    Issue #1427（TASK-38・`RENDER-3`・`SEC-2`）の予備実験。Servo は使わない。`run_probe.py` が書いた計画（宛先・ポート・
    プロトコルの一覧）を順に試し、結果の種類（成功・errno・WSA コード・タイムアウト）と経過時間 ms だけを JSON に残す。
    単一ファイルで自己完結させる（AppContainer の複製先には本ファイルしか置かないため、他モジュールを import しない）。

宛先の安全規則（オーナー承認 2026-11-05・#1427）:
    127.0.0.0/8、192.0.2.0/24（RFC 5737 TEST-NET-1）、引数で渡されたランナー自身の NIC の IPv4 だけ。
    範囲外は `validate_dest` が ValueError で止める。DNS 解決はしない（IP 直指定）。nonce・秘密情報は扱わない。
Python 標準ライブラリのみ。
"""

from __future__ import annotations

import argparse
import errno
import ipaddress
import json
import socket
import time
from pathlib import Path
from typing import Any

MIN_PORT = 1024
MAX_ATTEMPTS = 64
TCP_TIMEOUT_S = 3.0
PROTOCOLS = ("tcp", "udp")
_LOOPBACK = ipaddress.IPv4Network("127.0.0.0/8")
_TEST_NET_1 = ipaddress.IPv4Network("192.0.2.0/24")
# UDP の送信内容は固定のダミー（DNS ヘッダ風の 17 バイト。秘密情報を含まない）。
_UDP_PAYLOAD = b"\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\x00\x00\x01\x00\x01"


def parse_nic_ip(raw: str | None) -> ipaddress.IPv4Address | None:
    """NIC の IPv4 を検証する。None は「なし」。loopback・未指定・multicast・TEST-NET-1 は NIC として扱わず ValueError。"""
    if raw is None or raw == "":
        return None
    try:
        addr = ipaddress.IPv4Address(raw)
    except ValueError as exc:
        raise ValueError("nic ip must be an IPv4 address") from exc
    if addr in _LOOPBACK or addr in _TEST_NET_1 or addr.is_unspecified or addr.is_multicast or addr.is_reserved:
        raise ValueError("nic ip must be a unicast address outside 127.0.0.0/8 and 192.0.2.0/24")
    return addr


def validate_dest(ip: str, port: int, nic_ip: str | None = None) -> str:
    """宛先を検証して種別（`loopback` / `testnet` / `nic`）を返す。範囲外・不正なポートは ValueError。"""
    if isinstance(port, bool) or not isinstance(port, int) or not (MIN_PORT <= port <= 65535):
        raise ValueError("dest port out of range")
    try:
        addr = ipaddress.IPv4Address(ip)
    except (ValueError, TypeError) as exc:
        raise ValueError("dest must be an IPv4 literal") from exc
    if addr in _LOOPBACK:
        return "loopback"
    if addr in _TEST_NET_1:
        return "testnet"
    nic = parse_nic_ip(nic_ip)
    if nic is not None and addr == nic:
        return "nic"
    raise ValueError("dest is outside the approved ranges")


def _error_fields(exc: OSError) -> dict[str, Any]:
    return {"errno": errno.errorcode.get(exc.errno or 0, "oserror"),
            "errno_num": exc.errno, "winerror": getattr(exc, "winerror", None)}


def attempt(proto: str, ip: str, port: int, nic_ip: str | None) -> dict[str, Any]:
    """1 回の試行。結果の種類（kind）と経過 ms を返す。宛先は先に検証する。"""
    dest_kind = validate_dest(ip, port, nic_ip)
    rec: dict[str, Any] = {"proto": proto, "dest_kind": dest_kind, "port": port}
    t0 = time.monotonic()
    if proto == "tcp":
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.settimeout(TCP_TIMEOUT_S)
        try:
            sock.connect((ip, port))
            rec["kind"] = "connected"
        except socket.timeout:
            rec["kind"] = "timeout"
        except OSError as exc:
            rec["kind"] = "error"
            rec.update(_error_fields(exc))
        finally:
            sock.close()
    elif proto == "udp":
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        try:
            n = sock.sendto(_UDP_PAYLOAD, (ip, port))
            rec["kind"] = "sent"
            rec["bytes"] = n
        except OSError as exc:
            rec["kind"] = "error"
            rec.update(_error_fields(exc))
        finally:
            sock.close()
    else:
        raise ValueError("unknown protocol")
    rec["elapsed_ms"] = int((time.monotonic() - t0) * 1000)
    return rec


def run_plan(plan: dict[str, Any]) -> dict[str, Any]:
    """計画（`nic_ip` と `attempts`）を検証してから順に実行する。1 件でも範囲外なら 1 件も試さず ValueError。"""
    nic_ip = plan.get("nic_ip")
    items = plan.get("attempts")
    if not isinstance(items, list) or not (0 < len(items) <= MAX_ATTEMPTS):
        raise ValueError("attempts must be a non-empty list within the cap")
    for it in items:
        if not isinstance(it, dict) or it.get("proto") not in PROTOCOLS:
            raise ValueError("invalid attempt entry")
        validate_dest(it.get("ip"), it.get("port"), nic_ip)  # type: ignore[arg-type]
    results = []
    for it in items:
        res = attempt(it["proto"], it["ip"], it["port"], nic_ip)
        res["id"] = str(it.get("id", ""))[:64]
        results.append(res)
    return {"results": results}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="#1427 egress probe (approved destinations only).")
    parser.add_argument("--plan", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    args = parser.parse_args(argv)
    try:
        plan = json.loads(args.plan.read_text(encoding="utf-8"))
        out = run_plan(plan)
    except (OSError, ValueError) as exc:
        args.out.write_text(json.dumps({"fatal": f"{type(exc).__name__}: {str(exc)[:120]}"}) + "\n", encoding="utf-8")
        return 2
    args.out.write_text(json.dumps(out, sort_keys=True) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
