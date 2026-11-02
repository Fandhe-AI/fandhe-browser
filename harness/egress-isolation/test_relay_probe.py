"""relay_probe.py の自己テスト（Issue #1431・TASK-38・`RENDER-3`・`SEC-2`）。bwrap は使わない（loopback と UDS のみ）。"""

from __future__ import annotations

import copy
import os
import shutil
import socket
import sys
import tempfile
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import relay_probe as rp  # noqa: E402

ALLOW = frozenset({("127.0.0.4", 5000), ("127.0.0.2", 5001)})


def good_inputs() -> dict:
    return dict(
        bwrap_found=True, base_rc=0, net_rc=0,
        base={"direct": {"target": {"ok": True, "errno": None}, "internal": {"ok": True, "errno": None}}},
        net={"precheck": {"ok": True, "reason": "x"}, "lo": {"ok": True, "errno": None},
             "uds": {"rw": {"ok": True, "errno": None}, "ro": {"ok": True, "errno": None}},
             "direct": {"target": {"ok": False, "errno": "ECONNREFUSED"},
                        "internal": {"ok": False, "errno": "ECONNREFUSED"}},
             "relay": {"status": 200, "echo_ok": True, "bytes": 16, "errno": None},
             "ip_recheck": {"status": 403, "errno": None}},
        outer_base={"target": 1, "internal": 1}, outer_net={"target": 1, "internal": 0},
        proxy={"codes": {"closed_early": 2, "allowed": 1, "ip_denied": 1}, "upstream_ok": 1})


def verdicts(**over: object) -> dict:
    kw = good_inputs()
    kw.update(over)
    return {k: v["verdict"] for k, v in rp.judge_relay(**kw).items()}


class ParseTest(unittest.TestCase):
    def test_valid(self) -> None:
        self.assertEqual(rp.parse_connect_request(b"CONNECT 127.0.0.4:5000 HTTP/1.1\r\nHost: x\r\n\r\n"),
                         (200, "127.0.0.4", 5000))

    def test_rejects(self) -> None:
        cases = {
            b"GET 127.0.0.4:5000 HTTP/1.1\r\n\r\n": 405,
            b"CONNECT example.test:443 HTTP/1.1\r\n\r\n": 400,
            b"CONNECT 127.0.0.4:0 HTTP/1.1\r\n\r\n": 400,
            b"CONNECT 127.0.0.4:70000 HTTP/1.1\r\n\r\n": 400,
            b"CONNECT 127.0.0.4 HTTP/1.1\r\n\r\n": 400,
            b"CONNECT 999.0.0.4:80 HTTP/1.1\r\n\r\n": 400,
            b"CONNECT 127.0.0.4:5000 HTTP/1.1": 400,
            b"CONNECT 127.0.0.4:5000 HTTP/1.1\r\n" + b"a" * 9000 + b"\r\n\r\n": 400,
        }
        for raw, status in cases.items():
            self.assertEqual(rp.parse_connect_request(raw), (status, None, None), raw[:40])


class PolicyTest(unittest.TestCase):
    def test_order_and_codes(self) -> None:
        self.assertEqual(rp.evaluate_policy("10.0.0.1", 80, ALLOW), "out_of_range")
        self.assertEqual(rp.evaluate_policy("192.0.2.1", 80, ALLOW), "out_of_range")
        # 許可リストに載っていても内部 IP は再検査で拒否する（再検査が先）。
        self.assertEqual(rp.evaluate_policy("127.0.0.2", 5001, ALLOW), "ip_denied")
        self.assertEqual(rp.evaluate_policy("127.0.0.4", 5000, ALLOW), "allowed")
        self.assertEqual(rp.evaluate_policy("127.0.0.4", 5002, ALLOW), "not_allowlisted")


class ArgvTest(unittest.TestCase):
    def args(self, netns: bool) -> list[str]:
        return rp.build_relay_argv(netns=netns, sockdir="/tmp/fer-x", inner_out="/tmp/o.json",
                                   ports={"target": 5000, "internal": 5001}, phase="netns")

    def test_netns_flag(self) -> None:
        self.assertIn("--unshare-net", self.args(True))
        self.assertNotIn("--unshare-net", self.args(False))

    def test_order_and_types(self) -> None:
        a = self.args(True)
        self.assertTrue(all(isinstance(x, str) for x in a))
        self.assertEqual(a[:2], ["bwrap", "--die-with-parent"])
        self.assertLess(a.index("--tmpfs"), a.index("--bind"))
        self.assertLess(a.index("--tmpfs"), a.index("--ro-bind"))
        self.assertEqual(a[a.index("--bind") + 2], rp.MOUNT_RW)
        self.assertEqual(a[a.index("--ro-bind") + 2], rp.MOUNT_RO)


class JudgeTest(unittest.TestCase):
    def test_all_pass(self) -> None:
        self.assertEqual(set(verdicts().values()), {"pass"})

    def test_missing_prerequisites_are_unobservable(self) -> None:
        for over in (dict(bwrap_found=False), dict(net_rc=1), dict(net_rc=None), dict(net=None),
                     dict(net={"precheck": {"ok": False, "reason": "r"}})):
            self.assertEqual(set(verdicts(**over).values()), {"unobservable"}, over)

    def test_direct_leak_fails(self) -> None:
        self.assertEqual(verdicts(outer_net={"target": 2, "internal": 0})["direct"], "fail")

    def test_internal_received_fails(self) -> None:
        self.assertEqual(verdicts(outer_net={"target": 1, "internal": 1})["ip_recheck"], "fail")

    def test_ip_recheck_reason_must_be_ip_denied(self) -> None:
        p = {"codes": {"closed_early": 2, "allowed": 1, "not_allowlisted": 1}, "upstream_ok": 1}
        self.assertEqual(verdicts(proxy=p)["ip_recheck"], "unobservable")

    def test_baseline_unreached(self) -> None:
        v = verdicts(outer_base={"target": 0, "internal": 0})
        self.assertEqual((v["direct"], v["ip_recheck"]), ("unobservable", "unobservable"))
        self.assertEqual(v["relay"], "pass")

    def test_ro_only_failure(self) -> None:
        net = copy.deepcopy(good_inputs()["net"])
        net["uds"]["ro"] = {"ok": False, "errno": "EACCES"}
        r = rp.judge_relay(**{**good_inputs(), "net": net})
        self.assertEqual(r["uds"]["verdict"], "fail")
        self.assertIn("ro", r["uds"]["reason"])

    def test_relay_failure(self) -> None:
        net = copy.deepcopy(good_inputs()["net"])
        net["relay"]["status"] = 403
        self.assertEqual(rp.judge_relay(**{**good_inputs(), "net": net})["relay"]["verdict"], "fail")


@unittest.skipUnless(hasattr(socket, "AF_UNIX"), "AF_UNIX required")
class FunctionalTest(unittest.TestCase):
    """netns なし。同一プロセスで echo listener・プロキシ・中継・クライアントを動かす。"""

    def setUp(self) -> None:
        try:
            self.dir = tempfile.mkdtemp(prefix="fer-", dir="/tmp")
            self.path = os.path.join(self.dir, rp.SOCK_NAME)
            self.echo_t, self.echo_i = rp.EchoListener("127.0.0.4"), rp.EchoListener("127.0.0.2")
        except OSError:
            self.skipTest("cannot set up temp dir or loopback aliases")
        self.proxy = rp.ConnectProxy(self.path, frozenset({("127.0.0.4", self.echo_t.port),
                                                           ("127.0.0.2", self.echo_i.port)}))
        for s in (self.echo_t, self.echo_i, self.proxy):
            s.start()
        self.relay = rp.Relay(self.path)

    def tearDown(self) -> None:
        self.relay.stop()
        for s in (self.echo_t, self.echo_i, self.proxy):
            s.stop()
        shutil.rmtree(self.dir, ignore_errors=True)

    def test_allowed_roundtrip(self) -> None:
        r = rp.via_relay(self.relay.port, f"127.0.0.4:{self.echo_t.port}", b"0123456789abcdef")
        self.assertEqual((r["status"], r["echo_ok"], r["bytes"]), (200, True, 16))
        self.assertEqual(self.proxy.snapshot()["upstream_ok"], 1)
        self.assertEqual(self.echo_t.connections, 1)

    def test_internal_denied(self) -> None:
        r = rp.via_relay(self.relay.port, f"127.0.0.2:{self.echo_i.port}")
        self.assertEqual(r["status"], 403)
        self.assertEqual(self.proxy.snapshot()["codes"], {"ip_denied": 1})
        self.assertEqual(self.echo_i.connections, 0)

    def test_uds_probe_counts_closed_early(self) -> None:
        self.assertTrue(rp.probe_uds(self.path)["ok"])
        for _ in range(50):
            if self.proxy.snapshot()["codes"].get("closed_early"):
                break
            time.sleep(0.05)
        self.assertEqual(self.proxy.snapshot()["codes"].get("closed_early"), 1)


class OuterTest(unittest.TestCase):
    def test_valid_out_dir_rejects_nonempty_and_relative(self) -> None:
        with tempfile.TemporaryDirectory() as d:
            rp.valid_out_dir(d)
            Path(d, "x").write_text("x")
            with self.assertRaises(ValueError):
                rp.valid_out_dir(d)
        with self.assertRaises(ValueError):
            rp.valid_out_dir("rel")

    def test_valid_ports(self) -> None:
        self.assertEqual(rp.valid_ports('{"target": 5000, "internal": 5001}'), {"target": 5000, "internal": 5001})
        for bad in ('{"target": 80, "internal": 5001}', '{"target": 5000}', "[1]",
                    '{"target": true, "internal": 5001}'):
            with self.assertRaises(ValueError):
                rp.valid_ports(bad)


if __name__ == "__main__":
    unittest.main()
