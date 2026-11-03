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
        idx.feed({"type": "end", "t": 101.0, "truncated": False, "reason": "stop"})
    return idx


CANARY_LINE = line(100.3, "lo", "Out", f"127.0.0.1.{CANARY} > 127.0.0.1.50000: UDP, length 26")
OUT_TCP = line(100.4, "eth0", "Out", "10.0.0.5.6000 > 8.8.8.8.443: Flags [S], seq 1, win 1000, length 0")


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
        v = run([CANARY_LINE, OUT_TCP, line(100.3, "eth0", "Out", "10.0.0.5.5000 > 8.8.8.8.53: UDP, length 5")], make_index(sockets=socks))
        self.assertEqual(v["verdict"], "fail")
        self.assertEqual(v["counts"][hc.ROLE_ISOLATED]["dns"], 1)

    def test_packet_outside_window_not_attributed(self):
        socks = [{"proto": "udp", "port": 5000, "pid": 200, "role": hc.ROLE_ISOLATED}]
        v = run([CANARY_LINE, OUT_TCP, line(150.0, "eth0", "Out", "10.0.0.5.5000 > 8.8.8.8.53: UDP, length 5")], make_index(sockets=socks))
        self.assertEqual(v["counts"][hc.ROLE_NONE]["dns"], 1)
        self.assertEqual(v["verdict"], "pass")


class VerdictTests(unittest.TestCase):
    def ok(self, idx=None, lines=None, **kw):
        return run(lines if lines is not None else [CANARY_LINE, OUT_TCP], idx or make_index(), **kw)

    def test_pass(self):
        v = self.ok()
        self.assertEqual(v["verdict"], "pass", v)
        self.assertEqual(v["servo_pids"], {"isolated": 1, "unisolated": 0})
        self.assertEqual(v["canary"], {"port": CANARY, "seen": 1, "attributed_to_self": 1})
        self.assertIn("**pass**", hc.render_summary(v))

    def test_bwrap_with_same_netns_fails(self):
        v = self.ok(make_index(netns=SELF_NS))
        self.assertEqual(v["verdict"], "fail")

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
        v = run([CANARY_LINE, OUT_TCP], hc.SampleIndex())
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

    def test_fail_takes_precedence_over_unobservable(self):
        v = self.ok(make_index(netns=SELF_NS), tcpdump_exit=1)
        self.assertEqual(v["verdict"], "fail")


class IoTests(unittest.TestCase):
    def test_judge_command_writes_verdict_and_returns_exit_code(self):
        with tempfile.TemporaryDirectory() as d:
            out = Path(d)
            (out / "capture.txt").write_text(CANARY_LINE + "\n" + OUT_TCP + "\n")
            recs = [
                {"type": "meta", "self_netns": SELF_NS, "canary_port": CANARY, "pid": 10},
                {"type": "sample", "t": 100.0, "servo": [{"pid": 200, "bwrap": True, "netns": ISO_NS}],
                 "sockets": [{"proto": "udp", "port": CANARY, "pid": 10, "role": "self"}]},
                {"type": "tick", "t": 100.5},
                {"type": "end", "t": 101.0, "truncated": False, "reason": "stop"},
            ]
            (out / "samples.jsonl").write_text("\n".join(json.dumps(r) for r in recs) + "\n")
            (out / "tcpdump.exit").write_text("0\n")
            ns = type("A", (), {"out_dir": str(out), "max_packets": 1000})()
            self.assertEqual(hc.cmd_judge(ns), 0)
            verdict = json.loads((out / "verdict.json").read_text())
            self.assertEqual(verdict["verdict"], "pass")
            self.assertTrue((out / "summary.md").is_file())
            (out / "tcpdump.exit").write_text("2\n")
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
        servo, _sockets = hc.take_sample(os.path.realpath(sys.executable), os.getpid(), hc._read_ns_net(os.getpid()))
        me = [s for s in servo if s["pid"] == os.getpid()]
        self.assertEqual(len(me), 1)
        self.assertFalse(me[0]["bwrap"])


if __name__ == "__main__":
    unittest.main()
