#!/usr/bin/env python3
"""egress 隔離の実測で配るフィクスチャ（固定テンプレート）。

役割・責務境界:
    Issue #1377。経路ごとの HTML を `string.Template` で作る。置換するのは bind で得た int のポートだけ
    （外部入力をテンプレートへ連結しない。インジェクション対策）。宛先名は `judge.py` の定数。

呼び出し文脈: `in_sandbox.py` の配信サーバーが `build_pages()` の結果を返す。
"""

from __future__ import annotations

import re
import string
from typing import Any

import judge
import testnet

_PAGES: dict[str, str] = {
    "/ok": "<!doctype html><html><body><p>ok</p></body></html>",
    "/img": f'<!doctype html><html><body><img src="http://{judge.HOST_BLOCKED}:$p_img/img"></body></html>',
    "/fetch": (
        "<!doctype html><html><body><script>"
        f"fetch('http://{judge.HOST_BLOCKED}:$p_fetch/fetch').catch(function(){{}});"
        "</script></body></html>"
    ),
    "/ws": (
        "<!doctype html><html><body><script>"
        f"try{{var w=new WebSocket('ws://{judge.HOST_BLOCKED}:$p_ws/ws');w.onerror=function(){{}};}}catch(e){{}}"
        "</script></body></html>"
    ),
    "/internal": (
        "<!doctype html><html><body><script>"
        f"fetch('http://{judge.HOST_INTERNAL_STANDIN}:$p_internal/internal').catch(function(){{}});"
        "</script></body></html>"
    ),
    "/internal_real": (
        "<!doctype html><html><body><script>"
        f"fetch('http://{judge.REAL_INTERNAL_V4[0]}:81/').catch(function(){{}});"
        f"fetch('http://{judge.REAL_INTERNAL_V4[1]}:80/').catch(function(){{}});"
        f"fetch('http://[{judge.REAL_INTERNAL_V6}]:80/').catch(function(){{}});"
        "</script></body></html>"
    ),
    "/dns": f'<!doctype html><html><body><img src="http://{judge.HOST_NXDOMAIN}/x"></body></html>',
}
# 失敗経路のビーコン（macOS・Windows のページだけ。Issue #1395）。許可オリジンへの同一オリジン相対 GET で、
# 許可外宛先への要求が失敗したコールバック（onerror / catch）の中でだけ送る。ブラウザが要求を発行して失敗した
# 後にしか走らないが、ブラウザ内部の要求拒否でも発火するため接続試行の根拠にはならない（補助情報のみ。
# 試行の根拠は UDP の errno。判定には使わない）。
# redirect にはビーコンを付けない（補足用の fetch は同じ宛先へ接続し拒否ログ・件数に混ざるため置かない）。
BEACON_ROUTES = ("img", "fetch", "ws", "internal")
BEACON_PREFIX = "/beacon/"


def _beacon(route: str) -> str:
    return f"new Image().src='{BEACON_PREFIX}{route}';"


_BEACON_PAGES: dict[str, str] = {
    "/img": (f'<!doctype html><html><body><img src="http://{judge.HOST_BLOCKED}:$p_img/img" '
             f'onerror="{_beacon("img")}"></body></html>'),
    "/fetch": (
        "<!doctype html><html><body><script>"
        f"fetch('http://{judge.HOST_BLOCKED}:$p_fetch/fetch').catch(function(){{{_beacon('fetch')}}});"
        "</script></body></html>"
    ),
    "/ws": (
        "<!doctype html><html><body><script>"
        f"try{{var w=new WebSocket('ws://{judge.HOST_BLOCKED}:$p_ws/ws');"
        f"w.onerror=function(){{{_beacon('ws')}}};}}catch(e){{}}"
        "</script></body></html>"
    ),
    "/internal": (
        "<!doctype html><html><body><script>"
        f"fetch('http://{judge.HOST_INTERNAL_STANDIN}:$p_internal/internal')"
        f".catch(function(){{{_beacon('internal')}}});"
        "</script></body></html>"
    ),
}

# リダイレクト（許可外ホストへの 302）。Location はポートだけを埋めた固定形。
REDIRECT_PATH = "/redirect-out"
_REDIRECT_LOCATION = f"http://{judge.HOST_BLOCKED}:$p_redirect/redirected"

# 診断用の経路（macOS だけ。Issue #1415）。redirect の拒否行が取れない原因（順序・トップレベルナビゲーション・
# 302 の追従）を切り分けるための補助で、判定（`judge.routes_for_mode` 等）には使わない。
#   diag_redirect_late: 判定経路の末尾でもう一度 302 を試す（先頭であることによる間引きの確認）。
#   diag_nav_direct: 302 を経ずに許可外宛先をトップレベルで直接 load する（302 の追従の確認）。
DIAG_ROUTES = ("diag_redirect_late", "diag_nav_direct")
REDIRECT_LATE_PATH = "/redirect-late"
DIAG_NAV_DIRECT_PATH = "/direct"
_DIAG_REDIRECT_LOCATION = f"http://{judge.HOST_BLOCKED}:$p_diag_redirect_late/redirected"

# 予熱用の経路（macOS だけ。Issue #1415）。判定経路の前に K 回（既定 5）「遮断ダミー宛先への 302」を読み込む。
# 遮断されるトップレベルナビゲーションの先頭 1〜3 件で拒否行が 0 件になる事象を、判定しない経路に吸わせるとともに、
# 拒否行が出始めるのが何件目・何 ms 後かを回ごとに比べるためのもの。回ごとに専用ポート（`p_warmup_redirect_<i>`）を使い、
# 拒否行をどの回のものか帰属できるようにする。判定（`judge.routes_for_mode` 等）・attempts・外側受信の集計には入れない。
MAX_WARMUP_COUNT = 10
WARMUP_ROUTE_PREFIX = "warmup_redirect"


def _warmup_index(index: Any) -> int:
    if isinstance(index, bool) or not isinstance(index, int) or not (1 <= index <= MAX_WARMUP_COUNT):
        raise ValueError(f"warm-up index must be an int in [1, {MAX_WARMUP_COUNT}]")
    return index


def warmup_routes(count: Any) -> tuple[str, ...]:
    """予熱経路名 `warmup_redirect_1`..`warmup_redirect_<count>`。count は 0..MAX_WARMUP_COUNT の int だけ。"""
    if isinstance(count, bool) or not isinstance(count, int) or not (0 <= count <= MAX_WARMUP_COUNT):
        raise ValueError(f"warm-up count must be an int in [0, {MAX_WARMUP_COUNT}]")
    return tuple(f"{WARMUP_ROUTE_PREFIX}_{i}" for i in range(1, count + 1))


def warmup_redirect_path(index: int) -> str:
    """予熱用 302 のパス `/redirect-warmup-<index>`。"""
    return f"/redirect-warmup-{_warmup_index(index)}"


# 経路 -> ページのパス
ROUTE_PATH = {
    "redirect": REDIRECT_PATH,
    "img": "/img",
    "fetch": "/fetch",
    "ws": "/ws",
    "internal": "/internal",
    "internal_real": "/internal_real",
    "dns": "/dns",
}


def _subst(template: str, ports: dict[str, Any]) -> str:
    mapping: dict[str, str] = {}
    for key, value in ports.items():
        if isinstance(value, bool) or not isinstance(value, int) or not (1024 <= value <= 65535):
            raise ValueError(f"port {key!r} must be an int in [1024, 65535]")
        mapping[f"p_{key}"] = str(value)
    return string.Template(template).substitute(mapping)


def _literal(text: str) -> str:
    """ホスト名を 127.0.0.0/8 の IP リテラルへ置き換える（macOS・Windows 用。#1387）。

    システムの hosts を変えられない（root / admin が要る）OS では名前解決に頼れないため、
    宛先名は `judge.py` の定数どおり固定のダミー IP に対応づける。
    """
    for name, ip in ((judge.HOST_BLOCKED, judge.IP_BLOCKED),
                     (judge.HOST_INTERNAL_STANDIN, judge.IP_INTERNAL_STANDIN)):
        text = text.replace(name, ip)
    return text


def build_pages(ports: dict[str, Any], *, literal: bool = False) -> dict[str, str]:
    """パス -> HTML。`ports` に不足があれば KeyError、int 以外は ValueError。

    `literal=True` はダミー宛先を IP リテラルにし、失敗経路のビーコン付きページへ差し替える（DNS 経路の `/dns` は
    NXDOMAIN 名のまま。Issue #1395）。`literal=False`（Linux）の出力は従来から変えない。
    """
    pages = {**_PAGES, **_BEACON_PAGES} if literal else _PAGES
    return {path: _subst(_literal(body) if literal else body, ports) for path, body in pages.items()}


def redirect_location(ports: dict[str, Any], *, literal: bool = False) -> str:
    return _subst(_literal(_REDIRECT_LOCATION) if literal else _REDIRECT_LOCATION, ports)


def diag_redirect_location(ports: dict[str, Any], *, literal: bool = False) -> str:
    """診断用 302（`/redirect-late`）の Location。`redirect_location` と同じく int のポートだけを置換する。"""
    return _subst(_literal(_DIAG_REDIRECT_LOCATION) if literal else _DIAG_REDIRECT_LOCATION, ports)


def warmup_redirect_location(ports: dict[str, Any], index: int, *, literal: bool = False) -> str:
    """予熱用 302 の Location。index 回目の専用ポート（int）だけを置換する（Issue #1415）。"""
    template = f"http://{judge.HOST_BLOCKED}:$p_{WARMUP_ROUTE_PREFIX}_{_warmup_index(index)}/redirected"
    return _subst(_literal(template) if literal else template, ports)


# --- TEST-NET-1 宛ての経路（Windows の AppContainer 実測だけ。Issue #1515） ---------------------------------------
# 宛先は IP リテラルの TEST-NET-1（192.0.2.1）で、ポートは経路ごとに一意（`testnet.testnet_ports`）。失敗ビーコンは付けない
# （試行の根拠はランナー側の WFP drop。ブラウザ内部の拒否でも発火するビーコンは根拠にならない）。
# `build_pages` の既定の出力は変えず、このフラグのあるときだけ `in_sandbox.py` が `build_testnet_pages` を配信に足す。
TESTNET_REDIRECT_PATH = "/testnet-redirect-out"
TESTNET_ROUTE_PATH = {
    "testnet_redirect": TESTNET_REDIRECT_PATH,
    "testnet_img": "/testnet_img",
    "testnet_fetch": "/testnet_fetch",
    "testnet_ws": "/testnet_ws",
}
_TESTNET_PAGES: dict[str, str] = {
    "/testnet_img": f'<!doctype html><html><body><img src="http://{testnet.TESTNET_IP}:$p_testnet_img/img"></body></html>',
    "/testnet_fetch": (
        "<!doctype html><html><body><script>"
        f"fetch('http://{testnet.TESTNET_IP}:$p_testnet_fetch/fetch').catch(function(){{}});"
        "</script></body></html>"
    ),
    "/testnet_ws": (
        "<!doctype html><html><body><script>"
        f"try{{var w=new WebSocket('ws://{testnet.TESTNET_IP}:$p_testnet_ws/ws');w.onerror=function(){{}};}}catch(e){{}}"
        "</script></body></html>"
    ),
}
_TESTNET_REDIRECT_LOCATION = f"http://{testnet.TESTNET_IP}:$p_testnet_redirect/redirected"
_AUTHORITY = re.compile(r"(?:https?|wss?)://([^/:\s'\"<>]+):(\d+)")


def _checked_testnet(text: str) -> str:
    """置換後の本文に含まれる宛先がすべて `testnet.validate_egress_dest` を満たすことを確かめる（満たさなければ ValueError）。"""
    found = _AUTHORITY.findall(text)
    if not found:
        raise ValueError("TEST-NET page has no destination")
    for host, port in found:
        testnet.validate_egress_dest(host, int(port))
    return text


def build_testnet_pages(ports: dict[str, Any]) -> dict[str, str]:
    """TEST-NET 宛てのページ（パス -> HTML）。`ports` に `testnet_img` 等が不足なら KeyError、int 以外は ValueError。"""
    return {path: _checked_testnet(_subst(body, ports)) for path, body in _TESTNET_PAGES.items()}


def testnet_redirect_location(ports: dict[str, Any]) -> str:
    """TEST-NET 宛て 302 の Location。`testnet_redirect` の int のポートだけを置換する。"""
    return _checked_testnet(_subst(_TESTNET_REDIRECT_LOCATION, ports))
