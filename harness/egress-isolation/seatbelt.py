#!/usr/bin/env python3
"""macOS Seatbelt（`sandbox-exec`）の隔離プロファイルと、拒否ログの解析（純粋関数中心）。

役割・責務境界:
    Issue #1387（TASK-38・`RENDER-3`・`SEC-2`・`XOS-1`）。`measure.py` が macOS の `seatbelt` モードで
    内側ランナーを `sandbox-exec -p <profile>` から起動するための固定プロファイルを作り、
    サンドボックスの拒否ログ（`log show`）から「試行の観測」（宛先 ip:port と件数だけ）を取り出す。
    macOS には netns もプロセス単位の egress_guard も無く、共有 loopback のため内側の写し listener も立てられない。
    そのため試行の根拠は拒否ログに置く。ログが読めない・0 件なら試行は未観測（unobservable。合格にしない）。

呼び出し文脈: `measure.py`（プロファイル生成・ログ収集）、`test_seatbelt.py`（自己テスト）。
安全上の規則: プロファイルに置換するのは検証済みの int ポートだけ。`log` の述語は固定文字列。
URL・本文は扱わない（ip:port と件数のみ）。方式の採否は決めない（#1003 の判断）。
"""

from __future__ import annotations

import re
from collections import Counter

SANDBOX_EXEC = "/usr/bin/sandbox-exec"
LOG_BIN = "/usr/bin/log"
# 隔離と無関係な失敗（GPU・WindowServer 等）を切り分ける陽性対照用の全許可プロファイル。
BASELINE_PROFILE = "(version 1)(allow default)"
# 隔離プロファイル。許可するのはページ配信用の loopback 1 ポートだけ（ホストが許可オリジンを読む経路）。
_ISOLATED_TEMPLATE = (
    '(version 1)(allow default)(deny network-outbound (remote ip))'
    '(allow network-outbound (remote ip "localhost:{port}"))'
)
LOG_PREDICATE = 'sender == "Sandbox"'
MAX_LOG_LINES = 20000
MAX_LINE_LEN = 4096
# `log show` を逐次読む際の打ち切り上限（読み取った行数・総バイト数）。超えたら収集を諦める（unobservable）。
MAX_SCAN_LINES = 500000
MAX_SCAN_BYTES = 64 * 1024 * 1024
_DENY_RE = re.compile(r"(?:\((\d+)\)\s+)?deny\(\d+\) network-outbound (\d{1,3}(?:\.\d{1,3}){3}):(\d{1,5})\b")


def isolated_profile(page_port: object) -> str:
    """隔離プロファイル。`page_port` は 1024〜65535 の int のみ（それ以外は ValueError）。"""
    if isinstance(page_port, bool) or not isinstance(page_port, int) or not (1024 <= page_port <= 65535):
        raise ValueError("page_port must be an int in [1024, 65535]")
    return _ISOLATED_TEMPLATE.format(port=page_port)


def log_show_argv(start: str) -> list[str]:
    """`log show` の固定 argv。`start` は `YYYY-MM-DD HH:MM:SS` 形式のみ受け付ける。"""
    if not re.fullmatch(r"\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}", start):
        raise ValueError("start must be 'YYYY-MM-DD HH:MM:SS'")
    return [LOG_BIN, "show", "--style", "ndjson", "--start", start, "--predicate", LOG_PREDICATE]


def parse_deny_line(line: str, pids: frozenset[int] | None = None) -> str | None:
    """1 行から拒否の宛先 "ip:port" を取り出す。該当しない・長すぎる行は None。

    `pids` を渡すと、拒否ログの `proc(<pid>)` が集合に含まれる行だけを採る（対象の Servo ホストとその子孫に
    帰属する拒否に限る。pid が読めない行も除外する）。None は帰属を問わない（自己テスト・後方互換）。
    """
    if len(line) > MAX_LINE_LEN:
        return None
    m = _DENY_RE.search(line)
    if m and int(m.group(3)) <= 65535:
        if pids is not None and (m.group(1) is None or int(m.group(1)) not in pids):
            return None
        return f"{m.group(2)}:{int(m.group(3))}"
    return None


def parse_denies(text: str, pids: frozenset[int] | None = None) -> Counter[str]:
    """ndjson 本文から network-outbound の拒否を数え、`"ip:port" -> 件数` を返す。

    行数・1 行の長さに上限を設ける。不正な行・拒否以外の行は無視する。ip:port 以外は記録しない
    （プロセス名・パス・URL を結果に載せない）。
    """
    counts: Counter[str] = Counter()
    for i, line in enumerate(text.splitlines()):
        if i >= MAX_LOG_LINES:
            break
        dest = parse_deny_line(line, pids)
        if dest:
            counts[dest] += 1
    return counts


def attempts_from_denies(denies: Counter[str], dest_by_route: dict[str, str]) -> dict[str, bool]:
    """経路ごとの宛先 ("ip:port") が拒否ログに 1 件以上あれば試行ありとする。"""
    return {route: denies.get(dest, 0) >= 1 for route, dest in dest_by_route.items()}
