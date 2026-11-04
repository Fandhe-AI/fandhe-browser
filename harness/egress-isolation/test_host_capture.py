"""host_capture.py（隔離下の実ホストの DNS・UDP・直接 TCP 送信の外部観測。Issue #1461・`RENDER-3`・`SEC-2`）の自己テスト。

合成した tcpdump テキストと合成サンプルだけを使う（tcpdump・sudo・実ホストは使わない）。tcpdump の行形式は
実機（tcpdump 4.99.6・SLL2）の `-nn -tt -r` 出力に合わせてある。`/proc` を読むテストは Linux のみ。
"""

import json
import os
import sys
import tempfile
import time
import unittest
from unittest import mock
import argparse
import socket
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import host_capture as hc  # noqa: E402

SELF_NS = 4026531840
ISO_NS = 4026532999
CANARY = 40000


def line(ts, iface, d, rest, fam="IP"):
    return f"{ts:.6f} {iface:<5} {d:<3} {fam} {rest}"


def pkts(*lines):
    out = []
    for ln in lines:
        p = hc.parse_tcpdump_line(ln)
        assert p is not None, ln
        out.append(p)
    return out


def make_index(servo=None, sockets=None, bwrap=True, netns=ISO_NS, ticks=3, end=True, canary_sock=True):
    idx = hc.SampleIndex()
    idx.feed({"type": "meta", "self_netns": SELF_NS, "canary_port": CANARY, "pid": 10})
    servo = servo if servo is not None else [{"pid": 200, "bwrap": bwrap, "netns": netns}]
    socks = list(sockets or [])
    if canary_sock:
        socks.append({"proto": "udp", "port": CANARY, "pid": 10, "role": hc.ROLE_SELF})
    idx.feed({"type": "sample", "t": 100.0, "servo": servo, "sockets": socks})
    for i in range(ticks):
        idx.feed({"type": "tick", "t": 100.2 + i * 0.2})
    if end:
        idx.feed({"type": "end", "t": 101.0, "truncated": False, "reason": "stop", "final_canary_t": 100.65})
    return idx


CANARY_LINE = line(100.3, "lo", "Out", f"127.0.0.1.{CANARY} > 127.0.0.1.50000: UDP, length 26")
# make_index の観測区間は [100.0, 100.6]。キャプチャがその終端以降まで続いている（区間を覆う）ことを示す後続パケット
# サンプラーが停止時に送る最終カナリア（end 記録の final_canary_t=100.65 の直後）
FINAL_CANARY_LINE = line(100.650100, "lo", "Out", f"127.0.0.1.{CANARY} > 127.0.0.1.50000: UDP, length 26")
OUT_TCP = line(100.7, "eth0", "Out", "10.0.0.5.6000 > 8.8.8.8.443: Flags [S], seq 1, win 1000, length 0")


EARLY_ACK = line(100.2, "eth0", "Out", "10.0.0.5.9998 > 8.8.8.8.443: Flags [.], ack 1, win 1, length 0")
LATE_ACK = line(100.7, "eth0", "Out", "10.0.0.5.9999 > 8.8.8.8.443: Flags [.], ack 1, win 1, length 0")


def run(packet_lines, idx, **kw):
    return hc.judge(pkts(*packet_lines), idx, **kw)


class ParseTests(unittest.TestCase):
    def test_ipv4_udp_decoded_dns(self):
        p = hc.parse_tcpdump_line(line(1730000002.000002, "eth0", "Out", "10.0.0.5.5000 > 8.8.8.8.53: domain [length 5 < 12] (invalid)"))
        self.assertEqual((p["iface"], p["dir"], p["proto"], p["src"], p["sport"], p["dst"], p["dport"]), ("eth0", "Out", "udp", "10.0.0.5", 5000, "8.8.8.8", 53))
        self.assertEqual(hc.count_category(p), "dns")

    def test_ipv6_udp_and_tcp(self):
        u = hc.parse_tcpdump_line(line(1.0, "eth0", "Out", "fd00::5.7000 > 2001:db8::1.5353: UDP, length 5", fam="IP6"))
        self.assertEqual((u["proto"], u["src"], u["sport"], u["dst"], u["dport"]), ("udp", "fd00::5", 7000, "2001:db8::1", 5353))
        self.assertEqual(hc.count_category(u), "udp_other")
        t = hc.parse_tcpdump_line(line(1.0, "eth0", "Out", "fd00::5.7001 > 2001:db8::1.80: Flags [S], seq 1, win 1000, length 0", fam="IP6"))
        self.assertEqual((t["proto"], t["syn"]), ("tcp", True))
        self.assertEqual(hc.count_category(t), "tcp_syn")

    def test_syn_ack_and_direction_not_counted(self):
        synack = hc.parse_tcpdump_line(line(1.0, "eth0", "Out", "10.0.0.5.443 > 8.8.8.8.6000: Flags [S.], seq 1, ack 0, win 1000, length 0"))
        self.assertFalse(synack["syn"])
        self.assertIsNone(hc.count_category(synack))
        inbound = hc.parse_tcpdump_line(line(1.0, "eth0", "In", "8.8.8.8.53 > 10.0.0.5.5000: UDP, length 5"))
        self.assertIsNone(hc.count_category(inbound))

    def test_icmp_has_no_port_and_is_ignored(self):
        p = hc.parse_tcpdump_line(line(1.0, "eth0", "Out", "10.0.0.5 > 8.8.8.8: ICMP echo request, id 1, seq 1, length 8"))
        self.assertEqual((p["proto"], p["sport"]), ("other", None))
        self.assertIsNone(hc.count_category(p))

    def test_lo_counts_only_port_53(self):
        dns = hc.parse_tcpdump_line(line(1.0, "lo", "Out", "127.0.0.1.4000 > 127.0.0.53.53: UDP, length 5"))
        self.assertEqual(hc.count_category(dns), "dns")
        other = hc.parse_tcpdump_line(line(1.0, "lo", "Out", "127.0.0.1.4000 > 127.0.0.1.9999: UDP, length 5"))
        self.assertIsNone(hc.count_category(other))

    def test_lo_in_direction_is_handled(self):
        dns = hc.parse_tcpdump_line(line(1.0, "lo", "In", "127.0.0.1.4000 > 127.0.0.53.53: UDP, length 5"))
        self.assertEqual(hc.count_category(dns), "dns")
        nonlo_in = hc.parse_tcpdump_line(line(1.0, "eth0", "In", "8.8.8.8.53 > 10.0.0.5.5000: UDP, length 5"))
        self.assertIsNone(hc.count_category(nonlo_in))

    def test_kernel_dropped_parse(self):
        self.assertEqual(hc.parse_kernel_dropped("1 packet captured\n0 packets dropped by kernel\n"), 0)
        self.assertEqual(hc.parse_kernel_dropped("7 packets dropped by kernel\n"), 7)
        self.assertIsNone(hc.parse_kernel_dropped("listening on any\n"))

    def test_garbage_is_none(self):
        self.assertIsNone(hc.parse_tcpdump_line("reading from file -, link-type LINUX_SLL2"))
        self.assertIsNone(hc.parse_tcpdump_line("   0x0000:  4500 0021"))

    def test_non_ip_family_is_other(self):
        p = hc.parse_tcpdump_line("1.000000 eth0  Out ARP, Request who-has 10.0.0.1 tell 10.0.0.5, length 28")
        self.assertEqual(p["proto"], "other")


class AttributionTests(unittest.TestCase):
    def test_four_classes_counted(self):
        socks = [
            {"proto": "udp", "port": 5000, "pid": 200, "role": hc.ROLE_UNISOLATED},
            {"proto": "udp", "port": 5001, "pid": 300, "role": hc.ROLE_OTHER},
            {"proto": "tcp", "port": 6000, "pid": 10, "role": hc.ROLE_SELF},
        ]
        idx = make_index(sockets=socks)
        v = run(
            [
                CANARY_LINE,
                line(100.3, "eth0", "Out", "10.0.0.5.5000 > 8.8.8.8.53: UDP, length 5"),
                line(100.3, "eth0", "Out", "10.0.0.5.5001 > 8.8.8.8.5353: UDP, length 5"),
                line(100.3, "eth0", "Out", "10.0.0.5.6000 > 8.8.8.8.443: Flags [S], seq 1, win 1, length 0"),
                line(100.3, "eth0", "Out", "10.0.0.5.7777 > 8.8.8.8.53: UDP, length 5"),
                FINAL_CANARY_LINE,
            ],
            idx,
        )
        self.assertEqual(v["verdict"], "pass", v)
        self.assertEqual(v["counts"][hc.ROLE_UNISOLATED]["dns"], 1)
        self.assertEqual(v["counts"][hc.ROLE_OTHER]["udp_other"], 1)
        self.assertEqual(v["counts"][hc.ROLE_SELF]["tcp_syn"], 1)
        self.assertEqual(v["counts"][hc.ROLE_NONE]["dns"], 1)
        self.assertEqual(v["counts"][hc.ROLE_ISOLATED], {"dns": 0, "udp_other": 0, "tcp_syn": 0})

    def test_isolated_servo_packet_fails(self):
        socks = [{"proto": "udp", "port": 5000, "pid": 200, "role": hc.ROLE_ISOLATED}]
        v = run([CANARY_LINE, FINAL_CANARY_LINE, OUT_TCP, line(100.3, "eth0", "Out", "10.0.0.5.5000 > 8.8.8.8.53: UDP, length 5")], make_index(sockets=socks))
        self.assertEqual(v["verdict"], "fail")
        self.assertEqual(v["counts"][hc.ROLE_ISOLATED]["dns"], 1)

    def test_packet_outside_window_not_attributed(self):
        socks = [{"proto": "udp", "port": 5000, "pid": 200, "role": hc.ROLE_ISOLATED}]
        v = run([CANARY_LINE, FINAL_CANARY_LINE, OUT_TCP, line(150.0, "eth0", "Out", "10.0.0.5.5000 > 8.8.8.8.53: UDP, length 5")], make_index(sockets=socks))
        self.assertEqual(v["counts"][hc.ROLE_NONE]["dns"], 1)
        self.assertEqual(v["verdict"], "pass")


class VerdictTests(unittest.TestCase):
    def ok(self, idx=None, lines=None, **kw):
        return run(lines if lines is not None else [CANARY_LINE, FINAL_CANARY_LINE, OUT_TCP], idx or make_index(), **kw)

    def test_pass(self):
        v = self.ok()
        self.assertEqual(v["verdict"], "pass", v)
        self.assertEqual(v["servo_pids"], {"isolated": 1, "unisolated": 0})
        self.assertEqual(v["canary"], {"port": CANARY, "seen": 2, "attributed_to_self": 2, "in_servo_window": 1})
        self.assertIn("**pass**", hc.render_summary(v))

    def test_bwrap_with_same_netns_fails(self):
        v = self.ok(make_index(netns=SELF_NS))
        self.assertEqual(v["verdict"], "fail")

    def _two_samples(self, first_ns, second_ns):
        idx = hc.SampleIndex()
        idx.feed({"type": "meta", "self_netns": SELF_NS, "canary_port": CANARY, "pid": 10})
        canary = {"proto": "udp", "port": CANARY, "pid": 10, "role": hc.ROLE_SELF}
        for t, ns in ((100.0, first_ns), (100.4, second_ns)):
            idx.feed({"type": "sample", "t": t, "servo": [{"pid": 200, "bwrap": True, "netns": ns}], "sockets": [canary]})
        idx.feed({"type": "tick", "t": 100.6})
        idx.feed({"type": "end", "t": 101.0, "truncated": False, "reason": "stop", "final_canary_t": 100.65})
        return idx

    def test_same_pid_isolated_first_then_shared_netns_fails(self):
        v = self.ok(self._two_samples(ISO_NS, SELF_NS))
        self.assertEqual(v["verdict"], "fail", v)
        self.assertEqual(v["counts"][hc.ROLE_ISOLATED], {"dns": 0, "udp_other": 0, "tcp_syn": 0})

    def test_same_pid_shared_first_then_isolated_netns_fails(self):
        v = self.ok(self._two_samples(SELF_NS, ISO_NS))
        self.assertEqual(v["verdict"], "fail", v)

    def test_same_pid_unreadable_netns_later_is_unobservable(self):
        v = self.ok(self._two_samples(ISO_NS, None))
        self.assertEqual(v["verdict"], "unobservable", v)

    def test_unisolated_only_is_unobservable(self):
        v = self.ok(make_index(bwrap=False, netns=SELF_NS))
        self.assertEqual(v["verdict"], "unobservable")
        self.assertEqual(v["servo_pids"]["unisolated"], 1)

    def test_netns_differs_without_bwrap_counts_as_isolated(self):
        v = self.ok(make_index(bwrap=False, netns=ISO_NS))
        self.assertEqual(v["verdict"], "pass", v)

    def test_missing_canary_is_unobservable(self):
        v = self.ok(lines=[OUT_TCP])
        self.assertEqual(v["verdict"], "unobservable")
        v = self.ok(make_index(canary_sock=False))
        self.assertEqual(v["verdict"], "unobservable")

    def test_no_servo_is_unobservable(self):
        self.assertEqual(self.ok(make_index(servo=[]))["verdict"], "unobservable")

    def test_no_samples_is_unobservable(self):
        v = run([CANARY_LINE, FINAL_CANARY_LINE, OUT_TCP], hc.SampleIndex())
        self.assertEqual(v["verdict"], "unobservable")

    def test_no_nonlo_packets_is_unobservable(self):
        self.assertEqual(self.ok(lines=[CANARY_LINE])["verdict"], "unobservable")

    def test_no_packets_is_unobservable(self):
        self.assertEqual(self.ok(lines=[])["verdict"], "unobservable")

    def test_missing_end_record_is_unobservable(self):
        self.assertEqual(self.ok(make_index(end=False))["verdict"], "unobservable")

    def test_truncated_sampler_is_unobservable(self):
        idx = make_index(end=False)
        idx.feed({"type": "end", "t": 101.0, "truncated": True, "reason": "sample-or-byte-limit"})
        self.assertEqual(self.ok(idx)["verdict"], "unobservable")

    def test_tcpdump_failure_is_unobservable(self):
        self.assertEqual(self.ok(tcpdump_exit=1)["verdict"], "unobservable")
        self.assertEqual(self.ok(tcpdump_exit=None)["verdict"], "unobservable")

    def test_packet_limit_is_unobservable(self):
        self.assertEqual(self.ok(max_packets=2)["verdict"], "unobservable")

    def test_unparsed_ratio(self):
        self.assertEqual(self.ok(unparsed=5, total_lines=100)["verdict"], "pass")
        self.assertEqual(self.ok(unparsed=50, total_lines=100)["verdict"], "unobservable")

    def test_lo_in_canary_is_observed(self):
        canary_in = line(100.3, "lo", "In", f"127.0.0.1.{CANARY} > 127.0.0.1.50000: UDP, length 26")
        final_in = line(100.650200, "lo", "In", f"127.0.0.1.{CANARY} > 127.0.0.1.50000: UDP, length 26")
        v = run([canary_in, final_in, OUT_TCP], make_index())
        self.assertEqual(v["verdict"], "pass")

    def test_convert_failure_and_drops_are_unobservable(self):
        for kw in ({"convert_exit": 1}, {"convert_exit": None}, {"kernel_dropped": 2}, {"kernel_dropped": None}):
            v = run([CANARY_LINE, FINAL_CANARY_LINE, OUT_TCP], make_index(), **kw)
            self.assertEqual(v["verdict"], "unobservable", kw)

    def test_fail_takes_precedence_over_unobservable(self):
        v = self.ok(make_index(netns=SELF_NS), tcpdump_exit=1)
        self.assertEqual(v["verdict"], "fail")


class IoTests(unittest.TestCase):
    def test_judge_command_writes_verdict_and_returns_exit_code(self):
        with tempfile.TemporaryDirectory() as d:
            out = Path(d)
            (out / "capture.txt").write_text(CANARY_LINE + "\n" + FINAL_CANARY_LINE + "\n" + OUT_TCP + "\n")
            recs = [
                {"type": "meta", "self_netns": SELF_NS, "canary_port": CANARY, "pid": 10},
                {"type": "sample", "t": 100.0, "servo": [{"pid": 200, "bwrap": True, "netns": ISO_NS}],
                 "sockets": [{"proto": "udp", "port": CANARY, "pid": 10, "role": "self"}]},
                {"type": "tick", "t": 100.5},
                {"type": "end", "t": 101.0, "truncated": False, "reason": "stop", "final_canary_t": 100.65},
            ]
            (out / "samples.jsonl").write_text("\n".join(json.dumps(r) for r in recs) + "\n")
            (out / "tcpdump.exit").write_text("0\n")
            (out / "convert.exit").write_text("0\n")
            (out / "tcpdump.stderr").write_text("5 packets captured\n5 packets received by filter\n0 packets dropped by kernel\n")
            ns = type("A", (), {"out_dir": str(out), "max_packets": 1000})()
            self.assertEqual(hc.cmd_judge(ns), 0)
            verdict = json.loads((out / "verdict.json").read_text())
            self.assertEqual(verdict["verdict"], "pass")
            self.assertTrue((out / "summary.md").is_file())
            (out / "tcpdump.exit").write_text("2\n")
            self.assertEqual(hc.cmd_judge(ns), 1)
            (out / "tcpdump.exit").write_text("0\n")
            (out / "convert.exit").write_text("1\n")
            self.assertEqual(hc.cmd_judge(ns), 1)
            (out / "convert.exit").write_text("0\n")
            (out / "tcpdump.stderr").write_text("5 packets captured\n3 packets dropped by kernel\n")
            self.assertEqual(hc.cmd_judge(ns), 1)
            (out / "tcpdump.stderr").write_text("no stats\n")
            self.assertEqual(hc.cmd_judge(ns), 1)
            (out / "tcpdump.stderr").write_text("0 packets dropped by kernel\n")
            (out / "convert.exit").unlink()
            self.assertEqual(hc.cmd_judge(ns), 1)

    def test_missing_files_are_unobservable(self):
        with tempfile.TemporaryDirectory() as d:
            ns = type("A", (), {"out_dir": d, "max_packets": 1000})()
            self.assertEqual(hc.cmd_judge(ns), 1)
            self.assertEqual(json.loads((Path(d) / "verdict.json").read_text())["verdict"], "unobservable")


@unittest.skipUnless(sys.platform == "linux", "/proc is Linux only")
class SamplerTests(unittest.TestCase):
    def test_sampler_attributes_canary_to_itself(self):
        with tempfile.TemporaryDirectory() as d:
            out = Path(d) / "samples.jsonl"
            hc.run_sampler(os.path.join(d, "no-such-servo"), out, 0.05, None, max_seconds=0.6)
            recs = [json.loads(x) for x in out.read_text().splitlines()]
            self.assertEqual(recs[0]["type"], "meta")
            self.assertEqual(recs[-1]["type"], "end")
            self.assertFalse(recs[-1]["truncated"])
            port = recs[0]["canary_port"]
            idx = hc.SampleIndex()
            for r in recs:
                idx.feed(r)
            pkt = hc.parse_tcpdump_line(line(time.time(), "lo", "Out", f"127.0.0.1.{port} > 127.0.0.1.1: UDP, length 26"))
            self.assertEqual(hc.attribute(idx, pkt), hc.ROLE_SELF)
            self.assertEqual(idx.servo, {})

    def test_take_sample_finds_exe_match_and_classifies(self):
        servo, _sockets, limits = hc.take_sample(os.path.realpath(sys.executable), os.getpid(), hc._read_ns_net(os.getpid()))
        me = [s for s in servo if s["pid"] == os.getpid()]
        self.assertEqual(len(me), 1)
        self.assertFalse(me[0]["bwrap"])
        self.assertEqual(limits, [])

    def test_scan_limits_are_reported(self):
        with mock.patch.object(hc, "MAX_PROCESSES", 1):
            pids, hit, _failed = hc._list_pids()
        self.assertEqual(len(pids), 1)
        self.assertTrue(hit)
        with mock.patch.object(hc, "MAX_FDS_PER_PROCESS", 0):
            _owners, hit, _errors = hc._socket_owners([os.getpid()])
        self.assertTrue(hit)
        with mock.patch.object(hc, "MAX_NET_LINES", 0):
            _rows, hit, _failed, _unparsed = hc._read_table("udp")
        self.assertTrue(hit)

    def test_each_scan_limit_makes_judge_unobservable(self):
        # 上限到達ごとに end.truncated が立ち、judge が pass にならない（観測の欠落を合格にしない）
        cases = {
            "processes-limit": ("MAX_PROCESSES", 1),
            "fds-per-process-limit": ("MAX_FDS_PER_PROCESS", 0),
            "net-lines-limit": ("MAX_NET_LINES", 0),
        }
        for reason, (attr, value) in cases.items():
            with self.subTest(reason=reason), tempfile.TemporaryDirectory() as d:
                out = Path(d) / "samples.jsonl"
                with mock.patch.object(hc, attr, value):
                    hc.run_sampler(os.path.join(d, "no-such-servo"), out, 0.05, None, max_seconds=0.3)
                recs = [json.loads(x) for x in out.read_text().splitlines()]
                self.assertTrue(recs[-1]["truncated"])
                self.assertIn(reason, recs[-1]["reason"])
                idx = hc.SampleIndex()
                for r in recs:
                    idx.feed(r)
                self.assertTrue(idx.end.get("truncated"))

    def _sample_with_open_error(self, fail_names, errno_exc):
        real_open = open

        def fake_open(path, *a, **k):
            for n in fail_names:
                if str(path) == f"/proc/self/net/{n}":
                    raise errno_exc
            return real_open(path, *a, **k)

        with mock.patch("builtins.open", fake_open):
            return hc.take_sample("/no/such/servo", os.getpid(), hc._read_ns_net(os.getpid()))

    def test_required_table_read_error_is_recorded(self):
        # tcp 表だけ読めない場合も、udp 表でカナリアを帰属できるだけで pass させない
        _s, _k, limits = self._sample_with_open_error(["tcp"], PermissionError(13, "denied"))
        self.assertEqual(limits, ["proc-read-error:net-tcp"])
        _s, _k, limits = self._sample_with_open_error(["udp"], FileNotFoundError(2, "gone"))
        self.assertEqual(limits, ["proc-read-error:net-udp"])

    def test_optional_ipv6_table_missing_is_not_an_error_when_ipv6_absent(self):
        with mock.patch.object(hc, "_ipv6_absent", lambda: True):
            _s, _k, limits = self._sample_with_open_error(["tcp6", "udp6"], FileNotFoundError(2, "no ipv6"))
            self.assertEqual(limits, [])
            # 不在以外の失敗（権限等）は任意表でも記録する
            _s, _k, limits = self._sample_with_open_error(["tcp6"], PermissionError(13, "denied"))
            self.assertEqual(limits, ["proc-read-error:net-tcp6"])

    def test_ipv6_table_missing_while_ipv6_present_is_recorded(self):
        # if_inet6 があるのに表が無い状態は、片方だけでも異常として記録する
        with mock.patch.object(hc, "_ipv6_absent", lambda: False):
            _s, _k, limits = self._sample_with_open_error(["tcp6", "udp6"], FileNotFoundError(2, "gone"))
            self.assertEqual(limits, ["proc-read-error:net-udp6", "proc-read-error:net-tcp6"])
            _s, _k, limits = self._sample_with_open_error(["tcp6"], FileNotFoundError(2, "gone"))
            self.assertEqual(limits, ["proc-read-error:net-tcp6"])
            _s, _k, limits = self._sample_with_open_error(["udp6"], FileNotFoundError(2, "gone"))
            self.assertEqual(limits, ["proc-read-error:net-udp6"])

    def test_ipv6_absent_only_for_missing_if_inet6(self):
        with mock.patch.object(hc.os, "stat", side_effect=FileNotFoundError(2, "x")):
            self.assertTrue(hc._ipv6_absent())
        with mock.patch.object(hc.os, "stat", side_effect=PermissionError(13, "x")):
            self.assertFalse(hc._ipv6_absent())
        with mock.patch.object(hc.os, "stat", return_value=object()):
            self.assertFalse(hc._ipv6_absent())

    def test_unparsed_net_line_is_recorded(self):
        real_open = open
        header = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n"
        good = "   0: 0100007F:9C40 00000000:0000 07 00000000:00000000 00:00000000 00000000  1000        0 12345 2 0 0 2 0\n"

        def fake_open(path, *a, **k):
            if str(path) == "/proc/self/net/udp":
                import io

                return io.StringIO(header + good + "garbage line\n   \n")
            return real_open(path, *a, **k)

        with mock.patch("builtins.open", fake_open):
            _s, _k, limits = hc.take_sample("/no/such/servo", os.getpid(), hc._read_ns_net(os.getpid()))
        self.assertEqual(limits, ["proc-read-error:net-udp-unparsed"])

    def test_list_pids_error_is_recorded(self):
        with mock.patch.object(hc.os, "listdir", side_effect=PermissionError(13, "denied")):
            pids, hit, failed = hc._list_pids()
            self.assertEqual((pids, hit, failed), ([], False, True))
            _s, _k, limits = hc.take_sample("/no/such/servo", os.getpid(), 1)
        self.assertIn("proc-read-error:list-pids", limits)

    def _take_sample_for_servo_pid(self, listdir_exc):
        x = 4_000_123
        real_listdir = os.listdir

        def fake_listdir(path):
            if str(path) == f"/proc/{x}/fd":
                raise listdir_exc
            return real_listdir(path)

        with mock.patch.object(hc, "_list_pids", lambda: ([x], False, False)), mock.patch.object(
            hc, "_exe", lambda pid: "/servo"
        ), mock.patch.object(hc, "_ppid", lambda pid: None), mock.patch.object(
            hc, "_read_ns_net", lambda pid: None
        ), mock.patch.object(hc, "_read_table", lambda n, optional=False: ([], False, False, 0)), mock.patch.object(
            hc, "_ipv6_absent", lambda: True
        ), mock.patch.object(hc.os, "listdir", fake_listdir):
            return hc.take_sample("/servo", 1, None)

    def test_vanished_process_is_not_an_error(self):
        # 走査中に消えた Servo 候補 pid の fd が読めない（FileNotFoundError・ProcessLookupError）のは通常の競合
        for exc in (FileNotFoundError(2, "gone"), ProcessLookupError(3, "gone")):
            servo, _k, limits = self._take_sample_for_servo_pid(exc)
            self.assertEqual([s["pid"] for s in servo], [4_000_123])
            self.assertEqual(limits, [])
        # 対照: 権限で読めないときは記録する
        _s, _k, limits = self._take_sample_for_servo_pid(PermissionError(13, "denied"))
        self.assertEqual(limits, ["proc-read-error:fd-4000123"])

    def test_eacces_on_non_servo_pid_is_not_recorded(self):
        # Servo 以外の pid の fd の EACCES は帰属不能が前提で記録しない（#1473 D）
        with mock.patch.object(hc.os, "listdir", side_effect=PermissionError(13, "denied")):
            owners, hit, errors = hc._socket_owners([4_000_999], frozenset({4_000_123}))
        self.assertEqual((owners, hit, errors), ({}, False, []))

    def test_table_read_error_makes_judge_unobservable(self):
        with tempfile.TemporaryDirectory() as d:
            out = Path(d) / "samples.jsonl"
            with mock.patch.object(hc, "_read_table", lambda n, optional=False: ([], False, n == "tcp", 0)):
                hc.run_sampler(os.path.join(d, "no-such-servo"), out, 0.05, None, max_seconds=0.3)
            recs = [json.loads(x) for x in out.read_text().splitlines()]
            self.assertTrue(recs[-1]["truncated"])
            self.assertEqual(recs[-1]["reason"], "proc-read-error:net-tcp")
            idx = hc.SampleIndex()
            for r in recs:
                idx.feed(r)
            v = hc.judge([], idx)
            self.assertEqual(v["verdict"], "unobservable")
            self.assertTrue(any("proc-read-error:net-tcp" in r for r in v["unobservable_reasons"]))


class SelfNetnsTests(unittest.TestCase):
    def _idx(self, self_netns):
        idx = hc.SampleIndex()
        idx.feed({"type": "meta", "self_netns": self_netns, "canary_port": CANARY, "pid": 10})
        idx.feed(
            {
                "type": "sample",
                "t": 100.0,
                "servo": [{"pid": 200, "bwrap": False, "netns": ISO_NS}],
                "sockets": [{"proto": "udp", "port": CANARY, "pid": 10, "role": hc.ROLE_SELF}],
            }
        )
        for i in range(3):
            idx.feed({"type": "tick", "t": 100.2 + i * 0.2})
        idx.feed({"type": "end", "t": 101.0, "truncated": False, "reason": "stop", "final_canary_t": 100.65})
        return idx

    def test_servo_netns_is_not_isolated_when_self_netns_unknown(self):
        idx = self._idx(None)
        self.assertFalse(idx.servo[200]["isolated_seen"])
        v = run([CANARY_LINE, FINAL_CANARY_LINE, OUT_TCP], idx)
        self.assertEqual(v["verdict"], "unobservable")
        self.assertTrue(any("netns of the sampler" in r for r in v["unobservable_reasons"]))
        self.assertEqual(v["servo_pids"], {"isolated": 0, "unisolated": 1})

    def test_known_self_netns_still_isolates_by_netns_difference(self):
        idx = self._idx(SELF_NS)
        self.assertTrue(idx.servo[200]["isolated_seen"])
        self.assertEqual(run([CANARY_LINE, FINAL_CANARY_LINE, OUT_TCP], idx)["verdict"], "pass")

class UdpHintTests(unittest.TestCase):
    def test_suspicious_udp_text_truth_table(self):
        self.assertTrue(hc.suspicious_udp_text(line(1.0, "eth0", "Out", "10.0.0.5 > 8.8.8.8: ip-proto-17, length 40")))
        self.assertTrue(hc.suspicious_udp_text(line(1.0, "eth0", "Out", "10.0.0.5.5000 > 8.8.8.8.53: broken")))
        self.assertTrue(hc.suspicious_udp_text("garbage UDP, length 5"))
        self.assertTrue(hc.suspicious_udp_text("garbage udp, length 5"))
        self.assertTrue(hc.suspicious_udp_text("garbage Udp, length 5"))
        self.assertFalse(hc.suspicious_udp_text(line(1.0, "eth0", "Out", "10.0.0.5 > 8.8.8.8: ICMP 8.8.8.8 udp port 53 unreachable, length 36")))
        self.assertFalse(hc.suspicious_udp_text("1.000000 eth0  Out ARP, Request who-has 10.0.0.1 tell 10.0.0.5, length 28"))

    def test_parse_sets_udp_hint_for_other_proto(self):
        frag = hc.parse_tcpdump_line(line(1.0, "eth0", "Out", "10.0.0.5 > 8.8.8.8: ip-proto-17, length 40"))
        self.assertEqual((frag["proto"], frag["udp_hint"]), ("other", True))
        icmp = hc.parse_tcpdump_line(line(1.0, "eth0", "Out", "10.0.0.5 > 8.8.8.8: ICMP echo request, id 1, seq 1, length 8"))
        self.assertEqual((icmp["proto"], icmp["udp_hint"]), ("other", False))
        udp = hc.parse_tcpdump_line(line(1.0, "eth0", "Out", "10.0.0.5.5000 > 8.8.8.8.53: UDP, length 5"))
        self.assertEqual((udp["proto"], udp["udp_hint"]), ("udp", False))

    def test_single_udp_like_unparsed_line_is_unobservable(self):
        v = run([CANARY_LINE, FINAL_CANARY_LINE, OUT_TCP], make_index(), unparsed=1, unparsed_udp_like=1, total_lines=1000)
        self.assertEqual(v["verdict"], "unobservable")
        self.assertEqual(v["unparsed_udp_like"], 1)
        self.assertIn("1 unparsed packet line(s) look like UDP/DNS", v["unobservable_reasons"])
        # 痕跡の無い解析不能行は従来の許容枠のまま
        v = run([CANARY_LINE, FINAL_CANARY_LINE, OUT_TCP], make_index(), unparsed=5, unparsed_udp_like=0, total_lines=100)
        self.assertEqual(v["verdict"], "pass", v)

    def test_single_other_proto_udp_like_packet_is_unobservable(self):
        frag = line(100.5, "eth0", "Out", "10.0.0.5 > 8.8.8.8: ip-proto-17, length 40")
        v = run([CANARY_LINE, FINAL_CANARY_LINE, OUT_TCP, frag], make_index())
        self.assertEqual(v["verdict"], "unobservable")
        self.assertEqual(v["other_udp_like"], 1)
        self.assertTrue(any("1 packet(s) with unknown protocol look like UDP/DNS" in r for r in v["unobservable_reasons"]))

    def test_load_packets_counts_udp_like_unparsed(self):
        with tempfile.TemporaryDirectory() as d:
            f = Path(d) / "capture.txt"
            f.write_text(
                "\n".join(
                    [
                        CANARY_LINE,
                        "garbage UDP, length 9",
                        "0x0000:  4500 0021",
                        OUT_TCP,
                    ]
                )
                + "\n"
            )
            r = hc.load_packets(f, 1000)
        self.assertEqual((r["unparsed"], r["unparsed_udp_like"], r["total_lines"], r["truncated"]), (2, 1, 4, False))
        self.assertEqual(len(r["packets"]), 2)


class CoverageTests(unittest.TestCase):
    def test_final_canary_after_last_sample_passes(self):
        v = run([EARLY_ACK, CANARY_LINE, FINAL_CANARY_LINE], make_index())
        self.assertEqual(v["verdict"], "pass", v)

    def test_runner_traffic_after_last_sample_without_final_canary_is_unobservable(self):
        # 最後のパケットが Servo の最後のサンプル（100.6）より後でも、runner の通常通信は終端の証拠にならない（#1470 の回帰）
        v = run([CANARY_LINE, OUT_TCP, LATE_ACK], make_index())
        self.assertEqual(v["verdict"], "unobservable")
        self.assertTrue(
            any(r.startswith("final canary not observed after the last Servo sample (final_canary_t=100.65, servo last=100.6") for r in v["unobservable_reasons"]),
            v["unobservable_reasons"],
        )

    def test_regular_canary_before_final_canary_t_is_not_the_final_canary(self):
        v = run([CANARY_LINE, OUT_TCP], make_index())
        self.assertEqual(v["verdict"], "unobservable")

    def test_end_record_without_final_canary_t_is_unobservable(self):
        idx = make_index(end=False)
        idx.feed({"type": "end", "t": 101.0, "truncated": False, "reason": "stop"})
        v = run([CANARY_LINE, FINAL_CANARY_LINE, OUT_TCP], idx)
        self.assertEqual(v["verdict"], "unobservable")
        self.assertTrue(any("no final_canary_t" in r for r in v["unobservable_reasons"]), v["unobservable_reasons"])
        idx = make_index(end=False)
        idx.feed({"type": "end", "t": 101.0, "truncated": False, "reason": "stop", "final_canary_t": None})
        self.assertEqual(run([CANARY_LINE, FINAL_CANARY_LINE], idx)["verdict"], "unobservable")

    def test_quiet_period_after_last_sample_with_only_final_canary_passes(self):
        # 今回の現象の回帰: Servo の最後のサンプルの後はカナリア以外に通信が無い。パケットの最終時刻が観測区間の終端より
        # 前でも（通常のカナリア 100.3 と最終カナリア 100.6501 だけ）、最終カナリアが入っていれば pass
        v = run([EARLY_ACK, CANARY_LINE, FINAL_CANARY_LINE], make_index())
        self.assertEqual(v["verdict"], "pass", v)
        self.assertEqual(v["canary"]["attributed_to_self"], 2)

    def test_canary_outside_servo_window_is_unobservable(self):
        out_win = [
            line(99.0, "lo", "Out", f"127.0.0.1.{CANARY} > 127.0.0.1.50000: UDP, length 26"),
            line(101.0, "lo", "Out", f"127.0.0.1.{CANARY} > 127.0.0.1.50000: UDP, length 26"),
        ]
        idx = make_index()
        # 区間外のカナリアも自身に帰属できるよう、ソケット区間を広げる
        idx.feed({"type": "sample", "t": 100.6, "servo": [{"pid": 200, "bwrap": True, "netns": ISO_NS}], "sockets": [{"proto": "udp", "port": CANARY, "pid": 10, "role": hc.ROLE_SELF}]})
        v = run(out_win + [OUT_TCP], idx)
        self.assertEqual(v["verdict"], "unobservable", v)
        self.assertEqual(v["canary"]["in_servo_window"], 0)
        self.assertIn("canary not observed within the Servo observation window", v["unobservable_reasons"])
        v = run([CANARY_LINE, FINAL_CANARY_LINE, OUT_TCP], make_index())
        self.assertEqual(v["verdict"], "pass", v)
        self.assertEqual(v["canary"]["in_servo_window"], 1)

    def test_malformed_sample_line_is_unobservable(self):
        idx = make_index()
        idx.bad_lines = 1
        v = run([CANARY_LINE, FINAL_CANARY_LINE, OUT_TCP], idx)
        self.assertEqual(v["verdict"], "unobservable")
        self.assertIn("1 malformed sample line(s)", v["unobservable_reasons"])

    def test_load_samples_counts_bad_json_line(self):
        with tempfile.TemporaryDirectory() as d:
            f = Path(d) / "samples.jsonl"
            f.write_text('{"type":"tick","t":1.0}\n{not json\n')
            idx = hc.load_samples(f)
        self.assertEqual(idx.bad_lines, 1)

    def test_lines_over_limit_are_counted_and_unobservable(self):
        with tempfile.TemporaryDirectory() as d:
            f = Path(d) / "samples.jsonl"
            f.write_text("".join('{"type":"tick","t":1.0}\n' for _ in range(7)))
            with mock.patch.object(hc, "MAX_SAMPLES", 2):
                idx = hc.load_samples(f)
        self.assertEqual(idx.lines_over_limit, 3)
        v = run([CANARY_LINE, FINAL_CANARY_LINE, OUT_TCP], idx)
        self.assertEqual(v["verdict"], "unobservable")
        self.assertTrue(any("samples file exceeded the line limit (3 line(s) not read)" in r for r in v["unobservable_reasons"]))

    def test_packet_limit_counts_unparsed_lines(self):
        v = run([CANARY_LINE, FINAL_CANARY_LINE, OUT_TCP], make_index(), max_packets=3, unparsed=1, total_lines=3)
        self.assertEqual(v["verdict"], "unobservable")
        self.assertIn("capture reached the packet limit (3)", v["unobservable_reasons"])

    def test_max_packets_argument_rejects_zero(self):
        with self.assertRaises(SystemExit), mock.patch("sys.stderr"):
            hc.main(["judge", "--out-dir", "/nonexistent", "--max-packets", "0"])
        # CLI は既定値を持たない（#1473）。省略は argparse エラー（exit code 2）
        with self.assertRaises(SystemExit) as cm, mock.patch("sys.stderr"):
            hc.main(["judge", "--out-dir", "/nonexistent"])
        self.assertEqual(cm.exception.code, 2)
        with self.assertRaises(argparse.ArgumentTypeError):
            hc._positive_int("0")
        self.assertEqual(hc._positive_int("300000"), 300000)


@unittest.skipUnless(sys.platform == "linux", "/proc is Linux only")
class FinalCanaryTests(unittest.TestCase):
    def test_sampler_sends_one_last_canary_before_end(self):
        sent = []
        real = socket.socket

        class Spy(real):
            def sendto(self, *a, **k):
                sent.append(a[0])
                return super().sendto(*a, **k)

        with tempfile.TemporaryDirectory() as d:
            out = Path(d) / "samples.jsonl"
            with mock.patch.object(hc.socket, "socket", Spy):
                # max_seconds が定期カナリア（開始 1 秒後）より短いので、送信は停止時の 1 回だけ
                hc.run_sampler(os.path.join(d, "no-such-servo"), out, 0.05, None, max_seconds=0.3)
            recs = [json.loads(x) for x in out.read_text().splitlines()]
        self.assertEqual(recs[-1]["type"], "end")
        self.assertEqual(sent, [hc._CANARY_PAYLOAD])
        self.assertIsInstance(recs[-1]['final_canary_t'], float)
        self.assertLessEqual(recs[-1]['final_canary_t'], recs[-1]['t'])


class UnattributedWindowTests(unittest.TestCase):
    def test_packet_outside_window_not_counted_in_window(self):
        out = line(150.0, "eth0", "Out", "10.0.0.5.5000 > 8.8.8.8.53: UDP, length 5")
        v = run([CANARY_LINE, FINAL_CANARY_LINE, OUT_TCP, out], make_index())
        self.assertEqual(v["counts"][hc.ROLE_NONE]["dns"], 1)
        u = v["unattributed_in_servo_window"]
        self.assertEqual((u["dns"], u["udp_other"], u["destinations"]), (0, 0, []))
        self.assertEqual(u["window"][0], 100.0)
        self.assertAlmostEqual(u["window"][1], 100.6)
        self.assertAlmostEqual(u["counted_window"][0], 99.5)
        self.assertAlmostEqual(u["counted_window"][1], 101.1)
        self.assertEqual(u["slack_seconds"], 0.5)

    def test_destinations_are_cut_to_top_n(self):
        lines = [CANARY_LINE, FINAL_CANARY_LINE, OUT_TCP]
        for i in range(11):
            lines.append(line(100.3, "eth0", "Out", f"10.0.0.5.{5000 + i} > 9.9.9.{i + 1}.53: UDP, length 5"))
        lines.append(line(100.3, "eth0", "Out", "10.0.0.5.5999 > 9.9.9.1.53: UDP, length 5"))
        v = run(lines, make_index())
        u = v["unattributed_in_servo_window"]
        self.assertEqual(len(u["destinations"]), 10)
        self.assertEqual(u["destinations"][0], {"dst": "9.9.9.1", "dport": 53, "count": 2})
        self.assertEqual((u["destinations_total"], u["destinations_omitted"], u["dns"]), (11, 1, 12))
        self.assertIn("(+1 more)", hc.render_summary(v))

    def test_unattributed_dns_keeps_verdict_pass_and_is_summarized(self):
        dns = line(100.3, "eth0", "Out", "10.0.0.5.5000 > 8.8.8.8.53: UDP, length 5")
        v = run([CANARY_LINE, FINAL_CANARY_LINE, OUT_TCP, dns], make_index())
        self.assertEqual(v["verdict"], "pass", v)
        u = v["unattributed_in_servo_window"]
        self.assertEqual((u["dns"], u["udp_other"]), (1, 0))
        self.assertEqual(u["destinations"], [{"dst": "8.8.8.8", "dport": 53, "count": 1}])
        summary = hc.render_summary(v)
        self.assertIn("unattributed DNS/UDP within the isolated Servo window [100.000..100.600] (counted over [99.500..101.100], +/-0.5 s slack): dns=1 udp_other=0", summary)
        self.assertIn("| 8.8.8.8 | 53 | 1 |", summary)
        self.assertIn("unattributed DNS/UDP cannot be ruled out as Servo's", summary)

    def test_loopback_stub_dns_is_counted_in_window(self):
        out = line(100.3, "lo", "Out", "127.0.0.1.4000 > 127.0.0.53.53: UDP, length 5")
        inn = line(100.4, "lo", "In", "127.0.0.1.4001 > 127.0.0.53.53: UDP, length 5")
        v = run([CANARY_LINE, FINAL_CANARY_LINE, OUT_TCP, out, inn], make_index())
        u = v["unattributed_in_servo_window"]
        self.assertEqual((u["dns"], u["udp_other"]), (2, 0))
        self.assertEqual(u["destinations"], [{"dst": "127.0.0.53", "dport": 53, "count": 2}])

    def test_zero_count_line_is_still_in_summary(self):
        v = run([CANARY_LINE, FINAL_CANARY_LINE, OUT_TCP], make_index())
        summary = hc.render_summary(v)
        self.assertIn("within the isolated Servo window [100.000..100.600] (counted over [99.500..101.100], +/-0.5 s slack): dns=0 udp_other=0", summary)
        self.assertIn("| none | | |", summary)


def _end_idx(final):
    idx = make_index(end=False)
    idx.feed({"type": "end", "t": 101.0, "truncated": False, "reason": "stop", "final_canary_t": final})
    return idx


class SlackAndTerminationTests(unittest.TestCase):
    """#1473: 観測区間の余裕・最終カナリアの検証・stop ステップのポーリング。"""

    def test_unattributed_dns_just_before_first_sample_is_counted_with_slack(self):
        base = [CANARY_LINE, FINAL_CANARY_LINE, OUT_TCP]
        inside = line(99.7, "eth0", "Out", "10.0.0.5.5000 > 8.8.8.8.53: UDP, length 5")
        v = run(base + [inside], make_index())
        self.assertEqual(v["verdict"], "pass", v)
        self.assertEqual(v["unattributed_in_servo_window"]["dns"], 1)
        outside = line(99.4, "eth0", "Out", "10.0.0.5.5000 > 8.8.8.8.53: UDP, length 5")
        v = run(base + [outside], make_index())
        self.assertEqual(v["unattributed_in_servo_window"]["dns"], 0)
        self.assertEqual(v["counts"][hc.ROLE_NONE]["dns"], 1)
        end_in = line(101.05, "eth0", "Out", "10.0.0.5.5000 > 8.8.8.8.53: UDP, length 5")
        end_out = line(101.2, "eth0", "Out", "10.0.0.5.5000 > 8.8.8.8.53: UDP, length 5")
        self.assertEqual(run(base + [end_in], make_index())["unattributed_in_servo_window"]["dns"], 1)
        self.assertEqual(run(base + [end_out], make_index())["unattributed_in_servo_window"]["dns"], 0)

    def test_slack_does_not_widen_canary_window(self):
        # 区間の外（余裕の内側）のカナリアしか無いなら陽性対照は成立しない
        early = line(99.8, "lo", "Out", f"127.0.0.1.{CANARY} > 127.0.0.1.50000: UDP, length 26")
        v = run([early, FINAL_CANARY_LINE, OUT_TCP], make_index())
        self.assertEqual(v["verdict"], "unobservable")
        self.assertEqual(v["canary"]["in_servo_window"], 0)

    def test_missing_final_canary_packet_is_unobservable(self):
        v = run([CANARY_LINE, OUT_TCP, LATE_ACK], make_index())
        self.assertEqual(v["verdict"], "unobservable")
        self.assertTrue(
            any(r.startswith("final canary not observed after the last Servo sample") for r in v["unobservable_reasons"]),
            v["unobservable_reasons"],
        )

    def test_epsilon_boundary(self):
        def mk(ts):
            return line(ts, "lo", "Out", f"127.0.0.1.{CANARY} > 127.0.0.1.50000: UDP, length 26")

        ok = run([CANARY_LINE, mk(100.6495), OUT_TCP], make_index())
        self.assertEqual(ok["verdict"], "pass", ok)
        ng = run([CANARY_LINE, mk(100.648), OUT_TCP], make_index())
        self.assertEqual(ng["verdict"], "unobservable")
        self.assertTrue(any("final canary not observed" in r for r in ng["unobservable_reasons"]))

    def test_invalid_final_canary_t_is_unobservable(self):
        for bad in (float("nan"), float("inf"), float("-inf"), True, "100.65", 100.0):
            with self.subTest(bad=bad):
                v = run([CANARY_LINE, OUT_TCP], _end_idx(bad))
                self.assertEqual(v["verdict"], "unobservable", v)
                self.assertTrue(
                    any(r.startswith("final_canary_t is invalid") for r in v["unobservable_reasons"]),
                    v["unobservable_reasons"],
                )

    def test_valid_final_canary_t_helper(self):
        self.assertEqual(hc._valid_final_canary_t({"final_canary_t": 100.65}, (100.0, 100.6)), (100.65, None))
        self.assertEqual(hc._valid_final_canary_t({}, (100.0, 100.6)), (None, None))
        self.assertEqual(hc._valid_final_canary_t(None, None), (None, None))

    def test_final_canary_observed_boundaries(self):
        def mk(ts, iface="lo", port=CANARY):
            return pkts(line(ts, iface, "Out", f"127.0.0.1.{port} > 127.0.0.1.50000: UDP, length 26"))

        w = (100.0, 100.6)
        self.assertTrue(hc.final_canary_observed(mk(100.649), CANARY, 100.65, w))
        self.assertFalse(hc.final_canary_observed(mk(100.648999), CANARY, 100.65, w))
        self.assertFalse(hc.final_canary_observed(mk(100.7, port=CANARY + 1), CANARY, 100.65, w))
        self.assertFalse(hc.final_canary_observed(mk(100.7, iface="eth0"), CANARY, 100.65, w))

    def _wait_dir(self, d, end=True, final=100.65):
        out = Path(d)
        recs = [
            {"type": "meta", "self_netns": SELF_NS, "canary_port": CANARY, "pid": 10},
            {"type": "sample", "t": 100.0, "servo": [{"pid": 200, "bwrap": True, "netns": ISO_NS}], "sockets": []},
            {"type": "tick", "t": 100.6},
        ]
        if end:
            recs.append({"type": "end", "t": 101.0, "truncated": False, "reason": "stop", "final_canary_t": final})
        (out / "samples.jsonl").write_text("\n".join(json.dumps(r) for r in recs) + "\n")
        return type("A", (), {"out_dir": str(out), "timeout": 15.0, "interval": 0.1})()

    @staticmethod
    def _popen(stdout):
        """Popen の偽物。stdout は StringIO で、kill / wait / poll は no-op。"""
        import io

        p = mock.MagicMock()
        p.stdout = io.StringIO(stdout)
        p.stderr = io.StringIO("")
        p.poll.return_value = 0
        p.wait.return_value = 0
        p.returncode = 0
        return p

    @staticmethod
    def _fake_clock():
        """sleep で進む偽時計（monotonic の呼び出し回数に依存しない）。"""
        now = [0.0]
        return now, (lambda: now[0]), (lambda s: now.__setitem__(0, now[0] + s))

    def test_wait_final_canary_succeeds_on_second_poll(self):
        final = FINAL_CANARY_LINE + "\n"
        with tempfile.TemporaryDirectory() as d:
            ns = self._wait_dir(d)
            with mock.patch.object(hc.subprocess, "Popen", side_effect=[self._popen(""), self._popen(final)]) as popen, \
                 mock.patch.object(hc.time, "sleep"):
                self.assertEqual(hc.cmd_wait_final_canary(ns), 0)
            self.assertEqual(popen.call_count, 2)
            args = popen.call_args.args[0]
            self.assertEqual(args, ["tcpdump", "-r", str(Path(d) / "capture.pcap"), "-nn", "-tt", f"udp and src port {CANARY}"])
            self.assertNotIn("shell", popen.call_args.kwargs)

    def test_wait_final_canary_times_out_with_shrinking_budget(self):
        with tempfile.TemporaryDirectory() as d:
            ns = self._wait_dir(d)
            ns.timeout = 3.0
            ns.interval = 1.0
            now, mono, sleep = self._fake_clock()
            budgets = []

            def poll(pcap, port, final_t, window, budget, *a, **k):
                budgets.append(budget)
                return hc.PollResult(False, hc.POLL_BUDGET_KILLED, "x")

            with mock.patch.object(hc, "_poll_pcap_once", side_effect=poll), \
                 mock.patch.object(hc.time, "sleep", side_effect=sleep), mock.patch.object(hc.time, "monotonic", side_effect=mono), \
                 mock.patch("sys.stdout"):
                self.assertEqual(hc.cmd_wait_final_canary(ns), 1)
            self.assertEqual(budgets, [3.0, 2.0, 1.0, hc.WAIT_CANARY_POLL_MIN_SECONDS])  # 締切到達後の最終 1 回は下限
            self.assertTrue(all(b <= hc.WAIT_CANARY_POLL_MAX_SECONDS for b in budgets))

    def test_wait_final_canary_tcpdump_missing_is_not_success(self):
        with tempfile.TemporaryDirectory() as d:
            ns = self._wait_dir(d)
            now, mono, sleep = self._fake_clock()
            with mock.patch.object(hc.subprocess, "Popen", side_effect=FileNotFoundError), \
                 mock.patch.object(hc.time, "sleep", side_effect=sleep), mock.patch.object(hc.time, "monotonic", side_effect=mono), \
                 mock.patch("sys.stdout"):
                self.assertEqual(hc.cmd_wait_final_canary(ns), 1)
            with mock.patch.object(hc.subprocess, "Popen", side_effect=OSError):
                r = hc._poll_pcap_once(Path(d) / "capture.pcap", CANARY, 100.65, (100.0, 100.6), 1.0)
                self.assertFalse(r.observed)
                self.assertEqual(r.reason, hc.POLL_SPAWN_FAILED)

    def test_wait_final_canary_invalid_inputs_do_not_run_tcpdump(self):
        for kw in ({"end": False}, {"final": float("nan")}, {"final": 100.0}):
            with self.subTest(kw=kw), tempfile.TemporaryDirectory() as d:
                ns = self._wait_dir(d, **kw)
                with mock.patch.object(hc.subprocess, "Popen") as popen, mock.patch("sys.stdout"):
                    self.assertEqual(hc.cmd_wait_final_canary(ns), 2)
                popen.assert_not_called()
        with tempfile.TemporaryDirectory() as d, mock.patch("sys.stdout"):
            self.assertEqual(hc.cmd_wait_final_canary(type("A", (), {"out_dir": d, "timeout": 1.0, "interval": 0.1})()), 2)

    # --- 偽の tcpdump（Python スクリプト）による実プロセステスト（#1476）。OS を問わず動く ---

    def _fake_tcpdump(self, d, body):
        fake = Path(d) / "fake_tcpdump.py"
        fake.write_text("import sys, time\n" + body)
        return mock.patch.object(hc, "TCPDUMP_ARGV", (sys.executable, "-I", "-B", str(fake)))

    def _poll(self, d, **kw):
        return hc._poll_pcap_once(Path(d) / "capture.pcap", CANARY, 100.65, (100.0, 100.6), **kw)

    def test_wait_final_canary_slow_tcpdump_stays_within_deadline(self):
        with tempfile.TemporaryDirectory() as d:
            ns = self._wait_dir(d)
            ns.timeout = 1.0
            ns.interval = 0.1
            calls = []
            real = hc._poll_pcap_once

            def rec(*a, **k):
                r = real(*a, **k)
                calls.append((a[4], r))
                return r

            with self._fake_tcpdump(d, "time.sleep(30)\n"), mock.patch.object(hc, "_poll_pcap_once", side_effect=rec), \
                 mock.patch("sys.stdout") as out:
                t0 = time.monotonic()
                self.assertEqual(hc.cmd_wait_final_canary(ns), 1)
                elapsed = time.monotonic() - t0
            self.assertTrue(1 <= len(calls) <= 2, len(calls))
            for budget, r in calls:
                self.assertEqual(r.reason, hc.POLL_BUDGET_KILLED)
                self.assertLessEqual(budget, 1.0)
            written = "".join(c.args[0] for c in out.write.call_args_list)
            self.assertIn("last poll: budget-killed", written)
            # 実時間は 30 秒の sleep を待ってしまう桁の退行だけを捕まえる。締切の正しさは下の決定的なテストと理由・budget で見る
            self.assertLess(elapsed, 10.0)

    def test_poll_stops_early_on_final_canary(self):
        body = f"print({CANARY_LINE!r}); print({FINAL_CANARY_LINE!r}); sys.stdout.flush(); time.sleep(30)\n"
        with tempfile.TemporaryDirectory() as d, self._fake_tcpdump(d, body):
            t0 = time.monotonic()
            self.assertTrue(self._poll(d, budget=20.0).observed)
            self.assertLess(time.monotonic() - t0, 10.0)

    def test_poll_uses_shared_threshold_and_boundaries(self):
        def lo(ts):
            return line(ts, "lo", "Out", f"127.0.0.1.{CANARY} > 127.0.0.1.50000: UDP, length 26")

        with tempfile.TemporaryDirectory() as d:
            with self._fake_tcpdump(d, f"print({lo(100.648999)!r})\n"):
                r = self._poll(d, budget=5.0)
                self.assertFalse(r.observed)
                self.assertEqual(r.reason, hc.POLL_EOF)
            with self._fake_tcpdump(d, f"print({lo(100.649)!r})\n"), \
                 mock.patch.object(hc, "_final_canary_threshold", wraps=hc._final_canary_threshold) as thr:
                self.assertTrue(self._poll(d, budget=5.0).observed)
                thr.assert_called()

    def test_poll_max_packets_cap_is_not_observation(self):
        body = f"for _ in range(10): print({CANARY_LINE!r})\nprint({FINAL_CANARY_LINE!r})\n"
        with tempfile.TemporaryDirectory() as d, self._fake_tcpdump(d, body):
            r = self._poll(d, budget=5.0, max_packets=5)
            self.assertFalse(r.observed)
            self.assertEqual(r.reason, hc.POLL_PACKET_LIMIT)
            self.assertTrue(self._poll(d, budget=5.0, max_packets=50).observed)

    def test_poll_long_line_is_bounded_and_following_lines_are_read(self):
        long_ = hc.MAX_PACKET_LINE_CHARS * 5
        with tempfile.TemporaryDirectory() as d:
            with self._fake_tcpdump(d, f"sys.stdout.write('x' * {long_}); sys.stdout.flush(); time.sleep(30)\n"):
                t0 = time.monotonic()
                r = self._poll(d, budget=0.5)
                self.assertFalse(r.observed)
                self.assertEqual(r.reason, hc.POLL_BUDGET_KILLED)
                self.assertLess(time.monotonic() - t0, 5.0)
            with self._fake_tcpdump(d, f"print('x' * {long_}); print({FINAL_CANARY_LINE!r})\n"):
                self.assertTrue(self._poll(d, budget=5.0).observed)

    def test_poll_missing_executable_is_not_observation(self):
        with tempfile.TemporaryDirectory() as d, mock.patch.object(hc, "TCPDUMP_ARGV", (str(Path(d) / "nope"),)):
            r = self._poll(d, budget=1.0)
            self.assertFalse(r.observed)
            self.assertEqual(r.reason, hc.POLL_SPAWN_FAILED)

    # --- 未観測の理由・読み捨ての上限・警告・決定的な締切（#1481） ---

    def test_poll_result_is_not_evaluable_as_bool(self):
        # `if result:` が常に成功扱いになる fail-open を型で防ぐ
        with self.assertRaises(TypeError):
            bool(hc.PollResult(False, hc.POLL_EOF))

    def test_poll_eof_reports_exit_code_and_sanitized_stderr(self):
        # ESC を含む行を最後に置き、末尾の抜粋に残ることを見る（#1483。先頭に置くと切り捨てられて assert が自明になる）
        body = ("sys.stderr.write('z' * 5000 + 'tcpdump: bad\\n\\x1b[31m::error::boom\\r\\n')\n"
                "sys.stderr.flush()\nsys.exit(3)\n")
        with tempfile.TemporaryDirectory() as d, self._fake_tcpdump(d, body):
            r = self._poll(d, budget=10.0)
        self.assertFalse(r.observed)
        self.assertEqual(r.reason, hc.POLL_EOF)
        self.assertIn("exited with 3", r.detail)
        self.assertIn("stderr: ", r.detail)
        self.assertNotIn("\n", r.detail)
        self.assertNotIn("\r", r.detail)
        self.assertNotIn("\x1b", r.detail)
        self.assertIn("[31m::error::boom", r.detail)
        self.assertTrue(r.detail.endswith("tcpdump: bad [31m::error::boom"), r.detail)
        self.assertLessEqual(len(r.detail), 128 + hc.WAIT_CANARY_STDERR_NOTE_CHARS)

    def test_poll_eof_detail_exact_for_short_stderr(self):
        body = "sys.stderr.write('tcpdump: bad\\n\\x1b[31m::error::boom\\r\\n')\nsys.stderr.flush()\nsys.exit(3)\n"
        with tempfile.TemporaryDirectory() as d, self._fake_tcpdump(d, body):
            r = self._poll(d, budget=10.0)
        self.assertEqual(r.detail, "tcpdump exited with 3 before the final canary; stderr: tcpdump: bad [31m::error::boom")

    def test_poll_large_stderr_is_bounded_and_does_not_block_or_use_tempfile(self):
        # #1483: 数 MB の stderr でも pipe が詰まらず（kill に至らず）、一時ファイルも作らない
        body = ("sys.stderr.write('e' * 3_000_000 + 'END-MARKER')\nsys.stderr.flush()\nsys.exit(3)\n")
        with tempfile.TemporaryDirectory() as d, self._fake_tcpdump(d, body), \
             mock.patch("tempfile.TemporaryFile") as tf:
            t0 = time.monotonic()
            r = self._poll(d, budget=20.0)
            elapsed = time.monotonic() - t0
        tf.assert_not_called()
        self.assertEqual(r.reason, hc.POLL_EOF)
        self.assertIn("END-MARKER", r.detail)
        self.assertLess(elapsed, 15.0)

    def test_stderr_tail_keeps_only_the_tail(self):
        import io

        with mock.patch.object(hc, "WAIT_CANARY_STDERR_TAIL_BYTES", 10):
            t = hc._StderrTail(io.BytesIO(b"0123456789abcdefghij" * 1000 + b"TAIL-END!"))
            t.start()
            self.assertTrue(t.join(5.0))
            snap = t.snapshot()
        self.assertEqual(snap, (b"0123456789abcdefghij" * 1000 + b"TAIL-END!")[-10:])

    def test_stderr_tail_handles_str_eof_and_oserror_streams(self):
        import io

        class Boom:
            def read(self, n):
                raise OSError("boom")

            def close(self):
                pass

        t = hc._StderrTail(io.StringIO("hello"))
        t.start()
        self.assertTrue(t.join(5.0))
        self.assertEqual(t.snapshot(), b"hello")
        t = hc._StderrTail(io.BytesIO(b""))
        t.start()
        self.assertTrue(t.join(5.0))
        self.assertEqual(t.snapshot(), b"")
        t = hc._StderrTail(Boom())
        t.start()
        self.assertTrue(t.join(5.0))
        self.assertEqual(t.snapshot(), b"")

    def test_kill_if_running_only_kills_live_process(self):
        import threading

        done = mock.MagicMock()
        done.poll.return_value = 0
        ev = threading.Event()
        hc._kill_if_running(done, ev)
        self.assertFalse(ev.is_set())
        done.kill.assert_not_called()
        live = mock.MagicMock()
        live.poll.return_value = None
        hc._kill_if_running(live, ev)
        self.assertTrue(ev.is_set())
        live.kill.assert_called_once()

    def test_poll_timer_firing_after_natural_exit_is_eof_not_budget_killed(self):
        # #1483: 自然終了の直後に Timer が発火しても budget-killed にしない
        import io

        p = self._popen("")
        p.poll.return_value = 0
        captured = {}

        class FakeTimer:
            def __init__(self, interval, fn, args=()):
                captured["fn"], captured["args"] = fn, args
                self.daemon = False

            def start(self):
                pass

            def cancel(self):
                pass

        class Stdout(io.StringIO):
            def readline(self, n=-1):
                captured["fn"](*captured["args"])
                return ""

        p.stdout = Stdout("")
        with tempfile.TemporaryDirectory() as d, mock.patch.object(hc.subprocess, "Popen", return_value=p), \
             mock.patch.object(hc.threading, "Timer", FakeTimer):
            r = self._poll(d, budget=5.0)
        self.assertEqual(r.reason, hc.POLL_EOF)
        self.assertFalse(r.observed)
        p.kill.assert_not_called()

    def test_poll_cleans_up_when_timer_start_fails(self):
        # #1483: Timer の起動失敗でも kill・wait・close を行い、例外は伝播して成功扱いにしない
        p = self._popen(FINAL_CANARY_LINE + "\n")
        p.poll.return_value = None
        stdout = p.stdout

        class BadTimer:
            def __init__(self, *a, **k):
                self.daemon = False

            def start(self):
                raise RuntimeError("cannot start")

            def cancel(self):
                raise AssertionError("cancel must not be called")

        with tempfile.TemporaryDirectory() as d, mock.patch.object(hc.subprocess, "Popen", return_value=p), \
             mock.patch.object(hc.threading, "Timer", BadTimer):
            with self.assertRaises(RuntimeError):
                self._poll(d, budget=5.0)
        p.kill.assert_called()
        p.wait.assert_called()
        self.assertTrue(stdout.closed)

    def test_poll_kill_reports_budget_killed(self):
        with tempfile.TemporaryDirectory() as d, self._fake_tcpdump(d, "time.sleep(30)\n"):
            r = self._poll(d, budget=0.5)
        self.assertEqual(r.reason, hc.POLL_BUDGET_KILLED)
        self.assertIn("0.5s", r.detail)

    def test_read_bounded_line_discard_is_capped(self):
        import io

        n = hc.MAX_PACKET_LINE_CHARS
        line, discarded = hc._read_bounded_line(io.StringIO("a" * (n * 10)), n * 2)
        self.assertEqual(line, "a" * n)
        self.assertGreater(discarded, n * 2)
        self.assertLess(discarded, n * 4)
        s = io.StringIO("b" * (n * 3) + "\nnext\n")
        line, discarded = hc._read_bounded_line(s, n * 100)
        self.assertEqual(line, "b" * n)
        self.assertEqual(discarded, n * 2 + 1)
        self.assertEqual(hc._read_bounded_line(s, n * 100), ("next\n", 0))

    def test_poll_discard_limit_stops_endless_unterminated_output(self):
        body = "while True:\n    sys.stdout.write('x' * 4096); sys.stdout.flush()\n"
        with tempfile.TemporaryDirectory() as d, self._fake_tcpdump(d, body), \
             mock.patch.object(hc, "WAIT_CANARY_MAX_DISCARD_CHARS", 10_000):
            r = self._poll(d, budget=30.0)
        self.assertFalse(r.observed)
        self.assertEqual(r.reason, hc.POLL_DISCARD_LIMIT)

    def test_poll_warns_when_tcpdump_does_not_exit_after_kill(self):
        import io
        import subprocess

        p = self._popen("")
        p.poll.return_value = None
        p.wait.side_effect = subprocess.TimeoutExpired("tcpdump", 1.0)
        err = io.StringIO()
        with tempfile.TemporaryDirectory() as d, mock.patch.object(hc.subprocess, "Popen", return_value=p), \
             mock.patch.object(hc.sys, "stderr", err):
            r = self._poll(d, budget=5.0)
        self.assertFalse(r.observed)
        self.assertIn("warning: tcpdump did not exit", err.getvalue())
        p.kill.assert_called()

    def test_wait_final_canary_timeout_message_carries_last_reason(self):
        with tempfile.TemporaryDirectory() as d:
            ns = self._wait_dir(d)
            ns.timeout = 3.0
            now, mono, sleep = self._fake_clock()
            res = hc.PollResult(False, hc.POLL_PACKET_LIMIT, "no final canary in the first 5 lines")
            with mock.patch.object(hc, "_poll_pcap_once", return_value=res), \
                 mock.patch.object(hc.time, "sleep", side_effect=sleep), mock.patch.object(hc.time, "monotonic", side_effect=mono), \
                 mock.patch("sys.stdout") as out:
                self.assertEqual(hc.cmd_wait_final_canary(ns), 1)
            written = "".join(c.args[0] for c in out.write.call_args_list)
            self.assertTrue(written.startswith("note: final canary not seen in pcap within 3s"), written)
            self.assertIn("last poll: packet-limit", written)

    def test_wait_final_canary_deadline_is_deterministic(self):
        # tcpdump が kill まで終わらない最悪の場合（各ポーリングが budget いっぱい使う）を偽時計で再現する
        # interval=0.1 の sleep が挟まるため 25 秒の最後の budget は 25 - (10 + 0.1 + 10 + 0.1) = 4.8
        expected = {1.0: [1.0], 25.0: [10.0, 10.0, 4.8]}
        for timeout, want in expected.items():
            with self.subTest(timeout=timeout), tempfile.TemporaryDirectory() as d:
                ns = self._wait_dir(d)
                ns.timeout = timeout
                ns.interval = 0.1
                now, mono, sleep = self._fake_clock()
                budgets = []

                def poll(pcap, port, final_t, window, budget, *a, **k):
                    budgets.append(budget)
                    now[0] += budget
                    return hc.PollResult(False, hc.POLL_BUDGET_KILLED, "x")

                with mock.patch.object(hc, "_poll_pcap_once", side_effect=poll), \
                     mock.patch.object(hc.time, "sleep", side_effect=sleep), mock.patch.object(hc.time, "monotonic", side_effect=mono), \
                     mock.patch("sys.stdout"):
                    self.assertEqual(hc.cmd_wait_final_canary(ns), 1)
                self.assertEqual(len(budgets), len(want))
                for got, exp in zip(budgets, want):
                    self.assertAlmostEqual(got, exp, places=6)
                self.assertLessEqual(max(budgets), hc.WAIT_CANARY_POLL_MAX_SECONDS)
                self.assertLessEqual(now[0], timeout + hc.WAIT_CANARY_POLL_MIN_SECONDS + 0.1)

    def test_missing_individual_files_are_unobservable(self):
        # upload-artifact の if-no-files-found は個々の欠落を検出しない。欠落は judge が unobservable にする前提（#1473 C）
        cases = {
            "tcpdump.exit": "tcpdump exit status is missing",
            "tcpdump.stderr": "kernel drop statistics are missing",
            "samples.jsonl": "capture text or samples file is missing",
        }
        for missing, reason in cases.items():
            with self.subTest(missing=missing), tempfile.TemporaryDirectory() as d:
                out = Path(d)
                (out / "capture.txt").write_text(CANARY_LINE + "\n" + FINAL_CANARY_LINE + "\n" + OUT_TCP + "\n")
                recs = [
                    {"type": "meta", "self_netns": SELF_NS, "canary_port": CANARY, "pid": 10},
                    {"type": "sample", "t": 100.0, "servo": [{"pid": 200, "bwrap": True, "netns": ISO_NS}],
                     "sockets": [{"proto": "udp", "port": CANARY, "pid": 10, "role": "self"}]},
                    {"type": "tick", "t": 100.5},
                    {"type": "end", "t": 101.0, "truncated": False, "reason": "stop", "final_canary_t": 100.65},
                ]
                (out / "samples.jsonl").write_text("\n".join(json.dumps(r) for r in recs) + "\n")
                (out / "tcpdump.exit").write_text("0\n")
                (out / "convert.exit").write_text("0\n")
                (out / "tcpdump.stderr").write_text("0 packets dropped by kernel\n")
                (out / missing).unlink()
                ns = type("A", (), {"out_dir": str(out), "max_packets": 1000})()
                with mock.patch("sys.stdout"):
                    self.assertEqual(hc.cmd_judge(ns), 1)
                v = json.loads((out / "verdict.json").read_text())
                self.assertEqual(v["verdict"], "unobservable")
                self.assertTrue(any(reason in r for r in v["unobservable_reasons"]), v["unobservable_reasons"])


@unittest.skipUnless(sys.platform == "linux", "/proc is Linux only")
class FinalCanarySendFailureTests(unittest.TestCase):
    def test_sendto_oserror_leaves_no_final_canary_t(self):
        real = socket.socket

        class Boom(real):
            def sendto(self, *a, **k):
                raise OSError("boom")

        with tempfile.TemporaryDirectory() as d:
            out = Path(d) / "samples.jsonl"
            with mock.patch.object(hc.socket, "socket", Boom):
                hc.run_sampler(os.path.join(d, "no-such-servo"), out, 0.05, None, max_seconds=0.3)
            recs = [json.loads(x) for x in out.read_text().splitlines()]
            idx = hc.load_samples(out)
        self.assertEqual(recs[-1]["type"], "end")
        self.assertIsNone(recs[-1].get("final_canary_t"))
        v = run([CANARY_LINE, OUT_TCP], idx)
        self.assertEqual(v["verdict"], "unobservable")
        self.assertTrue(any("no final_canary_t" in r for r in v["unobservable_reasons"]), v["unobservable_reasons"])


if __name__ == "__main__":
    unittest.main()
