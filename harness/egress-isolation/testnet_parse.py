#!/usr/bin/env python3
"""pktmon の pcapng と WFP net events の xml を解析して、TEST-NET-1 宛ての集計だけを返す（純粋関数）。

役割・責務境界:
    Issue #1515（TASK-38・`RENDER-3`・`XOS-1`・`SEC-2`・予備実験 #1427）。`testnet_observe.py` が採取した生データ
    （etl2pcap の pcapng・`netsh wfp show netevents` の xml・`netsh wfp show filters` の xml）を、標準ライブラリだけで読み、
    宛先ポートで経路に帰属させた件数・種類に落とす。生データそのものは返さない・保持しない（アーティファクトとログに
    出さないための境界。送信元 IP・SID の値・範囲外の宛先は集計の外に出さず、範囲外は件数だけ数える）。
    I/O はファイルオブジェクトかパスの受け取りだけで、Linux でも合成した入力で自己テストできる。

呼び出し文脈: `testnet_observe.TestnetObserver` が採取後に呼び、結果を `judge.judge_testnet` へ渡す。

壊れた入力は例外にせず `diag.error` / `diag.parse_error` に残して、そこまでの集計を返す（呼び出し側が unobservable にする）。
"""

from __future__ import annotations

import collections
import re
import socket
import struct
import xml.etree.ElementTree as ET
from typing import Any

import testnet

_LINK_ETHERNET = 1
_LINK_NULL = 0
_LINK_RAW_TYPES = (12, 14, 101, 228)
_PCAPNG_SHB, _PCAPNG_IDB, _PCAPNG_SPB, _PCAPNG_EPB = 0x0A0D0D0A, 1, 3, 6
MAX_BLOCK_BYTES = 16 * 1024 * 1024
MAX_UNIQUE_FLOWS = 100000
MAX_DIAG_ENTRIES = 16
UNATTRIBUTED = "unattributed"

# net event のうち「遮断した」と数える type。実測で観測した値に限る（それ以外は件数を診断に残すだけ）。
DROP_TYPES = frozenset({"FWPM_NET_EVENT_TYPE_CLASSIFY_DROP", "FWPM_NET_EVENT_TYPE_PUBLIC_CLASSIFY_DROP"})
PACKAGE_SID_RE = re.compile(r"^S-1-15-2-[0-9]+(?:-[0-9]+)+$", re.IGNORECASE)
# 足場の確認に使う filter 名（固定集合）。AppContainer の既定の遮断規則。
SCAFFOLD_FILTER_PATTERNS = ("UWP Default Outbound Block Rule", "Block Outbound Default Rule",
                            "InternetClient Default Rule")


def shape(line: str, limit: int = 120) -> str:
    """生出力の「形」だけを残す（数字を # に潰す。IP・GUID・SID の値も潰れる）。"""
    return re.sub(r"\d+", "#", line.strip())[:limit]


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


def _attribute(index: dict[tuple[str, int], tuple[str, str]], proto: str, port: int) -> str:
    hit = index.get((proto, port))
    return testnet.route_key(*hit) if hit else UNATTRIBUTED


def parse_pcapng(f: Any, index: dict[tuple[str, int], tuple[str, str]]) -> dict[str, Any]:
    """pktmon の pcapng（SHB・IDB・EPB・SPB）を読み、TEST-NET-1 宛てだけを経路キーごとに集計する。

    pktmon は同じパケットを複数コンポーネントで記録するため、`packets`（生の件数）と `unique_flows`（送信元込みの
    5 タプル + kind の重複除去後の件数）の両方を出す。範囲外の宛先は件数（`out_of_scope`）だけで値は出さない。
    壊れたファイルは例外にせず `diag.error` に残し、そこまでの集計を返す。
    """
    diag: dict[str, Any] = {"block_types": collections.Counter(), "linktypes": collections.Counter(),
                            "frames": 0, "ipv4_decoded": 0, "in_scope": 0, "out_of_scope": 0}
    out: dict[str, dict[str, Any]] = {}
    seen: dict[str, set[tuple[Any, ...]]] = {}
    endian = "<"
    links: list[int] = []
    try:
        while True:
            head = f.read(8)
            if len(head) == 0:
                break
            if len(head) < 8:
                diag["error"] = "truncated block header"
                break
            if struct.unpack("<I", head[:4])[0] == _PCAPNG_SHB:
                # ブロック長の解釈にエンディアンが要るため、先に byte-order magic を見る。
                body = f.read(8)
                if len(body) < 8:
                    diag["error"] = "truncated SHB"
                    break
                magic = struct.unpack("<I", body[:4])[0]
                endian = ">" if magic == 0x4D3C2B1A else "<"
                total = struct.unpack(endian + "I", head[4:8])[0]
                if total < 16 or total > MAX_BLOCK_BYTES:
                    diag["error"] = "bad SHB length"
                    break
                rest = f.read(total - 16)
                if len(rest) < total - 16:
                    diag["error"] = "truncated SHB"
                    break
                links = []
                diag["block_types"]["SHB"] += 1
                continue
            btype, total = struct.unpack(endian + "II", head)
            if total < 12 or total > MAX_BLOCK_BYTES:
                diag["error"] = "bad block length"
                break
            body = f.read(total - 12)
            tail = f.read(4)
            if len(body) < total - 12 or len(tail) < 4:
                diag["error"] = "truncated block"
                break
            name = {_PCAPNG_IDB: "IDB", _PCAPNG_EPB: "EPB", _PCAPNG_SPB: "SPB"}.get(btype, "other")
            diag["block_types"][name] += 1
            frame: bytes | None = None
            link = -1
            if btype == _PCAPNG_IDB and len(body) >= 8:
                links.append(struct.unpack_from(endian + "H", body, 0)[0])
                diag["linktypes"][str(links[-1])] += 1
            elif btype == _PCAPNG_EPB and len(body) >= 20:
                ifid, _hi, _lo, caplen, _orig = struct.unpack_from(endian + "IIIII", body, 0)
                frame = body[20:20 + caplen]
                link = links[ifid] if ifid < len(links) else -1
            elif btype == _PCAPNG_SPB and len(body) >= 4:
                orig = struct.unpack_from(endian + "I", body, 0)[0]
                frame = body[4:4 + min(orig, len(body) - 4)]
                link = links[0] if links else -1
            if frame is None:
                continue
            diag["frames"] += 1
            dec = decode_ipv4_packet(frame, link)
            if dec is None:
                continue
            diag["ipv4_decoded"] += 1
            src, sport, dst, dport, kind = dec
            if not testnet.in_testnet(dst):
                diag["out_of_scope"] += 1
                continue
            diag["in_scope"] += 1
            key = _attribute(index, "udp" if kind == "udp" else "tcp", dport)
            ent = out.setdefault(key, {"packets": collections.Counter(), "unique_flows": collections.Counter()})
            ent["packets"][kind] += 1
            flow_set = seen.setdefault(key, set())
            flow = (src, sport, dport, kind)
            if flow not in flow_set and len(flow_set) < MAX_UNIQUE_FLOWS:
                flow_set.add(flow)
                ent["unique_flows"][kind] += 1
    except (OSError, struct.error) as exc:
        diag["error"] = type(exc).__name__
    for k in ("block_types", "linktypes"):
        diag[k] = dict(diag[k])
    routes = {k: {f2: dict(v) for f2, v in ent.items()} for k, ent in out.items()}
    return {"routes": routes, "diag": diag}


def _local(tag: str) -> str:
    return tag.rsplit("}", 1)[-1]


def _flat(el: ET.Element) -> dict[str, str]:
    """要素以下のタグ名（ローカル名）-> 最初の非空テキスト。"""
    out: dict[str, str] = {}
    for sub in el.iter():
        text = (sub.text or "").strip()
        if text and _local(sub.tag) not in out:
            out[_local(sub.tag)] = text
    return out


def _ipv4_text(text: str | None) -> str | None:
    return text if text is not None and re.fullmatch(r"\d+\.\d+\.\d+\.\d+", text) else None


def _package_sid(el: ET.Element) -> str | None:
    for sub in el.iter():
        text = (sub.text or "").strip()
        if text and PACKAGE_SID_RE.match(text):
            return text
    return None


def parse_wfp_events(source: Any, index: dict[tuple[str, int], tuple[str, str]],
                     profile_sid: str | None = None) -> dict[str, Any]:
    """`netsh wfp show netevents` の xml の net event を TEST-NET-1 宛てで絞り、経路キーごとに集計する。

    `drops` は許可リスト（`DROP_TYPES`）の type の件数、`drops_with_sid` はそのうち package SID（S-1-15-2-…）つき、
    `package_sid_match` はさらに `profile_sid`（ハーネスが作った AppContainer の SID）と一致したもの。SID の値は
    メモリ上で比べるだけで、結果には件数だけを残す。`source` は ET.iterparse が受け付けるパスかファイルオブジェクト。
    """
    out: dict[str, dict[str, Any]] = {}
    diag: dict[str, Any] = {"items_with_header": 0, "in_scope": 0, "out_of_scope": 0, "unparsed_addr": 0,
                            "first_item_tags": None, "type_names": collections.Counter()}
    want_sid = profile_sid.lower() if profile_sid else None
    try:
        for _ev, el in ET.iterparse(source, events=("end",)):
            if _local(el.tag) != "item":
                continue
            header = next((c for c in el if _local(c.tag) == "header"), None)
            if header is None:
                continue
            diag["items_with_header"] += 1
            flat, hflat = _flat(el), _flat(header)
            if diag["first_item_tags"] is None:
                diag["first_item_tags"] = sorted({_local(s.tag) for s in el.iter()})[:60]
            typ = flat.get("type", "unknown")[:64]
            if typ in diag["type_names"] or len(diag["type_names"]) < MAX_DIAG_ENTRIES:
                diag["type_names"][typ] += 1
            remote = _ipv4_text(hflat.get("remoteAddrV4") or hflat.get("remoteAddr"))
            if remote is None:
                diag["unparsed_addr"] += 1
                el.clear()
                continue
            if not testnet.in_testnet(remote):
                diag["out_of_scope"] += 1
                el.clear()
                continue
            diag["in_scope"] += 1
            proto = {"6": "tcp", "17": "udp"}.get(hflat.get("ipProtocol", ""), "other")
            try:
                port = int(hflat.get("remotePort", "0"))
            except ValueError:
                port = 0
            key = _attribute(index, proto, port)
            ent = out.setdefault(key, {"events": collections.Counter(), "filters": collections.Counter(),
                                       "drops": 0, "drops_with_sid": 0, "package_sid_match": 0})
            if typ in ent["events"] or len(ent["events"]) < MAX_DIAG_ENTRIES:
                ent["events"][typ] += 1
            if "filterId" in flat and (flat["filterId"][:16] in ent["filters"] or len(ent["filters"]) < MAX_DIAG_ENTRIES):
                ent["filters"][flat["filterId"][:16]] += 1
            if typ in DROP_TYPES:
                ent["drops"] += 1
                sid = _package_sid(el)
                if sid is not None:
                    ent["drops_with_sid"] += 1
                    if want_sid is not None and sid.lower() == want_sid:
                        ent["package_sid_match"] += 1
            el.clear()
    except ET.ParseError as exc:
        diag["parse_error"] = str(exc)[:80]
    diag["type_names"] = dict(diag["type_names"])
    routes = {k: {f: (dict(v) if isinstance(v, collections.Counter) else v) for f, v in ent.items()}
              for k, ent in out.items()}
    return {"routes": routes, "diag": diag}


def parse_filters_xml(source: Any) -> dict[str, Any]:
    """`netsh wfp show filters` の xml から filterId -> 名前、全体の件数、足場の filter 名の一致件数を得る（名前のみ）。"""
    names: dict[str, str] = {}
    total = 0
    patterns = {p: 0 for p in SCAFFOLD_FILTER_PATTERNS}
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
            name = next(((c.text or "").strip() for c in kids["displayData"] if _local(c.tag) == "name"), "")[:80]
            names[fid] = name
            low = name.lower()
            for p in SCAFFOLD_FILTER_PATTERNS:
                if p.lower() in low:
                    patterns[p] += 1
            el.clear()
    except ET.ParseError as exc:
        diag["parse_error"] = str(exc)[:80]
    return {"names": names, "total": total, "pattern_counts": patterns, "diag": diag}


def parse_netevents_option(text: str) -> bool | None:
    """`netsh wfp show options optionsfor=netevents` の出力から net events が有効かを推定する。判別できなければ None。"""
    for line in text.splitlines():
        low = line.lower()
        if "netevent" in low or "net event" in low:
            if re.search(r"\b(on|enabled|true)\b", low):
                return True
            if re.search(r"\b(off|disabled|false)\b", low):
                return False
    return None


def parse_pktmon_running(text: str) -> bool | None:
    """`pktmon status` の出力から採取中かを推定する。判別できなければ None（呼び出し側は fail-closed にする）。"""
    low = text.lower()
    if re.search(r"not\s+(running|started|active)|\bstopped\b|\binactive\b", low):
        return False
    if re.search(r"\b(running|active|started|collecting)\b", low):
        return True
    return None


def parse_pktmon_filters_empty(text: str) -> bool | None:
    """`pktmon filter list` の出力から filter が空かを推定する。判別できなければ None。"""
    low = text.lower()
    if re.search(r"no\s+filters?\b", low):
        return True
    if re.search(r"\b(ip address|protocol|port|mac address)\b", low):
        return False
    return None


def route_summary(routes: dict[str, Any], key: str) -> dict[str, int]:
    """経路キーの pktmon 集計から (生の件数, 重複除去後の件数) を取り出す。無ければ 0。"""
    ent = routes.get(key) or {}
    return {"raw": sum((ent.get("packets") or {}).values()), "unique": sum((ent.get("unique_flows") or {}).values())}
