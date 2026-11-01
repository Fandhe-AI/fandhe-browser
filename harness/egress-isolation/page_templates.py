#!/usr/bin/env python3
"""egress 隔離の実測で配るフィクスチャ（固定テンプレート）。

役割・責務境界:
    Issue #1377。経路ごとの HTML を `string.Template` で作る。置換するのは bind で得た int のポートだけ
    （外部入力をテンプレートへ連結しない。インジェクション対策）。宛先名は `judge.py` の定数。

呼び出し文脈: `in_sandbox.py` の配信サーバーが `build_pages()` の結果を返す。
"""

from __future__ import annotations

import string
from typing import Any

import judge

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
