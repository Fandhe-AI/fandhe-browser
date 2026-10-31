"""measure.py・in_sandbox.py・platform_probe.py の自己テスト（Issue #1377・TASK-38・`RENDER-3`・`SEC-2`）。

実ホスト・bwrap・docker は使わない。偽ホスト（`fixtures/fake_host.py`）を baseline 相当（隔離なし）で
駆動し、外側 listener の受信が判定の入力になる経路を通しで確かめる。
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
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
        return argparse.Namespace(mode=mode, host=str(host), ports=json.dumps(self.ports()), out=self.tmp / "o.json")

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


class MeasureTest(unittest.TestCase):
    def build(self, mode: str) -> list[str]:
        return measure.build_argv(mode, host=Path("/h"), ports={"img": 2001}, out_dir=Path("/o"),
                                  hosts_file=Path("/hosts"), image="img:tag")

    def test_baseline_differs_from_netns_only_by_unshare_net_and_output_name(self) -> None:
        base, iso = self.build("baseline"), self.build("bwrap-netns")
        self.assertNotIn("--unshare-net", base)
        self.assertEqual([a.replace("bwrap-netns", "baseline") for a in iso if a != "--unshare-net"], base)

    def test_docker_argv_is_network_none_and_fixed(self) -> None:
        argv = self.build("docker-none")
        self.assertEqual(argv[argv.index("--network") + 1], "none")
        self.assertIn(":/harness:ro", " ".join(argv))
        self.assertNotIn("--privileged", argv)

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
        recs, ports = measure.start_outer_listeners()
        try:
            self.assertEqual(set(ports), {"redirect", "img", "fetch", "ws", "internal", "udp_direct"})
            self.assertTrue(all(r.ip.startswith("127.") for r in recs.values()))
        finally:
            for r in recs.values():
                r.stop()

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


if __name__ == "__main__":
    unittest.main()
