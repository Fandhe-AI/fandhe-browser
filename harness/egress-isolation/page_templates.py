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
# リダイレクト（許可外ホストへの 302）。Location はポートだけを埋めた固定形。
REDIRECT_PATH = "/redirect-out"
_REDIRECT_LOCATION = f"http://{judge.HOST_BLOCKED}:$p_redirect/redirected"

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

    `literal=True` はダミー宛先を IP リテラルにする（DNS 経路の `/dns` は NXDOMAIN 名のまま）。
    """
    return {path: _subst(_literal(body) if literal else body, ports) for path, body in _PAGES.items()}


def redirect_location(ports: dict[str, Any], *, literal: bool = False) -> str:
    return _subst(_literal(_REDIRECT_LOCATION) if literal else _REDIRECT_LOCATION, ports)
