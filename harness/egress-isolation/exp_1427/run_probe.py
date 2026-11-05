#!/usr/bin/env python3
"""#1427 の予備実験（Windows の egress 遮断を loopback 以外の宛先で測れるか）のランナー側オーケストレーション。

役割・責務境界:
    Issue #1427（TASK-38・`RENDER-3`・`SEC-2`）。Servo を使わず、AppContainer（capability 3 通り × loopback 免除 2 通り）と
    通常プロセス（陽性対照）から、`127.0.0.2`・ランナー自身の NIC の IP・`192.0.2.1`（TEST-NET-1）へ TCP connect / UDP sendto
    した結果を、ランナー側の listener・`pktmon`・WFP net events（`netsh wfp set options netevents=on` / `show netevents`）で外部から観測し、集計だけを JSON と summary.md に出す。
    AppContainer の作成・起動・片付けは既存の `appcontainer.py` を再利用する（既存ファイルは変更しない）。

承認範囲（オーナー承認 2026-11-05・#1427）:
    宛先は 127.0.0.0/8・ランナー自身の NIC の IP・192.0.2.0/24 だけ（`probe.validate_dest` が範囲外を例外で止める）。
    採取の生データ（etl・txt・cab・xml）はアーティファクトに載せず、出力ディレクトリとは別の作業ディレクトリに置いて
    最後に削除する。出力するのは宛先で絞った集計のみ（IP は種別に伏せる）。使い捨てランナー専用で、main にはマージしない。

観測ごとに失敗を記録して続行する。pktmon / netsh の出力形式は仮定のため、生出力の「形」（行数・数字を # に潰した先頭行）を
`diagnostics` に残す。
"""

from __future__ import annotations

import argparse
import collections
import ipaddress
import json
import os
import re
import selectors
import struct
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import xml.etree.ElementTree as ET
from pathlib import Path
from typing import Any, Iterable

_HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(_HERE))
sys.path.insert(0, str(_HERE.parent))

import probe  # noqa: E402  (同ディレクトリの自己完結プローブ)

BASE_PORT = 47000
CMD_TIMEOUT_S = 60
MAX_CMD_OUT_BYTES = 4 * 1024 * 1024
OVERALL_DEADLINE_S = 15 * 60
MAX_XML_BYTES = 400 * 1024 * 1024
MAX_TEXT_BYTES = 800 * 1024 * 1024
DEST_KEYS = ("a", "b", "c")
DEST_LABEL = {"a": "127.0.0.2", "b": "nic", "c": "192.0.2.1"}
PROTOS = ("tcp", "udp")
CAP_INTERNET_CLIENT = "S-1-15-3-1"
FILTER_NAME_PATTERNS = ("Block Outbound Default Rule", "InternetClient Default Rule", "AppContainerLoopback",
                        "AppContainer", "Block Inbound Default Rule", "Loopback")
KIND_OF_DEST = {"a": "loopback", "b": "nic", "c": "testnet"}
_IP_LINE = re.compile(r"(\d+\.\d+\.\d+\.\d+)\.(\d+) > (\d+\.\d+\.\d+\.\d+)\.(\d+):\s*(.*)")
_COMPONENT = re.compile(r"\bComponent\s+(\d+)")
_DIRECTION = re.compile(r"\bDirection\s+(\w+)")
_DROP = re.compile(r"\bDrop\w*(?:\s*Reason)?\s*[:=]?\s*([^,]{0,60})", re.IGNORECASE)
_IPV4_ANY = re.compile(r"\b\d{1,3}(?:\.\d{1,3}){3}\b")
_MAC = re.compile(r"\b[0-9A-Fa-f]{2}(?:[-:][0-9A-Fa-f]{2}){5}\b")


# --- 純粋関数（Linux でも自己テストできる） ------------------------------------------------------------
def ip_kind(ip: str) -> str:
    """NIC の IPv4 を種類だけに丸める（Summary に実アドレスを出さない）。"""
    try:
        a = ipaddress.IPv4Address(ip)
    except ValueError:
        return "invalid"
    if a in ipaddress.IPv4Network("10.0.0.0/8"):
        return "private-10.x"
    if a in ipaddress.IPv4Network("172.16.0.0/12"):
        return "private-172.16/12"
    if a in ipaddress.IPv4Network("192.168.0.0/16"):
        return "private-192.168.x"
    if a in ipaddress.IPv4Network("169.254.0.0/16"):
        return "link-local"
    if a in ipaddress.IPv4Network("100.64.0.0/10"):
        return "cgnat"
    if a.is_loopback:
        return "loopback"
    return "other"


def shape(line: str, limit: int = 120) -> str:
    """生出力の「形」だけを残す（数字を # に潰し、IP・MAC 風の並びも潰れる）。"""
    return re.sub(r"\d+", "#", line.strip())[:limit]


def mask_text(line: str, limit: int = 100) -> str:
    """診断用に IPv4・MAC 風の文字列を伏せて切り詰める。"""
    return _MAC.sub("<mac>", _IPV4_ANY.sub("<ip>", line.strip()))[:limit]


def classify_dest(ip: str, nic_ip: str | None) -> str | None:
    """集計対象の宛先種別（loopback / testnet / nic）。対象外は None。"""
    try:
        return probe.validate_dest(ip, probe.MIN_PORT, nic_ip)
    except ValueError:
        return None


def parse_pktmon_text(lines: Iterable[str], nic_ip: str | None, port_index: dict[tuple[str, int], str]) -> dict[str, Any]:
    """`pktmon etl2txt` の出力を宛先（loopback / testnet / nic）で絞って、port 単位の件数だけに集計する。

    形式は仮定: ヘッダ行（`PktGroupId` を含む）の次に `src.port > dst.port: Flags [S]` / `UDP, length n` 形式の行が続く。
    仮定が外れても例外にせず、`diag` に行数と先頭行の形を残す。port_index は (proto, port) → variant id。
    """
    out: dict[str, dict[str, Any]] = {}
    diag: dict[str, Any] = {"lines": 0, "headers": 0, "ip_lines": 0, "ip_lines_in_scope": 0, "first_header_shape": None,
                            "first_ip_line_shape": None, "component_ids": collections.Counter()}
    header = ""
    for raw in lines:
        diag["lines"] += 1
        line = raw.rstrip("\r\n")
        if "PktGroupId" in line:
            diag["headers"] += 1
            header = line
            if diag["first_header_shape"] is None:
                diag["first_header_shape"] = shape(line, 160)
            continue
        m = _IP_LINE.search(line)
        if not m:
            continue
        diag["ip_lines"] += 1
        if diag["first_ip_line_shape"] is None:
            diag["first_ip_line_shape"] = shape(line, 160)
        dst_ip, dst_port, rest = m.group(3), int(m.group(4)), m.group(5)
        if classify_dest(dst_ip, nic_ip) is None:
            continue
        diag["ip_lines_in_scope"] += 1
        flags = re.search(r"Flags \[([^\]]*)\]", rest)
        if flags:
            proto = "tcp"
            kind = "tcp_syn" if flags.group(1).strip() == "S" else "tcp_other"
        elif "UDP" in rest:
            proto, kind = "udp", "udp"
        else:
            proto, kind = "other", "other"
        key = port_index.get((proto, dst_port), "unattributed")
        ent = out.setdefault(key, {"packets": collections.Counter(), "components": collections.Counter(),
                                   "directions": collections.Counter(), "drops": collections.Counter()})
        ent["packets"][kind] += 1
        comp = _COMPONENT.search(header)
        if comp:
            ent["components"][comp.group(1)] += 1
            diag["component_ids"][comp.group(1)] += 1
        d = _DIRECTION.search(header)
        if d:
            ent["directions"][d.group(1)] += 1
        dr = _DROP.search(header.replace("Direction", ""))
        if dr:
            ent["drops"][dr.group(1).strip() or "drop"] += 1
    diag["component_ids"] = dict(diag["component_ids"])
    return {"by_variant": {k: {f: dict(v) for f, v in ent.items()} for k, ent in out.items()}, "diag": diag}


_LINK_ETHERNET = 1
_LINK_NULL = 0
_LINK_RAW_TYPES = (12, 14, 101, 228)
_PCAPNG_SHB, _PCAPNG_IDB, _PCAPNG_SPB, _PCAPNG_EPB = 0x0A0D0D0A, 1, 3, 6
_MAX_BLOCK_BYTES = 16 * 1024 * 1024
_COMPONENT_IN_COMMENT = re.compile(r"(?i)component\W*(\d+)")


def decode_ipv4_packet(frame: bytes, linktype: int) -> tuple[str, int, str, int, str] | None:
    """リンク層つきのフレームから IPv4 の (src, sport, dst, dport, kind) を取る。kind は tcp_syn / tcp_other / udp。

    IPv4 の TCP・UDP 以外、先頭断片でない断片、途中で切れたパケットは None。
    """
    off = 0
    if linktype == _LINK_ETHERNET:
        if len(frame) < 14:
            return None
        et = struct.unpack_from("!H", frame, 12)[0]
        off = 14
        while et in (0x8100, 0x88A8) and len(frame) >= off + 4:
            et = struct.unpack_from("!H", frame, off + 2)[0]
            off += 4
        if et != 0x0800:
            return None
    elif linktype == _LINK_NULL:
        off = 4
    elif linktype in _LINK_RAW_TYPES:
        off = 0
    else:
        return None
    ip = frame[off:]
    if len(ip) < 20 or ip[0] >> 4 != 4:
        return None
    ihl = (ip[0] & 0x0F) * 4
    if ihl < 20 or len(ip) < ihl + 8:
        return None
    if struct.unpack_from("!H", ip, 6)[0] & 0x1FFF:
        return None
    proto = ip[9]
    src, dst = socket.inet_ntoa(ip[12:16]), socket.inet_ntoa(ip[16:20])
    sport, dport = struct.unpack_from("!HH", ip, ihl)
    if proto == 17:
        return src, sport, dst, dport, "udp"
    if proto == 6:
        if len(ip) < ihl + 14:
            return None
        flags = ip[ihl + 13]
        return src, sport, dst, dport, ("tcp_syn" if flags & 0x02 and not flags & 0x10 else "tcp_other")
    return None


def _epb_comment(options: bytes, endian: str) -> str | None:
    """EPB のオプション列から opt_comment（code 1）を取る。"""
    pos = 0
    while pos + 4 <= len(options):
        code, length = struct.unpack_from(endian + "HH", options, pos)
        if code == 0:
            return None
        if code == 1:
            return options[pos + 4:pos + 4 + length].decode("utf-8", errors="replace")
        pos += 4 + ((length + 3) & ~3)
    return None


def parse_pcapng(f: Any, nic_ip: str | None, port_index: dict[tuple[str, int], str]) -> dict[str, Any]:
    """pktmon の pcapng（SHB・IDB・EPB・SPB）を読み、宛先（loopback / testnet / nic）で絞って variant 単位に集計する。

    pktmon は同じパケットを複数コンポーネントで記録するため、`packets`（生の件数）と `unique_flows`（送信元込みの
    5 タプル + kind の重複除去後の件数）の両方を出す。範囲外の IP は件数だけ数え、値は出さない。壊れたファイルは
    例外にせず `diag.error` に残して、そこまでの集計を返す。
    """
    diag: dict[str, Any] = {"block_types": collections.Counter(), "linktypes": collections.Counter(),
                            "frames": 0, "ipv4_decoded": 0, "in_scope": 0, "out_of_scope": 0,
                            "epb_with_comment": 0, "comment_shapes": collections.Counter(),
                            "component_ids": collections.Counter()}
    out: dict[str, dict[str, Any]] = {}
    seen: dict[str, set[tuple[Any, ...]]] = {}
    endian = "<"
    links: list[int] = []
    try:
        while True:
            head = f.read(8)
            if len(head) < 8:
                break
            btype_le = struct.unpack("<I", head[:4])[0]
            if btype_le == _PCAPNG_SHB:
                # ブロック長の解釈にエンディアンが要るため、先に byte-order magic を見る。
                body = f.read(8)
                if len(body) < 8:
                    diag["error"] = "truncated SHB"
                    break
                magic = struct.unpack("<I", body[:4])[0]
                endian = "<" if magic == 0x1A2B3C4D else ">" if magic == 0x4D3C2B1A else "<"
                total = struct.unpack(endian + "I", head[4:8])[0]
                rest = f.read(max(0, total - 16))
                if total < 16 or len(rest) < total - 16:
                    diag["error"] = "truncated SHB"
                    break
                links = []
                diag["block_types"]["SHB"] += 1
                continue
            btype, total = struct.unpack(endian + "II", head)
            if total < 12 or total > _MAX_BLOCK_BYTES:
                diag["error"] = f"bad block length (type {btype})"
                break
            body = f.read(total - 12)
            tail = f.read(4)
            if len(body) < total - 12 or len(tail) < 4:
                diag["error"] = "truncated block"
                break
            name = {_PCAPNG_IDB: "IDB", _PCAPNG_EPB: "EPB", _PCAPNG_SPB: "SPB"}.get(btype, f"other-{btype}")
            diag["block_types"][name] += 1
            frame: bytes | None = None
            link = -1
            comment = None
            if btype == _PCAPNG_IDB and len(body) >= 8:
                links.append(struct.unpack_from(endian + "H", body, 0)[0])
                diag["linktypes"][str(links[-1])] += 1
            elif btype == _PCAPNG_EPB and len(body) >= 20:
                ifid, _hi, _lo, caplen, _orig = struct.unpack_from(endian + "IIIII", body, 0)
                frame = body[20:20 + caplen]
                link = links[ifid] if ifid < len(links) else -1
                comment = _epb_comment(body[20 + ((caplen + 3) & ~3):], endian)
            elif btype == _PCAPNG_SPB and len(body) >= 4:
                orig = struct.unpack_from(endian + "I", body, 0)[0]
                frame = body[4:4 + min(orig, len(body) - 4)]
                link = links[0] if links else -1
            if frame is None:
                continue
            diag["frames"] += 1
            if comment:
                diag["epb_with_comment"] += 1
                if len(diag["comment_shapes"]) < 8:
                    diag["comment_shapes"][shape(comment, 80)] += 1
            dec = decode_ipv4_packet(frame, link)
            if dec is None:
                continue
            diag["ipv4_decoded"] += 1
            src, sport, dst, dport, kind = dec
            if classify_dest(dst, nic_ip) is None:
                diag["out_of_scope"] += 1
                continue
            diag["in_scope"] += 1
            proto = "udp" if kind == "udp" else "tcp"
            key = port_index.get((proto, dport), "unattributed")
            ent = out.setdefault(key, {"packets": collections.Counter(), "unique_flows": collections.Counter(),
                                       "present": {}, "components": collections.Counter()})
            ent["packets"][kind] += 1
            flow = (src, sport, dst, dport, kind)
            sset = seen.setdefault(key, set())
            if flow not in sset and len(sset) < 100000:
                sset.add(flow)
                ent["unique_flows"][kind] += 1
            ent["present"][kind] = 1
            m = _COMPONENT_IN_COMMENT.search(comment or "")
            if m:
                ent["components"][m.group(1)] += 1
                diag["component_ids"][m.group(1)] += 1
    except (OSError, struct.error) as exc:
        diag["error"] = f"{type(exc).__name__}"
    for k in ("block_types", "linktypes", "comment_shapes", "component_ids"):
        diag[k] = dict(diag[k])
    by = {k: {f2: dict(v) for f2, v in ent.items()} for k, ent in out.items()}
    return {"by_variant": by, "diag": diag}


def _local(tag: str) -> str:
    return tag.rsplit("}", 1)[-1]


def _flat(el: ET.Element) -> dict[str, str]:
    """要素以下のタグ名（ローカル名）→ 最初の非空テキスト。"""
    out: dict[str, str] = {}
    for sub in el.iter():
        t = (sub.text or "").strip()
        if t and _local(sub.tag) not in out:
            out[_local(sub.tag)] = t
    return out


def _addr(text: str | None) -> str | None:
    if text is None:
        return None
    if re.fullmatch(r"\d+\.\d+\.\d+\.\d+", text):
        return text
    return None


def parse_wfp_events(source: Any, nic_ip: str | None, port_index: dict[tuple[str, int], str]) -> dict[str, Any]:
    """`netsh wfp show netevents` の xml（wfpdiag.xml と同じ net event の形と仮定）の net event を宛先で絞り、port 単位に種類・filter id・package SID / app id の有無へ集計する。

    形式は仮定: `<item><header>…remoteAddrV4 / remotePort / ipProtocol / packageSid / appId…</header><type>…</type>…<filterId>` 。
    source は ET.iterparse が受け付けるファイルパスまたはファイルオブジェクト。
    """
    out: dict[str, dict[str, Any]] = {}
    diag: dict[str, Any] = {"items_with_header": 0, "in_scope": 0, "unparsed_addr": 0, "first_item_tags": None,
                            "type_names": collections.Counter()}
    try:
        for _ev, el in ET.iterparse(source, events=("end",)):
            if _local(el.tag) != "item":
                continue
            header = next((c for c in el if _local(c.tag) == "header"), None)
            if header is None:
                continue
            diag["items_with_header"] += 1
            flat = _flat(el)
            hflat = _flat(header)
            if diag["first_item_tags"] is None:
                diag["first_item_tags"] = sorted({_local(s.tag) for s in el.iter()})[:60]
            typ = flat.get("type", "unknown")[:48]
            diag["type_names"][typ] += 1
            remote = _addr(hflat.get("remoteAddrV4") or hflat.get("remoteAddr"))
            if remote is None:
                diag["unparsed_addr"] += 1
                el.clear()
                continue
            if classify_dest(remote, nic_ip) is None:
                el.clear()
                continue
            diag["in_scope"] += 1
            proto = {"6": "tcp", "17": "udp"}.get(hflat.get("ipProtocol", ""), "other")
            try:
                port = int(hflat.get("remotePort", "0"))
            except ValueError:
                port = 0
            key = port_index.get((proto, port), "unattributed")
            ent = out.setdefault(key, {"events": collections.Counter(), "filters": collections.Counter(),
                                       "layers": collections.Counter(), "directions": collections.Counter(),
                                       "package_sid_events": 0, "app_id_events": 0})
            ent["events"][typ] += 1
            if "filterId" in flat:
                ent["filters"][flat["filterId"][:12]] += 1
            for lk in ("layerId", "layerKey"):
                if lk in flat:
                    ent["layers"][flat[lk][:40]] += 1
                    break
            if "msFwpDirection" in flat:
                ent["directions"][flat["msFwpDirection"][:16]] += 1
            if any("package" in k.lower() and "sid" in k.lower() for k in hflat) or \
                    any(k.lower() in ("appcontainersid", "userid") and v.startswith("S-1-15-2") for k, v in hflat.items()):
                ent["package_sid_events"] += 1
            if hflat.get("appId"):
                ent["app_id_events"] += 1
            el.clear()
    except ET.ParseError as exc:
        diag["parse_error"] = str(exc)[:80]
    diag["type_names"] = dict(diag["type_names"])
    by = {k: {f: (dict(v) if isinstance(v, collections.Counter) else v) for f, v in ent.items()} for k, ent in out.items()}
    return {"by_variant": by, "diag": diag}


def parse_filters_xml(source: Any) -> dict[str, Any]:
    """`netsh wfp show filters` の xml から filterId → 名前、全体の件数、名前パターン別の一致件数を得る（名前のみ）。"""
    names: dict[str, str] = {}
    total = 0
    patterns = {p: 0 for p in FILTER_NAME_PATTERNS}
    diag: dict[str, Any] = {}
    try:
        for _ev, el in ET.iterparse(source, events=("end",)):
            if _local(el.tag) != "item":
                continue
            kids = {_local(c.tag): c for c in el}
            if "filterId" not in kids or "displayData" not in kids:
                continue
            total += 1
            fid = (kids["filterId"].text or "").strip()
            nm = next(((c.text or "").strip() for c in kids["displayData"] if _local(c.tag) == "name"), "")[:80]
            names[fid] = nm
            low = nm.lower()
            for p in FILTER_NAME_PATTERNS:
                if p.lower() in low:
                    patterns[p] += 1
            el.clear()
    except ET.ParseError as exc:
        diag["parse_error"] = str(exc)[:80]
    return {"names": names, "total": total, "pattern_counts": patterns, "diag": diag}


def parse_netevents_option(text: str) -> bool | None:
    """`netsh wfp show options optionsfor=netevents` の出力から net events が有効かを推定する。判別できなければ None。

    形式は仮定（`netevents` / `net events` を含む行に on・enabled・off・disabled のいずれかがある）。外れたら None。
    """
    for line in text.splitlines():
        low = line.lower()
        if "netevent" in low or "net event" in low:
            if re.search(r"\b(on|enabled|true)\b", low):
                return True
            if re.search(r"\b(off|disabled|false)\b", low):
                return False
    return None


def merge_counts(dst: dict[str, Any], src: dict[str, Any]) -> None:
    """入れ子の件数 dict を足し込む（port 単位の集計を複数回の採取で統合する用）。"""
    for k, v in src.items():
        if isinstance(v, dict):
            merge_counts(dst.setdefault(k, {}), v)
        elif isinstance(v, (int, float)):
            dst[k] = dst.get(k, 0) + v
        else:
            dst.setdefault(k, v)


# --- 計画 -----------------------------------------------------------------------------------------------
def build_variants() -> list[dict[str, Any]]:
    """対照（通常プロセス）と AppContainer の組。ポートは (variant, 宛先, proto) ごとに一意に割り当てる。"""
    from appcontainer import NETWORK_CAPABILITIES  # noqa: PLC0415

    specs: list[tuple[str, str, bool | None, tuple[str, ...]]] = [("control", "control", None, ())]
    for exempt in (False, True):
        for cname, caps in (("nocap", ()), ("ic", (CAP_INTERNET_CLIENT,)), ("ic+pnc", tuple(NETWORK_CAPABILITIES))):
            specs.append((f"ac-{'exempt' if exempt else 'noexempt'}-{cname}", "appcontainer", exempt, caps))
    variants = []
    port = BASE_PORT
    for name, actor, exempt, caps in specs:
        ports = {}
        for d in DEST_KEYS:
            for p in PROTOS:
                ports[f"{d}/{p}"] = port
                port += 1
        variants.append({"name": name, "actor": actor, "exempt": exempt, "caps": list(caps), "ports": ports})
    return variants


def attempt_plan(variant: dict[str, Any], nic_ip: str | None) -> list[dict[str, Any]]:
    out = []
    for d in DEST_KEYS:
        if d == "b" and nic_ip is None:
            continue
        ip = {"a": "127.0.0.2", "b": nic_ip, "c": "192.0.2.1"}[d]
        for p in PROTOS:
            out.append({"id": f"{variant['name']}/{d}/{p}", "proto": p, "ip": ip, "port": variant["ports"][f"{d}/{p}"]})
    return out


def port_index_of(variants: list[dict[str, Any]]) -> dict[tuple[str, int], str]:
    idx = {}
    for v in variants:
        for key, port in v["ports"].items():
            d, p = key.split("/")
            idx[(p, port)] = f"{v['name']}/{key}"
    return idx


# --- 実行系（Windows 専用の部分） ------------------------------------------------------------------------
_T0 = time.monotonic()
_DEADLINE = [_T0 + OVERALL_DEADLINE_S]


def expired() -> bool:
    return time.monotonic() >= _DEADLINE[0]


def remaining_s(cap: int) -> int:
    """全体期限までの残り秒（cap で頭打ち、最低 5 秒）。"""
    return int(max(5, min(cap, _DEADLINE[0] - time.monotonic())))


class stage:  # noqa: N801
    """段の開始と終了を時刻と経過 ms つきで stderr に出す（IP・秘密情報は出さない）。例外は握りつぶさず通す。"""

    def __init__(self, name: str, timings: list[dict[str, Any]] | None = None) -> None:
        self.name, self.timings = name, timings

    def __enter__(self) -> "stage":
        self.t = time.monotonic()
        print(f"[{time.strftime('%H:%M:%S')}] start {self.name}", file=sys.stderr, flush=True)
        return self

    def __exit__(self, et: Any, ev: Any, tb: Any) -> None:
        ms = int((time.monotonic() - self.t) * 1000)
        status = "ok" if et is None else f"raised {et.__name__}"
        print(f"[{time.strftime('%H:%M:%S')}] end   {self.name} ({ms} ms, {status})", file=sys.stderr, flush=True)
        if self.timings is not None:
            self.timings.append({"stage": self.name, "ms": ms, "status": status})


def sysroot() -> str:
    import appcontainer  # noqa: PLC0415

    return appcontainer.trusted_system_root()


def _kill_tree(proc: "subprocess.Popen[bytes]") -> None:
    """taskkill /T /F で子孫ごと止める（Windows では孫が継承ハンドルを握ったまま残ることがある）。待ちは短く打ち切る。"""
    try:
        k = subprocess.Popen([sys32("taskkill.exe"), "/T", "/F", "/PID", str(proc.pid)],  # noqa: S603
                             stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            k.wait(timeout=10)
        except subprocess.TimeoutExpired:
            k.kill()
    except OSError:
        pass
    try:
        proc.kill()
    except OSError:
        pass
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        pass


def run_cmd(args: list[str], *, cwd: Path | None = None, timeout: int = CMD_TIMEOUT_S) -> dict[str, Any]:
    """外部コマンドを実行し {rc, out, error} を返す。例外は呼び出し側へ出さない。

    stdout / stderr はパイプではなく一時ファイルへ書く（パイプだと kill 後に孫が握るハンドルのせいで読み取りが戻らない。
    初回実行 run 41961452377 の教訓）。timeout 超過は木ごと kill して短い待ちで必ず戻り、rc=None・error="TimeoutExpired"。
    """
    try:
        with tempfile.TemporaryFile() as fo, tempfile.TemporaryFile() as fe:
            try:
                proc = subprocess.Popen(args, stdin=subprocess.DEVNULL, stdout=fo, stderr=fe,  # noqa: S603
                                        cwd=str(cwd) if cwd else None)
            except OSError as exc:
                return {"rc": None, "out": "", "error": type(exc).__name__}
            error = None
            rc: int | None
            try:
                rc = proc.wait(timeout=max(1, timeout))
            except subprocess.TimeoutExpired:
                _kill_tree(proc)
                rc, error = None, "TimeoutExpired"
            parts = []
            for f in (fo, fe):
                f.seek(0)
                parts.append(f.read(MAX_CMD_OUT_BYTES))
            return {"rc": rc, "out": b"\n".join(parts).decode("utf-8", errors="replace"), "error": error}
    except OSError as exc:
        return {"rc": None, "out": "", "error": type(exc).__name__}


def sys32(name: str) -> str:
    return sysroot().rstrip("\\/") + "\\System32\\" + name


def ps_json(script: str) -> Any:
    exe = sysroot().rstrip("\\/") + "\\System32\\WindowsPowerShell\\v1.0\\powershell.exe"
    r = run_cmd([exe, "-NoProfile", "-NonInteractive", "-Command", f"{script} | ConvertTo-Json -Compress"], timeout=60)
    if r["rc"] != 0:
        return {"error": r["error"] or f"rc={r['rc']}"}
    try:
        return json.loads(r["out"].strip().splitlines()[0] if r["out"].strip() else "null")
    except ValueError:
        return {"error": "unparsable output", "shape": shape(r["out"][:80])}


def detect_nic_ip() -> tuple[str | None, str]:
    """UDP connect（パケットは送らない）で既定経路の送信元 IPv4 を得る。bind できなければ NIC として採用しない。"""
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        probe.validate_dest("192.0.2.1", 9999)
        s.connect(("192.0.2.1", 9999))
        ip = s.getsockname()[0]
    except OSError as exc:
        return None, f"route lookup failed: {type(exc).__name__}"
    finally:
        s.close()
    try:
        probe.parse_nic_ip(ip)
        b = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        try:
            b.bind((ip, 0))
        finally:
            b.close()
    except (ValueError, OSError) as exc:
        return None, f"not usable as a NIC address: {type(exc).__name__}"
    return ip, "ok"


def recon(nic_ip: str | None) -> dict[str, Any]:
    r: dict[str, Any] = {"nic_ip_kind": ip_kind(nic_ip) if nic_ip else None}
    r["connection_profile"] = ps_json("Get-NetConnectionProfile | Select-Object "
                                      "@{n='NetworkCategory';e={$_.NetworkCategory.ToString()}},"
                                      "@{n='IPv4Connectivity';e={$_.IPv4Connectivity.ToString()}}")
    r["firewall_profile"] = ps_json("Get-NetFirewallProfile | Select-Object Name,"
                                    "@{n='Enabled';e={$_.Enabled.ToString()}}")
    r["services"] = ps_json("Get-Service mpssvc,bfe | Select-Object Name,@{n='Status';e={$_.Status.ToString()}}")
    r["nic_prefix_kinds"] = ps_json("Get-NetIPAddress -AddressFamily IPv4 | Select-Object PrefixLength,"
                                    "@{n='PrefixOrigin';e={$_.PrefixOrigin.ToString()}}")
    return r


class Hub:
    """承認済みの宛先に bind する TCP / UDP の受信カウンタ（1 スレッドの selector。内容は読まず件数だけ）。"""

    def __init__(self, nic_ip: str | None) -> None:
        self.nic_ip = nic_ip
        self.sel = selectors.DefaultSelector()
        self.counts: dict[tuple[str, int], int] = collections.defaultdict(int)
        self.errors: list[str] = []
        self._socks: list[socket.socket] = []
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._run, daemon=True)

    def add(self, proto: str, ip: str, port: int) -> None:
        try:
            probe.validate_dest(ip, port, self.nic_ip)
            s = socket.socket(socket.AF_INET, socket.SOCK_STREAM if proto == "tcp" else socket.SOCK_DGRAM)
            s.setblocking(False)
            s.bind((ip, port))
            if proto == "tcp":
                s.listen(16)
            self._socks.append(s)
            self.sel.register(s, selectors.EVENT_READ, (proto, port))
        except (OSError, ValueError) as exc:
            self.errors.append(f"{proto}/{port}: {type(exc).__name__}")

    def start(self) -> None:
        self._thread.start()

    def _run(self) -> None:
        while not self._stop.is_set():
            for key, _ in self.sel.select(0.2):
                proto, port = key.data
                try:
                    if proto == "tcp":
                        conn, _peer = key.fileobj.accept()  # type: ignore[union-attr]
                        conn.close()
                    else:
                        key.fileobj.recvfrom(64)  # type: ignore[union-attr]
                    self.counts[(proto, port)] += 1
                except OSError:
                    continue

    def stop(self) -> None:
        self._stop.set()
        self._thread.join(timeout=2)
        for s in self._socks:
            try:
                self.sel.unregister(s)
            except (KeyError, ValueError):
                pass
            s.close()
        self.sel.close()


class Captures:
    """pktmon と WFP net events の採取。開始・停止・片付けの成否を記録し、失敗しても続行する。

    WFP は `netsh wfp capture`（返ってこない）ではなく、`netsh wfp set options netevents=on` → 試行 →
    `netsh wfp show netevents file=` → 元の設定へ戻す、の順で観測する。
    """

    def __init__(self, workdir: Path, nic_ip: str | None) -> None:
        self.workdir = workdir
        self.nic_ip = nic_ip
        self.etl = workdir / "pm.etl"
        self.pcap = workdir / "pktmon.pcapng"
        self.status: dict[str, Any] = {}
        self.pktmon_started = False
        self.pktmon_filters_added = False
        self.wfp_set = False
        self.wfp_orig_on: bool | None = None

    def _pm(self, *args: str, timeout: int = CMD_TIMEOUT_S) -> dict[str, Any]:
        return run_cmd([sys32("pktmon.exe"), *args], cwd=self.workdir, timeout=timeout)

    def _wfp(self, *args: str) -> dict[str, Any]:
        return run_cmd([sys32("netsh.exe"), "wfp", *args], cwd=self.workdir, timeout=remaining_s(CMD_TIMEOUT_S))

    def start(self, timings: list[dict[str, Any]]) -> None:
        st = self.status
        with stage("pktmon-start", timings):
            st["pktmon_prestop_rc"] = self._pm("stop", timeout=20)["rc"]
            st["pktmon_filter_remove_rc"] = self._pm("filter", "remove", timeout=20)["rc"]
            filters = [("testnet", "192.0.2.0/24"), ("loopback", "127.0.0.0/8")]
            if self.nic_ip:
                filters.append(("nic", self.nic_ip))
            self.pktmon_filters_added = True
            st["pktmon_filter_add_rc"] = {n: self._pm("filter", "add", n, "-i", ip, timeout=20)["rc"] for n, ip in filters}
            r = self._pm("start", "--capture", "--comp", "all", "--pkt-size", "0", "--file-name", str(self.etl))
            if r["rc"] != 0:
                st["pktmon_start_comp_all_rc"] = r["rc"]
                r = self._pm("start", "--capture", "--file-name", str(self.etl))
            st["pktmon_start_rc"] = r["rc"]
            st["pktmon_start_error"] = r["error"]
            self.pktmon_started = r["rc"] == 0
            lst = self._pm("list", timeout=20)
            st["pktmon_list_lines"] = [mask_text(x) for x in lst["out"].splitlines() if x.strip()][:30]
        with stage("wfp-netevents-on", timings):
            before = self._wfp("show", "options", "optionsfor=netevents")
            st["wfp_options_before_rc"] = before["rc"]
            st["wfp_options_before_lines"] = [mask_text(x) for x in before["out"].splitlines() if x.strip()][:20]
            self.wfp_orig_on = parse_netevents_option(before["out"])
            st["wfp_netevents_originally_on"] = self.wfp_orig_on
            r = self._wfp("set", "options", "netevents=on")
            st["wfp_set_on_rc"] = r["rc"]
            st["wfp_set_on_error"] = r["error"]
            # 付与を試みたら、部分適用を除外できないため解除（復元）も必ず試す。
            self.wfp_set = True

    def stop_and_collect(self, port_index: dict[tuple[str, int], str], filter_names: dict[str, str],
                         timings: list[dict[str, Any]]) -> dict[str, Any]:
        """停止して、宛先で絞った集計だけを返す。各段の失敗は diagnostics に残して続ける。"""
        st = self.status
        result: dict[str, Any] = {"pktmon": {"by_variant": {}, "diag": {}}, "wfp": {"by_variant": {}, "diag": {}}}
        if self.wfp_set:
            with stage("wfp-show-netevents", timings):
                xml_path = self.workdir / "netevents.xml"
                r = self._wfp("show", "netevents", f"file={xml_path}")
                st["wfp_show_netevents_rc"] = r["rc"]
                st["wfp_show_netevents_error"] = r["error"]
                try:
                    if xml_path.is_file() and xml_path.stat().st_size <= MAX_XML_BYTES:
                        result["wfp"] = parse_wfp_events(str(xml_path), self.nic_ip, port_index)
                    else:
                        st["wfp_netevents_missing_or_large"] = True
                except (OSError, ET.ParseError) as exc:
                    st["wfp_parse_error"] = type(exc).__name__
                finally:
                    try:
                        xml_path.unlink()
                    except OSError:
                        pass
        if self.pktmon_started:
            with stage("pktmon-stop-and-parse", timings):
                st["pktmon_stop_rc"] = self._pm("stop")["rc"]
                self.pktmon_started = False
                r = self._pm("etl2pcap", str(self.etl), "--out", str(self.pcap))
                if r["rc"] != 0 or not self.pcap.exists():
                    st["pktmon_etl2pcap_long_rc"] = r["rc"]
                    r = self._pm("etl2pcap", str(self.etl), "-o", str(self.pcap))
                st["pktmon_etl2pcap_rc"] = r["rc"]
                try:
                    size = self.pcap.stat().st_size
                    st["pktmon_pcap_bytes_bucket"] = f"<={1 << max(size, 1).bit_length()}"
                    if size > MAX_TEXT_BYTES:
                        raise ValueError("pktmon pcapng too large")
                    with self.pcap.open("rb") as f:
                        result["pktmon"] = parse_pcapng(f, self.nic_ip, port_index)
                except (OSError, ValueError) as exc:
                    st["pktmon_parse_error"] = type(exc).__name__
        # filter id → 名前（名前のみ）。event に現れた id だけを解決して出す。
        for ent in result["wfp"]["by_variant"].values():
            ent["filter_names"] = {fid: filter_names.get(fid, "?") for fid in ent.get("filters", {})}
        return result

    def cleanup(self) -> dict[str, Any]:
        """pktmon の停止・フィルタ削除と WFP net events の復元。期限に関係なく必ず試し、成否を返す。"""
        c: dict[str, Any] = {}
        c["pktmon_stop_rc"] = self._pm("stop", timeout=20)["rc"] if self.pktmon_started else "not-running"
        c["pktmon_filter_remove_rc"] = self._pm("filter", "remove", timeout=20)["rc"] if self.pktmon_filters_added \
            else "not-added"
        if self.wfp_set:
            want = "on" if self.wfp_orig_on else "off"
            r = run_cmd([sys32("netsh.exe"), "wfp", "set", "options", f"netevents={want}"], cwd=self.workdir, timeout=30)
            c["wfp_netevents_restore_to"] = want
            c["wfp_netevents_restore_rc"] = r["rc"]
            after = run_cmd([sys32("netsh.exe"), "wfp", "show", "options", "optionsfor=netevents"],
                            cwd=self.workdir, timeout=30)
            now = parse_netevents_option(after["out"])
            c["wfp_netevents_after_on"] = now
            c["wfp_netevents_after_lines"] = [mask_text(x) for x in after["out"].splitlines() if x.strip()][:10]
        else:
            c["wfp_netevents_restore_rc"] = "not-set"
        return c


def snapshot_filters(workdir: Path, tag: str) -> dict[str, Any]:
    """`netsh wfp show filters` を取り、名前の一致件数と id → 名前を返す。出力ファイルは解析後に削除する。"""
    f = workdir / f"filters-{tag}.xml"
    r = run_cmd([sys32("netsh.exe"), "wfp", "show", "filters", f"file={f}"], cwd=workdir)
    res: dict[str, Any] = {"rc": r["rc"], "names": {}, "total": None, "pattern_counts": {}}
    try:
        if f.is_file() and f.stat().st_size <= MAX_XML_BYTES:
            parsed = parse_filters_xml(str(f))
            res.update({"names": parsed["names"], "total": parsed["total"], "pattern_counts": parsed["pattern_counts"],
                        "diag": parsed["diag"]})
        else:
            res["missing_or_large"] = True
    except (OSError, ET.ParseError) as exc:
        res["error"] = type(exc).__name__
    finally:
        try:
            f.unlink()
        except OSError:
            pass
    return res


def run_control(variant: dict[str, Any], nic_ip: str | None, workdir: Path) -> dict[str, Any]:
    plan = workdir / "plan-control.json"
    res = workdir / "res-control.json"
    plan.write_text(json.dumps({"nic_ip": nic_ip, "attempts": attempt_plan(variant, nic_ip)}), encoding="utf-8")
    r = run_cmd([sys.executable, "-I", "-B", str(_HERE / "probe.py"), "--plan", str(plan), "--out", str(res)], timeout=remaining_s(90))
    try:
        data = json.loads(res.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        data = None
    return {"exit_code": r["rc"], "error": r["error"], "result_json": data}


def make_session_class() -> Any:
    import appcontainer  # noqa: PLC0415

    class ProbeSession(appcontainer.AppContainerSession):
        """既存の複製処理に加えて、実験用の probe.py を複製先の `harness/exp_1427/` へ置く（既存ファイルは変えない）。"""

        def _copy_files(self) -> None:
            super()._copy_files()
            assert self._root is not None
            dest = self._root / "harness" / "exp_1427"
            dest.mkdir(parents=True, exist_ok=True)
            shutil.copy2(_HERE / "probe.py", dest / "probe.py")

    return ProbeSession


def run_appcontainer_group(exempt: bool, variants: list[dict[str, Any]], nic_ip: str | None,
                           record: dict[str, Any], filters: dict[str, Any],
                           timings: list[dict[str, Any]]) -> dict[str, Any]:
    """1 つのプロファイル（免除 あり / なし）で capability 3 通りを順に起動する。結果は variant 名をキーに返す。"""
    import appcontainer  # noqa: PLC0415

    cls = make_session_class()
    host = Path(sys.base_prefix) / "python.exe"  # 既存セッションが要求する host。ここでは複製されるだけで実行しない
    group: dict[str, Any] = {"variants": {}, "error": None, "cleanup": {}, "loopback_exempt": {"requested": exempt}}
    session = None
    try:
        session = cls(host=host, harness_dir=_HERE.parents[1], loopback_exempt=exempt, timeout_s=90)
        with stage(f"appcontainer-session-enter exempt={exempt}", timings), session:
            for i, v in enumerate(x for x in variants if x["actor"] == "appcontainer" and x["exempt"] is exempt):
                if expired():
                    group["variants"][v["name"]] = {"error": "overall deadline reached; not run"}
                    continue
                with stage(f"variant {v['name']}", timings):
                    plan_name, res_name = f"plan-{i}.json", f"res-{i}.json"
                    Path(session.out_path(plan_name)).write_text(
                        json.dumps({"nic_ip": nic_ip, "attempts": attempt_plan(v, nic_ip)}), encoding="utf-8")
                    res = session.run("exp_1427/probe.py",
                                      ["--plan", session.out_path(plan_name), "--out", session.out_path(res_name)],
                                      capabilities=tuple(v["caps"]), result_name=res_name, timeout_s=remaining_s(90))
                    group["variants"][v["name"]] = {"exit_code": res.get("exit_code"), "timed_out": res.get("timed_out"),
                                                   "error": res.get("error"), "result_json": res.get("result_json"),
                                                   "stderr_tail": (res.get("stderr_tail") or "")[:200]}
                time.sleep(0.5)
            if not expired():
                with stage(f"wfp-filters-snapshot exempt={exempt}", timings):
                    filters[f"exempt-{exempt}"] = snapshot_filters(Path(record["workdir"]), f"ac-{int(exempt)}")
    except (appcontainer.AppContainerError, OSError, ValueError) as exc:
        group["error"] = f"{type(exc).__name__}: {str(exc)[:160]}"
    finally:
        group["cleanup"] = dict(getattr(session, "cleanup", {}) or {})
        group["loopback_exempt"] = dict(getattr(session, "loopback_exempt_state", None) or {"requested": exempt})
    return group


def final_exempt_leftover() -> dict[str, Any]:
    """全セッション後に免除一覧へ `S-1-15-2-` の項目が残っていないか（件数のみ）。"""
    import appcontainer  # noqa: PLC0415

    try:
        rc, out = appcontainer._run_checknetisolation(appcontainer.checknetisolation_args("show", None, sysroot()))
    except (OSError, subprocess.SubprocessError, appcontainer.AppContainerError) as exc:
        return {"checked": False, "error": type(exc).__name__}
    return {"checked": rc == 0, "rc": rc, "appcontainer_sid_entries": out.count("S-1-15-2-")}


# --- 集計と出力 ------------------------------------------------------------------------------------------
def assemble(variants: list[dict[str, Any]], launches: dict[str, Any], hub_counts: dict[tuple[str, int], int],
             cap: dict[str, Any]) -> list[dict[str, Any]]:
    """各試行に listener 受信・pktmon・WFP の集計を port 経由で結び付けた行を作る。"""
    rows = []
    for v in variants:
        launch = launches.get(v["name"]) or {}
        data = launch.get("result_json") or {}
        for r in (data.get("results") or []):
            parts = str(r.get("id", "")).split("/")
            key = "/".join(parts[1:3]) if len(parts) >= 3 else ""
            vid = r.get("id", "")
            row = {"variant": v["name"], "actor": v["actor"], "exempt": v["exempt"], "caps": v["caps"],
                   "dest": parts[1] if len(parts) > 1 else "?", "proto": r.get("proto"),
                   "result": r.get("kind"), "errno": r.get("errno"), "winerror": r.get("winerror"),
                   "elapsed_ms": r.get("elapsed_ms"),
                   "listener_received": hub_counts.get((r.get("proto"), r.get("port")), 0),
                   "pktmon": cap["pktmon"]["by_variant"].get(vid), "wfp": cap["wfp"]["by_variant"].get(vid)}
            row["port_key"] = key
            rows.append(row)
    return rows


def _fmt_result(r: dict[str, Any]) -> str:
    base = str(r.get("result"))
    if r.get("result") == "error":
        base = f"error {r.get('errno')}/{r.get('winerror')}"
    return base


def render_summary(doc: dict[str, Any]) -> str:
    lines = ["## exp-1427: Windows egress probe (preliminary, not for merge)", ""]
    rc = doc.get("recon") or {}
    lines.append(f"- NIC IPv4 kind: `{rc.get('nic_ip_kind')}` (address hidden as `<nic-ip>`)")
    lines.append(f"- connection profile: `{json.dumps(rc.get('connection_profile'))[:300]}`")
    lines.append(f"- firewall profiles: `{json.dumps(rc.get('firewall_profile'))[:300]}`")
    lines.append(f"- services: `{json.dumps(rc.get('services'))[:200]}`")
    for tag, f in (doc.get("filters") or {}).items():
        lines.append(f"- WFP filters [{tag}]: total={f.get('total')} name matches={json.dumps(f.get('pattern_counts'))}")
    lines += ["", "| variant | dest | proto | result | ms | listener | pktmon pkts raw (unique flows) | wfp events |",
              "| ------- | ---- | ----- | ------ | -- | -------- | ------------------- | ---------- |"]
    for r in doc.get("rows") or []:
        pm = r.get("pktmon") or {}
        pkts = sum((pm.get("packets") or {}).values())
        uniq = sum((pm.get("unique_flows") or {}).values())
        wf = r.get("wfp") or {}
        lines.append(f"| {r['variant']} | {r['dest']} | {r['proto']} | {_fmt_result(r)} | {r.get('elapsed_ms')} | "
                     f"{r['listener_received']} | {pkts} (uniq {uniq}) | {json.dumps(wf.get('events') or {})} |")
    lines += ["", "### stage timings (ms)", ""]
    lines += [f"- {t['stage']}: {t['ms']} ({t['status']})" for t in doc.get("stage_timings") or []]
    lines += ["", "### cleanup", "", f"`{json.dumps(doc.get('cleanup'), sort_keys=True)}`", "",
              f"### errors: {json.dumps(doc.get('errors'))}"]
    return "\n".join(lines) + "\n"


def cleanup_ok(doc: dict[str, Any]) -> bool:
    cl = doc.get("cleanup") or {}
    for g in (cl.get("groups") or {}).values():
        if g.get("error") and not g.get("cleanup"):
            return False
        c = g.get("cleanup") or {}
        if any(v is False for v in c.values()):
            return False
        if g.get("loopback_exempt", {}).get("requested") and c.get("loopback_exempt_absent_after_remove") is not True:
            return False
    capc = cl.get("capture") or {}
    if capc.get("wfp_netevents_restore_rc") not in (0, "not-set", None):
        return False
    left = cl.get("exempt_leftover") or {}
    if left.get("checked") and left.get("appcontainer_sid_entries"):
        return False
    return True


def run(out_dir: Path) -> dict[str, Any]:
    """全体を `OVERALL_DEADLINE_S` 以内に収める。期限切れ・例外でも finally で片付けと、取れた分の集計・JSON 出力を行う。"""
    _DEADLINE[0] = time.monotonic() + OVERALL_DEADLINE_S
    out_dir.mkdir(parents=True, exist_ok=True)
    workdir = Path(tempfile.mkdtemp(prefix="exp1427-raw-", dir=os.environ.get("RUNNER_TEMP") or None))
    timings: list[dict[str, Any]] = []
    doc: dict[str, Any] = {"errors": [], "diagnostics": {}, "cleanup": {"groups": {}}, "filters": {},
                           "stage_timings": timings, "overall_deadline_s": OVERALL_DEADLINE_S}
    variants = build_variants()
    port_index = port_index_of(variants)
    hub = None
    caps = None
    launches: dict[str, Any] = {}
    cap_result: dict[str, Any] = {"pktmon": {"by_variant": {}, "diag": {}}, "wfp": {"by_variant": {}, "diag": {}}}
    filter_names: dict[str, str] = {}
    try:
        with stage("nic-detect", timings):
            nic_ip, nic_note = detect_nic_ip()
            doc["nic_detect"] = nic_note
        try:
            with stage("recon", timings):
                doc["recon"] = recon(nic_ip)
        except Exception as exc:  # noqa: BLE001  (観測ごとの失敗は記録して続行する)
            doc["errors"].append(f"recon: {type(exc).__name__}")
        try:
            with stage("wfp-filters-baseline", timings):
                base = snapshot_filters(workdir, "base")
                filter_names.update(base.pop("names", {}))
                doc["filters"]["baseline"] = base
        except Exception as exc:  # noqa: BLE001
            doc["errors"].append(f"filters-baseline: {type(exc).__name__}")
        with stage("listeners-start", timings):
            hub = Hub(nic_ip)
            for v in variants:
                for key, port in v["ports"].items():
                    d, p = key.split("/")
                    if d == "a":
                        hub.add(p, "127.0.0.2", port)
                    elif d == "b" and nic_ip:
                        hub.add(p, nic_ip, port)
            hub.start()
            doc["listener_bind_errors"] = list(hub.errors)
        caps = Captures(workdir, nic_ip)
        try:
            caps.start(timings)
        except Exception as exc:  # noqa: BLE001
            doc["errors"].append(f"capture-start: {type(exc).__name__}")
        time.sleep(1.0)
        if not expired():
            try:
                with stage("variant control", timings):
                    launches["control"] = run_control(variants[0], nic_ip, workdir)
            except Exception as exc:  # noqa: BLE001
                doc["errors"].append(f"control: {type(exc).__name__}")
        time.sleep(0.5)
        filt: dict[str, Any] = {}
        for exempt in (False, True):
            if expired():
                doc["errors"].append(f"group exempt={exempt}: overall deadline reached; not run")
                continue
            try:
                g = run_appcontainer_group(exempt, variants, nic_ip, {"workdir": str(workdir)}, filt, timings)
            except Exception as exc:  # noqa: BLE001
                g = {"error": f"{type(exc).__name__}", "variants": {}, "cleanup": {}}
            doc["cleanup"]["groups"][f"exempt-{exempt}"] = {k: g.get(k) for k in ("error", "cleanup", "loopback_exempt")}
            launches.update(g.get("variants") or {})
            if g.get("error"):
                doc["errors"].append(f"group exempt={exempt}: {g['error']}")
        for tag, f in filt.items():
            filter_names.update(f.pop("names", {}))
            doc["filters"][tag] = f
        time.sleep(1.0)
        # 集計は期限切れでも取れた分だけ行う（停止・読み出しは個別のコマンド timeout で必ず戻る）。
        try:
            cap_result = caps.stop_and_collect(port_index, filter_names, timings)
        except Exception as exc:  # noqa: BLE001
            doc["errors"].append(f"capture-collect: {type(exc).__name__}")
        doc["capture_status"] = caps.status
    except Exception as exc:  # noqa: BLE001
        doc["errors"].append(f"fatal: {type(exc).__name__}: {str(exc)[:120]}")
    finally:
        if expired():
            doc["errors"].append("overall deadline reached")
        counts = dict(hub.counts) if hub else {}
        if hub:
            hub.stop()
        if caps:
            try:
                with stage("capture-cleanup", timings):
                    doc["cleanup"]["capture"] = caps.cleanup()
                doc["capture_status"] = caps.status
            except Exception as exc:  # noqa: BLE001
                doc["cleanup"]["capture"] = {"error": type(exc).__name__}
        try:
            with stage("exempt-leftover-check", timings):
                doc["cleanup"]["exempt_leftover"] = final_exempt_leftover()
        except Exception as exc:  # noqa: BLE001
            doc["cleanup"]["exempt_leftover"] = {"checked": False, "error": type(exc).__name__}
        shutil.rmtree(workdir, ignore_errors=True)
        doc["cleanup"]["raw_workdir_removed"] = not workdir.exists()
        doc["rows"] = assemble(variants, launches, counts, cap_result)
        doc["diagnostics"] = {"pktmon": cap_result["pktmon"].get("diag"), "wfp": cap_result["wfp"].get("diag"),
                              "unattributed": {"pktmon": cap_result["pktmon"]["by_variant"].get("unattributed"),
                                               "wfp": cap_result["wfp"]["by_variant"].get("unattributed")}}
        doc["launch_status"] = {k: {kk: vv for kk, vv in v.items() if kk != "result_json"} for k, v in launches.items()}
        with stage("write-output", timings):
            (out_dir / "exp1427.json").write_text(json.dumps(doc, indent=2, sort_keys=True, default=str) + "\n",
                                                  encoding="utf-8")
            (out_dir / "summary.md").write_text(render_summary(doc), encoding="utf-8")
    return doc


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="#1427 Windows egress preliminary experiment (runner side).")
    parser.add_argument("--out", required=True, type=Path)
    args = parser.parse_args(argv)
    if sys.platform != "win32":
        print("error: this experiment runs on a disposable Windows runner only")
        return 2
    doc = run(args.out)
    print(f"done: rows={len(doc.get('rows') or [])} errors={len(doc['errors'])} cleanup_ok={cleanup_ok(doc)}")
    return 0 if cleanup_ok(doc) else 1


if __name__ == "__main__":
    raise SystemExit(main())
