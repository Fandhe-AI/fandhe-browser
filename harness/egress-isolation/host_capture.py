#!/usr/bin/env python3
"""隔離下の実 Servo ホストの DNS・UDP・直接 TCP 送信を、host netns の外側から観測して判定する（Linux 専用）。

役割・責務境界:
    Issue #1461（TASK-38・`RENDER-3`・`SEC-2`）。設計文書 `servo-host-egress-isolation.md` §6.4・§9.2 の
    残課題「Servo 自身の DNS・UDP 送信の外部観測」を、`xos-measure.yml` の `egress-host-ubuntu`
    （使い捨てランナー）で埋める。`dns` / `udp_direct`（measure.py）はハーネス自身のプローブと構造検査で、
    Servo の送信そのものは見ていない。ここでは次の 2 つを突き合わせる。
      - `sample`（非特権）: Servo ホスト pid・その netns・host netns のソケット所有 pid を jsonl に記録する。
      - tcpdump のテキスト（`sudo tcpdump -i any` の pcap を `tcpdump -r -nn -tt` で変換したもの。sudo は
        tcpdump の起動・停止だけ。ワークフロー側で実行する）。
      - `judge`（非特権）: パケットの送信元ソケットを所有 pid に帰属させ、隔離下の Servo に帰属する
        外向き DNS・UDP・直接 TCP SYN が 0 件かを fail-closed で判定する。
    ワークフロー（xos-measure.yml の `[egress-host] host-capture *`）から呼ばれる。本体のコード
    （crates/・servo-host/）には触れない。

帰属の方法と限界:
    - Servo ホスト pid は `/proc/<pid>/exe` の実体パスが `--servo-exe` と一致するもので同定する
      （本体コードを変えずに済ませるため。`HostProcess` の pid API は使わない）。
    - 隔離は「bwrap の祖先がいる」または「netns がサンプラーと異なる」で判定する。bwrap の祖先がいるのに
      netns が host と同じなら隔離破れとして fail。
    - bwrap 配下の Servo のソケットは別 netns にあり、host netns の外向きインターフェースには原理的に出ない。
      そのため判定の柱は (a) netns 構造検査 (b) host 側パケットに隔離下 Servo の帰属が 0 件、の併用。
    - サンプル間隔より短命なソケットは帰属できない（unattributed）。root のプロセス（systemd-resolved 等）の
      ソケットも `/proc/<pid>/fd` が読めず帰属できない。これらは件数を記録するだけで合否に使わない。
    - 同定済み Servo pid の fd が一時的に読めない競合（#1493）は、診断（fd-diag 記録）を残しつつ上限つきで読み直す。
      読み直しでも読めなければ従来どおり観測不能（`_socket_owners` の docstring）。
    - 隔離下のホストのスタブ問い合わせは ns 内の `lo` で閉じ、host には届かない（観測対象外）。
    - 陽性対照（カナリア）: サンプラーが 127.0.0.1 の UDP で自分宛てに送り、`lo` の記録に現れて自分の pid に
      帰属することを要求する。観測できなければ unobservable（合格にしない）。
    - Linux 専用。宛先は 127.0.0.0/8 のカナリアのみで、外部アドレスへのプローブは送らない。
"""

from __future__ import annotations

import argparse
import bisect
import json
import math
import os
import re
import signal
import socket
import subprocess
import sys
import threading
import time
from collections import Counter
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable

# 流用の理由: 走査の上限値と /proc/net の 1 行パーサーを monitor.py と同じ定義にそろえるため（コピーしない）。
# 非公開関数（_descendant_pids 等）は使わない。
_HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(_HERE.parent / "render-screenshot"))
import egress_guard  # noqa: E402

MAX_PROCESSES = egress_guard.MAX_PROCESSES
MAX_FDS_PER_PROCESS = egress_guard.MAX_FDS_PER_PROCESS
MAX_NET_LINES = egress_guard.MAX_NET_LINES
MAX_ANCESTOR_DEPTH = 16
MAX_SAMPLES = 200_000
MAX_SAMPLE_BYTES = 64 * 1024 * 1024
MAX_SOCKETS_PER_SAMPLE = 512
MAX_JSONL_LINE_CHARS = 262_144
MAX_PACKET_LINE_CHARS = 1024
# Servo と同定した pid の `/proc/<pid>/fd` が PermissionError になったときの読み直し（#1493・#1497）。run 41819946868 で
# 同定済み pid の fd が一時的に読めず unobservable になった。読み直しは「一時的な状態」を吸収するための上限つきの
# 措置で、予算内に読めなければ従来どおり記録して観測不能にする（fail-closed）。
# 時間予算は pid ごと（最初の denied から FD_READ_RETRY_BUDGET_SECONDS）。走査全体の締切を 1 回だけ計算すると、
# 走査が遅いとき・先頭の pid が使い切ったときに後ろの Servo pid が読み直し 0 回になるため（#1497）。
# 1 回の走査で読み直しに使う時間の累計は FD_READ_RETRY_SAMPLE_BUDGET_SECONDS で抑える（走査の時間は数えない）。
# 大小関係は pid ごと <= 累計 < サンプル間隔の既定（workflow の --interval-ms 200）で、denied の pid が多くても
# 1 サンプルの延びを有界に保つ。累計を使い切った後の pid は読み直し 0 回で persistent（観測不能）になる。
# 回数と時間の関係: FD_READ_RETRIES × FD_READ_RETRY_SECONDS（5 × 0.02）≒ FD_READ_RETRY_BUDGET_SECONDS（0.1）。
# 回数上限は読み取りが速い環境の、時間上限は sleep の遅延・読み取りが遅い環境の歯止め。
# 上界: sleep は締切までの残りに切り詰めるため、待ちの累計は pid ごと 0.1 秒・走査の累計 0.15 秒を超えない。
# 締切直前の最後の読み取り（fd・starttime・スナップショット）の時間と OS の sleep 遅延の分だけ実時間は超えうる。
FD_READ_RETRIES = 5
FD_READ_RETRY_SECONDS = 0.02
FD_READ_RETRY_BUDGET_SECONDS = 0.1
FD_READ_RETRY_SAMPLE_BUDGET_SECONDS = 0.15
# fd-diag の `stop`（読み直しを止めた理由）の固定集合。persistent が予算切れ由来か持続的な失敗かの切り分けに使う。
FD_DIAG_STOPS = (
    "read-ok",
    "gone",
    "reused",
    "starttime-unknown",
    "other-error",
    "attempts",
    "pid-budget",
    "sample-budget",
)


class _Clock:
    """時刻・待ちの注入点。既定は呼び出し時点の `time` を引く（テストが偽の時計を渡して実 sleep を避ける。#1497）。

    モジュール内のキーワード引数でだけ差し替えられ、CLI・環境変数からは変えられない。
    """

    def wall(self) -> float:
        return time.time()

    def mono(self) -> float:
        return time.monotonic()

    def sleep(self, seconds: float) -> None:
        time.sleep(seconds)


SYSTEM_CLOCK = _Clock()
# サンプラー 1 回の実行で samples.jsonl に出す fd-diag 記録の上限。超えた分は end 記録の fd_diag_dropped に数える
# （診断の取りこぼしは観測の欠落ではないので truncated にはしない）。
MAX_FD_DIAG_RECORDS = 64
# verdict.json に載せる fd-diag 記録の件数の上限
MAX_FD_DIAG_VERDICT_RECORDS = 8
FD_DIAG_OUTCOMES = ("recovered", "vanished", "persistent")
# judge() の kwarg 既定値（純関数のテスト・関数呼び出し用）。CLI は既定値を持たず、workflow が job env の
# HOST_CAPTURE_MAX_PACKETS を tcpdump の -c と judge の --max-packets の両方へ明示で渡す（二重管理のずれを防ぐ。#1473）。
DEFAULT_MAX_PACKETS = 300_000
UNATTRIBUTED_TOP_N = 10
ATTRIBUTION_WINDOW_SECONDS = 0.5
# 帰属できない DNS・UDP を数える Servo の観測区間の両端に足す余裕（秒）。サンプリング間隔（200ms）より広く、
# 起動直後・終了直前の送信が [最初のサンプル, 最後のサンプル] から漏れるのを防ぐ。件数（参考値）にだけ効かせ、
# 陽性対照（canary_in_window）と最終カナリアの閾値は生の区間のまま（広げると pass しやすくなる・閾値が最終カナリアを追い越す。#1473）。
WINDOW_SLACK_SECONDS = ATTRIBUTION_WINDOW_SECONDS
# wait-final-canary の待ち時間の既定と上限（秒）
WAIT_CANARY_DEFAULT_TIMEOUT = 15.0
WAIT_CANARY_MAX_TIMEOUT = 60.0
WAIT_CANARY_DEFAULT_INTERVAL = 0.5
# ポーリング 1 回（tcpdump -r の起動から読み切りまで）の待ちの上限・下限と、kill 後の wait の猶予（秒。#1476）。
# 上限は従来の timeout=10。下限は残り時間が 0 以下でも最低 1 回は読みに行くための小さな正の値。
WAIT_CANARY_POLL_MAX_SECONDS = 10.0
WAIT_CANARY_POLL_MIN_SECONDS = 0.1
WAIT_CANARY_KILL_GRACE_SECONDS = 1.0
# tcpdump の起動コマンド。テストが偽の遅い tcpdump に差し替えるためのモジュール定数の注入点で、
# 環境変数・CLI 引数からは変えられない（実行ファイルを外部入力で差し替えさせない）。
TCPDUMP_ARGV: tuple[str, ...] = ("tcpdump",)
# final_canary_t と pcap 上の時刻（マイクロ秒切り捨て）の比較に許す下駄（秒）。
FINAL_CANARY_EPSILON_SECONDS = 0.001
UNPARSED_RATIO_LIMIT = 0.01
UNPARSED_MIN = 10
MIN_CANARY_SAMPLES = 3

PASS_BASIS_NOTE = (
    "pass rests on the netns structural check (bwrap ancestor / distinct netns inode) and the observed range only; "
    "unattributed DNS/UDP cannot be ruled out as Servo's, review this summary manually"
)

ROLE_ISOLATED = "servo_isolated"
ROLE_UNISOLATED = "servo_unisolated"
ROLE_SELF = "self"
ROLE_OTHER = "other"
ROLE_NONE = "unattributed"

CATEGORIES = ("dns", "udp_other", "tcp_syn")

_LINE_RE = re.compile(
    r"^(?P<ts>\d+\.\d+) (?P<iface>\S+)\s+(?P<dir>In|Out|B|M|P)\s+(?P<fam>\S+) (?P<rest>.*)$"
)
_HOSTPORT_RE = re.compile(r"^(?P<host>[0-9a-fA-F:.]+?)\.(?P<port>\d{1,5})$")
_CANARY_PAYLOAD = b"fandhe-host-capture-canary"
# 解析できない行・proto=other の行に UDP・DNS の痕跡があるかの判定（非先頭フラグメントは `ip-proto-17` と出る）
_UDP_TEXT_RE = re.compile(r"ip-proto-17|\bUDP\b|\.53(?=[:\s,]|$)", re.IGNORECASE)
_ICMP_RE = re.compile(r"\bICMP6?\b")


# --------------------------------------------------------------------------------------
# tcpdump テキストの解析（純粋関数）
# --------------------------------------------------------------------------------------


def _split_hostport(token: str, fam: str) -> tuple[str, int | None]:
    """`10.0.0.1.443` / `fd00::5.7000` を (host, port) に分ける。ポートが無い形（ICMP 等）は port=None。"""
    m = _HOSTPORT_RE.match(token)
    if m is None:
        return token, None
    host = m.group("host")
    port = int(m.group("port"))
    if port > 65535:
        return token, None
    if fam == "IP" and host.count(".") != 3:
        return token, None
    return host, port


def suspicious_udp_text(text: str) -> bool:
    """行のテキストに UDP・DNS の痕跡（`ip-proto-17`・`UDP`・ポート 53 らしいトークン）があるか。

    解析できない行や proto=other の行の中に UDP・DNS が紛れても、許容枠の範囲で気づかず pass になるのを防ぐ
    ための粗い検出（過検出は unobservable 側＝安全側）。ICMP の行（`udp port 53 unreachable` 等）は対象外にする。
    """
    m = _LINE_RE.match(text[:MAX_PACKET_LINE_CHARS].rstrip("\n"))
    body = m.group("rest") if m else text[:MAX_PACKET_LINE_CHARS]
    if _ICMP_RE.search(body):
        return False
    return _UDP_TEXT_RE.search(body) is not None


def parse_tcpdump_line(line: str) -> dict | None:
    """`tcpdump -nn -tt`（`-i any` の pcap を読んだ形）の 1 行を dict にする。解析できない行は None。

    キー: ts・iface・dir・proto（"udp" / "tcp" / "other"）・src・sport・dst・dport・syn（TCP の SYN のみ真）・
    udp_hint（proto が "other" で、行に UDP・DNS の痕跡があるとき真。judge が観測不能にする）。
    形式は SLL2 の `1730000000.000000 eth0  Out IP 10.0.0.5.5000 > 8.8.8.8.53: ...`（実機の tcpdump 4.99 で確認）。
    """
    m = _LINE_RE.match(line[:MAX_PACKET_LINE_CHARS].rstrip("\n"))
    if m is None:
        return None
    try:
        ts = float(m.group("ts"))
    except ValueError:
        return None
    pkt = {
        "ts": ts,
        "iface": m.group("iface"),
        "dir": m.group("dir"),
        "proto": "other",
        "src": None,
        "sport": None,
        "dst": None,
        "dport": None,
        "syn": False,
        "udp_hint": False,
    }
    fam = m.group("fam")
    body = m.group("rest")
    if fam not in ("IP", "IP6") or " > " not in body:
        pkt["udp_hint"] = suspicious_udp_text(body)
        return pkt
    left, right = body.split(" > ", 1)
    dst_token, _, rest = right.partition(": ")
    src, sport = _split_hostport(left.strip(), fam)
    dst, dport = _split_hostport(dst_token.strip(), fam)
    pkt.update(src=src, sport=sport, dst=dst, dport=dport)
    if rest.startswith("ICMP"):
        return pkt
    if rest.startswith("Flags ["):
        flags = rest[len("Flags [") :].split("]", 1)[0]
        if sport is not None and dport is not None:
            pkt["proto"] = "tcp"
            pkt["syn"] = "S" in flags and "." not in flags
        else:
            pkt["udp_hint"] = suspicious_udp_text(body)
        return pkt
    if sport is not None and dport is not None:
        # `UDP, length N` のほか、DNS 等はデコードされた形（`domain [...]`）で出る
        pkt["proto"] = "udp"
    else:
        pkt["udp_hint"] = suspicious_udp_text(body)
    return pkt


def count_category(pkt: dict) -> str | None:
    """判定対象の外向き送信なら "dns" / "udp_other" / "tcp_syn"、対象外なら None。

    対象: 非 lo の Out で UDP 全ポートまたは TCP SYN、および lo の Out / In で宛先 53 番（スタブ resolver 経由の
    問い合わせを拾うため）。TCP の 53 番は dns に数える。
    lo は libpcap が送信側の重複を除外し通常 In で記録されるため、lo だけ In も扱う（重複計上は 0 件判定に影響しない）。
    """
    on_lo = pkt.get("iface") == "lo"
    if pkt.get("dir") not in (("Out", "In") if on_lo else ("Out",)) or pkt.get("proto") not in ("udp", "tcp"):
        return None
    if pkt["proto"] == "tcp" and not pkt.get("syn"):
        return None
    if on_lo and pkt.get("dport") != 53:
        return None
    if pkt.get("dport") == 53:
        return "dns"
    return "udp_other" if pkt["proto"] == "udp" else "tcp_syn"


# --------------------------------------------------------------------------------------
# サンプルの解析と帰属（純粋関数）
# --------------------------------------------------------------------------------------


def _fd_diag_verdict_record(rec: dict) -> dict:
    """fd-diag 記録を verdict.json に載せる許可キーへ絞る（#1497）。生の記録は samples.jsonl に残る。

    値は整数・有限な数値・固定カテゴリに丸め、/proc 由来の生文字列を verdict と Step Summary に入れない。
    first / last の許可キーは fd_dir_owner・state・exe・stage（FD_DIAG_STAGES か unknown）・errno（0..4095 の整数か None）。
    """

    def snap(v: object) -> dict | None:
        if not isinstance(v, dict):
            return None
        out: dict = {k: _diag_cat(v.get(k)) for k in ("fd_dir_owner", "state", "exe")}
        st = v.get("stage")
        out["stage"] = None if st is None else (st if isinstance(st, str) and st in FD_DIAG_STAGES else "unknown")
        en = v.get("errno")
        ok = isinstance(en, int) and not isinstance(en, bool) and 0 <= en <= FD_DIAG_MAX_ERRNO
        out["errno"] = en if ok else None
        return out

    attempts = rec.get("attempts")
    elapsed = rec.get("elapsed")
    stop = rec.get("stop")
    return {
        "pid": rec["pid"],
        "outcome": rec["outcome"],
        "attempts": attempts if isinstance(attempts, int) and not isinstance(attempts, bool) else None,
        "elapsed": elapsed
        if isinstance(elapsed, (int, float)) and not isinstance(elapsed, bool) and math.isfinite(elapsed)
        else None,
        "stop": stop if stop in FD_DIAG_STOPS else "unknown",
        "first": snap(rec.get("first")),
        "last": snap(rec.get("last")),
    }


class SampleIndex:
    """サンプラーの jsonl から、帰属に必要な情報を組み立てる。

    sample 記録は変化があったときだけ書かれ、tick 記録は直前の状態の継続を表す。ソケットは
    (proto, port, role) ごとに連続した存在区間にまとめ、パケット時刻が区間の前後 window 秒以内なら帰属する。
    """

    def __init__(self) -> None:
        self.meta: dict | None = None
        self.end: dict | None = None
        self.sample_count = 0
        self.record_count = 0
        self.bad_lines = 0
        self.lines_over_limit = 0
        self.servo: dict[int, dict] = {}
        self._intervals: dict[tuple[str, int], list[list]] = {}
        self._state_sockets: list[dict] = []
        self._state_servo: list[dict] = []
        self._open: dict[tuple[str, int, str], list] = {}
        self._starts: dict[tuple[str, int], list[float]] = {}
        self.last_t: float | None = None
        # fd-diag 記録（#1493）。合否には使わない診断で、件数は outcome 別に数え、記録本体は先頭の数件だけ保持する。
        self.fd_diag_counts: Counter = Counter()
        self.fd_diags: list[dict] = []

    def feed(self, rec: dict) -> None:
        kind = rec.get("type")
        if kind == "meta":
            self.meta = rec
            return
        if kind == "end":
            self.end = rec
            return
        if kind == "fd-diag":
            # 診断記録。形式が不正なら他の記録と同様に bad_lines（fail-closed）。サンプル数・区間には影響させない。
            pid = rec.get("pid")
            if (
                not isinstance(rec.get("t"), (int, float))
                or not isinstance(pid, int)
                or isinstance(pid, bool)
                or rec.get("outcome") not in FD_DIAG_OUTCOMES
            ):
                self.bad_lines += 1
                return
            self.fd_diag_counts[rec["outcome"]] += 1
            if len(self.fd_diags) < MAX_FD_DIAG_VERDICT_RECORDS:
                self.fd_diags.append(_fd_diag_verdict_record(rec))
            return
        if kind not in ("sample", "tick"):
            self.bad_lines += 1
            return
        t = rec.get("t")
        if not isinstance(t, (int, float)):
            self.bad_lines += 1
            return
        if kind == "sample":
            sockets = rec.get("sockets")
            servo = rec.get("servo")
            if not isinstance(sockets, list) or not isinstance(servo, list):
                self.bad_lines += 1
                return
            self._state_sockets = [s for s in sockets if isinstance(s, dict)]
            self._state_servo = [s for s in servo if isinstance(s, dict)]
        self.sample_count += 1
        self.last_t = float(t)
        for s in self._state_servo:
            pid = s.get("pid")
            if isinstance(pid, int):
                info = self.servo.setdefault(
                    pid,
                    {
                        "bwrap": False,
                        "netns": s.get("netns"),
                        "first": float(t),
                        "last": float(t),
                        "isolated_seen": False,
                        "shared_ns_seen": False,
                        "ns_unknown_seen": False,
                        "netns_seen": set(),
                    },
                )
                info["last"] = float(t)
                # 初回値で固定せず、サンプルごとに検査して違反・確認不能を累積する（一時的な違反を失わない）。
                bwrap = bool(s.get("bwrap"))
                netns = s.get("netns")
                self_netns = (self.meta or {}).get("self_netns")
                info["bwrap"] = info["bwrap"] or bwrap
                if bwrap and netns is not None and netns == self_netns:
                    info["shared_ns_seen"] = True
                if bwrap and netns is None:
                    info["ns_unknown_seen"] = True
                if bwrap or (netns is not None and self_netns is not None and netns != self_netns):
                    info["isolated_seen"] = True
                if netns is not None:
                    info["netns_seen"].add(netns)
        present = set()
        for s in self._state_sockets:
            proto, port, role = s.get("proto"), s.get("port"), s.get("role")
            if proto not in ("udp", "tcp") or not isinstance(port, int) or not isinstance(role, str):
                continue
            key = (proto, port, role)
            present.add(key)
            cur = self._open.get(key)
            if cur is None:
                cur = [float(t), float(t), role]
                self._open[key] = cur
                self._intervals.setdefault((proto, port), []).append(cur)
            cur[1] = float(t)
        for key in [k for k in self._open if k not in present]:
            del self._open[key]

    def isolated_window(self) -> tuple[float, float] | None:
        """隔離下と判定された Servo pid の観測区間 [最小 first, 最大 last]。観測が無ければ None。"""
        spans = [(i["first"], i["last"]) for i in self.servo.values() if i.get("isolated_seen")]
        if not spans:
            return None
        return min(a for a, _ in spans), max(b for _, b in spans)

    def roles_for(self, proto: str, port: int | None, ts: float, window: float) -> set[str]:
        if port is None:
            return set()
        roles = set()
        for start, end, role in self._intervals.get((proto, port), ()):
            if start - window <= ts <= end + window:
                roles.add(role)
        return roles


def attribute(index: SampleIndex, pkt: dict, window: float = ATTRIBUTION_WINDOW_SECONDS) -> str:
    """パケットの送信元ソケットを所有 pid の役割に帰属させる。優先順は隔離下 Servo > 隔離外 Servo > 自身 > その他。"""
    roles = index.roles_for(pkt.get("proto", ""), pkt.get("sport"), pkt["ts"], window)
    for role in (ROLE_ISOLATED, ROLE_UNISOLATED, ROLE_SELF, ROLE_OTHER):
        if role in roles:
            return role
    return ROLE_NONE


def _valid_final_canary_t(
    end: dict | None, window: tuple[float, float] | None
) -> tuple[float | None, str | None]:
    """end 記録の final_canary_t を検証して (値, 不正理由) を返す。judge と wait-final-canary が共用する（#1473）。

    キー無し・None は (None, None)。bool・非数値・NaN/inf・最後の Servo サンプルより前（window[1] 未満）は
    (None, 理由) で、呼び出し側は観測不能にする（-inf で閾値が window[1] に落ち通常のカナリアが通るのを防ぐ）。
    """
    raw = end.get("final_canary_t") if end else None
    if raw is None:
        return None, None
    if isinstance(raw, bool) or not isinstance(raw, (int, float)) or not math.isfinite(raw):
        return None, f"final_canary_t is invalid (not a finite number at or after the last Servo sample): {raw!r}"
    if window is not None and raw < window[1]:
        return None, f"final_canary_t is invalid (not a finite number at or after the last Servo sample): {raw!r}"
    return float(raw), None


def _final_canary_threshold(final_canary_t: float, window: tuple[float, float] | None) -> float:
    """最終カナリアとみなす pcap 時刻の下限。judge と wait-final-canary で二重定義しない。"""
    threshold = final_canary_t - FINAL_CANARY_EPSILON_SECONDS
    if window is not None:
        threshold = max(threshold, window[1])
    return threshold


def _is_final_canary(pkt: dict, canary_port: int, threshold: float) -> bool:
    """1 パケットが最終カナリア（lo 上の canary_port 発 UDP で閾値以降）か。逐次読みと一括判定で共用する。"""
    return (
        pkt.get("iface") == "lo"
        and pkt.get("dir") in ("Out", "In")
        and pkt.get("proto") == "udp"
        and pkt.get("sport") == canary_port
        and pkt.get("ts", 0.0) >= threshold
    )


def final_canary_observed(
    packets: Iterable[dict], canary_port: int, final_canary_t: float, window: tuple[float, float] | None = None
) -> bool:
    """パケット列に最終カナリア（lo 上の canary_port 発 UDP で閾値以降）があるか。stop ステップのポーリング用。"""
    threshold = _final_canary_threshold(final_canary_t, window)
    return any(_is_final_canary(pkt, canary_port, threshold) for pkt in packets)


def _empty_counts() -> dict:
    return {role: {c: 0 for c in CATEGORIES} for role in (ROLE_ISOLATED, ROLE_UNISOLATED, ROLE_SELF, ROLE_OTHER, ROLE_NONE)}


def judge(
    packets: Iterable[dict],
    index: SampleIndex,
    *,
    unparsed: int = 0,
    unparsed_udp_like: int = 0,
    total_lines: int = 0,
    max_packets: int = DEFAULT_MAX_PACKETS,
    tcpdump_exit: int | None = 0,
    tcpdump_version: str = "",
    truncated_lines: bool = False,
    convert_exit: int | None = 0,
    kernel_dropped: int | None = 0,
) -> dict:
    """verdict（"pass" / "fail" / "unobservable"）・理由・件数を返す。観測不能を合格にしない（fail-closed）。

    観測しきれていないものは unobservable にする: 解析できない行・proto=other の行に UDP・DNS の痕跡がある、
    最終カナリア（end 記録の final_canary_t 以降のサンプラー帰属カナリア）が観測されない、カナリアが Servo の観測区間内に無い、sample 行の欠損。
    帰属できない DNS・UDP は verdict に使わず、観測区間内の件数・宛先上位を記録する（#1457 案 A）。
    """
    fails: list[str] = []
    unobs: list[str] = []
    counts = _empty_counts()
    nonlo_out = 0
    packet_total = 0
    canary_seen = 0
    canary_ok = 0
    canary_port = index.meta.get("canary_port") if index.meta else None
    ifaces: set[str] = set()
    window = index.isolated_window()
    canary_in_window = 0
    canary_after_last = 0
    count_window = (
        (window[0] - WINDOW_SLACK_SECONDS, window[1] + WINDOW_SLACK_SECONDS) if window is not None else None
    )
    final_canary_t, final_canary_invalid = _valid_final_canary_t(index.end, window)
    # pcap の時刻表記（マイクロ秒切り捨て）との差を吸収する下駄は閾値側に含む。通常カナリア（2 秒間隔）を最終扱いしない幅に留める。
    after_threshold: float | None = None
    if final_canary_t is not None:
        after_threshold = _final_canary_threshold(final_canary_t, window)
    other_udp_like = 0
    win_counts = {"dns": 0, "udp_other": 0}
    win_dests: Counter = Counter()

    for pkt in packets:
        packet_total += 1
        if pkt.get("proto") == "other" and pkt.get("udp_hint"):
            other_udp_like += 1
        if pkt["iface"] != "lo" and pkt["dir"] == "Out":
            nonlo_out += 1
            ifaces.add(pkt["iface"])
        if (
            canary_port is not None
            and pkt["iface"] == "lo"
            and pkt["dir"] in ("Out", "In")
            and pkt["proto"] == "udp"
            and pkt["sport"] == canary_port
        ):
            canary_seen += 1
            if attribute(index, pkt) == ROLE_SELF:
                canary_ok += 1
                if window is not None and window[0] <= pkt["ts"] <= window[1]:
                    canary_in_window += 1
                if after_threshold is not None and pkt["ts"] >= after_threshold:
                    canary_after_last += 1
        cat = count_category(pkt)
        if cat is None:
            continue
        role = attribute(index, pkt)
        counts[role][cat] += 1
        if (
            role == ROLE_NONE
            and cat in win_counts
            and count_window is not None
            and count_window[0] <= pkt["ts"] <= count_window[1]
        ):
            win_counts[cat] += 1
            win_dests[(str(pkt.get("dst")), pkt.get("dport"))] += 1

    isolated_attr = sum(counts[ROLE_ISOLATED].values())
    if isolated_attr:
        fails.append(f"{isolated_attr} outbound packet(s) attributed to an isolated Servo host")

    meta = index.meta or {}
    self_netns = meta.get("self_netns")
    isolated_pids = 0
    unisolated_pids = 0
    netns_values: set = set()
    for pid, info in index.servo.items():
        if info.get("shared_ns_seen"):
            fails.append(f"Servo pid {pid} has a bwrap ancestor but shares the host netns")
        if info.get("ns_unknown_seen"):
            unobs.append(f"netns of isolated Servo pid {pid} could not be read")
        if info.get("isolated_seen"):
            isolated_pids += 1
            for n in info.get("netns_seen", ()):
                if n != self_netns:
                    netns_values.add(n)
        else:
            unisolated_pids += 1

    if index.meta is None:
        unobs.append("sampler meta record is missing")
    elif self_netns is None:
        unobs.append("netns of the sampler could not be read (isolation cannot be compared)")
    if index.end is None:
        unobs.append("sampler end record is missing (sampler died or was killed)")
    elif index.end.get("truncated"):
        unobs.append(f"sampler output was truncated ({index.end.get('reason')})")
    if index.sample_count == 0:
        unobs.append("no samples were recorded")
    if isolated_pids == 0:
        unobs.append("no isolated Servo host pid was observed during the capture window")
    if canary_port is None or canary_ok == 0:
        unobs.append(f"canary not observed and attributed to the sampler (seen={canary_seen}, attributed={canary_ok})")
    if tcpdump_exit is None:
        unobs.append("tcpdump exit status is missing")
    elif tcpdump_exit != 0:
        unobs.append(f"tcpdump exited with status {tcpdump_exit}")
    if convert_exit is None:
        unobs.append("pcap conversion exit status is missing")
    elif convert_exit != 0:
        unobs.append(f"pcap conversion exited with status {convert_exit} (capture text may be partial)")
    if kernel_dropped is None:
        unobs.append("kernel drop statistics are missing from tcpdump stderr")
    elif kernel_dropped != 0:
        unobs.append(f"tcpdump reported {kernel_dropped} packet(s) dropped by kernel")
    if packet_total == 0:
        unobs.append("no packets were captured")
    elif nonlo_out == 0:
        unobs.append("no outbound packet on a non-loopback interface was captured")
    # 解析できない行は packet_total に入らないため、行数との大きい方で打ち切り（-c）への到達を見る
    if max(packet_total, total_lines) >= max_packets:
        unobs.append(f"capture reached the packet limit ({max_packets})")
    if truncated_lines:
        unobs.append("packet text exceeded the line limit")
    if unparsed_udp_like:
        unobs.append(f"{unparsed_udp_like} unparsed packet line(s) look like UDP/DNS")
    if other_udp_like:
        unobs.append(f"{other_udp_like} packet(s) with unknown protocol look like UDP/DNS (e.g. non-first fragment)")
    if total_lines and unparsed > UNPARSED_MIN and unparsed / total_lines > UNPARSED_RATIO_LIMIT:
        unobs.append(f"too many unparsed packet lines ({unparsed}/{total_lines})")
    if index.bad_lines > 0:
        unobs.append(f"{index.bad_lines} malformed sample line(s)")
    if index.lines_over_limit:
        unobs.append(f"samples file exceeded the line limit ({index.lines_over_limit} line(s) not read)")
    if final_canary_invalid is not None:
        unobs.append(final_canary_invalid)
    elif index.end is not None and final_canary_t is None:
        unobs.append("sampler end record has no final_canary_t (the final canary was not sent)")
    elif final_canary_t is not None and canary_after_last == 0:
        unobs.append(
            f"final canary not observed after the last Servo sample (final_canary_t={final_canary_t}, "
            f"servo last={window[1] if window is not None else None}); capture may have stopped early"
        )
    if window is not None:
        if canary_ok > 0 and canary_in_window == 0:
            unobs.append("canary not observed within the Servo observation window")

    verdict = "fail" if fails else ("unobservable" if unobs else "pass")
    return {
        "verdict": verdict,
        "fail_reasons": fails,
        "unobservable_reasons": unobs,
        "counts": counts,
        "attributed_unobserved_note": "unattributed counts are informational and never affect the verdict",
        "servo_pids": {"isolated": isolated_pids, "unisolated": unisolated_pids},
        "servo_netns_inodes": sorted(str(n) for n in netns_values),
        "self_netns": self_netns,
        "samples": index.sample_count,
        "packets": packet_total,
        "nonlo_out_packets": nonlo_out,
        "canary": {
            "port": canary_port,
            "seen": canary_seen,
            "attributed_to_self": canary_ok,
            "in_servo_window": canary_in_window,
        },
        "unparsed_udp_like": unparsed_udp_like,
        "other_udp_like": other_udp_like,
        "unattributed_in_servo_window": {
            "window": list(window) if window is not None else None,
            "counted_window": list(count_window) if count_window is not None else None,
            "slack_seconds": WINDOW_SLACK_SECONDS,
            "dns": win_counts["dns"],
            "udp_other": win_counts["udp_other"],
            "destinations": [
                {"dst": d, "dport": port, "count": c}
                for (d, port), c in sorted(win_dests.items(), key=lambda kv: (-kv[1], kv[0][0], kv[0][1] or 0))[
                    :UNATTRIBUTED_TOP_N
                ]
            ],
            "destinations_total": len(win_dests),
            "destinations_omitted": max(0, len(win_dests) - UNATTRIBUTED_TOP_N),
        },
        "pass_basis_note": PASS_BASIS_NOTE,
        "interfaces": sorted(ifaces),
        "tcpdump_version": tcpdump_version,
        "tcpdump_exit": tcpdump_exit,
        "convert_exit": convert_exit,
        "kernel_dropped": kernel_dropped,
        "fd_read_retries": {
            "recovered": index.fd_diag_counts.get("recovered", 0),
            "vanished": index.fd_diag_counts.get("vanished", 0),
            "persistent": index.fd_diag_counts.get("persistent", 0),
            "records": list(index.fd_diags),
            "dropped": _fd_diag_dropped(index.end),
        },
    }


def _fd_diag_dropped(end: dict | None) -> int:
    """end 記録の fd_diag_dropped（上限超過で捨てた fd-diag の件数）。無い・不正なら 0。"""
    val = (end or {}).get("fd_diag_dropped", 0)
    return val if isinstance(val, int) and not isinstance(val, bool) else 0


# `_read_fd_inodes` が denied で返す段階（listdir / readlink）。fd-diag の stage を固定カテゴリに丸める
FD_DIAG_STAGES = ("listdir", "readlink")
FD_DIAG_MAX_ERRNO = 4095  # Linux の MAX_ERRNO

_DIAG_CATEGORIES = frozenset(
    {"self", "root", "other", "unknown", "servo", "unreadable", "gone"} | set("RSDZTtXxIKWP")
)


def _diag_cat(value: object) -> str:
    """Summary（Markdown）へ出す診断値を既知のカテゴリに限る。未知の値は unknown にして注入を防ぐ。"""
    return value if isinstance(value, str) and value in _DIAG_CATEGORIES else "unknown"


def render_summary(v: dict) -> str:
    """verdict を Step Summary 用の Markdown にする（ペイロード・環境変数の値は含めない）。"""
    lines = [
        "## host-capture (ubuntu)",
        f"- verdict: **{v['verdict']}**",
    ]
    for r in v["fail_reasons"]:
        lines.append(f"- FAIL: {r}")
    for r in v["unobservable_reasons"]:
        lines.append(f"- UNOBSERVABLE: {r}")
    lines += [
        f"- samples: {v['samples']} / packets: {v['packets']} / non-lo Out packets: {v['nonlo_out_packets']}",
        f"- Servo pids: isolated={v['servo_pids']['isolated']} unisolated={v['servo_pids']['unisolated']}"
        f" (netns inodes: {', '.join(v['servo_netns_inodes']) or 'n/a'}; sampler netns: {v['self_netns']})",
        f"- canary: seen={v['canary']['seen']} attributed_to_self={v['canary']['attributed_to_self']}",
        f"- interfaces: {', '.join(v['interfaces']) or 'n/a'} / tcpdump: {v['tcpdump_version'] or 'n/a'} (exit {v['tcpdump_exit']}, convert exit {v['convert_exit']}, kernel dropped {v['kernel_dropped']})",
        "",
        "Counts over the whole capture:",
        "",
        "| attribution | dns | udp_other | tcp_syn |",
        "| --- | ---: | ---: | ---: |",
    ]
    for role, c in v["counts"].items():
        lines.append(f"| {role} | {c['dns']} | {c['udp_other']} | {c['tcp_syn']} |")
    u = v["unattributed_in_servo_window"]
    win = u["window"]
    span = f"[{win[0]:.3f}..{win[1]:.3f}]" if win else "n/a"
    cwin = u.get("counted_window")
    counted = (
        f" (counted over [{cwin[0]:.3f}..{cwin[1]:.3f}], +/-{u.get('slack_seconds', 0)} s slack)" if cwin else ""
    )
    r = v.get("fd_read_retries") or {}
    lines += [
        "",
        f"- fd read retries (Servo pids): recovered={r.get('recovered', 0)} vanished={r.get('vanished', 0)}"
        f" persistent={r.get('persistent', 0)} (dropped {r.get('dropped', 0)})",
    ]
    for rec in r.get("records", []):
        if rec.get("outcome") != "persistent":
            continue
        # カテゴリ値だけを出す（/proc の生の文字列は入れない）
        first = rec.get("first") if isinstance(rec.get("first"), dict) else {}
        lines.append(
            f"  - persistent pid {int(rec.get('pid', 0))}: fd_dir_owner={_diag_cat(first.get('fd_dir_owner'))}"
            f" state={_diag_cat(first.get('state'))} exe={_diag_cat(first.get('exe'))}"
        )
    lines += [
        "",
        f"- unattributed DNS/UDP within the isolated Servo window {span}{counted}: dns={u['dns']} udp_other={u['udp_other']}",
        "",
        "| dst | dport | count |",
        "| --- | ---: | ---: |",
    ]
    for d in u["destinations"]:
        lines.append(f"| {d['dst']} | {d['dport']} | {d['count']} |")
    if not u["destinations"]:
        lines.append("| none | | |")
    if u["destinations_omitted"]:
        lines.append(f"(+{u['destinations_omitted']} more)")
    lines += ["", f"> {v['pass_basis_note']}", ""]
    return "\n".join(lines) + "\n"


# --------------------------------------------------------------------------------------
# judge の入出力
# --------------------------------------------------------------------------------------


def load_packets(path: Path, max_packets: int) -> dict:
    """tcpdump テキストを読む。行数・行長は上限つき。

    戻り値のキー: packets・unparsed・unparsed_udp_like（解析できない行のうち UDP・DNS の痕跡があるもの）・
    total_lines・truncated。
    """
    packets: list[dict] = []
    unparsed = 0
    unparsed_udp_like = 0
    total = 0
    truncated = False
    with path.open("r", encoding="utf-8", errors="replace") as fh:
        for raw in fh:
            if not raw.strip():
                continue
            total += 1
            if total > max_packets + 1000:
                truncated = True
                break
            pkt = parse_tcpdump_line(raw)
            if pkt is None:
                unparsed += 1
                if suspicious_udp_text(raw):
                    unparsed_udp_like += 1
            else:
                packets.append(pkt)
    return {
        "packets": packets,
        "unparsed": unparsed,
        "unparsed_udp_like": unparsed_udp_like,
        "total_lines": total,
        "truncated": truncated,
    }


def load_samples(path: Path) -> SampleIndex:
    index = SampleIndex()
    count = 0
    with path.open("r", encoding="utf-8", errors="replace") as fh:
        for raw in fh:
            count += 1
            if not raw.strip():
                continue
            if count > MAX_SAMPLES * 2:
                index.lines_over_limit += 1
                continue
            try:
                rec = json.loads(raw[:MAX_JSONL_LINE_CHARS])
            except ValueError:
                index.bad_lines += 1
                continue
            if isinstance(rec, dict):
                index.feed(rec)
            else:
                index.bad_lines += 1
    return index


_DROPPED_RE = re.compile(r"^(\d+) packets? dropped by kernel", re.MULTILINE)


def parse_kernel_dropped(stderr_text: str) -> int | None:
    """tcpdump 終了時の統計行 `N packets dropped by kernel` から N を返す。行が無ければ None（確認不能）。"""
    m = _DROPPED_RE.search(stderr_text)
    return int(m.group(1)) if m else None


def _read_int(path: Path) -> int | None:
    if not path.is_file():
        return None
    try:
        return int(path.read_text().strip())
    except ValueError:
        return None


def cmd_judge(args: argparse.Namespace) -> int:
    out_dir = Path(args.out_dir)
    text = out_dir / "capture.txt"
    samples = out_dir / "samples.jsonl"
    exit_file = out_dir / "tcpdump.exit"
    version = ""
    vfile = out_dir / "tcpdump.version"
    if vfile.is_file():
        version = vfile.read_text(encoding="utf-8", errors="replace").splitlines()[0][:80] if vfile.stat().st_size else ""
    tcpdump_exit = _read_int(exit_file)
    convert_exit = _read_int(out_dir / "convert.exit")
    kernel_dropped: int | None = None
    stderr_file = out_dir / "tcpdump.stderr"
    if stderr_file.is_file():
        kernel_dropped = parse_kernel_dropped(stderr_file.read_text(encoding="utf-8", errors="replace")[-65536:])
    if text.is_file() and samples.is_file():
        loaded = load_packets(text, args.max_packets)
        index = load_samples(samples)
    else:
        loaded = {"packets": [], "unparsed": 0, "unparsed_udp_like": 0, "total_lines": 0, "truncated": False}
        index = SampleIndex()
    verdict = judge(
        loaded["packets"],
        index,
        unparsed=loaded["unparsed"],
        unparsed_udp_like=loaded["unparsed_udp_like"],
        total_lines=loaded["total_lines"],
        max_packets=args.max_packets,
        tcpdump_exit=tcpdump_exit,
        tcpdump_version=version,
        truncated_lines=loaded["truncated"],
        convert_exit=convert_exit,
        kernel_dropped=kernel_dropped,
    )
    if not text.is_file() or not samples.is_file():
        verdict["unobservable_reasons"].append("capture text or samples file is missing")
        if verdict["verdict"] == "pass":
            verdict["verdict"] = "unobservable"
    (out_dir / "verdict.json").write_text(json.dumps(verdict, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    summary = render_summary(verdict)
    (out_dir / "summary.md").write_text(summary, encoding="utf-8")
    sys.stdout.write(summary)
    return 0 if verdict["verdict"] == "pass" else 1


# --------------------------------------------------------------------------------------
# サンプラー（/proc を読む非特権プロセス）
# --------------------------------------------------------------------------------------


def _read_ns_net(pid: int) -> int | None:
    try:
        target = os.readlink(f"/proc/{pid}/ns/net")
    except OSError:
        return None
    m = re.fullmatch(r"net:\[(\d+)\]", target)
    return int(m.group(1)) if m else None


def _ppid(pid: int) -> int | None:
    try:
        with open(f"/proc/{pid}/stat", "rb") as fh:
            data = fh.read(4096).decode("utf-8", "replace")
    except OSError:
        return None
    tail = data.rsplit(")", 1)[-1].split()
    try:
        return int(tail[1])
    except (IndexError, ValueError):
        return None


def _exe(pid: int) -> str | None:
    try:
        return os.path.realpath(os.readlink(f"/proc/{pid}/exe"))
    except OSError:
        return None


def _has_bwrap_ancestor(pid: int) -> bool:
    cur = _ppid(pid)
    for _ in range(MAX_ANCESTOR_DEPTH):
        if cur is None or cur <= 1:
            return False
        exe = _exe(cur)
        if exe is not None and os.path.basename(exe) == "bwrap":
            return True
        cur = _ppid(cur)
    return False


def _list_pids() -> tuple[list[int], bool, bool]:
    """(pids, 上限到達, 読み取り失敗)。上限到達・`/proc` の列挙失敗は観測が不完全なので True（呼び出し側が unobservable にする）。"""
    pids: list[int] = []
    try:
        for name in os.listdir("/proc"):
            if name.isdigit():
                if len(pids) >= MAX_PROCESSES:
                    return pids, True, False
                pids.append(int(name))
    except OSError:
        return pids, False, True
    return pids, False, False


def _proc_starttime(pid: int) -> int | None:
    """`/proc/<pid>/stat` の starttime（22 番目）。pid 再利用の検出に使う。読めなければ None。"""
    try:
        with open(f"/proc/{pid}/stat", "rb") as fh:
            data = fh.read(4096).decode("utf-8", "replace")
    except OSError:
        return None
    tail = data.rsplit(")", 1)[-1].split()
    try:
        return int(tail[19])
    except (IndexError, ValueError):
        return None


def _read_fd_inodes(pid: int) -> tuple[str, list[int], bool, str | None, int | None]:
    """1 pid の socket inode を読む。戻り値は (種別, inode 列, fd 数上限到達, 失敗の段階, errno)。

    種別は ok / denied（PermissionError。部分結果は捨てる）/ gone（FileNotFoundError・ProcessLookupError）/
    other（その他の OSError）。fd 個別の readlink が FileNotFoundError（fd が閉じられた）なら読み飛ばす。
    """
    try:
        names = os.listdir(f"/proc/{pid}/fd")
    except PermissionError as exc:
        return "denied", [], False, "listdir", exc.errno
    except (FileNotFoundError, ProcessLookupError):
        return "gone", [], False, None, None
    except OSError as exc:
        return "other", [], False, "listdir", exc.errno
    hit = len(names) > MAX_FDS_PER_PROCESS
    inodes: list[int] = []
    for name in names[:MAX_FDS_PER_PROCESS]:
        try:
            target = os.readlink(f"/proc/{pid}/fd/{name}")
        except PermissionError as exc:
            return "denied", [], hit, "readlink", exc.errno
        except OSError:
            continue
        if target.startswith("socket:["):
            try:
                inodes.append(int(target[8:-1]))
            except ValueError:
                continue
    return "ok", inodes, hit, None, None


def _fd_denied_snapshot(pid: int, servo_exe: str | None, stage: str | None, errno_: int | None) -> dict:
    """fd が読めなかった瞬間の対象 pid の状態（#1493）。値は整数・真偽値・固定カテゴリに限る。

    uid・パス・コマンド名などの生の文字列は出さない（samples.jsonl・Step Summary への注入と機微値の露出を防ぐ）。
    `fd_dir_owner=root` は、その瞬間タスクが dumpable でなかった（または mm が無かった）ことを示す。
    `self` なら DAC は通っており、EACCES は ptrace・LSM 側から来ている（読み直しでは対処にならない）。
    採取の各項目は個別に OSError を吸収する（採取中に pid が消えることがある）。
    """
    snap: dict = {
        "fd_dir_owner": "unknown",
        "state": "unknown",
        "threads": None,
        "nspid": None,
        "uid_matches_self": None,
        "exe": "unreadable",
        "userns_differs": None,
        "stage": stage,
        "errno": errno_,
    }
    try:
        uid = os.stat(f"/proc/{pid}/fd").st_uid
        snap["fd_dir_owner"] = "self" if uid == os.geteuid() else ("root" if uid == 0 else "other")
    except OSError:
        pass
    try:
        with open(f"/proc/{pid}/status", "rb") as fh:
            text = fh.read(4096).decode("utf-8", "replace")
        for ln in text.splitlines():
            key, _, val = ln.partition(":")
            val = val.strip()
            if key == "State":
                m = re.match(r"([RSDZTtXxIKWP])\b", val)
                snap["state"] = m.group(1) if m else "unknown"
            elif key == "Threads":
                if val.isdigit():
                    snap["threads"] = min(int(val), 100_000)
            elif key == "NSpid":
                parts = val.split()[:8]
                if parts and all(x.isdigit() for x in parts):
                    snap["nspid"] = [int(x) for x in parts]
            elif key == "Uid":
                parts = val.split()
                if len(parts) >= 2 and parts[1].isdigit():
                    snap["uid_matches_self"] = int(parts[1]) == os.geteuid()
    except OSError:
        pass
    if servo_exe is not None:
        try:
            exe = os.path.realpath(os.readlink(f"/proc/{pid}/exe"))
            snap["exe"] = "servo" if exe == servo_exe else "other"
        except FileNotFoundError:
            snap["exe"] = "gone"
        except OSError:
            snap["exe"] = "unreadable"
    try:
        snap["userns_differs"] = os.readlink(f"/proc/{pid}/ns/user") != os.readlink("/proc/self/ns/user")
    except OSError:
        pass
    return snap


def _socket_owners(
    pids: list[int],
    servo_pids: frozenset[int] | set[int] = frozenset(),
    servo_exe: str | None = None,
    diag: list | None = None,
    *,
    clock: _Clock = SYSTEM_CLOCK,
) -> tuple[dict[int, int], bool, list[str]]:
    """読める範囲の fd から inode -> pid を作る（同一ユーザーのプロセスだけ。root は読めず帰属不能）。

    戻り値は (owners, fd 数の上限で打ち切ったか, 読み取り失敗の記録)。記録するのは Servo ホストと同定済みの pid の
    fd が PermissionError で読めず、上限つきの読み直しでも読めなかったときだけ（`proc-read-error:fd-<pid>`）。
    消えたプロセス（FileNotFoundError・ProcessLookupError）は通常の競合で記録しない。他ユーザーの pid は帰属不能が
    前提で記録せず、読み直しもしない。

    #1493: 従来は「`/proc/<pid>/exe` の readlink と fd の listdir は同じ ptrace-read 検査を通るため、`_exe` で同定
    できた pid の fd が EACCES になることは通常ない」と見ていたが、run 41819946868（ubuntu-noaa）で同定済み pid の
    fd が一時的に読めず unobservable になった。原因は未確認の仮説で、`fs/proc/base.c` の task_dump_owner は
    dumpable でない（または mm が無い）タスクの /proc/<pid>/ 配下の所有者を root にし、fd ディレクトリは DAC だけで
    判定されるため、exec 中・終了処理中の瞬間に exe は読めても fd は読めない非対称が起きうる、というもの。
    そこで Servo pid の denied に限り、診断を採ってから FD_READ_RETRIES 回・pid ごとに最初の denied から
    FD_READ_RETRY_BUDGET_SECONDS 以内（1 回の走査の待ちの累計は FD_READ_RETRY_SAMPLE_BUDGET_SECONDS まで。最後の読み取り時間は超えうる）で読み直す
    （#1497: 走査全体で 1 つの締切にすると後ろの pid が読み直されない）。結果は recovered（読めた。結果を使う）・vanished（消えた、または starttime が変わった）・
    persistent（読めないまま。従来どおり記録して観測不能）で、診断は diag に fd-diag 記録として足す。
    starttime が初回・再読みのどちらかで読めないときは pid の再利用を判別できないため recovered にせず persistent
    にする。fd-diag の stop は読み直しを止めた理由（FD_DIAG_STOPS）。
    persistent を合格に変えることはない（持続する失敗の経路は変えない）。
    """
    owners: dict[int, int] = {}
    errors: list[str] = []
    hit = False
    spent = 0.0  # この走査で読み直しに使った時間の累計

    def _note(pid: int) -> None:
        tag = f"proc-read-error:fd-{pid}"
        if pid in servo_pids and tag not in errors:
            errors.append(tag)

    def _emit_diag(
        pid: int, outcome: str, stop: str, attempts: int, started: float, first: dict, last: dict | None
    ) -> None:
        if diag is not None:
            diag.append(
                {
                    "type": "fd-diag",
                    "t": clock.wall(),
                    "pid": pid,
                    "outcome": outcome,
                    "stop": stop,
                    "attempts": attempts,
                    "elapsed": round(clock.mono() - started, 4),
                    "first": first,
                    "last": last,
                }
            )

    for pid in pids:
        kind, inodes, over, stage, errno_ = _read_fd_inodes(pid)
        # 上限到達は denied・読み直しの結果によらず先に累積する（未走査 fd が残る観測欠落を落とさない）
        hit = hit or over
        if kind == "denied" and pid in servo_pids:
            started = clock.mono()
            start_time = _proc_starttime(pid)
            first = _fd_denied_snapshot(pid, servo_exe, stage, errno_)
            sample_left = max(0.0, FD_READ_RETRY_SAMPLE_BUDGET_SECONDS - spent)
            sample_bound = sample_left < FD_READ_RETRY_BUDGET_SECONDS  # 締切が累計の残りで決まったか
            pid_deadline = started + min(FD_READ_RETRY_BUDGET_SECONDS, sample_left)
            attempts = 0
            outcome = "persistent"
            stop = "attempts"
            while True:
                if attempts >= FD_READ_RETRIES:
                    stop = "attempts"
                    break
                remaining = pid_deadline - clock.mono()
                if remaining <= 0:
                    stop = "sample-budget" if sample_bound else "pid-budget"
                    break
                # sleep は締切までの残りに切り詰め、待ちの累計が予算を超えないようにする
                last_wait = remaining <= FD_READ_RETRY_SECONDS
                clock.sleep(min(FD_READ_RETRY_SECONDS, remaining))
                attempts += 1
                kind, inodes, over, stage, errno_ = _read_fd_inodes(pid)
                hit = hit or over
                if kind == "denied":
                    if last_wait and attempts < FD_READ_RETRIES:
                        # 切り詰めた最後の待ちの後は予算切れ。浮動小数点の誤差で極小 sleep の余分な 1 回が
                        # 起きるのを防ぐ（実時計の sleep 遅延でも判定が決定的になる）
                        stop = "sample-budget" if sample_bound else "pid-budget"
                        break
                    continue
                now_start = _proc_starttime(pid)
                if kind == "gone":
                    outcome, stop, kind, inodes, over = "vanished", "gone", "gone", [], False
                elif kind == "ok":
                    if start_time is None or now_start is None:
                        # pid の再利用を判別できない。recovered にせず fail-closed（#1497）
                        stop = "starttime-unknown"
                    elif now_start != start_time:
                        outcome, stop, kind, inodes, over = "vanished", "reused", "gone", [], False
                    else:
                        outcome, stop = "recovered", "read-ok"
                else:
                    stop = "other-error"
                break
            spent += max(0.0, clock.mono() - started)
            last = _fd_denied_snapshot(pid, servo_exe, stage, errno_) if outcome == "persistent" else None
            _emit_diag(pid, outcome, stop, attempts, started, first, last)
            if outcome == "persistent":
                _note(pid)
                continue
        elif kind == "denied":
            continue
        if kind != "ok":
            continue
        for inode in inodes:
            owners.setdefault(inode, pid)
    return owners, hit, errors


def _ipv6_absent() -> bool:
    """IPv6 が無効（カーネルが IPv6 を持たない）と確かめられたときだけ True。確認不能は不在と扱わない。

    tcp6・udp6 の不在を許す根拠: `ipv6.disable=1` のカーネルでは `/proc/net/if_inet6` も tcp6・udp6 も存在しない。
    sysctl の `disable_ipv6` では表は存在する。したがって `if_inet6` があるのに表が無い状態（片方だけ無い場合を含む）は
    異常として記録する。
    """
    try:
        os.stat("/proc/self/net/if_inet6")
    except FileNotFoundError:
        return True
    except OSError:
        return False
    return False


def _read_table(name: str, optional: bool = False) -> tuple[list[tuple[int, int]], bool, bool, int]:
    """`/proc/self/net/<name>` から ((local_port, inode) の列, 上限到達, 読み取り失敗, 解析できない行数) を返す。

    読み取り失敗は表の内容が空だったこととは別物で、呼び出し側が観測不能として記録する。
    optional な表（tcp6・udp6。`_ipv6_absent` で IPv6 無効と確かめたときだけ）の不在（FileNotFoundError）は失敗に数えない。
    解析できない行数はヘッダ（1 行目）と空白だけの行を除いて数える（捨てずに呼び出し側が記録する）。
    """
    rows: list[tuple[int, int]] = []
    hit = False
    failed = False
    unparsed = 0
    try:
        with open(f"/proc/self/net/{name}", "r", encoding="ascii", errors="replace") as fh:
            for i, line in enumerate(fh):
                if i > MAX_NET_LINES:
                    hit = True
                    break
                parsed = egress_guard.parse_net_line(line)
                if parsed is None:
                    if i > 0 and line.strip():
                        unparsed += 1
                elif parsed[3] != 0:
                    rows.append((parsed[4], parsed[3]))
    except FileNotFoundError:
        failed = not optional
    except OSError:
        failed = True
    return rows, hit, failed, unparsed


def take_sample(
    servo_exe: str,
    self_pid: int,
    self_netns: int | None,
    diag: list | None = None,
    *,
    clock: _Clock = SYSTEM_CLOCK,
) -> tuple[list[dict], list[dict], list[str]]:
    """1 回分の走査。戻り値は (servo, sockets, limits)。servo は pid・bwrap 祖先の有無・netns。

    limits は走査上限に到達した理由（空なら完全な走査）。呼び出し側のサンプラーが
    end.truncated へ累積し、judge が unobservable にする（観測の欠落を合格にしない）。
    diag を渡すと Servo pid の fd 読み直しの診断（fd-diag 記録。#1493）がそこへ足される。
    """
    limits: list[str] = []
    pids, hit, failed = _list_pids()
    if hit:
        limits.append("processes-limit")
    if failed:
        limits.append("proc-read-error:list-pids")
    servo: list[dict] = []
    servo_roles: dict[int, str] = {}
    for pid in pids:
        if _exe(pid) != servo_exe:
            continue
        bwrap = _has_bwrap_ancestor(pid)
        netns = _read_ns_net(pid)
        servo.append({"pid": pid, "bwrap": bwrap, "netns": netns})
        isolated = bwrap or (netns is not None and self_netns is not None and netns != self_netns)
        servo_roles[pid] = ROLE_ISOLATED if isolated else ROLE_UNISOLATED
    owners, hit, fd_errors = _socket_owners(pids, frozenset(servo_roles), servo_exe, diag, clock=clock)
    if hit:
        limits.append("fds-per-process-limit")
    limits.extend(fd_errors)
    sockets: list[dict] = []
    ipv6_absent = _ipv6_absent()
    for table, proto, optional in (
        ("udp", "udp", False),
        ("udp6", "udp", ipv6_absent),
        ("tcp", "tcp", False),
        ("tcp6", "tcp", ipv6_absent),
    ):
        rows, hit, failed, unparsed = _read_table(table, optional)
        if hit:
            limits.append("net-lines-limit")
        if failed:
            limits.append(f"proc-read-error:net-{table}")
        if unparsed:
            limits.append(f"proc-read-error:net-{table}-unparsed")
        for port, inode in rows:
            pid = owners.get(inode)
            if pid is None:
                continue
            if pid in servo_roles:
                role = servo_roles[pid]
            elif pid == self_pid:
                role = ROLE_SELF
            else:
                role = ROLE_OTHER
            if proto == "tcp" and role == ROLE_OTHER:
                continue  # 容量のため。他プロセスの TCP は合否に使わない
            sockets.append({"proto": proto, "port": port, "pid": pid, "role": role})
    sockets.sort(key=lambda s: (s["proto"], s["port"], s["pid"]))
    return servo, sockets, limits


def run_sampler(
    servo_exe: str,
    out: Path,
    interval: float,
    stop_file: Path | None,
    max_seconds: float | None = None,
    *,
    clock: _Clock = SYSTEM_CLOCK,
) -> int:
    """サンプラー本体。カナリアを張りつつ、変化があったときだけ sample、無変化は tick を書く。"""
    stop = {"flag": False}

    def _on_term(_sig, _frm) -> None:
        stop["flag"] = True

    signal.signal(signal.SIGTERM, _on_term)
    signal.signal(signal.SIGINT, _on_term)

    recv = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    recv.bind(("127.0.0.1", 0))
    recv.setblocking(False)
    send = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    send.bind(("127.0.0.1", 0))
    canary_port = send.getsockname()[1]
    recv_addr = recv.getsockname()
    self_pid = os.getpid()
    self_netns = _read_ns_net(self_pid)

    written = 0
    truncated = False
    reason = "stop"
    reasons: list[str] = []
    diag_emitted = 0
    diag_dropped = 0

    def _mark(r: str) -> None:
        nonlocal truncated, reason
        if r not in reasons:
            reasons.append(r)
        truncated, reason = True, ",".join(reasons)

    prev: tuple | None = None
    next_canary = clock.wall() + 1.0
    canary_sent = 0
    started = clock.wall()
    with out.open("w", encoding="utf-8") as fh:

        def emit(rec: dict) -> None:
            nonlocal written
            line = json.dumps(rec, separators=(",", ":")) + "\n"
            written += len(line)
            fh.write(line)
            fh.flush()

        def do_sample() -> None:
            nonlocal prev, diag_emitted, diag_dropped
            try:
                while True:
                    recv.recvfrom(256)
            except (BlockingIOError, OSError):
                pass
            diag: list[dict] = []
            servo, sockets, limits = take_sample(servo_exe, self_pid, self_netns, diag, clock=clock)
            for rec in diag:
                if diag_emitted < MAX_FD_DIAG_RECORDS:
                    emit(rec)
                    diag_emitted += 1
                else:
                    diag_dropped += 1
            for lim in limits:
                _mark(lim)
            if len(sockets) > MAX_SOCKETS_PER_SAMPLE:
                sockets = sockets[:MAX_SOCKETS_PER_SAMPLE]
                _mark("sockets-per-sample-limit")
            state = (
                tuple((s["pid"], s["bwrap"], s["netns"]) for s in servo),
                tuple((s["proto"], s["port"], s["pid"], s["role"]) for s in sockets),
            )
            if state != prev:
                emit({"type": "sample", "t": clock.wall(), "servo": servo, "sockets": sockets})
                prev = state
            else:
                emit({"type": "tick", "t": clock.wall()})

        emit(
            {
                "type": "meta",
                "pid": self_pid,
                "self_netns": self_netns,
                "servo_exe": servo_exe,
                "canary_port": canary_port,
                "interval": interval,
                "started": started,
            }
        )
        n = 0
        while not stop["flag"]:
            now = clock.wall()
            if stop_file is not None and stop_file.exists():
                break
            if max_seconds is not None and now - started > max_seconds:
                break
            if n >= MAX_SAMPLES or written >= MAX_SAMPLE_BYTES:
                _mark("sample-or-byte-limit")
                break
            if now >= next_canary and canary_sent < 100_000:
                try:
                    send.sendto(_CANARY_PAYLOAD, recv_addr)
                    canary_sent += 1
                except OSError:
                    pass
                next_canary = now + 2.0
            do_sample()
            n += 1
            clock.sleep(interval)
        # 停止要求の後、最後に 1 回だけ実サンプルを取ってから最終カナリアを送る。カナリアのソケットの存在区間が
        # 最終カナリアまで続くため、judge が自己帰属として数えられる。送信直前の時刻を final_canary_t として
        # end 記録に残す（pcap 上の時刻は送信後なので、これ以上になる）。judge は Servo の最後のサンプル以降に
        # final_canary_t 以降のカナリアが観測されたことで「キャプチャが観測区間の終端を覆う」ことを確かめる
        # （#1470 の回帰。終端を runner の通常通信ではなく自前のカナリアで確かめる。#1473 P3 の一部）。
        # workflow は end 記録を確認した後、有界の猶予を置いてから tcpdump を止める。
        do_sample()
        n += 1
        final_canary_t: float | None = None
        t_before = clock.wall()
        try:
            send.sendto(_CANARY_PAYLOAD, recv_addr)
            final_canary_t = t_before
        except OSError:
            pass
        emit(
            {
                "type": "end",
                "t": clock.wall(),
                "truncated": truncated,
                "reason": reason,
                "samples": n,
                "final_canary_t": final_canary_t,
                "fd_diag_dropped": diag_dropped,
            }
        )
    recv.close()
    send.close()
    return 0


# 1 回のポーリングで、MAX_PACKET_LINE_CHARS を超えた行の読み捨てに使ってよい総量（文字数。#1481）。
# text モードの readline(n) は文字数で数える。tcpdump の出力は ASCII のためほぼバイト数と同じ。
# 改行のない出力が続いても kill されるまで CPU を使い続けないための上限で、通常の pcap の出力量には届かない。
WAIT_CANARY_MAX_DISCARD_CHARS = 1 << 20
# 時間切れの診断に載せる tcpdump の stderr の読み取り量（末尾のバイト数）と、載せる文字数の上限（#1481）
WAIT_CANARY_STDERR_TAIL_BYTES = 4096
WAIT_CANARY_STDERR_NOTE_CHARS = 256

# _poll_pcap_once の未観測の理由（出力文字列なので英語）。POLL_OBSERVED だけが observed=True になる。
POLL_OBSERVED = "observed"
POLL_SPAWN_FAILED = "spawn-failed"
POLL_BUDGET_KILLED = "budget-killed"
POLL_PACKET_LIMIT = "packet-limit"
POLL_DISCARD_LIMIT = "discard-limit"
POLL_EOF = "eof"
POLL_READ_ERROR = "read-error"


@dataclass(frozen=True)
class PollResult:
    """_poll_pcap_once の結果（REPAIR-4: 真偽値だけを返さない。#1481）。

    cmd_wait_final_canary が分岐に使い、時間切れの診断メッセージに reason・detail を載せる。
    dataclass は常に truthy なので `if result:` が常に成功扱い（fail-open）になる。
    それを防ぐため真偽値としての評価は TypeError にし、呼び出し側に `.observed` の明示参照を強制する。
    """

    observed: bool
    reason: str
    detail: str = ""

    def __bool__(self) -> bool:
        raise TypeError("PollResult must not be evaluated as bool; use .observed")

    def describe(self) -> str:
        return f"{self.reason} ({self.detail})" if self.detail else self.reason


def _read_bounded_line(stream, discard_remaining: int = WAIT_CANARY_MAX_DISCARD_CHARS) -> tuple[str, int]:
    """1 行を MAX_PACKET_LINE_CHARS までだけ保持して返す。超過分は改行・EOF・discard_remaining まで読み捨てる。

    戻り値は (保持した行, 今回読み捨てた文字数)。読み捨てた量が discard_remaining を超えたら打ち切るので、
    呼び出し側（_poll_pcap_once）は累計が上限を超えたかで POLL_DISCARD_LIMIT を判定できる（#1481）。
    """
    raw = stream.readline(MAX_PACKET_LINE_CHARS + 1)
    discarded = max(0, len(raw) - MAX_PACKET_LINE_CHARS)
    if len(raw) > MAX_PACKET_LINE_CHARS and not raw.endswith("\n"):
        while discarded <= discard_remaining:
            rest = stream.readline(MAX_PACKET_LINE_CHARS + 1)
            discarded += len(rest)
            if not rest or rest.endswith("\n"):
                break
    return raw[:MAX_PACKET_LINE_CHARS], discarded


class _StderrTail:
    """tcpdump の stderr を読み続け、末尾 WAIT_CANARY_STDERR_TAIL_BYTES バイトだけをメモリに保持する（#1483）。

    _poll_pcap_once から使う。PIPE は読まないと詰まって tcpdump の stdout も止まるため専用スレッドで読む。
    ディスクには書かず、メモリは「末尾の上限 + 1 チャンク」で頭打ちになる。select を使わないので 3 OS で動く。
    ストリームの close は読みスレッド自身が行う（読み取り中の別スレッドからの close は内部ロックで待たされうるため）。
    """

    _CHUNK = 4096

    def __init__(self, stream) -> None:
        self._stream = stream
        self._buf = bytearray()
        self._lock = threading.Lock()
        self._thread: threading.Thread | None = None

    def start(self) -> None:
        t = threading.Thread(target=self._run, daemon=True)
        t.start()
        self._thread = t

    def _run(self) -> None:
        stream = self._stream
        # text=True の TextIOWrapper はバイト数で数えるため下の binary ストリームを使う
        raw = getattr(stream, "buffer", stream)
        read = getattr(raw, "read1", None) or raw.read
        try:
            while True:
                chunk = read(self._CHUNK)
                if not chunk:
                    break
                if isinstance(chunk, str):
                    chunk = chunk.encode("utf-8", errors="replace")
                with self._lock:
                    self._buf += chunk
                    if len(self._buf) > WAIT_CANARY_STDERR_TAIL_BYTES:
                        del self._buf[:-WAIT_CANARY_STDERR_TAIL_BYTES]
        except (OSError, ValueError):
            pass
        finally:
            try:
                stream.close()
            except (OSError, ValueError):
                pass

    def join(self, timeout: float) -> bool:
        """読みスレッドの終了を timeout 秒まで待つ。終了した（または未起動）なら True。未起動ならストリームをここで閉じる。"""
        t = self._thread
        if t is None:
            try:
                self._stream.close()
            except (OSError, ValueError, AttributeError):
                pass
            return True
        t.join(timeout)
        return not t.is_alive()

    def snapshot(self) -> bytes:
        with self._lock:
            return bytes(self._buf)


def _summarize_stderr(data: bytes) -> str:
    """tcpdump の stderr の末尾を 1 行・短い文字列にする（外部由来なので印字可能 ASCII 以外を除く。#1481・#1483）。

    戻り値はサニタイズ後も信頼できない参考情報（`[31m` や `::error::` は残る）。判定・コマンド・パスに使わない。
    """
    text = data[-WAIT_CANARY_STDERR_TAIL_BYTES:].decode("utf-8", errors="replace")
    text = re.sub(r"[^\x20-\x7e]+", " ", text)
    text = re.sub(r" {2,}", " ", text).strip()
    return text[-WAIT_CANARY_STDERR_NOTE_CHARS:]


def _kill_if_running(proc, killed: threading.Event) -> None:
    """予算切れの Timer から呼ぶ。まだ動いている tcpdump だけを kill して killed を立てる（#1483）。

    自然 EOF（プロセス終了済み）の直後に発火しても budget-killed と誤分類しないよう、poll() で生死を見てから立てる。
    残る競合（stdout は閉じたがプロセスは未終了の一瞬、poll と kill の間の終了）は診断の分類だけの話で、
    observed=False は変わらず合否は judge が決めるため許容する。Popen.poll() は内部ロックでスレッド間安全。
    kill の失敗は握る（finally でもう一度 kill する）。
    """
    try:
        if proc.poll() is not None:
            return
        killed.set()
        proc.kill()
    except (OSError, ValueError):
        pass


def _poll_pcap_once(
    pcap: Path,
    canary_port: int,
    final_canary_t: float,
    window: tuple[float, float] | None,
    budget: float = WAIT_CANARY_POLL_MAX_SECONDS,
    max_packets: int = DEFAULT_MAX_PACKETS,
) -> PollResult:
    """pcap を tcpdump -r で 1 回読み、最終カナリアがあるかを理由つきで返す（#1476・#1481・#1483）。

    stdout を 1 行ずつ有界に読み、最終カナリアを見つけた時点か件数上限（max_packets）で打ち切る。
    `budget` 秒を過ぎたら Timer が tcpdump を kill して EOF にする（select は Windows のパイプで使えないため）。
    書きかけの pcap は truncation で tcpdump が非 0 を返すことがあるが手前のパケットは出るため、終了コードは判定に使わない。
    observed=True は最終カナリアを検出した経路だけ。起動失敗・kill・件数上限・読み捨て上限・EOF・読みの例外は
    すべて observed=False（fail-closed）で、reason が原因を区別する。引数はリスト（shell=False）。port は呼び出し側が int・範囲を検証済み。
    stderr は PIPE で受け、専用スレッド（_StderrTail）が末尾だけをメモリに保持する（ディスクを使わない）。終了後にその末尾を診断（detail）に使う。
    Popen 成功後の想定外の例外（スレッド・Timer の起動失敗を含む）は後始末の後に伝播し、成功扱いにはならない。
    """
    threshold = _final_canary_threshold(final_canary_t, window)
    try:
        proc = subprocess.Popen(
            [*TCPDUMP_ARGV, "-r", str(pcap), "-nn", "-tt", f"udp and src port {canary_port}"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            errors="replace",
        )
    except (OSError, subprocess.SubprocessError, ValueError) as e:
        msg = getattr(e, "strerror", None) or ""
        return PollResult(False, POLL_SPAWN_FAILED, f"{type(e).__name__}{': ' + msg if msg else ''}")
    killed = threading.Event()
    tail = _StderrTail(proc.stderr)
    timer: threading.Timer | None = None
    reason = POLL_PACKET_LIMIT
    detail = f"no final canary in the first {max_packets} lines"
    observed = False
    err_note = ""
    exited = True
    try:
        tail.start()
        t = threading.Timer(max(0.0, budget), _kill_if_running, args=(proc, killed))
        t.daemon = True
        t.start()
        timer = t  # 起動に成功してから束縛する（未起動の cancel を避ける）
        discarded_total = 0
        for _ in range(max_packets):
            raw, discarded = _read_bounded_line(proc.stdout, WAIT_CANARY_MAX_DISCARD_CHARS - discarded_total)
            discarded_total += discarded
            if discarded_total > WAIT_CANARY_MAX_DISCARD_CHARS:
                reason, detail = POLL_DISCARD_LIMIT, f"discarded more than {WAIT_CANARY_MAX_DISCARD_CHARS} chars of overlong lines"
                break
            if not raw:
                if killed.is_set():
                    reason, detail = POLL_BUDGET_KILLED, f"tcpdump did not finish within {budget:g}s"
                else:
                    reason, detail = POLL_EOF, ""
                break
            pkt = parse_tcpdump_line(raw)
            if pkt is not None and _is_final_canary(pkt, canary_port, threshold):
                observed, reason, detail = True, POLL_OBSERVED, ""
                break
    except (OSError, ValueError) as e:
        reason, detail = POLL_READ_ERROR, type(e).__name__
    finally:
        if timer is not None:
            timer.cancel()
        if proc.poll() is None:
            proc.kill()
        try:
            proc.wait(timeout=WAIT_CANARY_KILL_GRACE_SECONDS)
        except subprocess.TimeoutExpired:
            exited = False
            sys.stderr.write(f"warning: tcpdump did not exit within {WAIT_CANARY_KILL_GRACE_SECONDS:g}s after kill\n")
        try:
            proc.stdout.close()
        except (OSError, ValueError):
            pass
        if not tail.join(WAIT_CANARY_KILL_GRACE_SECONDS):
            sys.stderr.write("warning: tcpdump stderr reader did not finish after kill\n")
        if not observed:
            err_note = _summarize_stderr(tail.snapshot())
    if reason == POLL_EOF:
        code = proc.returncode if exited and isinstance(proc.returncode, int) else "unknown"
        detail = f"tcpdump exited with {code} before the final canary"
    if err_note:
        detail = f"{detail}; stderr: {err_note}" if detail else f"stderr: {err_note}"
    return PollResult(observed, reason, detail)


def cmd_wait_final_canary(args: argparse.Namespace) -> int:
    """workflow の stop ステップから SIGINT の前に呼ばれる。最終カナリアが pcap に入るまで待つ（#1473 P2）。

    0: 観測した / 1: 時間切れ（メッセージに最後のポーリングの理由を付ける。#1481） / 2: samples から port・final_canary_t を得られない。いずれも合否は judge が決める。
    ポーリング 1 回の待ちは min(WAIT_CANARY_POLL_MAX_SECONDS, 締切までの残り)（下限 WAIT_CANARY_POLL_MIN_SECONDS）で、
    実時間の上限は概ね timeout + POLL_MIN + KILL_GRACE（#1476）。
    """
    out_dir = Path(args.out_dir)
    timeout = max(0.0, min(float(args.timeout), WAIT_CANARY_MAX_TIMEOUT))
    interval = max(0.05, min(float(args.interval), 5.0))
    samples = out_dir / "samples.jsonl"
    if not samples.is_file():
        sys.stdout.write("note: samples file is missing; cannot wait for the final canary\n")
        return 2
    index = load_samples(samples)
    window = index.isolated_window()
    port = index.meta.get("canary_port") if index.meta else None
    final_t, invalid = _valid_final_canary_t(index.end, window)
    if isinstance(port, bool) or not isinstance(port, int) or not 1 <= port <= 65535 or final_t is None:
        sys.stdout.write(f"note: cannot wait for the final canary ({invalid or 'port or final_canary_t unavailable'})\n")
        return 2
    pcap = out_dir / "capture.pcap"
    deadline = time.monotonic() + timeout
    while True:
        remaining = deadline - time.monotonic()
        budget = max(WAIT_CANARY_POLL_MIN_SECONDS, min(WAIT_CANARY_POLL_MAX_SECONDS, remaining))
        result = _poll_pcap_once(pcap, port, final_t, window, budget)
        if result.observed:
            return 0
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            sys.stdout.write(f"note: final canary not seen in pcap within {timeout:g}s (last poll: {result.describe()})\n")
            return 1
        time.sleep(min(interval, remaining))


def cmd_sample(args: argparse.Namespace) -> int:
    servo_exe = os.path.realpath(args.servo_exe)
    interval = max(0.05, min(args.interval_ms / 1000.0, 5.0))
    stop_file = Path(args.stop_file) if args.stop_file else None
    return run_sampler(servo_exe, Path(args.out), interval, stop_file, args.max_seconds)


def _positive_int(value: str) -> int:
    """argparse 用。tcpdump の -c と共用する上限なので 1 以上だけを受け付ける。"""
    n = int(value)
    if n < 1:
        raise argparse.ArgumentTypeError("must be >= 1")
    return n


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("sample", help="非特権サンプラー（/proc を読む）")
    s.add_argument("--servo-exe", required=True)
    s.add_argument("--out", required=True)
    s.add_argument("--interval-ms", type=int, default=200)
    s.add_argument("--stop-file")
    s.add_argument("--max-seconds", type=float)
    s.set_defaults(func=cmd_sample)
    j = sub.add_parser("judge", help="capture.txt と samples.jsonl から verdict を出す")
    j.add_argument("--out-dir", required=True)
    j.add_argument("--max-packets", type=_positive_int, required=True)
    j.set_defaults(func=cmd_judge)
    w = sub.add_parser("wait-final-canary", help="pcap に最終カナリアが現れるまで上限つきでポーリングする")
    w.add_argument("--out-dir", required=True)
    w.add_argument("--timeout", type=float, default=WAIT_CANARY_DEFAULT_TIMEOUT)
    w.add_argument("--interval", type=float, default=WAIT_CANARY_DEFAULT_INTERVAL)
    w.set_defaults(func=cmd_wait_final_canary)
    args = ap.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
