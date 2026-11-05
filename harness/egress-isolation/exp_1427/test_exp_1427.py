"""#1427 予備実験の純粋関数（宛先検証・pktmon / WFP 集計・計画）の自己テスト。Windows 専用の実行系は対象外。"""

from __future__ import annotations

import io
import socket
import struct
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import probe  # noqa: E402
import run_probe  # noqa: E402

NIC = "10.1.2.3"

PKTMON = """\
[ 3] 0000.1A2B::2026-11-05 10:00:00.1 [Microsoft-Windows-PktMon] PktGroupId 1, PktNumber 1, Appearance 1, Direction Tx , Type Ethernet , Component 7, Edge 1, Filter 1, OriginalSize 66, LoggedSize 66
\t10.1.2.3.50001 > 192.0.2.1.47010: Flags [S], seq 1, win 64240, length 0
[ 3] 0000.1A2B::2026-11-05 10:00:00.2 [Microsoft-Windows-PktMon] PktGroupId 2, PktNumber 1, Appearance 1, Direction Tx , Type Ethernet , Component 7, Edge 1, Filter 1, OriginalSize 60, LoggedSize 60
\t10.1.2.3.50002 > 192.0.2.1.47011: UDP, length 17
[ 3] 0000.1A2B::2026-11-05 10:00:00.3 [Microsoft-Windows-PktMon] PktGroupId 3, PktNumber 1, Appearance 2, Direction Rx , Type Ethernet , Drop Reason Filter drop, Component 9, Edge 1, Filter 1, OriginalSize 60, LoggedSize 60
\t10.1.2.3.50003 > 192.0.2.1.47010: Flags [S], seq 2, length 0
[ 3] 0000.1A2B::2026-11-05 10:00:00.4 [Microsoft-Windows-PktMon] PktGroupId 4, PktNumber 1, Appearance 1, Direction Tx , Type Ethernet , Component 7, Edge 1, Filter 1, OriginalSize 60, LoggedSize 60
\t10.1.2.3.50004 > 93.184.216.34.443: Flags [S], seq 3, length 0
"""

WFP = """\
<?xml version="1.0"?>
<wfpdiag><netEvents numItems="3">
<item><header><ipProtocol>6</ipProtocol><localAddrV4>10.1.2.3</localAddrV4><remoteAddrV4>192.0.2.1</remoteAddrV4>
<localPort>50001</localPort><remotePort>47010</remotePort><appId>c:\\python.exe</appId><packageSid>S-1-15-2-1</packageSid></header>
<type>FWPM_NET_EVENT_TYPE_CLASSIFY_DROP</type><classifyDrop><filterId>66012</filterId><layerId>48</layerId></classifyDrop></item>
<item><header><ipProtocol>17</ipProtocol><remoteAddrV4>10.1.2.3</remoteAddrV4><remotePort>47011</remotePort></header>
<type>FWPM_NET_EVENT_TYPE_CLASSIFY_DROP</type><classifyDrop><filterId>66013</filterId></classifyDrop></item>
<item><header><ipProtocol>6</ipProtocol><remoteAddrV4>8.8.8.8</remoteAddrV4><remotePort>47010</remotePort></header>
<type>FWPM_NET_EVENT_TYPE_CLASSIFY_DROP</type></item>
</netEvents></wfpdiag>
"""

FILTERS = """\
<wfpdiag><filters numItems="2">
<item><filterKey>{a}</filterKey><displayData><name>Block Outbound Default Rule</name></displayData><filterId>66012</filterId></item>
<item><filterKey>{b}</filterKey><displayData><name>AppContainerLoopback exempt</name></displayData><filterId>66013</filterId></item>
</filters></wfpdiag>
"""


class ValidateDest(unittest.TestCase):
    def test_approved_ranges(self) -> None:
        self.assertEqual(probe.validate_dest("127.0.0.2", 47000), "loopback")
        self.assertEqual(probe.validate_dest("192.0.2.1", 47000), "testnet")
        self.assertEqual(probe.validate_dest(NIC, 47000, NIC), "nic")

    def test_rejects_outside(self) -> None:
        for ip in ("8.8.8.8", "10.1.2.3", "192.0.3.1", "198.51.100.1", "example.com", "::1"):
            with self.assertRaises(ValueError, msg=ip):
                probe.validate_dest(ip, 47000)
        with self.assertRaises(ValueError):
            probe.validate_dest("10.1.2.4", 47000, NIC)
        with self.assertRaises(ValueError):
            probe.validate_dest("127.0.0.2", 80)

    def test_nic_ip_must_not_be_loopback_or_testnet(self) -> None:
        for bad in ("127.0.0.1", "192.0.2.5", "0.0.0.0", "224.0.0.1", "x"):
            with self.assertRaises(ValueError, msg=bad):
                probe.parse_nic_ip(bad)

    def test_run_plan_rejects_whole_plan_before_any_attempt(self) -> None:
        plan = {"nic_ip": NIC, "attempts": [{"id": "x", "proto": "udp", "ip": "127.0.0.2", "port": 47000},
                                            {"id": "y", "proto": "tcp", "ip": "1.1.1.1", "port": 47001}]}
        with self.assertRaises(ValueError):
            probe.run_plan(plan)

    def test_attempt_udp_loopback_sent(self) -> None:
        r = probe.attempt("udp", "127.0.0.2", 47999, None)
        self.assertEqual(r["kind"], "sent")
        self.assertEqual(r["bytes"], 17)


class Parsers(unittest.TestCase):
    index = {("tcp", 47010): "v/c/tcp", ("udp", 47011): "v/c/udp"}

    def test_pktmon(self) -> None:
        res = run_probe.parse_pktmon_text(io.StringIO(PKTMON), NIC, self.index)
        by = res["by_variant"]
        self.assertEqual(by["v/c/tcp"]["packets"], {"tcp_syn": 2})
        self.assertEqual(by["v/c/udp"]["packets"], {"udp": 1})
        self.assertEqual(by["v/c/tcp"]["components"], {"7": 1, "9": 1})
        self.assertEqual(sum(by["v/c/tcp"]["drops"].values()), 1)
        self.assertEqual(res["diag"]["ip_lines"], 4)
        self.assertEqual(res["diag"]["ip_lines_in_scope"], 3)
        self.assertNotIn("192.0.2", str(res["diag"]["first_ip_line_shape"]))

    def test_pktmon_unknown_format_does_not_raise(self) -> None:
        res = run_probe.parse_pktmon_text(io.StringIO("garbage\nmore\n"), NIC, self.index)
        self.assertEqual(res["by_variant"], {})
        self.assertEqual(res["diag"]["lines"], 2)

    def test_wfp(self) -> None:
        res = run_probe.parse_wfp_events(io.StringIO(WFP), NIC, self.index)
        by = res["by_variant"]
        self.assertEqual(by["v/c/tcp"]["events"], {"FWPM_NET_EVENT_TYPE_CLASSIFY_DROP": 1})
        self.assertEqual(by["v/c/tcp"]["filters"], {"66012": 1})
        self.assertEqual(by["v/c/tcp"]["package_sid_events"], 1)
        self.assertEqual(by["v/c/tcp"]["app_id_events"], 1)
        self.assertEqual(by["v/c/udp"]["package_sid_events"], 0)
        self.assertEqual(res["diag"]["in_scope"], 2)

    def test_wfp_broken_xml_is_recorded(self) -> None:
        res = run_probe.parse_wfp_events(io.StringIO("<a><item>"), NIC, self.index)
        self.assertIn("parse_error", res["diag"])

    def test_filters(self) -> None:
        res = run_probe.parse_filters_xml(io.StringIO(FILTERS))
        self.assertEqual(res["names"], {"66012": "Block Outbound Default Rule", "66013": "AppContainerLoopback exempt"})
        self.assertEqual(res["total"], 2)
        self.assertEqual(res["pattern_counts"]["Block Outbound Default Rule"], 1)
        self.assertEqual(res["pattern_counts"]["AppContainerLoopback"], 1)


def _eth_ipv4(src: str, dst: str, proto: int, l4: bytes) -> bytes:
    ip = struct.pack("!BBHHHBBH4s4s", 0x45, 0, 20 + len(l4), 1, 0, 64, proto, 0,
                     socket.inet_aton(src), socket.inet_aton(dst))
    return b"\x00" * 12 + b"\x08\x00" + ip + l4


def _tcp(sport: int, dport: int, flags: int) -> bytes:
    return struct.pack("!HHIIBBHHH", sport, dport, 0, 0, 5 << 4, flags, 1024, 0, 0)


def _udp(sport: int, dport: int) -> bytes:
    return struct.pack("!HHHH", sport, dport, 9, 0) + b"x"


def _block(btype: int, body: bytes) -> bytes:
    pad = body + b"\x00" * (-len(body) % 4)
    total = 12 + len(pad)
    return struct.pack("<II", btype, total) + pad + struct.pack("<I", total)


def make_pcapng(frames: list[bytes], comments: list[str | None] | None = None) -> bytes:
    shb = _block(0x0A0D0D0A, struct.pack("<IHHq", 0x1A2B3C4D, 1, 0, -1))
    idb = _block(1, struct.pack("<HHI", 1, 0, 0))
    out = shb + idb
    for i, fr in enumerate(frames):
        body = struct.pack("<IIIII", 0, 0, 0, len(fr), len(fr)) + fr + b"\x00" * (-len(fr) % 4)
        c = (comments or [None] * len(frames))[i]
        if c:
            cb = c.encode()
            body += struct.pack("<HH", 1, len(cb)) + cb + b"\x00" * (-len(cb) % 4) + struct.pack("<HH", 0, 0)
        out += _block(6, body)
    return out


class Pcapng(unittest.TestCase):
    index = {("tcp", 47010): "v/c/tcp", ("udp", 47011): "v/c/udp", ("tcp", 47020): "v/b/tcp"}

    def test_attribution_dedup_and_scope(self) -> None:
        syn = _eth_ipv4(NIC, "192.0.2.1", 6, _tcp(50001, 47010, 0x02))
        udp = _eth_ipv4(NIC, "192.0.2.1", 17, _udp(50002, 47011))
        nic_syn = _eth_ipv4("127.0.0.1", NIC, 6, _tcp(50003, 47020, 0x02))
        synack = _eth_ipv4(NIC, "192.0.2.1", 6, _tcp(50001, 47010, 0x12))
        out_scope = _eth_ipv4(NIC, "93.184.216.34", 6, _tcp(50004, 47010, 0x02))
        arp = b"\x00" * 12 + b"\x08\x06" + b"\x00" * 28
        data = make_pcapng([syn, syn, udp, nic_syn, synack, out_scope, arp],
                           ["Component 7", "Component 9", None, None, None, None, None])
        res = run_probe.parse_pcapng(io.BytesIO(data), NIC, self.index)
        by, diag = res["by_variant"], res["diag"]
        self.assertEqual(by["v/c/tcp"]["packets"], {"tcp_syn": 2, "tcp_other": 1})
        self.assertEqual(by["v/c/tcp"]["unique_flows"], {"tcp_syn": 1, "tcp_other": 1})
        self.assertEqual(by["v/c/tcp"]["present"], {"tcp_syn": 1, "tcp_other": 1})
        self.assertEqual(by["v/c/tcp"]["components"], {"7": 1, "9": 1})
        self.assertEqual(by["v/c/udp"]["packets"], {"udp": 1})
        self.assertEqual(by["v/b/tcp"]["packets"], {"tcp_syn": 1})
        self.assertEqual(diag["block_types"], {"SHB": 1, "IDB": 1, "EPB": 7})
        self.assertEqual(diag["linktypes"], {"1": 1})
        self.assertEqual(diag["frames"], 7)
        self.assertEqual(diag["ipv4_decoded"], 6)
        self.assertEqual(diag["in_scope"], 5)
        self.assertEqual(diag["out_of_scope"], 1)
        self.assertEqual(diag["epb_with_comment"], 2)
        self.assertNotIn("93.184", str(res))

    def test_truncated_file_keeps_partial_result(self) -> None:
        syn = _eth_ipv4(NIC, "192.0.2.1", 6, _tcp(50001, 47010, 0x02))
        data = make_pcapng([syn, syn])
        res = run_probe.parse_pcapng(io.BytesIO(data[:-10]), NIC, self.index)
        self.assertEqual(res["by_variant"]["v/c/tcp"]["packets"], {"tcp_syn": 1})
        self.assertIn("error", res["diag"])

    def test_garbage_does_not_raise(self) -> None:
        res = run_probe.parse_pcapng(io.BytesIO(b"\xff" * 64), NIC, self.index)
        self.assertEqual(res["by_variant"], {})
        self.assertIn("error", res["diag"])

    def test_raw_ipv4_linktype(self) -> None:
        raw = _eth_ipv4(NIC, "192.0.2.1", 17, _udp(50002, 47011))[14:]
        self.assertEqual(run_probe.decode_ipv4_packet(raw, 101)[2:4], ("192.0.2.1", 47011))
        self.assertIsNone(run_probe.decode_ipv4_packet(raw, 9999))


class Commands(unittest.TestCase):
    def test_netevents_option_parse(self) -> None:
        self.assertIs(run_probe.parse_netevents_option("Net events: ON\nother: off"), True)
        self.assertIs(run_probe.parse_netevents_option("netevents = disabled"), False)
        self.assertIsNone(run_probe.parse_netevents_option("nothing relevant"))

    def test_run_cmd_returns_on_timeout(self) -> None:
        import time

        t0 = time.monotonic()
        r = run_probe.run_cmd([sys.executable, "-c", "import time; time.sleep(30)"], timeout=1)
        self.assertEqual(r["error"], "TimeoutExpired")
        self.assertIsNone(r["rc"])
        self.assertLess(time.monotonic() - t0, 15)

    def test_run_cmd_collects_output(self) -> None:
        r = run_probe.run_cmd([sys.executable, "-c", "print('hello')"], timeout=20)
        self.assertEqual(r["rc"], 0)
        self.assertIn("hello", r["out"])

    def test_run_cmd_missing_executable(self) -> None:
        r = run_probe.run_cmd(["/nonexistent/definitely-not-here"], timeout=5)
        self.assertIsNone(r["rc"])
        self.assertEqual(r["error"], "FileNotFoundError")


class Plan(unittest.TestCase):
    def test_variants_and_unique_ports(self) -> None:
        vs = run_probe.build_variants()
        self.assertEqual([v["name"] for v in vs][:2], ["control", "ac-noexempt-nocap"])
        self.assertEqual(len(vs), 7)
        ports = [p for v in vs for p in v["ports"].values()]
        self.assertEqual(len(ports), len(set(ports)))
        self.assertEqual(len(ports), 7 * 6)
        plan = run_probe.attempt_plan(vs[1], NIC)
        self.assertEqual(len(plan), 6)
        for a in plan:
            probe.validate_dest(a["ip"], a["port"], NIC)
        self.assertEqual(len(run_probe.attempt_plan(vs[1], None)), 4)

    def test_ip_kind_hides_address(self) -> None:
        self.assertEqual(run_probe.ip_kind("10.1.2.3"), "private-10.x")
        self.assertEqual(run_probe.ip_kind("172.20.0.1"), "private-172.16/12")
        self.assertEqual(run_probe.ip_kind("8.8.8.8"), "other")
        self.assertNotIn("10.1", run_probe.shape("10.1.2.3.50001 > 192.0.2.1.47010"))


if __name__ == "__main__":
    unittest.main()
