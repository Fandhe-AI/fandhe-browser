"""judge.py・fixtures.py の自己テスト（Issue #1377・TASK-38・`RENDER-3`）。実ホスト・ネットワーク不要。"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import page_templates as fixtures  # noqa: E402
import judge  # noqa: E402

OK = dict(outer_count=0, control_count=1, attempt_observed=True, guard_unavailable=False, positive_ok=True)

DEV_LO = "Inter-|   Receive\n face |bytes\n    lo: 0 0\n"
ROUTE_EMPTY = "Iface\tDestination\tGateway\n"
V6_LO = "00000000000000000000000000000001 80 00000000000000000000000000000000 00 00000000000000000000000000000000 00000000 00000001 00000000 80200001       lo\n"


class JudgeRouteTest(unittest.TestCase):
    def test_all_conditions_met_is_pass(self) -> None:
        self.assertEqual(judge.judge_route(**OK)[0], judge.PASS)

    def test_outer_receipt_is_fail_even_if_positive_check_failed(self) -> None:
        self.assertEqual(judge.judge_route(**{**OK, "outer_count": 1, "positive_ok": False})[0], judge.FAIL)

    def test_baseline_control_zero_is_unobservable(self) -> None:
        self.assertEqual(judge.judge_route(**{**OK, "control_count": 0})[0], judge.UNOBSERVABLE)

    def test_no_attempt_is_unobservable(self) -> None:
        self.assertEqual(judge.judge_route(**{**OK, "attempt_observed": False})[0], judge.UNOBSERVABLE)

    def test_guard_unavailable_is_unobservable(self) -> None:
        self.assertEqual(judge.judge_route(**{**OK, "guard_unavailable": True})[0], judge.UNOBSERVABLE)

    def test_positive_check_failure_is_unobservable(self) -> None:
        self.assertEqual(judge.judge_route(**{**OK, "positive_ok": False})[0], judge.UNOBSERVABLE)


class JudgeAllTest(unittest.TestCase):
    def inner(self, **over: object) -> dict:
        base = {"positive_ok": True, "guard": {"unavailable": None},
                "attempts": {r: True for r in judge.LISTENER_ROUTES + judge.STRUCTURAL_ROUTES}}
        base.update(over)
        return base

    def test_isolated_mode_covers_all_routes_and_uses_baseline_control(self) -> None:
        control = {r: 1 for r in judge.LISTENER_ROUTES}
        out = judge.judge_all("bwrap-netns", self.inner(), {}, control)
        self.assertEqual(set(out), set(judge.LISTENER_ROUTES + judge.STRUCTURAL_ROUTES))
        self.assertTrue(all(v["verdict"] == judge.PASS for v in out.values()))

    def test_structural_route_uses_its_control_route(self) -> None:
        control = {r: 1 for r in judge.LISTENER_ROUTES}
        control["internal"] = 0
        out = judge.judge_all("docker-none", self.inner(), {}, control)
        self.assertEqual(out["internal_real"]["verdict"], judge.UNOBSERVABLE)
        self.assertEqual(out["internal"]["verdict"], judge.UNOBSERVABLE)

    def test_leak_on_one_route_is_fail_others_pass(self) -> None:
        control = {r: 1 for r in judge.LISTENER_ROUTES}
        out = judge.judge_all("bwrap-netns", self.inner(), {"ws": 2}, control)
        self.assertEqual(out["ws"]["verdict"], judge.FAIL)
        self.assertEqual(out["img"]["verdict"], judge.PASS)

    def test_failed_positive_check_makes_every_route_unobservable(self) -> None:
        control = {r: 1 for r in judge.LISTENER_ROUTES}
        out = judge.judge_all("bwrap-netns", self.inner(positive_ok=False), {}, control)
        self.assertTrue(all(v["verdict"] == judge.UNOBSERVABLE for v in out.values()))

    def test_guard_unavailable_is_unobservable(self) -> None:
        control = {r: 1 for r in judge.LISTENER_ROUTES}
        out = judge.judge_all("bwrap-netns", self.inner(guard={"unavailable": "x"}), {}, control)
        self.assertTrue(all(v["verdict"] == judge.UNOBSERVABLE for v in out.values()))


class ModeAllowlistTest(unittest.TestCase):
    def test_baseline_has_no_real_internal_or_dns_routes(self) -> None:
        self.assertEqual(judge.routes_for_mode("baseline"), judge.LISTENER_ROUTES)

    def test_unknown_mode_rejected(self) -> None:
        with self.assertRaises(ValueError):
            judge.routes_for_mode("host")

    def test_baseline_pages_never_name_real_addresses(self) -> None:
        for route in judge.routes_for_mode("baseline"):
            path = fixtures.ROUTE_PATH.get(route)
            if path is None or path == fixtures.REDIRECT_PATH:
                continue
            body = fixtures.build_pages({k: 2000 for k in ("redirect", "img", "fetch", "ws", "internal")})[path]
            for needle in (*judge.REAL_INTERNAL_V4, judge.REAL_INTERNAL_V6, judge.HOST_NXDOMAIN):
                self.assertNotIn(needle, body)

    def test_hosts_file_maps_only_loopback_addresses(self) -> None:
        for line in judge.HOSTS_FILE.splitlines():
            self.assertTrue(line.startswith("127.") or line.startswith("::1"), line)


class IsolationPrecheckTest(unittest.TestCase):
    def test_loopback_only_is_ok(self) -> None:
        self.assertTrue(judge.check_isolation(DEV_LO, ROUTE_EMPTY, V6_LO)[0])

    def test_unreadable_proc_aborts(self) -> None:
        self.assertFalse(judge.check_isolation(None, ROUTE_EMPTY, V6_LO)[0])
        self.assertFalse(judge.check_isolation(DEV_LO, None, V6_LO)[0])

    def test_empty_or_malformed_proc_aborts(self) -> None:
        self.assertFalse(judge.check_isolation("", "", "")[0])
        self.assertFalse(judge.check_isolation(DEV_LO, "garbage\n", V6_LO)[0])

    def test_missing_loopback_aborts(self) -> None:
        dev = "Inter-|   Receive\n face |bytes\n  eth0: 1 2\n"
        self.assertFalse(judge.check_isolation(dev, ROUTE_EMPTY, V6_LO)[0])

    def test_ipv6_absent_is_tolerated(self) -> None:
        self.assertTrue(judge.check_isolation(DEV_LO, ROUTE_EMPTY, None)[0])

    def test_extra_interface_aborts(self) -> None:
        dev = DEV_LO + "  eth0: 1 2\n"
        ok, why = judge.check_isolation(dev, ROUTE_EMPTY, V6_LO)
        self.assertFalse(ok)
        self.assertIn("eth0", why)

    def test_ipv4_default_route_aborts(self) -> None:
        route = ROUTE_EMPTY + "eth0\t00000000\t0102A8C0\n"
        self.assertFalse(judge.check_isolation(DEV_LO, route, V6_LO)[0])

    def test_ipv6_route_on_other_interface_aborts(self) -> None:
        v6 = V6_LO.replace("       lo", "     eth0")
        self.assertFalse(judge.check_isolation(DEV_LO, ROUTE_EMPTY, v6)[0])


class FixturesTest(unittest.TestCase):
    PORTS = {"redirect": 2001, "img": 2002, "fetch": 2003, "ws": 2004, "internal": 2005}

    def test_only_ports_are_substituted(self) -> None:
        pages = fixtures.build_pages(self.PORTS)
        self.assertIn("blocked.test:2002/img", pages["/img"])
        self.assertEqual(fixtures.redirect_location(self.PORTS), "http://blocked.test:2001/redirected")
        self.assertNotIn("$", "".join(pages.values()))

    def test_non_int_port_rejected(self) -> None:
        for bad in ("80; evil", 80, True, 70000, 1.5):
            with self.assertRaises(ValueError):
                fixtures.build_pages({**self.PORTS, "img": bad})

    def test_missing_port_rejected(self) -> None:
        with self.assertRaises(KeyError):
            fixtures.build_pages({"img": 2002})


if __name__ == "__main__":
    unittest.main()
