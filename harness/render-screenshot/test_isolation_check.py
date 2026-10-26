"""`isolation_check.py` の自己テスト（Issue #1281 / TASK-37 / RENDER-5）。

名前解決・接続・HTTP GET・`/proc/net/dev` をすべて偽物に差し替え、実ソケットは開かない。
"""

from __future__ import annotations

import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))

import capture_screenshots as cs  # noqa: E402
import isolation_check as ic  # noqa: E402

PROC_NET_DEV_LO_ONLY = (
    "Inter-|   Receive\n face |bytes packets\n    lo: 0 0 0 0\n"
)
PROC_NET_DEV_WITH_ETH0 = PROC_NET_DEV_LO_ONLY + "  eth0: 0 0 0 0\n"
URL = "http://pages.test:8080/a/index.html"


def _resolver(mapping: dict[str, list[str] | None]):  # noqa: ANN202
    def resolve(host: str, port: int) -> list[str]:
        result = mapping.get(host)
        if result is None:
            raise OSError(-2, "Name or service not known")
        return result

    return resolve


def _connector_fail(host: str, port: int, timeout: float) -> None:
    raise OSError(101, "Network is unreachable")


def _http_ok(url: str, timeout: float, max_bytes: int) -> tuple[int, int]:
    return 200, 10


class IsolationCheckTest(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.tmp = Path(self._tmp.name)
        self.proc = self.tmp / "dev"
        self.proc.write_text(PROC_NET_DEV_LO_ONLY, encoding="utf-8")

    def _run(self, urls: list[str] | None = None, **overrides):  # noqa: ANN003, ANN202
        kwargs = {
            "resolver": _resolver({"pages.test": ["127.0.0.1"]}),
            "connector": _connector_fail,
            "http_get": _http_ok,
            "proc_net_dev_path": self.proc,
        }
        kwargs.update(overrides)
        return ic.run_isolation_check(urls or [URL], pages_port=8080, **kwargs)

    def _failed(self, report: dict) -> list[str]:
        return [c["name"] for c in report["checks"] if not c["ok"]]

    def test_all_checks_pass(self) -> None:
        report = self._run()
        self.assertTrue(report["isolated"])
        self.assertEqual(report["schema_version"], 1)
        self.assertEqual(
            [c["name"] for c in report["checks"]],
            ["interfaces", "external_dns", "external_ipv4", "external_ipv6", "pages_host", "loopback_pages"],
        )
        self.assertEqual(self._failed(report), [])

    def test_external_ipv4_connection_success_fails(self) -> None:
        calls: list[tuple[str, int]] = []

        def connector(host: str, port: int, timeout: float) -> None:
            calls.append((host, port))
            if host == "169.254.169.254":
                return
            raise OSError(101, "x")

        report = self._run(connector=connector)
        self.assertFalse(report["isolated"])
        self.assertEqual(self._failed(report), ["external_ipv4"])
        self.assertIn(("1.1.1.1", 443), calls)

    def test_external_ipv6_connection_success_fails(self) -> None:
        def connector(host: str, port: int, timeout: float) -> None:
            if ":" in host:
                return
            raise OSError(101, "x")

        report = self._run(connector=connector)
        self.assertEqual(self._failed(report), ["external_ipv6"])

    def test_external_dns_resolution_fails_the_check(self) -> None:
        report = self._run(resolver=_resolver({"pages.test": ["127.0.0.1"], "example.com": ["93.184.216.34"]}))
        self.assertEqual(self._failed(report), ["external_dns"])

    def test_extra_interface_fails(self) -> None:
        self.proc.write_text(PROC_NET_DEV_WITH_ETH0, encoding="utf-8")
        report = self._run()
        self.assertEqual(self._failed(report), ["interfaces"])

    def test_missing_proc_is_undeterminable_and_fails(self) -> None:
        report = self._run(proc_net_dev_path=self.tmp / "missing")
        self.assertFalse(report["isolated"])
        self.assertEqual(self._failed(report), ["interfaces"])

    def test_pages_host_must_be_loopback(self) -> None:
        for addrs in (["10.0.0.5"], ["127.0.0.1", "10.0.0.5"], [], None):
            with self.subTest(addrs=addrs):
                report = self._run(resolver=_resolver({"pages.test": addrs}))
                self.assertEqual(self._failed(report), ["pages_host"])

    def test_pages_must_return_200_with_body(self) -> None:
        for response in ((404, 5), (200, 0)):
            with self.subTest(response=response):
                report = self._run(http_get=lambda u, t, m, r=response: r)
                self.assertEqual(self._failed(report), ["loopback_pages"])

    def test_partial_404_is_skipped_not_isolation_failure(self) -> None:
        urls = ["http://pages.test:8080/a/index.html", "http://pages.test:8080/b/index.html"]

        def http_get(url: str, timeout: float, max_bytes: int) -> tuple[int, int]:
            return (404, 5) if "/b/" in url else (200, 5)

        report = self._run(urls, http_get=http_get)
        self.assertTrue(report["isolated"])
        self.assertEqual(report["missing_urls"], [urls[1]])

    def test_page_timeout_fails(self) -> None:
        def http_get(url: str, timeout: float, max_bytes: int) -> tuple[int, int]:
            raise TimeoutError("timed out")

        report = self._run(http_get=http_get)
        self.assertEqual(self._failed(report), ["loopback_pages"])

    def test_report_does_not_leak_proxy_env_values(self) -> None:
        with mock.patch.dict(os.environ, {"HTTPS_PROXY": "http://user:secret@proxy.invalid:3128"}):
            report = self._run()
        self.assertEqual(report["environment"]["proxy_env_set"], ["HTTPS_PROXY"])
        self.assertNotIn("secret", json.dumps(report))


class IsolationCheckCliTest(unittest.TestCase):
    def test_exit_codes_and_report_written_without_following_symlink(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_str:
            tmp = Path(tmp_str)
            report_path = tmp / "report.json"
            sites = cs.DEFAULT_SAVED_PAGES_SITES

            ok_report = {"isolated": True, "checks": [{"name": "x", "ok": True, "detail": "d"}]}
            with mock.patch.object(ic, "run_isolation_check", return_value=ok_report):
                self.assertEqual(ic.main(["--sites", str(sites), "--report", str(report_path)]), 0)
            self.assertTrue(json.loads(report_path.read_text(encoding="utf-8"))["isolated"])

            ng_report = {"isolated": False, "checks": [{"name": "x", "ok": False, "detail": "d"}]}
            with mock.patch.object(ic, "run_isolation_check", return_value=ng_report):
                self.assertEqual(ic.main(["--sites", str(sites), "--report", str(tmp / "r2.json")]), 1)

            bad_sites = tmp / "bad.json"
            bad_sites.write_text("{}", encoding="utf-8")
            self.assertEqual(ic.main(["--sites", str(bad_sites), "--report", str(tmp / "r3.json")]), 2)
            self.assertEqual(ic.main(["--bogus"]), 2)

            # 既存の symlink を辿って上書きしない。
            target = tmp / "target.txt"
            target.write_text("keep", encoding="utf-8")
            link = tmp / "link.json"
            try:
                link.symlink_to(target)
            except OSError:
                self.skipTest("symlink not supported")
            with mock.patch.object(ic, "run_isolation_check", return_value=ok_report):
                self.assertEqual(ic.main(["--sites", str(sites), "--report", str(link)]), 2)
            self.assertEqual(target.read_text(encoding="utf-8"), "keep")


if __name__ == "__main__":
    unittest.main()
