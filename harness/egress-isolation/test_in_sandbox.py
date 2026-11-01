"""measure.py・in_sandbox.py・platform_probe.py の自己テスト（Issue #1377・TASK-38・`RENDER-3`・`SEC-2`）。

実ホスト・bwrap・docker は使わない。偽ホスト（`fixtures/fake_host.py`）を baseline 相当（隔離なし）で
駆動し、外側 listener の受信が判定の入力になる経路を通しで確かめる。
"""

from __future__ import annotations

import argparse
import errno
import json
import os
import shutil
import socket
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import in_sandbox  # noqa: E402
import judge  # noqa: E402
import listeners  # noqa: E402
import measure  # noqa: E402
import monitor  # noqa: E402
import platform_probe  # noqa: E402


def make_fake_host(tmp: Path, connect: list[list[object]]) -> Path:
    script = (HERE / "fixtures" / "fake_host.py").read_text(encoding="utf-8").split("\n", 1)[1]
    path = tmp / "fake_host.py"
    path.write_text(f"#!{sys.executable}\n{script}", encoding="utf-8")
    path.chmod(0o755)
    (tmp / "fake_host.config.json").write_text(json.dumps({"connect": connect}), encoding="utf-8")
    return path


@unittest.skipUnless(sys.platform.startswith("linux"), "Linux only (egress_guard reads /proc)")
class InnerRunnerTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="egress-test-"))
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        patcher = mock.patch.object(in_sandbox, "SETTLE_S", 0.05)
        patcher.start()
        self.addCleanup(patcher.stop)

    def ports(self) -> dict[str, int]:
        return {k: 20000 + i for i, k in enumerate(("redirect", "img", "fetch", "ws", "internal", "udp_direct"))}

    def args(self, mode: str, host: Path) -> argparse.Namespace:
        return argparse.Namespace(mode=mode, host=str(host), ports=json.dumps(self.ports()), out=self.tmp / "o.json", page_port=0)

    def test_baseline_reaching_outer_listener_is_recorded(self) -> None:
        outer = listeners.TcpRecorder("o", "127.0.0.2").start()
        self.addCleanup(outer.stop)
        host = make_fake_host(self.tmp, [["127.0.0.2", outer.port]])
        result = in_sandbox.run(self.args("baseline", host), self.ports())
        self.assertTrue(result["positive_ok"])
        self.assertEqual(result["loopback_load"]["ok"], True)
        self.assertEqual(set(result["loads"]), {"redirect", "img", "fetch", "ws", "internal"})
        self.assertEqual(outer.count, 5)  # 経路ごとの load で 1 回ずつ到達（/ok では接続しない）
        self.assertNotIn("secret", json.dumps(result) + repr(outer.records()))
        # 想定内のダミー宛先への接続後も監視が続き、全経路で走査が完了している（EgressGuard なら最初の接続で止まる）
        self.assertIsNone(result["guard"]["violation"])
        self.assertIsNone(result["guard"]["unavailable"])
        self.assertTrue(all(n >= 1 for n in result["guard_polls"].values()), result["guard_polls"])
        self.assertEqual(set(result["guard_polls"]), {"redirect", "img", "fetch", "ws", "internal"})

    def test_diag_routes_run_after_judged_routes_in_order(self) -> None:
        """Issue #1415: --diag-routes は判定経路の後に D1・D2 の順で load し、loads / attempts には混ぜない。"""
        host = make_fake_host(self.tmp, [])
        ports = {**self.ports(), "diag_redirect_late": 20010, "diag_nav_direct": 20011}
        args = self.args("baseline", host)
        args.diag_routes = True
        args.ports = json.dumps(ports)
        self.assertEqual(in_sandbox.validate_args(args)["diag_nav_direct"], 20011)
        result = in_sandbox.run(args, in_sandbox.validate_args(args))
        self.assertEqual(list(result["diag_loads"]), ["diag_redirect_late", "diag_nav_direct"])
        self.assertEqual(set(result["loads"]), {"redirect", "img", "fetch", "ws", "internal"})
        self.assertEqual(set(result["load_elapsed_ms"]),
                         {"redirect", "img", "fetch", "ws", "internal", "diag_redirect_late", "diag_nav_direct"})
        self.assertTrue(all(isinstance(v, int) for v in result["load_elapsed_ms"].values()))
        self.assertNotIn("diag_nav_direct", result["attempts"])

    def test_without_diag_flag_result_has_no_diag_loads(self) -> None:
        host = make_fake_host(self.tmp, [])
        result = in_sandbox.run(self.args("baseline", host), self.ports())
        self.assertNotIn("diag_loads", result)
        args = self.args("baseline", host)
        args.diag_routes = True  # ポートが無ければ検証で拒否される
        with self.assertRaises(ValueError):
            in_sandbox.validate_args(args)

    def test_late_redirect_handler_returns_302_to_late_port(self) -> None:
        import page_templates

        loc = page_templates.diag_redirect_location({"diag_redirect_late": 20010}, literal=True)
        self.assertEqual(loc, f"http://{judge.IP_BLOCKED}:20010/redirected")
        loc_name = page_templates.diag_redirect_location({"diag_redirect_late": 20010})
        self.assertEqual(loc_name, f"http://{judge.HOST_BLOCKED}:20010/redirected")

    def test_host_that_never_connects_leaves_outer_empty(self) -> None:
        outer = listeners.TcpRecorder("o", "127.0.0.2").start()
        self.addCleanup(outer.stop)
        host = make_fake_host(self.tmp, [])
        result = in_sandbox.run(self.args("baseline", host), self.ports())
        self.assertEqual(outer.count, 0)
        verdicts = judge.judge_all("bwrap-netns", {**result, "attempts": {}}, {}, {"redirect": 1})
        self.assertTrue(all(v["verdict"] == judge.UNOBSERVABLE for v in verdicts.values()))

    def test_isolated_mode_aborts_without_loading_when_not_isolated(self) -> None:
        host = make_fake_host(self.tmp, [])
        dev = "h\nh\n    lo: 0\n  eth0: 0\n"
        with mock.patch.object(in_sandbox, "read_proc", side_effect=[dev, "Iface\n", ""]):
            result = in_sandbox.run(self.args("bwrap-netns", host), self.ports())
        self.assertTrue(result["aborted"])
        self.assertNotIn("loads", result)
        self.assertNotIn("selftest_rc", result)

    def test_proxy_env_value_is_never_recorded(self) -> None:
        host = make_fake_host(self.tmp, [])
        with mock.patch.dict(os.environ, {"HTTPS_PROXY": "http://user:pw@proxy.invalid:3128"}):
            result = in_sandbox.run(self.args("baseline", host), self.ports())
        self.assertIs(result["proxy_env_present"], True)
        self.assertNotIn("proxy.invalid", json.dumps(result))

    def test_validate_args_rejects_bad_input(self) -> None:
        host = make_fake_host(self.tmp, [])
        good = self.args("baseline", host)
        self.assertEqual(in_sandbox.validate_args(good)["img"], 20001)
        for patch in ({"mode": "host"}, {"host": "relative/host"}, {"host": str(self.tmp)},
                      {"ports": "not json"}, {"ports": json.dumps({"img": 2000})},
                      {"ports": json.dumps({**self.ports(), "img": "80; id"})},
                      {"ports": json.dumps({**self.ports(), "ws": 22})}):
            with self.assertRaises(ValueError, msg=str(patch)):
                in_sandbox.validate_args(argparse.Namespace(**{**vars(good), **patch}))


class HostArgvTest(unittest.TestCase):
    def test_py_host_runs_via_current_interpreter(self) -> None:
        self.assertEqual(in_sandbox.host_argv("/x/fake_host.py"), [sys.executable, "/x/fake_host.py"])
        self.assertEqual(in_sandbox.host_argv("/x/servo-host"), ["/x/servo-host"])


class DestinationMonitorTest(unittest.TestCase):
    """既知のダミー宛先への試行では監視が止まらず、後続の経路も走査されること（codex P1 の回帰）。"""

    def test_expected_destination_does_not_stop_monitoring(self) -> None:
        import time

        results = iter([["127.0.0.2:2001"], ["127.0.0.2:2001"], ["10.9.9.9:80"]])
        mon = monitor.DestinationMonitor(1, 80, {"127.0.0.2:2001"},
                                         scanner=lambda *_a: next(results, []), interval_s=0.001)
        mon.start()
        deadline = time.monotonic() + 5
        while mon.snapshot_polls() < 10 and time.monotonic() < deadline:
            time.sleep(0.005)
        mon.stop()
        self.assertGreaterEqual(mon.polls, 10)  # 最初の想定内の接続後も走査が続く
        self.assertEqual(mon.expected_seen, {"127.0.0.2:2001"})
        self.assertEqual(mon.violation, "10.9.9.9:80")
        self.assertIsNone(mon.unavailable)

    def test_scan_failure_sets_unavailable(self) -> None:
        def boom(*_a: object) -> list[str]:
            raise OSError("proc unreadable")

        mon = monitor.DestinationMonitor(1, 80, set(), scanner=boom, interval_s=0.001)
        mon.start()
        mon.stop()
        self.assertIn("proc unreadable", mon.unavailable or "")
        self.assertEqual(mon.polls, 0)


class DnsAttemptTest(unittest.TestCase):
    """resolv.conf が切れた環境の DNS 失敗を netns の効果と取り違えないこと（Bugbot Medium の回帰）。"""

    def test_broken_resolv_conf_is_not_an_observed_attempt(self) -> None:
        self.assertFalse(in_sandbox.dns_attempt_observed({"result": "gaierror:-3", "nameservers": {}}))

    def test_loopback_nameserver_is_not_an_observed_attempt(self) -> None:
        dns = {"result": "gaierror:-3", "nameservers": {"127.0.0.53": "ECONNREFUSED"}}
        self.assertFalse(in_sandbox.dns_attempt_observed(dns))

    def test_reachable_resolver_socket_is_not_an_observed_attempt(self) -> None:
        dns = {"result": "gaierror:-3", "nameservers": {"192.0.2.53": "ENETUNREACH"},
               "resolver_sockets": ["/run/nscd/socket"]}
        self.assertFalse(in_sandbox.dns_attempt_observed(dns))

    def test_nameserver_unreachable_is_an_observed_attempt(self) -> None:
        dns = {"result": "gaierror:-3", "nameservers": {"192.0.2.53": "ENETUNREACH"}, "resolver_sockets": []}
        self.assertTrue(in_sandbox.dns_attempt_observed(dns))

    def test_nameserver_connect_success_is_not_an_observed_attempt(self) -> None:
        dns = {"result": "gaierror:-3", "nameservers": {"192.0.2.53": "connected"}}
        self.assertFalse(in_sandbox.dns_attempt_observed(dns))


class MeasureTest(unittest.TestCase):
    def build(self, mode: str) -> list[str]:
        return measure.build_argv(mode, host=Path("/h"), ports={"img": 2001}, out_dir=Path("/o"),
                                  hosts_file=Path("/hosts"), image="img:tag", platform_name="linux")

    def test_baseline_differs_from_netns_only_by_unshare_net_and_output_name(self) -> None:
        base, iso = self.build("baseline"), self.build("bwrap-netns")
        self.assertNotIn("--unshare-net", base)
        self.assertEqual([a.replace("bwrap-netns", "baseline") for a in iso if a != "--unshare-net"], base)

    def test_docker_argv_is_network_none_and_fixed(self) -> None:
        argv = self.build("docker-none")
        self.assertEqual(argv[argv.index("--network") + 1], "none")
        self.assertIn(":/harness:ro", " ".join(argv))
        self.assertNotIn("--privileged", argv)

    def test_docker_container_is_named_and_removed_on_timeout_and_error(self) -> None:
        argv = measure.build_argv("docker-none", host=Path("/h"), ports={"img": 2001}, out_dir=Path("/o"),
                                  hosts_file=Path("/hosts"), image="img:tag", name="egress-iso-1", platform_name="linux")
        self.assertEqual(argv[argv.index("--name") + 1], "egress-iso-1")
        with mock.patch.object(measure, "remove_container") as rm, \
                mock.patch.object(measure, "_run_argv", return_value=(None, "timed out")):
            self.assertEqual(measure.run_argv(["docker"], container="c1"), (None, "timed out"))
        rm.assert_called_once_with("c1")
        with mock.patch.object(measure, "remove_container") as rm, \
                mock.patch.object(measure, "_run_argv", side_effect=OSError("x")):
            with self.assertRaises(OSError):
                measure.run_argv(["docker"], container="c2")
        rm.assert_called_once_with("c2")
        with mock.patch.object(measure, "remove_container") as rm, \
                mock.patch.object(measure, "_run_argv", return_value=(0, "")):
            measure.run_argv(["bwrap"])
        rm.assert_not_called()

    def test_bwrap_binds_fixed_resolv_conf_after_run_tmpfs_in_both_modes(self) -> None:
        with mock.patch.object(measure.os.path, "realpath", return_value="/run/systemd/resolve/stub-resolv.conf"):
            base = measure.build_argv("baseline", host=Path("/h"), ports={"img": 2001}, out_dir=Path("/o"),
                                      hosts_file=Path("/hosts"), image=None, resolv_file=Path("/rc"), platform_name="linux")
            iso = measure.build_argv("bwrap-netns", host=Path("/h"), ports={"img": 2001}, out_dir=Path("/o"),
                                     hosts_file=Path("/hosts"), image=None, resolv_file=Path("/rc"), platform_name="linux")
        for argv in (base, iso):
            i = argv.index(str(Path("/rc")))  # Windows ではパス区切りが変わる
            self.assertEqual(argv[i - 1], "--ro-bind")
            self.assertEqual(argv[i + 1], "/run/systemd/resolve/stub-resolv.conf")
            self.assertLess(argv.index("--tmpfs"), i)
        self.assertEqual([a.replace("bwrap-netns", "baseline") for a in iso if a != "--unshare-net"], base)

    def test_validation(self) -> None:
        for bad in ("", "UPPER", "a b", "x;rm", "$(id)", "-v/x:/y"):
            with self.assertRaises(ValueError, msg=bad):
                measure.validate_image(bad)
        self.assertEqual(measure.validate_image("repo/name:tag"), "repo/name:tag")
        with self.assertRaises(ValueError):
            measure.validate_host("relative")
        with self.assertRaises(ValueError):
            measure.validate_host("/nonexistent/host")

    def test_outer_listeners_are_loopback_only(self) -> None:
        # macOS の lo0 は既定で 127.0.0.1 のみ。alias 不在（EADDRNOTAVAIL）の macOS に限って skip する
        # （実測側は ensure_loopback_aliases が案内つきで止める）。Linux・Windows や想定外の bind エラーは失敗のまま。
        try:
            measure.ensure_loopback_aliases()
        except ValueError as exc:
            cause = exc.__cause__
            if (sys.platform == "darwin" and isinstance(cause, OSError)
                    and cause.errno == errno.EADDRNOTAVAIL):
                self.skipTest(str(exc))
            raise
        recs, ports = measure.start_outer_listeners()
        try:
            self.assertEqual(set(ports), {"redirect", "img", "fetch", "ws", "internal", "udp_direct"})
            self.assertTrue(all(r.ip.startswith("127.") for r in recs.values()))
        finally:
            for r in recs.values():
                r.stop()

    def test_outer_listeners_without_diag_keep_original_keys(self) -> None:
        recs, ports = measure.start_outer_listeners(diag=False)
        try:
            self.assertEqual(set(ports), {"redirect", "img", "fetch", "ws", "internal", "udp_direct"})
        finally:
            for r in recs.values():
                r.stop()

    def test_outer_listeners_with_diag_add_two_routes(self) -> None:
        recs, ports = measure.start_outer_listeners(diag=True)
        try:
            self.assertTrue({"diag_redirect_late", "diag_nav_direct"} <= set(ports))
            self.assertTrue(all(recs[r].ip == judge.IP_BLOCKED for r in ("diag_redirect_late", "diag_nav_direct")))
        finally:
            for r in recs.values():
                r.stop()

    def test_diag_flag_only_for_darwin_argv(self) -> None:
        host = Path(sys.executable)
        ports = {"redirect": 2001, "img": 2002, "fetch": 2003, "ws": 2004, "internal": 2005, "udp_direct": 2006}
        kw = dict(host=host, out_dir=Path("/tmp"), hosts_file=Path("/tmp/h"), image=None, page_port=3000)
        plain = measure.build_argv("baseline", ports=ports, platform_name="darwin", **kw)
        self.assertNotIn("--diag-routes", plain)
        with_diag = measure.build_argv("baseline", ports={**ports, "diag_redirect_late": 2007, "diag_nav_direct": 2008},
                                       platform_name="darwin", **kw)
        self.assertIn("--diag-routes", with_diag)
        win = measure.build_argv("baseline", ports={**ports, "diag_redirect_late": 2007, "diag_nav_direct": 2008},
                                 platform_name="win32", **kw)
        self.assertNotIn("--diag-routes", win)

    def test_summary_shows_redirect_diagnostics_and_warning(self) -> None:
        inner = {"loads": {"redirect": {"ok": False, "kind": "load_failed"}},
                 "diag_loads": {"diag_nav_direct": {"ok": True, "final_host": "x"}},
                 "load_elapsed_ms": {"redirect": 120}, "host_alive_after_load": {"redirect": True}}
        mode = {"exit_code": 0, "inner": inner, "outer_counts": {"redirect": 0, "diag_nav_direct": 1},
                "deny_log": {"dest_counts": {"127.0.0.2:2001": 3}, "diagnostic_dest_counts": {"redirect": 3, "diag_nav_direct": 0,
                                                                                         "diag_redirect_late": 0}},
                "verdicts": {}}
        results = {"meta": {"date_utc": "d", "kernel": "k", "host_sha256": "s", "host_version": "v",
                            "proxy_env_present": False},
                   "modes": {"baseline": {"outer_counts": {"img": 1, "diag_nav_direct": 1, "diag_redirect_late": 1}},
                             "seatbelt": mode}}
        text = measure.summarize(results)
        self.assertIn("redirect diagnostics (not verdicts)", text)
        self.assertIn("| redirect | 0 | 3 | load_failed |", text)
        self.assertIn("| diag_nav_direct | 1 | 0 |", text)
        self.assertIn("WARNING: diagnostic route diag_nav_direct has 1 outer receipt(s)", text)
        notes = measure.redirect_notes(mode)
        self.assertIn("redirect load kind=load_failed in 120 ms", notes["redirect"])

    def test_summary_lists_verdicts_without_urls(self) -> None:
        verdicts = judge.judge_all("bwrap-netns", {"positive_ok": True, "guard": {}, "attempts": {}}, {}, {})
        results = {"meta": {"date_utc": "d", "kernel": "k", "host_sha256": "s", "host_version": "v",
                            "proxy_env_present": False},
                   "modes": {"baseline": {"outer_counts": {"img": 1}},
                             "bwrap-netns": {"exit_code": 0, "inner": {}, "verdicts": verdicts}}}
        text = measure.summarize(results)
        self.assertIn("unobservable", text)
        self.assertNotIn("http", text)


class PlatformProbeTest(unittest.TestCase):
    def test_classify(self) -> None:
        self.assertEqual(platform_probe.classify(0, ""), "ok")
        self.assertEqual(platform_probe.classify(1, "unshare: Operation not permitted"), "EPERM")
        self.assertEqual(platform_probe.classify(127, ""), "not installed")
        self.assertEqual(platform_probe.classify(None, "timed out"), "timed out")
        self.assertEqual(platform_probe.classify(3, "?"), "failed(rc=3)")

    def test_landlock_abi_value_errno_and_unsupported_arch(self) -> None:
        self.assertEqual(platform_probe.landlock_abi(lambda *a: 8, "x86_64")["abi"], 8)
        calls: list[tuple[object, ...]] = []

        def fail(*a: object) -> int:
            calls.append(a)
            return -1

        info = platform_probe.landlock_abi(fail, "aarch64")
        self.assertIsNone(info["abi"])
        self.assertEqual(calls[0], (444, None, 0, 1))  # ルールセットは作らない（問い合わせのみ）
        info = platform_probe.landlock_abi(lambda *a: 8, "riscv64")
        self.assertIsNone(info["abi"])
        self.assertIn("unsupported", info["note"])

    def test_netns_capabilities_use_injected_runner(self) -> None:
        seen: list[list[str]] = []

        def run(argv: list[str]) -> tuple[int | None, str]:
            seen.append(argv)
            return (0, "") if argv[0] == "bwrap" and "ip" not in argv else (1, "Operation not permitted")

        caps = platform_probe.netns_capabilities(run)
        self.assertEqual(caps["bwrap --unshare-net"], "ok")
        self.assertEqual(caps["unshare -rn"], "EPERM")
        self.assertEqual(len(seen), 4)

    def test_docker_probe_rejects_bad_image(self) -> None:
        with self.assertRaises(ValueError):
            platform_probe.docker_probe("x; rm -rf /")

    def test_docker_landlock_query_parses_json_and_fails_closed(self) -> None:
        """docker の終了コード 125・126・127 を Landlock の errno と取り違えず、壊れた出力は unobservable にする。"""
        def probe(rc: int | None, out: str, err: str = "") -> str:
            return platform_probe.docker_probe(
                "img", run=lambda argv: (0, ""), run_out=lambda argv: (rc, out, err))["landlock ABI query"]

        self.assertEqual(probe(0, '{"ret": 8, "errno": 0}\n'), "ok")
        self.assertEqual(probe(0, '{"ret": -1, "errno": 38}\n'), "errno ENOSYS")
        for rc in (125, 126, 127, 1, 15):
            self.assertEqual(probe(rc, "", "docker: error"), f"unobservable: docker run exited with rc={rc}")
        self.assertEqual(probe(125, '{"ret": 8, "errno": 0}'), "unobservable: docker run exited with rc=125")
        self.assertEqual(probe(None, "", "timed out"), "unobservable: timed out")
        self.assertEqual(probe(0, ""), "unobservable: no single JSON line on stdout")
        self.assertEqual(probe(0, "not json"), "unobservable: malformed JSON on stdout")
        self.assertEqual(probe(0, "[1]"), "unobservable: unexpected JSON shape")
        self.assertEqual(probe(0, '{"ret": -1, "errno": 0}'), "unobservable: unexpected JSON shape")


class ProbeFailureTest(unittest.TestCase):
    """AF_INET6 等のソケット作成失敗でも結果 JSON が残り、判定が観測不能に倒れること（Bugbot 指摘・#1380）。"""

    def test_probe_connect_socket_creation_failure_is_unobservable(self) -> None:
        err = OSError(errno.EAFNOSUPPORT, "no ipv6")
        # 名前は OS ごとに異なる（Windows は WSAEAFNOSUPPORT）ため、実装と同じ表から引く。
        name = errno.errorcode[errno.EAFNOSUPPORT]
        with mock.patch.object(in_sandbox.socket, "socket", side_effect=err):
            self.assertEqual(in_sandbox.probe_connect(socket.AF_INET6, ("fd00::1", 80, 0, 0)),
                             f"unobservable:{name}")
            self.assertEqual(in_sandbox.probe_udp("127.0.0.3", 9), f"unobservable:{name}")
            self.assertEqual(in_sandbox.reachable_resolver_sockets(), [])

    def test_ipv6_disabled_does_not_count_as_attempt(self) -> None:
        def fake_connect(family: int, addr: tuple[object, ...]) -> str:
            # 実ネットワークへは出ない。IPv6 無効カーネルを模擬し、IPv4 は「経路なし」を返す。
            return "unobservable:EAFNOSUPPORT" if family == socket.AF_INET6 else "ENETUNREACH"

        tmp = Path(tempfile.mkdtemp(prefix="egress-test-"))
        self.addCleanup(shutil.rmtree, tmp, ignore_errors=True)
        host = make_fake_host(tmp, [])
        ports = {k: 20100 + i for i, k in enumerate(("redirect", "img", "fetch", "ws", "internal", "udp_direct"))}
        args = argparse.Namespace(mode="bwrap-netns", host=str(host), ports=json.dumps(ports), out=tmp / "o.json", page_port=0)
        self.assertIn("bwrap-netns", judge.ISOLATED_MODES)
        with mock.patch.object(in_sandbox, "SETTLE_S", 0.01), \
                mock.patch.object(in_sandbox.judge, "check_isolation", return_value=(True, "")), \
                mock.patch.object(in_sandbox, "probe_connect", side_effect=fake_connect), \
                mock.patch.object(in_sandbox, "reachable_resolver_sockets", return_value=[]), \
                mock.patch.object(in_sandbox, "probe_nameservers", return_value={"nameservers": {}}), \
                mock.patch.object(in_sandbox, "probe_dns", return_value={"result": "gaierror:-2", "elapsed_s": 0}):
            rc = in_sandbox.main(["--mode", "bwrap-netns", "--host", str(host),
                                  "--ports", json.dumps(ports), "--out", str(args.out)])
        self.assertEqual(rc, 0)
        data = json.loads(args.out.read_text(encoding="utf-8"))
        self.assertEqual(data["probes"]["internal_real"][judge.REAL_INTERNAL_V6], "unobservable:EAFNOSUPPORT")  # fake_connect の固定値
        self.assertFalse(data["attempts"]["internal_real"])
        verdicts = judge.judge_all("bwrap-netns", data, {"internal_real": 0}, {"internal": 1})
        self.assertEqual(verdicts["internal_real"]["verdict"], judge.UNOBSERVABLE)

    def test_unobservable_dns_probe_is_not_an_observed_attempt(self) -> None:
        dns = {"result": "unobservable:RuntimeError", "nameservers": {"192.0.2.53": "ENETUNREACH"}}
        self.assertFalse(in_sandbox.dns_attempt_observed(dns))
        # 対照: 同じ nameserver 観測でも解決が通常の失敗なら試行ありとなる
        self.assertTrue(in_sandbox.dns_attempt_observed({**dns, "result": "gaierror:-3"}))

    def test_run_with_raising_probe_dns_is_unobservable(self) -> None:
        tmp = Path(tempfile.mkdtemp(prefix="egress-test-"))
        self.addCleanup(shutil.rmtree, tmp, ignore_errors=True)
        host = make_fake_host(tmp, [])
        ports = {k: 20300 + i for i, k in enumerate(("redirect", "img", "fetch", "ws", "internal", "udp_direct"))}
        out = tmp / "o.json"
        with mock.patch.object(in_sandbox, "SETTLE_S", 0.01), \
                mock.patch.object(in_sandbox.judge, "check_isolation", return_value=(True, "")), \
                mock.patch.object(in_sandbox, "probe_connect", return_value="ENETUNREACH"), \
                mock.patch.object(in_sandbox, "probe_dns", side_effect=RuntimeError("boom")), \
                mock.patch.object(in_sandbox, "reachable_resolver_sockets", return_value=[]), \
                mock.patch.object(in_sandbox, "probe_nameservers",
                                  return_value={"nameservers": {"192.0.2.53": "ENETUNREACH"}}):
            rc = in_sandbox.main(["--mode", "bwrap-netns", "--host", str(host),
                                  "--ports", json.dumps(ports), "--out", str(out)])
        self.assertEqual(rc, 0)
        data = json.loads(out.read_text(encoding="utf-8"))
        self.assertEqual(data["probes"]["dns"]["result"], "unobservable:RuntimeError")
        self.assertFalse(data["attempts"]["dns"])
        verdicts = judge.judge_all("bwrap-netns", data, {"dns": 0}, {"udp_direct": 1})
        self.assertEqual(verdicts["dns"]["verdict"], judge.UNOBSERVABLE)

    def test_unhandled_exception_still_writes_unobservable_json(self) -> None:
        tmp = Path(tempfile.mkdtemp(prefix="egress-test-"))
        self.addCleanup(shutil.rmtree, tmp, ignore_errors=True)
        host = make_fake_host(tmp, [])
        ports = {k: 20200 + i for i, k in enumerate(("redirect", "img", "fetch", "ws", "internal", "udp_direct"))}
        out = tmp / "o.json"
        with mock.patch.object(in_sandbox, "run", side_effect=RuntimeError("boom")):
            rc = in_sandbox.main(["--mode", "baseline", "--host", str(host),
                                  "--ports", json.dumps(ports), "--out", str(out)])
        self.assertEqual(rc, 0)
        data = json.loads(out.read_text(encoding="utf-8"))
        self.assertEqual(data["fatal"], "RuntimeError: boom")
        self.assertIs(data["positive_ok"], False)
        verdicts = judge.judge_all("bwrap-netns", data, {}, {})
        self.assertEqual({v["verdict"] for v in verdicts.values()}, {judge.UNOBSERVABLE})


class MakePrivateWorkdirTest(unittest.TestCase):
    """OS 非依存（`sys.platform` と `os.mkdir` をモック）。Issue #1401・`RENDER-3`。"""

    FLAG = {in_sandbox.APPCONTAINER_FLAG_ENV: in_sandbox.APPCONTAINER_FLAG_VALUE}

    def run_make(self, platform: str, side_effect=None, env: dict[str, str] | None = None):
        # 環境変数も差し替える（既定は AppContainer フラグあり。無しの経路は env={} で指定する）
        env = self.FLAG if env is None else env
        with mock.patch.object(in_sandbox.sys, "platform", platform), \
                mock.patch.dict(in_sandbox.os.environ, env, clear=True), \
                mock.patch.object(in_sandbox.os, "mkdir", side_effect=side_effect) as mk:
            path = in_sandbox.make_private_workdir("egress-isolation-")
        return path, mk

    def test_win32_without_flag_keeps_protected_dacl_0o700(self) -> None:
        for env in ({}, {in_sandbox.APPCONTAINER_FLAG_ENV: "0"}, {in_sandbox.APPCONTAINER_FLAG_ENV: ""}):
            with self.subTest(env=env):
                path, mk = self.run_make("win32", env=env)
                mk.assert_called_once_with(path, 0o700)

    def test_flag_is_ignored_on_non_windows(self) -> None:
        for platform in ("linux", "darwin"):
            with self.subTest(platform=platform):
                path, mk = self.run_make(platform, env=self.FLAG)
                mk.assert_called_once_with(path, 0o700)

    def test_win32_with_flag_does_not_pass_mode_so_dacl_is_inherited(self) -> None:
        path, mk = self.run_make("win32")
        mk.assert_called_once_with(path)
        self.assertEqual(mk.call_args.args, (path,))
        self.assertEqual(mk.call_args.kwargs, {})
        self.assertRegex(path.name, r"^egress-isolation-[0-9a-f]{16}$")

    def test_non_windows_uses_0o700(self) -> None:
        for platform in ("linux", "darwin"):
            with self.subTest(platform=platform):
                path, mk = self.run_make(platform)
                mk.assert_called_once_with(path, 0o700)

    def test_retries_on_collision_without_exist_ok(self) -> None:
        for platform in ("win32", "linux"):
            with self.subTest(platform=platform):
                path, mk = self.run_make(platform, side_effect=[FileExistsError(), FileExistsError(), None])
                self.assertEqual(mk.call_count, 3)
                self.assertEqual(mk.call_args.args[0], path)
                names = {c.args[0].name for c in mk.call_args_list}
                self.assertEqual(len(names), 3)

    def test_gives_up_after_retry_limit(self) -> None:
        with self.assertRaises(FileExistsError):
            self.run_make("win32", side_effect=FileExistsError())

    def test_real_directory_is_created_and_writable(self) -> None:
        path = in_sandbox.make_private_workdir("egress-test-")
        self.addCleanup(shutil.rmtree, path, ignore_errors=True)
        (path / "sub").mkdir()
        self.assertTrue(path.is_dir())

    def test_describe_error_keeps_long_message(self) -> None:
        msg = "x" * 900
        self.assertEqual(in_sandbox.describe_error(PermissionError(msg)), f"PermissionError: {msg}")
        self.assertEqual(len(in_sandbox.describe_error(OSError("y" * 5000))), len("OSError: ") + 1000)

    def test_describe_error_masks_windows_user_path_and_keeps_type_and_errno(self) -> None:
        exc = PermissionError(13, "Access is denied", "C:\\Users\\alice\\AppData\\Local\\Temp\\x")
        self.assertEqual(in_sandbox.describe_error(exc),
                         "PermissionError: [Errno 13] Access is denied: "
                         "'C:\\\\Users\\\\<user>\\\\AppData\\\\Local\\\\Temp\\\\x'")
        raw = "[WinError 5] denied: 'C:\\Users\\alice\\AppData\\x'"
        self.assertEqual(in_sandbox.describe_error(OSError(raw)),
                         "OSError: [WinError 5] denied: 'C:\\Users\\<user>\\AppData\\x'")

    def test_describe_error_masks_unix_user_paths(self) -> None:
        self.assertEqual(in_sandbox.describe_error(FileNotFoundError(2, "No such file", "/Users/bob/x/y")),
                         "FileNotFoundError: [Errno 2] No such file: '/Users/<user>/x/y'")
        self.assertEqual(in_sandbox.describe_error(OSError("open /home/carol/.cache/z failed")),
                         "OSError: open /home/<user>/.cache/z failed")

    def test_describe_error_masks_whole_username_element_with_spaces(self) -> None:
        cases = [
            ("C:\\Users\\Alice Smith\\AppData\\Local\\Temp\\x", "C:\\Users\\<user>\\AppData\\Local\\Temp\\x"),
            ("C:\\\\Users\\\\Alice Smith\\\\AppData\\\\x", "C:\\\\Users\\\\<user>\\\\AppData\\\\x"),
            ("/Users/Alice Smith/x", "/Users/<user>/x"),
            ("/home/a b/x", "/home/<user>/x"),
            ("C:\\Users\\Alice Smith", "C:\\Users\\<user>"),
            ("'C:\\Users\\Alice Smith'", "'C:\\Users\\<user>"),
            ("\"/home/Alice Smith\" failed", "\"/home/<user>"),
        ]
        for raw, want in cases:
            with self.subTest(raw=raw):
                out = in_sandbox.describe_error(OSError(raw))
                self.assertEqual(out, f"OSError: {want}")
                self.assertNotIn("Alice", out)
                self.assertNotIn("Smith", out)

    def test_describe_error_masks_username_with_apostrophe(self) -> None:
        cases = [
            ("C:\\Users\\O'Brien\\AppData\\x", "C:\\Users\\<user>\\AppData\\x"),
            ("/home/O'Brien/x", "/home/<user>/x"),
            ("'C:\\Users\\O'Brien'", "'C:\\Users\\<user>"),
            ("C:\\Users\\'Brien\\x", "C:\\Users\\<user>\\x"),
        ]
        for raw, want in cases:
            with self.subTest(raw=raw):
                out = in_sandbox.describe_error(OSError(raw))
                self.assertEqual(out, f"OSError: {want}")
                self.assertNotIn("Brien", out)
                self.assertNotIn("O'", out)

    def test_describe_error_masks_delimited_element_with_quotes_and_spaces(self) -> None:
        # 環境の利用者名は無関係な値にし、パス規則だけで伏せられることを見る
        env = {"USER": "unrelated-account"}
        with mock.patch.dict(in_sandbox.os.environ, env, clear=True), \
                mock.patch.object(in_sandbox.getpass, "getuser", return_value="unrelated-account"):
            for raw, want in [("/home/O' Brien/cache", "/home/<user>/cache"),
                              ("/home/O''Brien/cache", "/home/<user>/cache"),
                              ("C:\\Users\\O' Brien\\x", "C:\\Users\\<user>\\x")]:
                with self.subTest(raw=raw):
                    out = in_sandbox.describe_error(OSError(raw))
                    self.assertEqual(out, f"OSError: {want}")
                    self.assertNotIn("Brien", out)

    def test_known_user_name_is_replaced_even_without_trailing_separator(self) -> None:
        env = {"USER": "O' Brien"}
        with mock.patch.dict(in_sandbox.os.environ, env, clear=True), \
                mock.patch.object(in_sandbox.getpass, "getuser", side_effect=OSError("no user")):
            self.assertEqual(in_sandbox.describe_error(OSError("open /home/O' Brien")), "OSError: open /home/<user>")
            # repr 形式（`'` が `\'` にエスケープされる）
            out = in_sandbox.describe_error(PermissionError(13, "denied", "O' Brien"))
            self.assertEqual(out, "PermissionError: [Errno 13] denied: \"<user>\"")
            out = in_sandbox.describe_error(OSError("path 'x\\'O\\' Brien' end"))
            self.assertNotIn("Brien", out)

    def mask_with_user(self, name: str, text: str) -> str:
        with mock.patch.dict(in_sandbox.os.environ, {"USER": name}, clear=True), \
                mock.patch.object(in_sandbox.getpass, "getuser", side_effect=OSError("no user")):
            return in_sandbox.mask_identity(text)

    def test_known_name_equal_to_path_marker_does_not_break_path_rule(self) -> None:
        for marker in ("Users", "home"):
            with self.subTest(marker=marker):
                self.assertEqual(self.mask_with_user(marker, "C:\\Users\\Alice\\x"), "C:\\Users\\<user>\\x")
                self.assertEqual(self.mask_with_user(marker, "/home/Alice/x"), "/home/<user>/x")

    def test_known_name_user_does_not_inflate_placeholder(self) -> None:
        out = self.mask_with_user("user", "C:\\Users\\Alice\\x for user")
        self.assertEqual(out, "C:\\Users\\<user>\\x for <user>")
        self.assertNotIn("<<", out)

    def test_known_name_is_not_replaced_inside_other_words(self) -> None:
        self.assertEqual(self.mask_with_user("rr", "[Errno 13] Error rr"), "[Errno 13] Error <user>")

    def test_short_known_name_does_not_leave_partial_replacement(self) -> None:
        out = self.mask_with_user("Ali", "C:\\Users\\Alice\\x and Alice")
        self.assertEqual(out, "C:\\Users\\<user>\\x and Alice")
        self.assertNotIn("<user>ce", out)

    def test_path_rule_masks_whole_element_even_for_known_name_prefix(self) -> None:
        cases = [("/home/Alice Smith/cache", "/home/<user>/cache"),
                 ("C:\\Users\\Alice.Smith\\AppData\\x", "C:\\Users\\<user>\\AppData\\x"),
                 ("/home/Alice-Smith/x", "/home/<user>/x")]
        for raw, want in cases:
            with self.subTest(raw=raw):
                out = self.mask_with_user("Alice", raw)
                self.assertEqual(out, want)
                self.assertNotIn("Smith", out)

    def test_trailing_element_is_masked_to_end_of_line(self) -> None:
        out = self.mask_with_user("unrelated", "'C:\\Users\\O' Brien'")
        self.assertEqual(out, "'C:\\Users\\<user>")
        self.assertNotIn("Brien", out)
        self.assertEqual(self.mask_with_user("unrelated", "a /home/x y z\nnext line"), "a /home/<user>\nnext line")

    def test_text_before_the_path_is_kept(self) -> None:
        out = self.mask_with_user("unrelated", "[WinError 5] Access is denied: 'C:\\Users\\bob smith\\x'")
        self.assertEqual(out, "[WinError 5] Access is denied: 'C:\\Users\\<user>\\x'")
        exc = PermissionError(13, "Access is denied", "/home/a b")
        with mock.patch.dict(in_sandbox.os.environ, {}, clear=True), \
                mock.patch.object(in_sandbox.getpass, "getuser", side_effect=OSError("n")):
            self.assertEqual(in_sandbox.describe_error(exc), "PermissionError: [Errno 13] Access is denied: '/home/<user>")

    def test_known_user_names_collects_env_sources_longest_first(self) -> None:
        env = {"USERNAME": "al", "USER": "alice", "LOGNAME": "x", "USERPROFILE": "C:\\Users\\bobby\\",
               "HOME": "/home/carol"}
        with mock.patch.dict(in_sandbox.os.environ, env, clear=True), \
                mock.patch.object(in_sandbox.getpass, "getuser", return_value="dave"):
            self.assertEqual(in_sandbox.known_user_names(), ["alice", "bobby", "carol", "dave", "al"])

    def test_known_user_names_ignores_getuser_failure(self) -> None:
        with mock.patch.dict(in_sandbox.os.environ, {}, clear=True), \
                mock.patch.object(in_sandbox.getpass, "getuser", side_effect=KeyError("x")):
            self.assertEqual(in_sandbox.known_user_names(), [])

    def test_describe_error_masks_apostrophe_username_in_repr_form(self) -> None:
        exc = PermissionError(13, "Access is denied", "C:\\Users\\O'Brien\\AppData\\x")
        out = in_sandbox.describe_error(exc)
        self.assertEqual(out, "PermissionError: [Errno 13] Access is denied: "
                              "\"C:\\\\Users\\\\<user>\\\\AppData\\\\x\"")
        self.assertNotIn("Brien", out)

    def test_describe_error_masks_repr_form_of_spaced_username(self) -> None:
        exc = PermissionError(13, "Access is denied", "C:\\Users\\Alice Smith\\AppData\\x")
        out = in_sandbox.describe_error(exc)
        self.assertEqual(out, "PermissionError: [Errno 13] Access is denied: 'C:\\\\Users\\\\<user>\\\\AppData\\\\x'")
        self.assertNotIn("Alice", out)

    def test_describe_error_username_element_is_length_bounded(self) -> None:
        out = in_sandbox.describe_error(OSError("/home/" + "a" * 100000 + "/x"))
        self.assertTrue(out.startswith("OSError: /home/<user>"))

    def test_describe_error_masks_sid(self) -> None:
        msg = "denied for S-1-15-2-3624051433-2125758914-1423191267 on C:\\Users\\dave\\t"
        self.assertEqual(in_sandbox.describe_error(OSError(msg)), "OSError: denied for <sid> on C:\\Users\\<user>\\t")

    def test_describe_error_masks_before_truncating(self) -> None:
        out = in_sandbox.describe_error(OSError("x" * 990 + " /home/zed/f"))
        self.assertNotIn("zed", out)
        self.assertEqual(len(out), len("OSError: ") + 1000)


if __name__ == "__main__":
    unittest.main()
