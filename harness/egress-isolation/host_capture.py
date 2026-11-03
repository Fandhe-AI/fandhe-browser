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
    - 隔離下のホストのスタブ問い合わせは ns 内の `lo` で閉じ、host には届かない（観測対象外）。
    - 陽性対照（カナリア）: サンプラーが 127.0.0.1 の UDP で自分宛てに送り、`lo` の記録に現れて自分の pid に
      帰属することを要求する。観測できなければ unobservable（合格にしない）。
    - Linux 専用。宛先は 127.0.0.0/8 のカナリアのみで、外部アドレスへのプローブは送らない。
"""

from __future__ import annotations

import argparse
import bisect
import json
import os
import re
import signal
import socket
import sys
import time
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
DEFAULT_MAX_PACKETS = 300_000
ATTRIBUTION_WINDOW_SECONDS = 0.5
UNPARSED_RATIO_LIMIT = 0.01
UNPARSED_MIN = 10
MIN_CANARY_SAMPLES = 3

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


def parse_tcpdump_line(line: str) -> dict | None:
    """`tcpdump -nn -tt`（`-i any` の pcap を読んだ形）の 1 行を dict にする。解析できない行は None。

    キー: ts・iface・dir・proto（"udp" / "tcp" / "other"）・src・sport・dst・dport・syn（TCP の SYN のみ真）。
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
    }
    fam = m.group("fam")
    if fam not in ("IP", "IP6"):
        return pkt
    body = m.group("rest")
    if " > " not in body:
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
        return pkt
    if sport is not None and dport is not None:
        # `UDP, length N` のほか、DNS 等はデコードされた形（`domain [...]`）で出る
        pkt["proto"] = "udp"
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
        self.servo: dict[int, dict] = {}
        self._intervals: dict[tuple[str, int], list[list]] = {}
        self._state_sockets: list[dict] = []
        self._state_servo: list[dict] = []
        self._open: dict[tuple[str, int, str], list] = {}
        self._starts: dict[tuple[str, int], list[float]] = {}
        self.last_t: float | None = None

    def feed(self, rec: dict) -> None:
        kind = rec.get("type")
        if kind == "meta":
            self.meta = rec
            return
        if kind == "end":
            self.end = rec
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
                if bwrap or (netns is not None and netns != self_netns):
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


def _empty_counts() -> dict:
    return {role: {c: 0 for c in CATEGORIES} for role in (ROLE_ISOLATED, ROLE_UNISOLATED, ROLE_SELF, ROLE_OTHER, ROLE_NONE)}


def judge(
    packets: Iterable[dict],
    index: SampleIndex,
    *,
    unparsed: int = 0,
    total_lines: int = 0,
    max_packets: int = DEFAULT_MAX_PACKETS,
    tcpdump_exit: int | None = 0,
    tcpdump_version: str = "",
    truncated_lines: bool = False,
    convert_exit: int | None = 0,
    kernel_dropped: int | None = 0,
) -> dict:
    """verdict（"pass" / "fail" / "unobservable"）・理由・件数を返す。観測不能を合格にしない（fail-closed）。"""
    fails: list[str] = []
    unobs: list[str] = []
    counts = _empty_counts()
    nonlo_out = 0
    packet_total = 0
    canary_seen = 0
    canary_ok = 0
    canary_port = index.meta.get("canary_port") if index.meta else None
    ifaces: set[str] = set()

    for pkt in packets:
        packet_total += 1
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
        cat = count_category(pkt)
        if cat is None:
            continue
        counts[attribute(index, pkt)][cat] += 1

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
    if packet_total >= max_packets:
        unobs.append(f"capture reached the packet limit ({max_packets})")
    if truncated_lines:
        unobs.append("packet text exceeded the line limit")
    if total_lines and unparsed > UNPARSED_MIN and unparsed / total_lines > UNPARSED_RATIO_LIMIT:
        unobs.append(f"too many unparsed packet lines ({unparsed}/{total_lines})")
    if index.bad_lines > UNPARSED_MIN:
        unobs.append(f"too many malformed sample lines ({index.bad_lines})")

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
        "canary": {"port": canary_port, "seen": canary_seen, "attributed_to_self": canary_ok},
        "interfaces": sorted(ifaces),
        "tcpdump_version": tcpdump_version,
        "tcpdump_exit": tcpdump_exit,
        "convert_exit": convert_exit,
        "kernel_dropped": kernel_dropped,
    }


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
        "| attribution | dns | udp_other | tcp_syn |",
        "| --- | ---: | ---: | ---: |",
    ]
    for role, c in v["counts"].items():
        lines.append(f"| {role} | {c['dns']} | {c['udp_other']} | {c['tcp_syn']} |")
    lines.append("")
    return "\n".join(lines) + "\n"


# --------------------------------------------------------------------------------------
# judge の入出力
# --------------------------------------------------------------------------------------


def load_packets(path: Path, max_packets: int) -> tuple[list[dict], int, int, bool]:
    """tcpdump テキストを読む。戻り値は (packets, unparsed, total_lines, truncated)。行数・行長は上限つき。"""
    packets: list[dict] = []
    unparsed = 0
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
            else:
                packets.append(pkt)
    return packets, unparsed, total, truncated


def load_samples(path: Path) -> SampleIndex:
    index = SampleIndex()
    count = 0
    with path.open("r", encoding="utf-8", errors="replace") as fh:
        for raw in fh:
            count += 1
            if count > MAX_SAMPLES * 2 or not raw.strip():
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
        packets, unparsed, total, truncated = load_packets(text, args.max_packets)
        index = load_samples(samples)
    else:
        packets, unparsed, total, truncated = [], 0, 0, False
        index = SampleIndex()
    verdict = judge(
        packets,
        index,
        unparsed=unparsed,
        total_lines=total,
        max_packets=args.max_packets,
        tcpdump_exit=tcpdump_exit,
        tcpdump_version=version,
        truncated_lines=truncated,
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


def _list_pids() -> tuple[list[int], bool]:
    """(pids, 上限到達)。上限で打ち切ったら観測が不完全なので True（呼び出し側が unobservable にする）。"""
    pids: list[int] = []
    try:
        for name in os.listdir("/proc"):
            if name.isdigit():
                if len(pids) >= MAX_PROCESSES:
                    return pids, True
                pids.append(int(name))
    except OSError:
        pass
    return pids, False


def _socket_owners(pids: list[int]) -> tuple[dict[int, int], bool]:
    """読める範囲の fd から inode -> pid を作る（同一ユーザーのプロセスだけ。root は読めず帰属不能）。

    戻り値の bool は fd 数の上限で打ち切ったか。
    """
    owners: dict[int, int] = {}
    hit = False
    for pid in pids:
        try:
            names = os.listdir(f"/proc/{pid}/fd")
        except OSError:
            continue
        if len(names) > MAX_FDS_PER_PROCESS:
            hit = True
        for name in names[:MAX_FDS_PER_PROCESS]:
            try:
                target = os.readlink(f"/proc/{pid}/fd/{name}")
            except OSError:
                continue
            if target.startswith("socket:["):
                try:
                    owners.setdefault(int(target[8:-1]), pid)
                except ValueError:
                    continue
    return owners, hit


def _read_table(name: str) -> tuple[list[tuple[int, int]], bool]:
    """`/proc/self/net/<name>` から ((local_port, inode) の列, 上限到達) を返す（行数上限つき）。"""
    rows: list[tuple[int, int]] = []
    hit = False
    try:
        with open(f"/proc/self/net/{name}", "r", encoding="ascii", errors="replace") as fh:
            for i, line in enumerate(fh):
                if i > MAX_NET_LINES:
                    hit = True
                    break
                parsed = egress_guard.parse_net_line(line)
                if parsed is not None and parsed[3] != 0:
                    rows.append((parsed[4], parsed[3]))
    except OSError:
        pass
    return rows, hit


def take_sample(
    servo_exe: str, self_pid: int, self_netns: int | None
) -> tuple[list[dict], list[dict], list[str]]:
    """1 回分の走査。戻り値は (servo, sockets, limits)。servo は pid・bwrap 祖先の有無・netns。

    limits は走査上限に到達した理由（空なら完全な走査）。呼び出し側のサンプラーが
    end.truncated へ累積し、judge が unobservable にする（観測の欠落を合格にしない）。
    """
    limits: list[str] = []
    pids, hit = _list_pids()
    if hit:
        limits.append("processes-limit")
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
    owners, hit = _socket_owners(pids)
    if hit:
        limits.append("fds-per-process-limit")
    sockets: list[dict] = []
    for table, proto in (("udp", "udp"), ("udp6", "udp"), ("tcp", "tcp"), ("tcp6", "tcp")):
        rows, hit = _read_table(table)
        if hit:
            limits.append("net-lines-limit")
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

    def _mark(r: str) -> None:
        nonlocal truncated, reason
        if r not in reasons:
            reasons.append(r)
        truncated, reason = True, ",".join(reasons)

    prev: tuple | None = None
    next_canary = time.time() + 1.0
    canary_sent = 0
    started = time.time()
    with out.open("w", encoding="utf-8") as fh:

        def emit(rec: dict) -> None:
            nonlocal written
            line = json.dumps(rec, separators=(",", ":")) + "\n"
            written += len(line)
            fh.write(line)
            fh.flush()

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
            now = time.time()
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
            try:
                while True:
                    recv.recvfrom(256)
            except (BlockingIOError, OSError):
                pass
            servo, sockets, limits = take_sample(servo_exe, self_pid, self_netns)
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
                emit({"type": "sample", "t": time.time(), "servo": servo, "sockets": sockets})
                prev = state
            else:
                emit({"type": "tick", "t": time.time()})
            n += 1
            time.sleep(interval)
        emit({"type": "end", "t": time.time(), "truncated": truncated, "reason": reason, "samples": n})
    recv.close()
    send.close()
    return 0


def cmd_sample(args: argparse.Namespace) -> int:
    servo_exe = os.path.realpath(args.servo_exe)
    interval = max(0.05, min(args.interval_ms / 1000.0, 5.0))
    stop_file = Path(args.stop_file) if args.stop_file else None
    return run_sampler(servo_exe, Path(args.out), interval, stop_file, args.max_seconds)


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
    j.add_argument("--max-packets", type=int, default=DEFAULT_MAX_PACKETS)
    j.set_defaults(func=cmd_judge)
    args = ap.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
