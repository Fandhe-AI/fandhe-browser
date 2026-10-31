"""seatbelt.py・OS 別モード（judge / page_templates）の自己テスト（Issue #1387・TASK-38・`RENDER-3`）。実 OS 非依存。"""

from __future__ import annotations

import sys
import unittest
from collections import Counter
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import judge  # noqa: E402
import measure  # noqa: E402
import page_templates  # noqa: E402
import seatbelt  # noqa: E402

PORTS = {"redirect": 2001, "img": 2002, "fetch": 2003, "ws": 2004, "internal": 2005, "udp_direct": 2006}
OK = dict(outer_count=0, control_count=1, attempt_observed=True, guard_unavailable=True, positive_ok=True)


class SeatbeltProfileTest(unittest.TestCase):
    def test_profile_embeds_only_the_port(self) -> None:
        self.assertEqual(
            seatbelt.isolated_profile(4321),
            '(version 1)(allow default)(deny network-outbound (remote ip))'
            '(allow network-outbound (remote ip "localhost:4321"))')

    def test_invalid_ports_are_rejected(self) -> None:
        for bad in (0, 80, 65536, "4321", True, None, 4321.0):
            with self.assertRaises(ValueError):
                seatbelt.isolated_profile(bad)

    def test_log_argv_is_fixed_and_start_is_validated(self) -> None:
        argv = seatbelt.log_show_argv("2026-10-31 12:00:00")
        self.assertEqual(argv[:3], ["/usr/bin/log", "show", "--style"])
        self.assertEqual(argv[-1], 'sender == "Sandbox"')
        with self.assertRaises(ValueError):
            seatbelt.log_show_argv("now; rm -rf /")


class DenyLogTest(unittest.TestCase):
    def test_counts_destinations_only(self) -> None:
        text = "\n".join([
            '{"eventMessage":"Sandbox: host(1) deny(1) network-outbound 127.0.0.2:2001"}',
            '{"eventMessage":"Sandbox: host(1) deny(1) network-outbound 127.0.0.2:2001"}',
            '{"eventMessage":"Sandbox: host(1) deny(1) network-outbound 127.0.0.3:2005"}',
            '{"eventMessage":"Sandbox: host(1) deny(1) file-read-data /etc/hosts"}',
            "not json at all",
        ])
        self.assertEqual(seatbelt.parse_denies(text), Counter({"127.0.0.2:2001": 2, "127.0.0.3:2005": 1}))

    def test_long_lines_and_bad_ports_are_ignored(self) -> None:
        long_line = "deny(1) network-outbound 127.0.0.2:2001 " + "x" * (seatbelt.MAX_LINE_LEN + 1)
        self.assertEqual(seatbelt.parse_denies(long_line), Counter())
        self.assertEqual(seatbelt.parse_denies("deny(1) network-outbound 127.0.0.2:99999"), Counter())

    def test_line_cap(self) -> None:
        line = "deny(1) network-outbound 127.0.0.2:2001\n"
        got = seatbelt.parse_denies(line * (seatbelt.MAX_LOG_LINES + 50))
        self.assertEqual(got["127.0.0.2:2001"], seatbelt.MAX_LOG_LINES)

    def test_attempts_from_denies(self) -> None:
        got = seatbelt.attempts_from_denies(Counter({"127.0.0.2:2001": 1}),
                                            {"redirect": "127.0.0.2:2001", "img": "127.0.0.2:2002"})
        self.assertEqual(got, {"redirect": True, "img": False})


class JudgeOsModesTest(unittest.TestCase):
    def test_monitor_not_required_passes_with_receipt_zero_and_attempt(self) -> None:
        self.assertEqual(judge.judge_route(**OK, monitor_required=False)[0], judge.PASS)

    def test_monitor_required_stays_unobservable(self) -> None:
        self.assertEqual(judge.judge_route(**OK)[0], judge.UNOBSERVABLE)

    def test_outer_receipt_fails_without_monitor(self) -> None:
        self.assertEqual(judge.judge_route(**{**OK, "outer_count": 2}, monitor_required=False)[0], judge.FAIL)

    def test_missing_attempt_is_unobservable_without_monitor(self) -> None:
        self.assertEqual(
            judge.judge_route(**{**OK, "attempt_observed": False}, monitor_required=False)[0], judge.UNOBSERVABLE)

    def test_mode_unavailable_is_always_unobservable(self) -> None:
        verdict, reason = judge.judge_route(**OK, monitor_required=False, mode_unavailable="no launcher")
        self.assertEqual((verdict, reason), (judge.UNOBSERVABLE, "no launcher"))

    def test_seatbelt_judge_all_has_eight_routes_and_structural_na(self) -> None:
        inner = {"positive_ok": True, "guard": {"unavailable": "n/a"},
                 "attempts": {r: True for r in judge.LISTENER_ROUTES}}
        control = {r: 1 for r in judge.LISTENER_ROUTES}
        out = judge.judge_all("seatbelt", inner, {}, control)
        self.assertEqual(len(out), 8)
        for r in judge.LISTENER_ROUTES:
            self.assertEqual(out[r]["verdict"], judge.PASS, r)
        for r in judge.STRUCTURAL_ROUTES:
            self.assertEqual(out[r]["verdict"], judge.UNOBSERVABLE, r)
            self.assertEqual(out[r]["reason"], judge.STRUCTURAL_NA_REASON)

    def test_seatbelt_leak_is_fail(self) -> None:
        inner = {"positive_ok": True, "guard": {"unavailable": "n/a"}, "attempts": {}}
        out = judge.judge_all("seatbelt", inner, {"fetch": 1}, {"fetch": 1})
        self.assertEqual(out["fetch"]["verdict"], judge.FAIL)

    def test_appcontainer_all_unobservable_with_reason(self) -> None:
        out = judge.judge_all("appcontainer", {}, {}, {}, mode_unavailable="needs FFI")
        self.assertEqual({v["verdict"] for v in out.values()}, {judge.UNOBSERVABLE})
        self.assertEqual({v["reason"] for v in out.values()}, {"needs FFI"})

    def test_platform_modes(self) -> None:
        self.assertEqual(judge.PLATFORM_MODES["darwin"], ("baseline", "seatbelt"))
        self.assertEqual(judge.PLATFORM_MODES["win32"], ("baseline", "appcontainer"))


class LiteralPagesTest(unittest.TestCase):
    def test_literal_pages_use_ip_destinations(self) -> None:
        pages = page_templates.build_pages(PORTS, literal=True)
        self.assertIn("http://127.0.0.2:2002/img", pages["/img"])
        self.assertIn("http://127.0.0.3:2005/internal", pages["/internal"])
        self.assertNotIn(".test", pages["/img"])
        self.assertEqual(page_templates.redirect_location(PORTS, literal=True), "http://127.0.0.2:2001/redirected")

    def test_default_pages_keep_host_names(self) -> None:
        self.assertIn("blocked.test", page_templates.build_pages(PORTS)["/img"])


class BuildArgvOsTest(unittest.TestCase):
    def _argv(self, mode: str, plat: str) -> list[str]:
        return measure.build_argv(mode, host=Path("/h"), ports=PORTS, out_dir=Path("/o"), hosts_file=Path("/x"),
                                  image=None, page_port=4321, platform_name=plat)

    def test_seatbelt_wraps_with_sandbox_exec(self) -> None:
        argv = self._argv("seatbelt", "darwin")
        self.assertEqual(argv[:2], ["/usr/bin/sandbox-exec", "-p"])
        self.assertIn("localhost:4321", argv[2])
        self.assertEqual(argv[argv.index("--page-port") + 1], "4321")

    def test_macos_baseline_uses_allow_default_profile(self) -> None:
        self.assertEqual(self._argv("baseline", "darwin")[2], seatbelt.BASELINE_PROFILE)

    def test_windows_baseline_is_direct(self) -> None:
        self.assertEqual(self._argv("baseline", "win32")[0], sys.executable)

    def test_unrunnable_mode_is_rejected(self) -> None:
        with self.assertRaises(ValueError):
            self._argv("appcontainer", "win32")


if __name__ == "__main__":
    unittest.main()
