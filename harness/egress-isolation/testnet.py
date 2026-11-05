#!/usr/bin/env python3
"""TEST-NET-1 宛ての経路の定数・宛先検証・ポート帯（純粋関数。Windows 専用の実測で使う）。

役割・責務境界:
    Issue #1515（TASK-38・`RENDER-3`・`XOS-1`・`SEC-2`・予備実験 #1427）。Windows の AppContainer は loopback 免除なしでは
    ページを配信できず、免除すると 127.0.0.2 の外側 listener にも capability なしで届くため、loopback 宛てでは egress の
    遮断を測れない。そこで宛先を TEST-NET-1（192.0.2.0/24。RFC 5737。実在のホストに割り当てられない文書用アドレス）にし、
    到達の有無をランナー側の観測（pktmon・WFP net events。`testnet_observe.py`）で判定する。
    ここは「どこへ・どのポートで」試すかの SSOT で、I/O と win32 の import を持たない。

宛先の規則（`SEC-2` の趣旨どおり、実在のサイト・内部ネットワークには向けない）:
    - 許可するのは 127.0.0.0/8 と 192.0.2.0/24 の IPv4 リテラルだけ。名前解決はしない。
    - ランナー自身の NIC の IP・RFC 1918・リンクローカル・他の文書用帯（198.51.100.0/24・203.0.113.0/24）は宛先にしない。
    - listener の bind 規則（`listeners.validate_bind`。127/8 だけ）はここでは変えない。宛先検証は `validate_egress_dest`。

呼び出し文脈: `page_templates.py`（ページ本文の宛先検査）・`in_sandbox.py`（UDP プローブ前の検査）・`measure.py`（モードごとの
ポート帯の割り当て）・`testnet_parse.py`（観測したパケットの帰属）・`judge.py`（経路名）。
"""

from __future__ import annotations

import ipaddress

TESTNET_IP = "192.0.2.1"
TESTNET_NET = ipaddress.IPv4Network("192.0.2.0/24")
LOOPBACK_NET = ipaddress.IPv4Network("127.0.0.0/8")
MIN_PORT = 1024

# TCP はホストが読み込むページの 4 経路、UDP は内側ランナー自身の sendto（`udp_direct` と同じくハーネスのプローブ）。
TESTNET_TCP_ROUTES = ("testnet_redirect", "testnet_img", "testnet_fetch", "testnet_ws")
TESTNET_UDP_ROUTE = "testnet_udp"
TESTNET_ROUTES = (*TESTNET_TCP_ROUTES, TESTNET_UDP_ROUTE)

# モードごとの固定のポート帯。TEST-NET 宛てには listener が無くポートを OS から得られないため、ポートだけで
# 経路とモードに帰属させる。経路の順は TESTNET_ROUTES の順。baseline: +0..+4、appcontainer: +16..+20。
TESTNET_PORT_BASE = 47300
TESTNET_MODE_OFFSET = {"baseline": 0, "appcontainer": 16}
TESTNET_MODES = tuple(TESTNET_MODE_OFFSET)


def validate_egress_dest(ip: str, port: int) -> None:
    """試験の宛先を検証する。127.0.0.0/8 か 192.0.2.0/24 の IPv4 リテラルで、ポートが 1024 以上でなければ ValueError。"""
    if not isinstance(ip, str):
        raise ValueError(f"destination ip must be an IPv4 literal: {ip!r}")
    try:
        addr = ipaddress.IPv4Address(ip)
    except ValueError as exc:
        raise ValueError(f"destination ip must be an IPv4 literal: {ip!r}") from exc
    if addr not in LOOPBACK_NET and addr not in TESTNET_NET:
        raise ValueError(f"destination ip must be in 127.0.0.0/8 or 192.0.2.0/24: {ip}")
    if isinstance(port, bool) or not isinstance(port, int) or not (MIN_PORT <= port <= 65535):
        raise ValueError(f"destination port must be an int in [{MIN_PORT}, 65535]: {port!r}")


def in_testnet(ip: str) -> bool:
    """IPv4 リテラルが TEST-NET-1 に入るか。解釈できなければ False。"""
    try:
        return ipaddress.IPv4Address(ip) in TESTNET_NET
    except ValueError:
        return False


def route_key(mode: str, route: str) -> str:
    """観測結果のキー（`<mode>/<route>`）。"""
    return f"{mode}/{route}"


def route_proto(route: str) -> str:
    return "udp" if route == TESTNET_UDP_ROUTE else "tcp"


def testnet_ports(mode: str) -> dict[str, int]:
    """モードに割り当てた TEST-NET 宛てのポート（経路名 -> int）。未知のモードは ValueError。"""
    offset = TESTNET_MODE_OFFSET.get(mode)
    if offset is None:
        raise ValueError(f"mode has no TEST-NET port range: {mode!r}")
    return {route: TESTNET_PORT_BASE + offset + i for i, route in enumerate(TESTNET_ROUTES)}


def port_index() -> dict[tuple[str, int], tuple[str, str]]:
    """(proto, 宛先ポート) -> (mode, route)。どちらのモード・経路にも当たらない宛先は呼び出し側で unattributed にする。"""
    index: dict[tuple[str, int], tuple[str, str]] = {}
    for mode in TESTNET_MODES:
        for route, port in testnet_ports(mode).items():
            index[(route_proto(route), port)] = (mode, route)
    return index
