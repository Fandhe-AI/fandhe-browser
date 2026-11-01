#!/usr/bin/env python3
"""egress 隔離の実測の判定ロジック（純粋関数）と、経路・宛先の定数。

役割・責務境界:
    Issue #1377（TASK-38・`RENDER-3`・`SEC-2`）。5 経路（リダイレクト・サブリソース・WebSocket・
    内部アドレス・DNS/UDP）の結果を fail-closed で判定する。観測できなかったものは合格にしない。
    実ホスト・ネットワークなしで自己テスト（`test_judge.py`）できるよう、I/O を持たない。

呼び出し文脈:
    `measure.py` が外側 listener の件数と内側ランナー（`in_sandbox.py`）の結果 JSON を渡し、
    `in_sandbox.py` は前提検査（`check_isolation`）と許可リスト（`routes_for_mode`）を使う。

設計: docs/design/servo-host-egress-isolation.md §6.1・§6.2。方式の採否は決めない（#1003 の判断）。
"""

from __future__ import annotations

from typing import Any

PASS = "pass"
FAIL = "fail"
UNOBSERVABLE = "unobservable"

# 名前と宛先は定数（外部入力から作らない）。許可外の宛先はすべて 127.0.0.0/8 のダミー。
HOST_ALLOWED = "pages.test"
HOST_BLOCKED = "blocked.test"
HOST_INTERNAL_STANDIN = "internal-standin.test"
HOST_NXDOMAIN = "egress-probe.invalid"
IP_ALLOWED = "127.0.0.1"
IP_BLOCKED = "127.0.0.2"
IP_INTERNAL_STANDIN = "127.0.0.3"
HOSTS_FILE = (
    f"{IP_ALLOWED} localhost {HOST_ALLOWED}\n"
    f"{IP_BLOCKED} {HOST_BLOCKED}\n"
    f"{IP_INTERNAL_STANDIN} {HOST_INTERNAL_STANDIN}\n"
    "::1 localhost\n"
)
# 隔離下だけで試す「実在の内部アドレス」。接続先に何も届かないこと（経路なし）を前提検査した後にだけ使う。
REAL_INTERNAL_V4 = ("10.255.255.1", "169.254.169.254")
REAL_INTERNAL_V6 = "fd00::1"
# DNS 経路の判定用に、サンドボックスへ固定で見せる resolv.conf（非ループバックの TEST-NET-1 宛。RFC 5737）。
# 実環境の resolv.conf が `/run` の隠蔽で切れて DNS が失敗しても netns の効果と取り違えないための足場。
IP_DNS_STANDIN = "192.0.2.53"
RESOLV_CONF = f"nameserver {IP_DNS_STANDIN}\n"

MODES = ("baseline", "bwrap-netns", "docker-none", "seatbelt", "appcontainer")
ISOLATED_MODES = ("bwrap-netns", "docker-none", "seatbelt", "appcontainer")
# OS ごとに試せるモード（#1387）。先頭の baseline は全 OS で陽性対照。
PLATFORM_MODES = {
    "linux": ("baseline", "bwrap-netns", "docker-none"),
    "darwin": ("baseline", "seatbelt"),
    "win32": ("baseline", "appcontainer"),
}
# ネットワーク名前空間（/proc/net の「lo のみ」検査・構造的な経路の errno）を前提にするモード。
# Seatbelt・AppContainer ではインターフェースが見えたままで、この前提が成り立たない。
NETNS_MODES = ("bwrap-netns", "docker-none")
STRUCTURAL_NA_REASON = "structural check requires a network namespace; not applicable on this OS"

# 宛先 listener があるもの（TCP/UDP）と、構造的に観測する経路（実在アドレス・DNS）。
LISTENER_ROUTES = ("redirect", "img", "fetch", "ws", "internal", "udp_direct")
STRUCTURAL_ROUTES = ("internal_real", "dns")
# ホストにページを読み込ませる経路。egress guard の走査が完了していなければ観測できていない。
LOADED_ROUTES = ("redirect", "img", "fetch", "ws", "internal")
# 構造的な経路の陽性対照（隔離なしで届くことを確かめた経路）。
CONTROL_ROUTE = {"internal_real": "internal", "dns": "udp_direct"}


def routes_for_mode(mode: str) -> tuple[str, ...]:
    """モードごとに試す経路。実在の内部アドレス・DNS は隔離下に限る（ベースラインは外へ出うるため）。"""
    if mode not in MODES:
        raise ValueError(f"unknown mode: {mode!r}")
    # 構造的な経路は表を 8 経路でそろえるため全隔離モードに含める（netns 前提でないモードは判定側が unobservable にする）。
    return LISTENER_ROUTES + (STRUCTURAL_ROUTES if mode in ISOLATED_MODES else ())


def judge_route(
    *,
    outer_count: int,
    control_count: int,
    attempt_observed: bool,
    guard_unavailable: bool,
    positive_ok: bool,
    guard_monitored: bool = True,
    guard_violation: str | None = None,
    monitor_required: bool = True,
    mode_unavailable: str | None = None,
) -> tuple[str, str]:
    """1 経路の判定。(verdict, reason) を返す。

    fail: 隔離の下で外側の宛先が受信した（漏れ）。
    pass: 受信 0 件・陽性対照が届いた・試行を観測した・guard が使え・ホストが動いた、をすべて満たす。
    unobservable: それ以外（合格とは扱わない）。
    guard_monitored: この経路の間に guard の走査が 1 回以上完了したか（止まった監視を合格にしない）。
    guard_violation: guard が想定外の宛先（ハーネスの既知ダミー以外）への接続を検知した場合の宛先。
    monitor_required: False のときだけ、プロセス単位の監視（egress_guard）を pass の条件から外す。
        macOS・Windows には /proc が無く、外側 listener の受信と拒否ログが主証拠になる（#1387）。Linux は常に True。
    mode_unavailable: 値があればそのモードは起動できなかった（例: AppContainer のランチャー未実装）。常に unobservable。
    """
    if mode_unavailable is not None:
        return UNOBSERVABLE, mode_unavailable
    if outer_count >= 1:
        return FAIL, f"outer destination received {outer_count} connection(s)"
    if guard_violation is not None:
        return FAIL, "egress guard saw a connection to an unexpected destination"
    if not positive_ok:
        return UNOBSERVABLE, "positive check failed (selftest or loopback load)"
    if control_count < 1:
        return UNOBSERVABLE, "baseline control did not reach the destination"
    if not attempt_observed:
        return UNOBSERVABLE, "no attempt was observed inside the sandbox"
    if monitor_required and guard_unavailable:
        return UNOBSERVABLE, "egress guard could not monitor"
    if monitor_required and not guard_monitored:
        return UNOBSERVABLE, "egress guard completed no scan during this route"
    return PASS, "no outer receipt; attempt observed; control reached"


def _iface_names(net_dev_text: str) -> list[str]:
    names: list[str] = []
    for line in net_dev_text.splitlines()[2:]:
        name, sep, _ = line.partition(":")
        if sep:
            names.append(name.strip())
    return names


def check_isolation(net_dev: str | None, route: str | None, ipv6_route: str | None) -> tuple[bool, str]:
    """`/proc/net/{dev,route,ipv6_route}` の本文から、lo 以外の経路が無いことを確かめる。

    実在の内部アドレスを含むページを読み込む前の安全側の前提検査（fail-closed）。
    読めない（None）・形式が不正・lo が無い場合は隔離を確認できないため (False, 理由)。
    例外は `ipv6_route` の不在（None）のみで、IPv6 無効のカーネルとして許容する。
    """
    if net_dev is None or route is None:
        return False, "/proc/net/dev or /proc/net/route unreadable"
    dev_lines = net_dev.splitlines()
    if len(dev_lines) < 3 or "|" not in dev_lines[1]:
        return False, "/proc/net/dev has an unexpected format"
    if "lo" not in _iface_names(net_dev):
        return False, "loopback interface not found in /proc/net/dev"
    route_lines = route.splitlines()
    if not route_lines or not route_lines[0].startswith("Iface"):
        return False, "/proc/net/route has an unexpected format"
    extra = [n for n in _iface_names(net_dev) if n != "lo"]
    if extra:
        return False, f"non-loopback interfaces present: {','.join(extra)[:80]}"
    for line in route.splitlines()[1:]:
        fields = line.split()
        if fields and fields[0] != "lo":
            return False, "IPv4 route table has a non-loopback route"
    for line in (ipv6_route or "").splitlines():
        fields = line.split()
        if len(fields) >= 10 and fields[-1] != "lo":
            return False, "IPv6 route table has a non-loopback route"
    return True, "only loopback is present"


def judge_all(
    mode: str, inner: dict[str, Any], outer_counts: dict[str, int], control_counts: dict[str, int],
    binary_match: bool = True, mode_unavailable: str | None = None,
) -> dict[str, dict[str, Any]]:
    """内側ランナーの結果 JSON と外側の件数から、隔離モードの全経路を判定する。"""
    positive_ok = bool(inner.get("positive_ok"))
    guard = inner.get("guard") or {}
    guard_unavailable = guard.get("unavailable") is not None
    guard_violation = guard.get("violation")
    guard_polls: dict[str, int] = inner.get("guard_polls") or {}
    attempts: dict[str, bool] = inner.get("attempts") or {}
    out: dict[str, dict[str, Any]] = {}
    monitor_required = mode in ("baseline", *NETNS_MODES)
    for route in routes_for_mode(mode):
        control_route = CONTROL_ROUTE.get(route, route)
        if route in STRUCTURAL_ROUTES and mode not in NETNS_MODES:
            # netns 前提の検査は Seatbelt・AppContainer では成り立たない。実在の内部アドレスにも向けない。
            out[route] = {"verdict": UNOBSERVABLE,
                          "reason": mode_unavailable or STRUCTURAL_NA_REASON, "outer_count": 0,
                          "control_count": control_counts.get(control_route, 0), "method": "not applicable"}
            continue
        verdict, reason = judge_route(
            outer_count=outer_counts.get(route, 0),
            control_count=control_counts.get(control_route, 0),
            attempt_observed=bool(attempts.get(route)),
            guard_unavailable=guard_unavailable,
            positive_ok=positive_ok,
            guard_monitored=route not in LOADED_ROUTES or int(guard_polls.get(route, 0)) >= 1,
            guard_violation=guard_violation if isinstance(guard_violation, str) else None,
            monitor_required=monitor_required, mode_unavailable=mode_unavailable,
        )
        method = (
            "route table + connect probe (structural; not observed from outside)"
            if route in STRUCTURAL_ROUTES
            else "outer listener receipt + inner listener attempt + egress_guard"
        )
        if not monitor_required:
            method = ("outer listener receipt + failure-path beacon / sandbox deny log / errno "
                      "(no process-level egress monitor on this OS)")
        if not binary_match and verdict == PASS:
            verdict, reason = UNOBSERVABLE, "binary inside the sandbox differs from the baseline control binary"
        if (not monitor_required and route == "redirect" and verdict == UNOBSERVABLE
                and reason == "no attempt was observed inside the sandbox"):
            # トップレベルの 302 はページの JS が走らずビーコンを付けられない。根拠は拒否ログだけ（Issue #1395）。
            reason += " (top-level redirect has no page-level beacon; deny log only)"
        out[route] = {
            "verdict": verdict,
            "reason": reason,
            "outer_count": outer_counts.get(route, 0),
            "control_count": control_counts.get(control_route, 0),
            "method": method,
        }
        evidence = (inner.get("attempt_evidence") or {}).get(route)
        if isinstance(evidence, str):
            out[route]["evidence"] = evidence
    return out
