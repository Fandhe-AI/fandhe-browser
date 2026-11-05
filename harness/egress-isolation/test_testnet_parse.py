"""testnet_parse.py の自己テスト（Issue #1515）。合成した pcapng と xml だけを使う（Windows・pktmon は不要）。"""

from __future__ import annotations

import io
import socket
import struct
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import testnet  # noqa: E402
import testnet_parse as tp  # noqa: E402

INDEX = testnet.port_index()
SID = "S-1-15-2-1111111111-2222222222-3333333333-4444444444-5555555555-6666666666-7777777777"
OTHER_SID = "S-1-15-2-9-9-9-9-9-9-9"
NIC = "10.1.2.3"


def ipv4(src: str, sport: int, dst: str, dport: int, kind: str) -> bytes:
    if kind == "udp":
        l4 = struct.pack("!HHHH", sport, dport, 8, 0)
        proto = 17
    else:
        flags = {"tcp_syn": 0x02, "tcp_other": 0x10}[kind]
        l4 = struct.pack("!HHIIBBHHH", sport, dport, 0, 0, 5 << 4, flags, 1000, 0, 0)
        proto = 6
    total = 20 + len(l4)
    hdr = struct.pack("!BBHHHBBH4s4s", 0x45, 0, total, 0, 0, 64, proto, 0, socket.inet_aton(src), socket.inet_aton(dst))
    return hdr + l4


def eth(packet: bytes) -> bytes:
    return b"\x00" * 12 + b"\x08\x00" + packet


def block(btype: int, body: bytes, endian: str = "<") -> bytes:
    body += b"\x00" * ((-len(body)) % 4)
    total = 12 + len(body)
    return struct.pack(endian + "II", btype, total) + body + struct.pack(endian + "I", total)


def shb(endian: str = "<") -> bytes:
    return block(0x0A0D0D0A, struct.pack(endian + "IHHq", 0x1A2B3C4D, 1, 0, -1), endian)


def idb(linktype: int = 1, endian: str = "<") -> bytes:
    return block(1, struct.pack(endian + "HHI", linktype, 0, 65535), endian)


def epb(frame: bytes, endian: str = "<") -> bytes:
    return block(6, struct.pack(endian + "IIIII", 0, 0, 0, len(frame), len(frame)) + frame, endian)


def pcap(*frames: bytes, endian: str = "<") -> io.BytesIO:
    return io.BytesIO(shb(endian) + idb(1, endian) + b"".join(epb(f, endian) for f in frames))


class DecodeTest(unittest.TestCase):
    def test_syn_other_and_udp(self) -> None:
        self.assertEqual(tp.decode_ipv4_packet(eth(ipv4(NIC, 5000, "192.0.2.1", 47300, "tcp_syn")), 1),
                         (NIC, 5000, "192.0.2.1", 47300, "tcp_syn"))
        self.assertEqual(tp.decode_ipv4_packet(eth(ipv4(NIC, 5000, "192.0.2.1", 47300, "tcp_other")), 1)[4], "tcp_other")
        self.assertEqual(tp.decode_ipv4_packet(eth(ipv4(NIC, 5000, "192.0.2.1", 47304, "udp")), 1)[4], "udp")

    def test_non_ipv4_and_short_frames_are_none(self) -> None:
        self.assertIsNone(tp.decode_ipv4_packet(b"\x00" * 10, 1))
        self.assertIsNone(tp.decode_ipv4_packet(b"\x00" * 12 + b"\x86\xdd" + b"\x00" * 40, 1))
        self.assertIsNone(tp.decode_ipv4_packet(b"x", 999))


class PcapngTest(unittest.TestCase):
    def test_attribution_by_mode_route_and_proto(self) -> None:
        res = tp.parse_pcapng(pcap(
            eth(ipv4(NIC, 5000, "192.0.2.1", 47300, "tcp_syn")),   # baseline/testnet_redirect
            eth(ipv4(NIC, 5001, "192.0.2.1", 47319, "tcp_syn")),   # appcontainer/testnet_ws
            eth(ipv4(NIC, 5002, "192.0.2.1", 47304, "udp")),       # baseline/testnet_udp
        ), INDEX)
        self.assertEqual(res["routes"]["baseline/testnet_redirect"]["packets"], {"tcp_syn": 1})
        self.assertEqual(res["routes"]["appcontainer/testnet_ws"]["packets"], {"tcp_syn": 1})
        self.assertEqual(res["routes"]["baseline/testnet_udp"]["packets"], {"udp": 1})
        self.assertNotIn("error", res["diag"])
        self.assertEqual(res["diag"]["in_scope"], 3)

    def test_duplicates_from_several_components_are_deduplicated(self) -> None:
        frame = eth(ipv4(NIC, 5000, "192.0.2.1", 47300, "tcp_syn"))
        res = tp.parse_pcapng(pcap(frame, frame, frame), INDEX)
        ent = res["routes"]["baseline/testnet_redirect"]
        self.assertEqual(ent["packets"], {"tcp_syn": 3})
        self.assertEqual(ent["unique_flows"], {"tcp_syn": 1})
        self.assertEqual(tp.route_summary(res["routes"], "baseline/testnet_redirect"), {"raw": 3, "unique": 1})
        self.assertEqual(tp.route_summary(res["routes"], "baseline/testnet_img"), {"raw": 0, "unique": 0})

    def test_flows_are_separated_by_source_port_and_kind(self) -> None:
        res = tp.parse_pcapng(pcap(
            eth(ipv4(NIC, 5000, "192.0.2.1", 47300, "tcp_syn")),
            eth(ipv4(NIC, 5000, "192.0.2.1", 47300, "tcp_other")),
            eth(ipv4(NIC, 5001, "192.0.2.1", 47300, "tcp_syn"))), INDEX)
        self.assertEqual(res["routes"]["baseline/testnet_redirect"]["unique_flows"], {"tcp_syn": 2, "tcp_other": 1})

    def test_out_of_scope_destinations_are_counted_not_reported(self) -> None:
        res = tp.parse_pcapng(pcap(eth(ipv4("192.0.2.1", 47300, NIC, 5000, "tcp_other")),
                                   eth(ipv4(NIC, 5000, "8.8.8.8", 443, "tcp_syn"))), INDEX)
        self.assertEqual(res["routes"], {})
        self.assertEqual(res["diag"]["out_of_scope"], 2)
        self.assertNotIn("8.8.8.8", repr(res))
        self.assertNotIn(NIC, repr(res))

    def test_unknown_port_is_unattributed(self) -> None:
        res = tp.parse_pcapng(pcap(eth(ipv4(NIC, 5000, "192.0.2.1", 50000, "tcp_syn")),
                                   eth(ipv4(NIC, 5000, "192.0.2.1", 47300, "udp"))), INDEX)  # UDP の 47300 は TCP の経路
        self.assertEqual(set(res["routes"]), {"unattributed"})
        self.assertEqual(res["routes"]["unattributed"]["packets"], {"tcp_syn": 1, "udp": 1})

    def test_big_endian_section_header(self) -> None:
        frame = eth(ipv4(NIC, 5000, "192.0.2.1", 47301, "tcp_syn"))
        res = tp.parse_pcapng(pcap(frame, endian=">"), INDEX)
        self.assertEqual(res["routes"]["baseline/testnet_img"]["packets"], {"tcp_syn": 1})

    def test_truncated_and_bad_blocks_record_an_error_without_raising(self) -> None:
        good = pcap(eth(ipv4(NIC, 5000, "192.0.2.1", 47300, "tcp_syn"))).getvalue()
        res = tp.parse_pcapng(io.BytesIO(good[:-6]), INDEX)
        self.assertIn("error", res["diag"])
        self.assertEqual(res["routes"], {})
        bad_len = shb() + idb() + struct.pack("<II", 6, 4)
        self.assertEqual(tp.parse_pcapng(io.BytesIO(bad_len), INDEX)["diag"]["error"], "bad block length")
        huge = shb() + struct.pack("<II", 6, tp.MAX_BLOCK_BYTES + 1)
        self.assertIn("error", tp.parse_pcapng(io.BytesIO(huge), INDEX)["diag"])
        self.assertIn("error", tp.parse_pcapng(io.BytesIO(b"\x01\x02\x03"), INDEX)["diag"])

    def test_empty_file_has_no_routes_and_no_exception(self) -> None:
        res = tp.parse_pcapng(io.BytesIO(b""), INDEX)
        self.assertEqual(res["routes"], {})
        self.assertEqual(res["diag"]["frames"], 0)


def netevent(typ: str, remote: str, port: int, proto: int = 6, sid: str | None = SID, fid: str = "100") -> str:
    sid_xml = f"<packageSid>{sid}</packageSid>" if sid else ""
    return (f"<item><header><ipProtocol>{proto}</ipProtocol><remoteAddrV4>{remote}</remoteAddrV4>"
            f"<remotePort>{port}</remotePort>{sid_xml}</header><type>{typ}</type>"
            f"<classifyDrop><filterId>{fid}</filterId></classifyDrop></item>")


def events_xml(*items: str) -> io.BytesIO:
    return io.BytesIO(("<wfpdiag><netEvents>" + "".join(items) + "</netEvents></wfpdiag>").encode())


DROP = "FWPM_NET_EVENT_TYPE_CLASSIFY_DROP"


def epb_trunc(frame: bytes, orig: int) -> bytes:
    """取り込み長 len(frame)・元の長さ orig の EPB（orig > len(frame) で切り詰め）。"""
    return block(6, struct.pack("<IIIII", 0, 0, 0, len(frame), orig) + frame)


def spb(data: bytes, orig: int) -> bytes:
    return block(3, struct.pack("<I", orig) + data)


class TruncatedFrameTest(unittest.TestCase):
    """切り詰められたフレームの扱い（Issue #1515。EPB・SPB）。ヘッダが読めなければ error、読めれば通常どおり集計する。"""

    FULL = eth(ipv4(NIC, 5000, "192.0.2.1", 47300, "tcp_syn"))

    def _parse(self, *blocks: bytes) -> dict:
        return tp.parse_pcapng(io.BytesIO(shb() + idb(1) + b"".join(blocks)), testnet.port_index())

    def test_epb_cut_inside_header_is_error(self) -> None:
        r = self._parse(epb_trunc(self.FULL[:24], len(self.FULL)))
        self.assertEqual(r["diag"]["truncated_frames"], 1)
        self.assertEqual(r["diag"]["error"], "truncated frame could not be decoded")
        self.assertEqual(r["routes"], {})

    def test_epb_truncated_but_decodable_is_counted_without_error(self) -> None:
        r = self._parse(epb_trunc(self.FULL, len(self.FULL) + 40))
        self.assertEqual(r["diag"]["truncated_frames"], 1)
        self.assertNotIn("error", r["diag"])
        self.assertEqual(r["routes"]["baseline/testnet_redirect"]["packets"], {"tcp_syn": 1})

    def test_epb_not_truncated_is_unchanged(self) -> None:
        r = self._parse(epb(self.FULL))
        self.assertEqual(r["diag"]["truncated_frames"], 0)
        self.assertNotIn("error", r["diag"])
        self.assertEqual(r["routes"]["baseline/testnet_redirect"]["packets"], {"tcp_syn": 1})

    def test_spb_cut_inside_header_is_error(self) -> None:
        r = self._parse(spb(self.FULL[:24], len(self.FULL)))
        self.assertEqual(r["diag"]["truncated_frames"], 1)
        self.assertEqual(r["diag"]["error"], "truncated frame could not be decoded")

    def test_spb_truncated_but_decodable_is_counted_without_error(self) -> None:
        r = self._parse(spb(self.FULL, len(self.FULL) + 40))
        self.assertEqual(r["diag"]["truncated_frames"], 1)
        self.assertNotIn("error", r["diag"])
        self.assertEqual(r["routes"]["baseline/testnet_redirect"]["packets"], {"tcp_syn": 1})

    def test_spb_not_truncated_is_unchanged(self) -> None:
        r = self._parse(spb(self.FULL, len(self.FULL)))
        self.assertEqual(r["diag"]["truncated_frames"], 0)
        self.assertNotIn("error", r["diag"])
        self.assertEqual(r["routes"]["baseline/testnet_redirect"]["packets"], {"tcp_syn": 1})


class ShortBlockTest(unittest.TestCase):
    """必須ヘッダに満たない IDB・EPB・SPB は diag.error にする（Issue #1515）。集計は続ける。"""

    GOOD = epb(eth(ipv4(NIC, 5000, "192.0.2.1", 47300, "tcp_syn")))

    def _parse(self, *blocks: bytes) -> dict:
        return tp.parse_pcapng(io.BytesIO(shb() + b"".join(blocks)), testnet.port_index())

    def test_short_blocks_are_errors_and_counting_continues(self) -> None:
        for name, short in (("IDB", block(1, b"\x01\x00\x00")), ("EPB", block(6, b"\x00" * 16)),
                            ("SPB", block(3, b""))):
            r = self._parse(idb(1), short, self.GOOD)
            self.assertEqual(r["diag"]["error"], f"{name} block shorter than its required header", name)
            self.assertEqual(r["routes"]["baseline/testnet_redirect"]["packets"], {"tcp_syn": 1}, name)

    def test_well_formed_blocks_and_unknown_types_are_not_errors(self) -> None:
        r = self._parse(idb(1), self.GOOD, block(3, struct.pack("<I", 0)), block(0x99, b""))
        self.assertNotIn("error", r["diag"])


class WfpEventsTest(unittest.TestCase):
    def test_drops_with_matching_sid_are_counted_per_route(self) -> None:
        res = tp.parse_wfp_events(events_xml(netevent(DROP, "192.0.2.1", 47316), netevent(DROP, "192.0.2.1", 47316),
                                             netevent(DROP, "192.0.2.1", 47317)), INDEX, SID)
        a = res["routes"]["appcontainer/testnet_redirect"]
        self.assertEqual((a["drops"], a["drops_with_sid"], a["package_sid_match"]), (2, 2, 2))
        self.assertEqual(a["events"], {DROP: 2})
        self.assertEqual(a["filters"], {"100": 2})
        self.assertEqual(res["routes"]["appcontainer/testnet_img"]["package_sid_match"], 1)
        self.assertNotIn(SID, repr(res))

    def test_other_sid_or_no_sid_does_not_match(self) -> None:
        res = tp.parse_wfp_events(events_xml(netevent(DROP, "192.0.2.1", 47316, sid=OTHER_SID),
                                             netevent(DROP, "192.0.2.1", 47316, sid=None)), INDEX, SID)
        a = res["routes"]["appcontainer/testnet_redirect"]
        self.assertEqual((a["drops"], a["drops_with_sid"], a["package_sid_match"]), (2, 1, 0))
        self.assertNotIn(OTHER_SID, repr(res))

    def test_no_profile_sid_means_no_match(self) -> None:
        res = tp.parse_wfp_events(events_xml(netevent(DROP, "192.0.2.1", 47316)), INDEX, None)
        self.assertEqual(res["routes"]["appcontainer/testnet_redirect"]["package_sid_match"], 0)

    def test_non_drop_types_are_not_drops(self) -> None:
        res = tp.parse_wfp_events(events_xml(netevent("FWPM_NET_EVENT_TYPE_CLASSIFY_ALLOW", "192.0.2.1", 47316)), INDEX, SID)
        a = res["routes"]["appcontainer/testnet_redirect"]
        self.assertEqual(a["drops"], 0)
        self.assertEqual(a["events"], {"FWPM_NET_EVENT_TYPE_CLASSIFY_ALLOW": 1})

    def test_udp_proto_and_unattributed_and_out_of_scope(self) -> None:
        res = tp.parse_wfp_events(events_xml(
            netevent(DROP, "192.0.2.1", 47320, proto=17),
            netevent(DROP, "192.0.2.1", 50000),
            netevent(DROP, "8.8.8.8", 443),
            netevent(DROP, "not-an-ip", 443)), INDEX, SID)
        self.assertEqual(res["routes"]["appcontainer/testnet_udp"]["drops"], 1)
        self.assertEqual(res["routes"]["unattributed"]["drops"], 1)
        self.assertEqual(res["diag"]["out_of_scope"], 1)
        self.assertEqual(res["diag"]["unparsed_addr"], 1)
        self.assertNotIn("8.8.8.8", repr(res))

    def test_parse_error_is_recorded(self) -> None:
        res = tp.parse_wfp_events(io.BytesIO(b"<wfpdiag><item><header>"), INDEX, SID)
        self.assertIn("parse_error", res["diag"])


class FiltersAndOptionsTest(unittest.TestCase):
    def test_filters_names_and_scaffold_matches(self) -> None:
        xml = ("<wfpdiag><filters>"
               "<item><filterId>100</filterId><displayData><name>UWP Default Outbound Block Rule</name></displayData></item>"
               "<item><filterId>101</filterId><displayData><name>Other</name></displayData></item></filters></wfpdiag>")
        res = tp.parse_filters_xml(io.BytesIO(xml.encode()))
        self.assertEqual(res["names"], {"100": "UWP Default Outbound Block Rule", "101": "Other"})
        self.assertEqual(res["total"], 2)
        self.assertEqual(res["pattern_counts"]["UWP Default Outbound Block Rule"], 1)

    def test_filters_parse_error(self) -> None:
        self.assertIn("parse_error", tp.parse_filters_xml(io.BytesIO(b"<a><item>"))["diag"])

    def test_netevents_option(self) -> None:
        self.assertIs(tp.parse_netevents_option("Net events : ON"), True)
        self.assertIs(tp.parse_netevents_option("netevents = off"), False)
        self.assertIsNone(tp.parse_netevents_option("something else"))

    def test_pktmon_state_parsers(self) -> None:
        self.assertIs(tp.parse_pktmon_running("Packet Monitor is not running."), False)
        self.assertIs(tp.parse_pktmon_running("Collection status: Running"), True)
        self.assertIsNone(tp.parse_pktmon_running("???"))
        self.assertIs(tp.parse_pktmon_filters_empty("No filters set."), True)
        self.assertIs(tp.parse_pktmon_filters_empty("Filter Name  IP Address  Port"), False)
        self.assertIsNone(tp.parse_pktmon_filters_empty("???"))

    def test_pktmon_filter_list_real_empty_output(self) -> None:
        # windows-latest の実出力（Issue #1515）。CRLF・前後の空白は許容する。
        for text in ("Packet Filters:\n    None\n", "Packet Filters:\r\n    None\r\n", "  Packet Filters:  \r\n\r\n None  "):
            self.assertIs(tp.parse_pktmon_filters_empty(text), True, repr(text))
        # None が無い・他の行が続く・None を含むだけ、は空と判定しない。
        for text in ("Packet Filters:\r\n", "Packet Filters:\n    None\n    Extra line\n",
                     "Packet Filters:\n    None of the above\n", "Something\nPacket Filters:\n None\n"):
            self.assertIsNot(tp.parse_pktmon_filters_empty(text), True, repr(text))

    def test_pktmon_running_real_output_shapes(self) -> None:
        # run 42000257965 の稼働中の出力（先頭 12 行）。LF・CRLF のどちらでも True。13 行目以降は未確認。
        real = ("Collected Data:\n    Packet counters, packet capture\nCapture Type:\n    All packets\n"
                "Monitored Components:\n    Network adapters\nPacket Filters:\n"
                "    # Name    IP Address\n    - ----    ----------\n    # testnet #.#.#.#/#\n"
                "Logger Parameters:\n    Logger name:        PktMon\n")
        self.assertIs(tp.parse_pktmon_running(real), True)
        self.assertIs(tp.parse_pktmon_running(real.replace("\n", "\r\n")), True)
        # 13 行目以降に stopped を含むパスの行が続いても True（False は危険側のため先に判定する）。
        self.assertIs(tp.parse_pktmon_running(real + "    Log file:  C:\\work\\stopped\\x.etl\n"), True)
        self.assertIs(tp.parse_pktmon_running("Packet Monitor is not running.\r\n"), False)

    def test_pktmon_filters_empty_is_full_match(self) -> None:
        for text in ("No filters set.", "no filter", "No filters set.\r\n", "  No filters  "):
            self.assertIs(tp.parse_pktmon_filters_empty(text), True, repr(text))
        # filter の行の中に no filter を含むだけの表は空と判定しない。
        table = "Packet Filters:\n    # Name    IP Address\n    1 no filter-like  192.0.2.0/24\n"
        self.assertIsNot(tp.parse_pktmon_filters_empty(table), True)
        self.assertIsNot(tp.parse_pktmon_filters_empty("There is no filter named x\n"), True)

    def test_shape_masks_digits(self) -> None:
        self.assertEqual(tp.shape("# testnet 192.0.2.0/24"), "# testnet #.#.#.#/#")

    def test_shape_keeps_real_output_lines(self) -> None:
        lines = ["Collected Data:", "Packet counters, packet capture", "Capture Type:", "All packets",
                 "Monitored Components:", "Network adapters", "Packet Filters:", "# Name    IP Address",
                 "- ----    ----------", "# testnet #.#.#.#/#", "Logger Parameters:", "Logger name:        PktMon",
                 "Packet Monitor is not running.", "None", "netevents = on"]
        for line in lines:
            self.assertEqual(tp.shape(line), line)
        self.assertEqual(tp.shape("1 testnet 192.0.2.0/24"), "# testnet #.#.#.#/#")

    def test_shape_drops_environment_specific_words(self) -> None:
        for text, forbidden in (("C:\\Users\\runneradmin\\AppData\\x.etl", "runneradmin"),
                                ("Adapter 00-15-5D-01-AB-CD up", "AB"),
                                ("Ethernet-1 Hyper-V Virtual Adapter", "Hyper"),
                                ("イーサネット 2", "イーサネット")):
            out = tp.shape(text)
            self.assertNotIn(forbidden, out, out)
        self.assertEqual(tp.shape("Ethernet 3"), "? #")
        self.assertEqual(tp.shape("C:\\Users\\runneradmin"), "?:\\?\\?")
        self.assertEqual(tp.shape("a" * 300, 5), "?")


def epb_raw(frame: bytes, caplen: int, orig: int) -> bytes:
    return block(6, struct.pack("<IIIII", 0, 0, 0, caplen, orig) + frame)


class ShortFrameTest(unittest.TestCase):
    FULL = eth(ipv4(NIC, 5000, "192.0.2.1", 47300, "tcp_syn"))

    def _parse(self, *epbs: bytes) -> dict:
        return tp.parse_pcapng(io.BytesIO(shb() + idb(1) + b"".join(epbs)), INDEX)

    def test_l4_cut_by_capture_is_an_error(self) -> None:
        cut = self.FULL[:14 + 20 + 4]
        res = self._parse(epb_raw(cut, len(cut), len(self.FULL)))
        self.assertEqual(res["diag"]["error"], "truncated frame could not be decoded")

    def test_short_frame_to_testnet_is_an_error(self) -> None:
        for kind in ("tcp_syn", "udp"):
            cut = eth(ipv4(NIC, 5000, "192.0.2.1", 47300, kind))[:14 + 20 + 4]
            res = self._parse(epb(cut))
            self.assertEqual(res["diag"]["error"], "short frame to testnet could not be decoded", kind)
            self.assertEqual((res["diag"]["frames"], res["diag"]["short_frames"]), (1, 1), kind)
            self.assertEqual(res["routes"], {}, kind)

    def test_short_frame_outside_testnet_is_counted_not_an_error(self) -> None:
        for kind in ("tcp_syn", "udp"):
            cut = eth(ipv4(NIC, 5000, "198.51.100.7", 47300, kind))[:14 + 20 + 4]
            res = self._parse(epb(cut))
            self.assertNotIn("error", res["diag"], kind)
            self.assertEqual((res["diag"]["frames"], res["diag"]["short_frames"]), (1, 1), kind)
            self.assertEqual(res["routes"], {}, kind)

    def test_short_frame_with_unreadable_dst_is_counted_not_an_error(self) -> None:
        for kind in ("tcp_syn", "udp"):
            cut = eth(ipv4(NIC, 5000, "192.0.2.1", 47300, kind))[:14 + 19]
            res = self._parse(epb(cut))
            self.assertNotIn("error", res["diag"], kind)
            self.assertEqual((res["diag"]["frames"], res["diag"]["short_frames"]), (1, 1), kind)
            self.assertEqual(res["routes"], {}, kind)

    def test_exactly_20_byte_epb(self) -> None:
        res = self._parse(epb_raw(b"", 0, 0))
        self.assertNotIn("error", res["diag"])
        self.assertEqual(res["diag"]["frames"], 1)
        res = self._parse(epb_raw(b"", 0, 60))
        self.assertEqual(res["diag"]["error"], "truncated frame could not be decoded")
        res = self._parse(epb_raw(b"", 10, 10))
        self.assertEqual(res["diag"]["error"], "EPB caplen exceeds block data")

    def test_out_of_scope_kinds_are_not_short_frames(self) -> None:
        arp = b"\x00" * 12 + b"\x08\x06" + b"\x00" * 28
        ipv6 = b"\x00" * 12 + b"\x86\xdd" + b"\x00" * 40
        pkt = bytearray(ipv4(NIC, 1, "192.0.2.1", 2, "udp"))
        pkt[6:8] = struct.pack("!H", 0x0001)  # 先頭でない断片
        frag = eth(bytes(pkt))
        icmp = bytearray(ipv4(NIC, 1, "192.0.2.1", 2, "udp"))
        icmp[9] = 1
        res = self._parse(epb(arp), epb(ipv6), epb(frag), epb(eth(bytes(icmp))))
        self.assertEqual(res["diag"]["frames"], 4)
        self.assertEqual(res["diag"]["short_frames"], 0)
        self.assertNotIn("error", res["diag"])


if __name__ == "__main__":
    unittest.main()
