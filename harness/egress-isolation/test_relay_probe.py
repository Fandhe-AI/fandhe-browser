"""relay_probe.py の自己テスト（Issue #1431・TASK-38・`RENDER-3`・`SEC-2`）。bwrap は使わない（loopback と UDS のみ）。"""

from __future__ import annotations

import copy
import os
import shutil
import socket
import sys
import tempfile
import threading
import time
import unittest
from unittest import mock
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
            b"CONNECT 127.0.0.4:5000\n HTTP/1.1\r\n\r\n": 400,  # authority 末尾の改行（#1433）
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
        return rp.build_relay_argv(netns=netns, sockdir="/tmp/fer-x", inner_dir="/tmp/out/inner",
                                   inner_out="/tmp/out/inner/o.json",
                                   ports={"target": 5000, "internal": 5001}, phase="netns")

    def test_netns_flag(self) -> None:
        self.assertIn("--unshare-net", self.args(True))
        self.assertNotIn("--unshare-net", self.args(False))

    def test_order_and_types(self) -> None:
        a = self.args(True)
        self.assertTrue(all(isinstance(x, str) for x in a))
        self.assertEqual(a[:2], ["bwrap", "--die-with-parent"])
        self.assertNotIn("--dev-bind", a)
        self.assertEqual(a[a.index("--ro-bind") : a.index("--ro-bind") + 3], ["--ro-bind", "/", "/"])
        self.assertEqual(a[a.index("--dev") : a.index("--dev") + 2], ["--dev", "/dev"])
        rw = a.index("--bind")
        ro = a.index(rp.MOUNT_RO) - 2
        self.assertEqual(a[rw + 2], rp.MOUNT_RW)
        self.assertEqual(a[ro], "--ro-bind")
        self.assertLess(a.index("--tmpfs"), rw)
        self.assertLess(a.index("--tmpfs"), ro)
        out = [k for k in range(len(a) - 2) if a[k] == "--bind" and a[k + 1] == "/tmp/out/inner"]
        self.assertEqual(len(out), 1)
        self.assertEqual(a[out[0] + 2], "/tmp/out/inner")
        self.assertGreater(out[0], rw)
        self.assertGreater(out[0], ro)
        self.assertGreater(out[0], a.index("--tmpfs"))
        # 親の out（result.json の置き場）は bind しない（#1442）
        self.assertNotIn("/tmp/out", a)

    def test_inner_out_must_be_under_inner_dir(self) -> None:
        for bad in ("/tmp/out/o.json", "/tmp/out/inner/sub/o.json", "/etc/o.json"):
            with self.assertRaises(ValueError, msg=bad):
                rp.build_relay_argv(netns=True, sockdir="/tmp/fer-x", inner_dir="/tmp/out/inner", inner_out=bad,
                                    ports={"target": 5000, "internal": 5001}, phase="netns")


class JudgeTest(unittest.TestCase):
    def test_all_pass(self) -> None:
        self.assertEqual(set(verdicts().values()), {"pass"})

    def test_missing_prerequisites_are_unobservable(self) -> None:
        for over in (dict(bwrap_found=False), dict(net_rc=1), dict(net_rc=None), dict(net=None),
                     dict(net={"precheck": {"ok": False, "reason": "r"}})):
            self.assertEqual(set(verdicts(**over).values()), {"unobservable"}, over)

    def test_direct_leak_fails(self) -> None:
        self.assertEqual(verdicts(outer_net={"target": 2, "internal": 0})["direct"], "fail")

    def test_direct_requires_explicit_failure_and_exact_zero(self) -> None:
        for tgt in (None, {}, {"errno": "X"}, {"ok": None}):
            net = copy.deepcopy(good_inputs()["net"])
            net["direct"]["target"] = tgt
            self.assertEqual(verdicts(net=net)["direct"], "unobservable", tgt)
        # 受信差が負（out_t < upstream_ok）は不整合
        p = {"codes": {"closed_early": 2, "allowed": 1, "ip_denied": 1}, "upstream_ok": 2}
        self.assertEqual(verdicts(proxy=p)["direct"], "unobservable")

    def test_direct_reason_keeps_errno_names(self) -> None:
        """direct の根拠欄に errno 名を残す。判定は変えない（#1442）。"""
        r = rp.judge_relay(**good_inputs())["direct"]
        self.assertEqual(r["verdict"], "pass")
        self.assertIn("target errno=ECONNREFUSED", r["reason"])
        self.assertIn("internal errno=ECONNREFUSED", r["reason"])
        net = copy.deepcopy(good_inputs()["net"])
        net["direct"]["target"] = {"ok": False, "errno": "ETIMEDOUT"}
        r = rp.judge_relay(**{**good_inputs(), "net": net})["direct"]
        self.assertEqual(r["verdict"], "pass")
        self.assertIn("target errno=ETIMEDOUT", r["reason"])
        net["direct"]["target"] = {"ok": False}
        r = rp.judge_relay(**{**good_inputs(), "net": net})["direct"]
        self.assertEqual(r["verdict"], "pass")
        self.assertIn("target errno=unknown", r["reason"])

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

    def test_non_linux_exits_2_without_uname(self) -> None:
        from unittest import mock
        with tempfile.TemporaryDirectory() as d, mock.patch.object(rp.sys, "platform", "win32"), \
                mock.patch.object(rp.os, "uname", side_effect=AttributeError, create=True):
            self.assertEqual(rp.run_outer(Path(d)), 2)
            self.assertTrue(Path(d, "result.json").exists())

    def test_valid_ports(self) -> None:
        self.assertEqual(rp.valid_ports('{"target": 5000, "internal": 5001}'), {"target": 5000, "internal": 5001})
        for bad in ('{"target": 80, "internal": 5001}', '{"target": 5000}', "[1]",
                    '{"target": true, "internal": 5001}'):
            with self.assertRaises(ValueError):
                rp.valid_ports(bad)


class _Recorder(socket.socket):
    """settimeout の呼び出しを記録するソケット。"""

    calls: list = []

    def settimeout(self, value) -> None:  # type: ignore[override]
        type(self).calls.append(value)
        super().settimeout(value)


class DeadlineTest(unittest.TestCase):
    """接続ごとの総時間の期限・conn の timeout・fd 例外（#1433）。"""

    def test_pump_stops_at_deadline_despite_drip(self) -> None:
        a_in, a = socket.socketpair()
        b_in, b = socket.socketpair()
        stop = threading.Event()

        def drip() -> None:
            while not stop.is_set():
                try:
                    a_in.send(b"x")
                except OSError:
                    return
                time.sleep(0.2)

        t = threading.Thread(target=drip, daemon=True)
        t.start()
        start = time.monotonic()
        try:
            rp.pump(a, b, deadline=start + 0.6)
            elapsed = time.monotonic() - start
        finally:
            stop.set()
            t.join(2)
            for x in (a_in, a, b_in, b):
                x.close()
        self.assertGreaterEqual(elapsed, 0.55)
        self.assertLess(elapsed, 2.1)

    def test_pump_closed_socket_returns(self) -> None:
        a, a2 = socket.socketpair()
        b, b2 = socket.socketpair()
        a.close()
        rp.pump(a, b)  # ValueError / OSError を握りつぶして返る
        for x in (a2, b, b2):
            x.close()

    @unittest.skipUnless(hasattr(socket, "AF_UNIX"), "AF_UNIX required")
    def test_proxy_closes_slow_header_connection(self) -> None:
        d = tempfile.mkdtemp(prefix="fer-t-", dir="/tmp")
        try:
            path = os.path.join(d, "p.sock")
            proxy = rp.ConnectProxy(path, ALLOW)
            with mock.patch.object(rp, "CONN_DEADLINE_S", 0.6):
                proxy.start()
                try:
                    c = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                    c.settimeout(5)
                    c.connect(path)
                    start = time.monotonic()
                    closed = False
                    while time.monotonic() - start < 4:
                        try:
                            c.send(b"C")
                        except OSError:
                            closed = True
                            break
                        time.sleep(0.2)
                        c.settimeout(0.01)
                        try:
                            if c.recv(1) == b"":
                                closed = True
                                break
                        except socket.timeout:
                            pass
                        except OSError:
                            closed = True
                            break
                    c.close()
                    time.sleep(0.3)
                    self.assertTrue(closed)
                    self.assertEqual(proxy.snapshot()["codes"].get("deadline"), 1)
                finally:
                    proxy.stop()
        finally:
            shutil.rmtree(d, ignore_errors=True)

    @unittest.skipUnless(hasattr(socket, "AF_UNIX"), "AF_UNIX required")
    def test_handle_restores_conn_timeout_before_pump(self) -> None:
        """ヘッダ読みで縮めた timeout が pump・_reply に残らない（#1442）。総時間の deadline は維持する。"""
        try:
            d = tempfile.mkdtemp(prefix="fer-t-", dir="/tmp")
            echo = rp.EchoListener("127.0.0.4")
        except OSError:
            self.skipTest("cannot set up temp dir or loopback alias")
        seen: dict = {}

        def record(a: socket.socket, b: socket.socket, **kw: object) -> None:
            seen["timeout"] = a.gettimeout()
            seen["deadline"] = kw.get("deadline")

        proxy = rp.ConnectProxy(os.path.join(d, "p.sock"), frozenset({("127.0.0.4", echo.port)}))
        client, server = socket.socketpair()
        try:
            echo.start()
            client.sendall(f"CONNECT 127.0.0.4:{echo.port} HTTP/1.1\r\n\r\n".encode("ascii"))
            with mock.patch.object(rp, "CONN_DEADLINE_S", 1.0), mock.patch.object(rp, "pump", side_effect=record):
                before = time.monotonic()
                proxy._sem.acquire()
                proxy._handle(server)  # finally で server を閉じ、semaphore を戻す
            self.assertEqual(seen.get("timeout"), rp.CONN_TIMEOUT_S)
            self.assertGreaterEqual(seen["deadline"], before + 1.0)
            self.assertLessEqual(seen["deadline"], time.monotonic() + 1.0)
        finally:
            client.close()
            server.close()
            echo.stop()
            shutil.rmtree(d, ignore_errors=True)

    @unittest.skipUnless(hasattr(socket, "AF_UNIX"), "AF_UNIX required")
    def test_relay_serve_sets_conn_timeout(self) -> None:
        relay = rp.Relay("/nonexistent/p.sock")
        try:
            a, b = socket.socketpair()
            _Recorder.calls = []
            wrapped = _Recorder(fileno=a.detach())
            relay._sem.acquire()
            relay._serve(wrapped)  # connect 失敗の経路でも設定される
            self.assertIn(rp.CONN_TIMEOUT_S, _Recorder.calls)
            b.close()
        finally:
            relay.stop()


if __name__ == "__main__":
    unittest.main()
