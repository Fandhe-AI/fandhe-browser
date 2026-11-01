"""seatbelt.py・OS 別モード（judge / page_templates）の自己テスト（Issue #1387・TASK-38・`RENDER-3`）。実 OS 非依存。"""

from __future__ import annotations

import sys
import threading
import unittest
import urllib.error
import urllib.request
import http.server
from collections import Counter
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import in_sandbox  # noqa: E402
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
        self.assertEqual(argv[-1], 'sender == "Sandbox" OR subsystem == "com.apple.sandbox.reporting"')
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

    def test_denies_are_attributed_to_target_pids(self) -> None:
        text = "\n".join([
            '{"eventMessage":"Sandbox: host(101) deny(1) network-outbound 127.0.0.2:2001"}',
            '{"eventMessage":"Sandbox: other(999) deny(1) network-outbound 127.0.0.2:2001"}',
            '{"eventMessage":"Sandbox: deny(1) network-outbound 127.0.0.3:2005"}',
        ])
        self.assertEqual(seatbelt.parse_denies(text, frozenset({101})), Counter({"127.0.0.2:2001": 1}))

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


def _record(msg: str, pid: int = 0) -> str:
    import json

    return json.dumps({"eventMessage": msg, "processID": pid})


class ScanLineTest(unittest.TestCase):
    """ndjson の解析・pid の帰属・診断カウンタ（Issue #1395）。"""

    def _scan(self, lines: list[str], pids: frozenset[int] | None) -> tuple[Counter[str], dict[str, int]]:
        diag = seatbelt.DenyDiagnostics()
        got: Counter[str] = Counter()
        for line in lines:
            dest = seatbelt.scan_line(line, pids, diag)
            if dest:
                got[dest] += 1
        return got, diag.as_dict()

    def test_pid_from_message_and_from_record(self) -> None:
        lines = [
            _record("Sandbox: host(101) deny(1) network-outbound 127.0.0.2:2001", 0),
            _record("deny(1) network-outbound 127.0.0.2:2002", 101),  # メッセージに pid が無い -> processID
            _record("Sandbox: other(999) deny(1) network-outbound 127.0.0.2:2003", 101),  # メッセージの pid を優先
            _record("deny(1) network-outbound 127.0.0.2:2004", 0),  # 帰属不能
        ]
        got, diag = self._scan(lines, frozenset({101}))
        self.assertEqual(got, Counter({"127.0.0.2:2001": 1, "127.0.0.2:2002": 1}))
        self.assertEqual(diag["deny_excluded_by_pid"], 2)
        self.assertEqual(diag["deny_attributed"], 2)

    def test_diagnostics_separate_stages(self) -> None:
        lines = [
            _record("Sandbox: host(101) deny(1) network-outbound 127.0.0.2:2001"),
            _record("Sandbox: host(101) deny(1) file-read-data /etc/hosts"),
            "Sandbox: host(101) deny(1) network-outbound 127.0.0.2:2002",  # JSON でない行は正規表現にフォールバック
            "{broken json deny(1)",
            _record("unrelated"),
        ]
        got, diag = self._scan(lines, frozenset({101}))
        self.assertEqual(got, Counter({"127.0.0.2:2001": 1, "127.0.0.2:2002": 1}))
        self.assertEqual(diag, {"scanned_lines": 5, "json_lines": 3, "sandbox_lines": 4,
                                "network_outbound_lines": 2, "deny_excluded_by_pid": 0, "deny_attributed": 2,
                                "unmatched_deny_unattributed": 1,
                                "unmatched_deny_samples": ["Sandbox: <proc>(<pid>) deny(1) file-read-data <path>"],
                                "unmatched_deny_samples_dropped": 0, "pid_excluded_samples": [],
                                "pid_excluded_samples_dropped": 0})

    def test_long_line_is_counted_but_not_parsed(self) -> None:
        long_line = _record("deny(1) network-outbound 127.0.0.2:2001 " + "x" * seatbelt.MAX_LINE_LEN, 101)
        got, diag = self._scan([long_line], frozenset({101}))
        self.assertEqual((got, diag["scanned_lines"], diag["network_outbound_lines"]), (Counter(), 1, 0))

    def test_diagnostics_hold_counts_only(self) -> None:
        _, diag = self._scan([_record("Sandbox: secret-proc(101) deny(1) network-outbound 127.0.0.2:2001")],
                             frozenset({101}))
        # 一致した帰属済みの行は本文を保持しない（サンプルは不一致・pid 除外の行だけ。Issue #1402）。
        self.assertEqual({k: v for k, v in diag.items() if not isinstance(v, int)},
                         {"unmatched_deny_samples": [], "pid_excluded_samples": []})
        self.assertNotIn("secret-proc", str(diag))


class SelfCheckTest(unittest.TestCase):
    UDP = "127.0.0.2:2006"

    def test_denied_probe_without_log_line_fails(self) -> None:
        self.assertEqual(seatbelt.self_check({}, "EPERM", self.UDP),
                         (False, "self-probe denial missing from deny log"))

    def test_denied_probe_with_log_line_passes(self) -> None:
        self.assertEqual(seatbelt.self_check({self.UDP: 1}, "EPERM", self.UDP), (True, "ok"))

    def test_probe_not_denied_cannot_be_checked(self) -> None:
        self.assertEqual(seatbelt.self_check({}, "sent", self.UDP), (True, "ok"))


class ComposeAttemptsTest(unittest.TestCase):
    DEST = {"redirect": "127.0.0.2:2001", "img": "127.0.0.2:2002"}
    UDP = "127.0.0.2:2006"

    def test_deny_log_does_not_overwrite_errno_evidence(self) -> None:
        attempts, evidence = seatbelt.compose_attempts(
            {"udp_direct": True}, {"udp_direct": "errno"}, {}, self.DEST, self.UDP)
        self.assertEqual(attempts["udp_direct"], True)
        self.assertEqual(evidence, {"udp_direct": "errno"})
        self.assertEqual((attempts["redirect"], attempts["img"]), (False, False))

    def test_deny_log_adds_evidence(self) -> None:
        attempts, evidence = seatbelt.compose_attempts(
            {}, {}, {"127.0.0.2:2001": 1, "127.0.0.2:2002": 2, self.UDP: 1},
            self.DEST, self.UDP)
        self.assertEqual((attempts["redirect"], attempts["udp_direct"]), (True, True))
        self.assertEqual(evidence, {"img": "deny_log", "redirect": "deny_log", "udp_direct": "deny_log"})


class BeaconTest(unittest.TestCase):
    """失敗経路のビーコン（Issue #1395）。"""

    def _serve(self) -> tuple[http.server.ThreadingHTTPServer, str]:
        in_sandbox._PageHandler.beacons = {}
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), in_sandbox._PageHandler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        return server, f"http://127.0.0.1:{server.server_address[1]}"

    def test_known_beacon_counts_and_unknown_is_404(self) -> None:
        _, base = self._serve()
        for name in ("img", "img", "fetch"):
            with urllib.request.urlopen(f"{base}/beacon/{name}") as resp:
                self.assertEqual(resp.status, 204)
        for bad in ("../etc/passwd", "unknown", "img/x", ""):
            with self.assertRaises(urllib.error.HTTPError) as ctx:
                urllib.request.urlopen(f"{base}/beacon/{bad}")
            self.assertEqual(ctx.exception.code, 404)
            ctx.exception.close()
        self.assertEqual(in_sandbox._PageHandler.beacons, {"img": 2, "fetch": 1})

    def test_literal_pages_send_beacon_only_in_failure_callbacks(self) -> None:
        pages = page_templates.build_pages(PORTS, literal=True)
        self.assertIn("onerror=\"new Image().src='/beacon/img';\"", pages["/img"])
        for path, route in (("/fetch", "fetch"), ("/internal", "internal")):
            self.assertIn(f".catch(function(){{new Image().src='/beacon/{route}';}})", pages[path])
        self.assertIn("w.onerror=function(){new Image().src='/beacon/ws';}", pages["/ws"])
        self.assertNotIn("/redirect-probe", pages)
        self.assertNotIn("redirect_fetch", "".join(pages.values()))
        for body in pages.values():
            self.assertNotIn("$", body)

    def test_default_pages_have_no_beacon(self) -> None:
        pages = page_templates.build_pages(PORTS)
        self.assertFalse(any("beacon" in body for body in pages.values()))


class JudgeBeaconEvidenceTest(unittest.TestCase):
    INNER = {"positive_ok": True, "guard": {"unavailable": "n/a"}}

    def test_beacon_only_is_unobservable(self) -> None:
        """ビーコンだけ（拒否ログなし）の経路は attempts が空なので pass にならない。"""
        inner = {**self.INNER, "attempts": {}, "beacons": {"img": 1}}
        out = judge.judge_all("seatbelt", inner, {}, {"img": 1})
        self.assertEqual(out["img"]["verdict"], judge.UNOBSERVABLE)
        self.assertEqual(out["img"]["reason"],
                         "no attempt was observed inside the sandbox (TCP attempt evidence is the sandbox "
                         "deny log only; failure beacons are not evidence)")
        self.assertNotIn("evidence", out["img"])

    def test_deny_log_attempt_passes_with_evidence(self) -> None:
        inner = {**self.INNER, "attempts": {"img": True}, "attempt_evidence": {"img": "deny_log"}}
        out = judge.judge_all("seatbelt", inner, {}, {"img": 1})
        self.assertEqual((out["img"]["verdict"], out["img"]["evidence"]), (judge.PASS, "deny_log"))

    def test_outer_receipt_wins_over_attempt(self) -> None:
        inner = {**self.INNER, "attempts": {"img": True}, "attempt_evidence": {"img": "deny_log"}}
        out = judge.judge_all("seatbelt", inner, {"img": 1}, {"img": 1})
        self.assertEqual(out["img"]["verdict"], judge.FAIL)


if __name__ == "__main__":
    unittest.main()
