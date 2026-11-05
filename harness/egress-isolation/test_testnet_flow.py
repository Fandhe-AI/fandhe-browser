"""TEST-NET-1 宛て経路の判定・ページ・フラグのゲート・出力の自己テスト（Issue #1515）。

`judge.judge_testnet*`・`page_templates.build_testnet_pages`・`in_sandbox` の `--testnet`・`measure` の
`--appcontainer-testnet-observe` を対象にする。既存の loopback 経路の出力が変わらないことも確かめる。
"""

from __future__ import annotations

import argparse
import json
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from typing import Any
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import in_sandbox  # noqa: E402
import judge  # noqa: E402
import measure  # noqa: E402
import page_templates as fixtures  # noqa: E402
import testnet  # noqa: E402
import testnet_observe as to  # noqa: E402
from test_appcontainer import CONTROL, PORTS, FakeSession, _can_bind_alias, _inner, _main, _Rec  # noqa: E402
from test_testnet_observe import FakeWindows, observer, syn  # noqa: E402
from test_testnet_parse import DROP, SID, netevent  # noqa: E402

OK = dict(negative_packets=0, control_packets=1, wfp_drops_with_sid=1, observation_unavailable=None,
          cleanup_incomplete=None, positive_ok=True, origin_ok=True, binary_match=True)
GOOD_CLEANUP = {"pktmon_stopped": True, "pktmon_filters_removed": True, "netevents_restored": True, "raw_removed": True}


class JudgeTestnetRouteTest(unittest.TestCase):
    def test_all_conditions_met_is_pass(self) -> None:
        self.assertEqual(judge.judge_testnet_route(**OK)[0], judge.PASS)

    def test_packet_from_the_container_is_fail_even_when_observation_is_incomplete(self) -> None:
        for over in ({}, {"observation_unavailable": "x"}, {"cleanup_incomplete": "x"}, {"positive_ok": False},
                     {"control_packets": 0}, {"wfp_drops_with_sid": 0}, {"binary_match": False}):
            self.assertEqual(judge.judge_testnet_route(**{**OK, "negative_packets": 1, **over})[0], judge.FAIL, over)

    def test_each_missing_condition_is_unobservable(self) -> None:
        for over in ({"observation_unavailable": "pktmon start failed"}, {"cleanup_incomplete": "dirty"},
                     {"positive_ok": False}, {"control_packets": 0}, {"wfp_drops_with_sid": 0},
                     {"binary_match": False}):
            verdict, reason = judge.judge_testnet_route(**{**OK, **over})
            self.assertEqual(verdict, judge.UNOBSERVABLE, over)
            self.assertTrue(reason)

    def test_reasons_are_specific(self) -> None:
        self.assertIn("pktmon start failed", judge.judge_testnet_route(**{**OK, "observation_unavailable": "pktmon start failed"})[1])
        self.assertEqual(judge.judge_testnet_route(**{**OK, "control_packets": 0})[1], judge.TESTNET_NO_CONTROL_REASON)
        self.assertEqual(judge.judge_testnet_route(**{**OK, "wfp_drops_with_sid": 0})[1], judge.TESTNET_NO_DROP_REASON)


def _obs(neg: int = 0, ctl: int = 3, match: int = 2, drops: int | None = None) -> dict[str, Any]:
    pk: dict[str, Any] = {}
    wf: dict[str, Any] = {}
    for r in judge.TESTNET_ROUTES:
        pk[f"baseline/{r}"] = {"packets": {"tcp_syn": ctl}, "unique_flows": {"tcp_syn": 1 if ctl else 0}}
        if neg:
            pk[f"appcontainer/{r}"] = {"packets": {"tcp_syn": neg}, "unique_flows": {"tcp_syn": neg}}
        wf[f"appcontainer/{r}"] = {"drops": match if drops is None else drops, "drops_with_sid": match,
                                   "package_sid_match": match, "filter_names": {"100": "UWP Default Outbound Block Rule"}}
    return {"unavailable": None, "pktmon": {"routes": pk}, "wfp": {"routes": wf}}


class JudgeTestnetOriginTest(unittest.TestCase):
    """ホスト名オリジンが成立しないとき、img・fetch・ws だけが観測不能になる（Issue #1519）。"""

    def test_origin_not_ok_is_unobservable_with_reason(self) -> None:
        verdict, reason = judge.judge_testnet_route(**{**OK, "origin_ok": False})
        self.assertEqual((verdict, reason), (judge.UNOBSERVABLE, judge.TESTNET_ORIGIN_REASON))

    def test_negative_packets_still_fail_without_origin(self) -> None:
        self.assertEqual(judge.judge_testnet_route(**{**OK, "origin_ok": False, "negative_packets": 1})[0], judge.FAIL)

    def test_only_hostname_origin_routes_are_affected(self) -> None:
        v = judge.judge_testnet("appcontainer", observation=_obs(), positive_ok=True, origin_ok=False, binary_match=True)
        for r in ("testnet_img", "testnet_fetch", "testnet_ws"):
            self.assertEqual(v[r]["verdict"], judge.UNOBSERVABLE, r)
            self.assertEqual(v[r]["reason"], judge.TESTNET_ORIGIN_REASON, r)
        for r in ("testnet_redirect", "testnet_udp"):
            self.assertEqual(v[r]["verdict"], judge.PASS, r)

    def test_hosts_file_maps_only_the_page_host(self) -> None:
        self.assertEqual(judge.TESTNET_HOSTS_FILE, "127.0.0.1 pages.test\n")


class JudgeTestnetTest(unittest.TestCase):
    def test_all_routes_pass_with_negative_zero_drop_and_control(self) -> None:
        v = judge.judge_testnet("appcontainer", observation=_obs(), positive_ok=True, origin_ok=True, binary_match=True)
        self.assertEqual(list(v), list(judge.TESTNET_ROUTES))
        self.assertTrue(all(x["verdict"] == judge.PASS for x in v.values()))
        row = v["testnet_fetch"]
        self.assertEqual((row["negative_packets"], row["control_packets"], row["wfp_package_sid_match"]), (0, 3, 2))
        self.assertEqual(row["control_unique"], 1)
        self.assertEqual(row["filter_names"], ["UWP Default Outbound Block Rule"])
        self.assertEqual(row["method"], judge.TESTNET_METHOD)
        json.dumps(v)

    def test_leak_is_fail_and_others_stay_pass(self) -> None:
        obs = _obs()
        obs["pktmon"]["routes"]["appcontainer/testnet_ws"] = {"packets": {"tcp_syn": 1}, "unique_flows": {"tcp_syn": 1}}
        v = judge.judge_testnet("appcontainer", observation=obs, positive_ok=True, origin_ok=True, binary_match=True)
        self.assertEqual(v["testnet_ws"]["verdict"], judge.FAIL)
        self.assertEqual(v["testnet_img"]["verdict"], judge.PASS)

    def test_drop_without_matching_package_sid_is_not_evidence(self) -> None:
        v = judge.judge_testnet("appcontainer", observation=_obs(match=0, drops=4), positive_ok=True, origin_ok=True, binary_match=True)
        self.assertTrue(all(x["verdict"] == judge.UNOBSERVABLE for x in v.values()))
        self.assertEqual(v["testnet_img"]["wfp_drops"], 4)

    def test_no_control_packets_is_unobservable(self) -> None:
        v = judge.judge_testnet("appcontainer", observation=_obs(ctl=0), positive_ok=True, origin_ok=True, binary_match=True)
        self.assertTrue(all(x["verdict"] == judge.UNOBSERVABLE for x in v.values()))

    def test_not_requested_and_malformed_observation(self) -> None:
        v = judge.judge_testnet("appcontainer", observation=None, positive_ok=True, origin_ok=True, binary_match=True)
        self.assertTrue(all(x["reason"].endswith(judge.TESTNET_NOT_REQUESTED_REASON) for x in v.values()))
        bad = {"unavailable": 5, "pktmon": {}, "wfp": {}}
        v2 = judge.judge_testnet("appcontainer", observation=bad, positive_ok=True, origin_ok=True, binary_match=True)
        self.assertTrue(all(x["verdict"] == judge.UNOBSERVABLE for x in v2.values()))

    def test_cleanup_incomplete_blocks_pass(self) -> None:
        v = judge.judge_testnet("appcontainer", observation=_obs(), positive_ok=True, origin_ok=True, binary_match=True,
                                cleanup_incomplete="dirty")
        self.assertTrue(all(x["verdict"] == judge.UNOBSERVABLE for x in v.values()))

    def test_unattributed_packets_make_every_route_unobservable(self) -> None:
        # Issue #1515: 帰属できない TEST-NET 宛てパケット -> 全経路 unobservable。
        o = _obs()
        o["pktmon"]["routes"]["unattributed"] = {"packets": {"tcp_syn": 1}}
        v = judge.judge_testnet("appcontainer", observation=o, positive_ok=True, origin_ok=True, binary_match=True)
        for x in v.values():
            self.assertEqual(x["verdict"], judge.UNOBSERVABLE)
            self.assertIn(judge.TESTNET_UNATTRIBUTED_REASON, x["reason"])

    def test_unattributed_with_negative_packets_is_still_fail_on_that_route(self) -> None:
        o = _obs()
        o["pktmon"]["routes"]["unattributed"] = {"packets": {"udp": 2}}
        o["pktmon"]["routes"]["appcontainer/testnet_udp"] = {"packets": {"udp": 1}, "unique_flows": {"udp": 1}}
        v = judge.judge_testnet("appcontainer", observation=o, positive_ok=True, origin_ok=True, binary_match=True)
        self.assertEqual(v["testnet_udp"]["verdict"], judge.FAIL)
        self.assertEqual(v["testnet_fetch"]["verdict"], judge.UNOBSERVABLE)

    def test_existing_unavailable_wins_over_unattributed_and_wfp_unattributed_is_ignored(self) -> None:
        o = _obs()
        o["unavailable"] = "pktmon start failed"
        o["pktmon"]["routes"]["unattributed"] = {"packets": {"tcp_syn": 1}}
        v = judge.judge_testnet("appcontainer", observation=o, positive_ok=True, origin_ok=True, binary_match=True)
        self.assertIn("pktmon start failed", v["testnet_fetch"]["reason"])
        o2 = _obs()
        o2["wfp"]["routes"]["unattributed"] = {"drops": 4, "package_sid_match": 4}
        o2["pktmon"]["routes"]["unattributed"] = {"packets": {}}
        v2 = judge.judge_testnet("appcontainer", observation=o2, positive_ok=True, origin_ok=True, binary_match=True)
        self.assertTrue(all(x["verdict"] == judge.PASS for x in v2.values()))

    def test_loopback_confound_does_not_apply_to_testnet_routes(self) -> None:
        # 既存の judge_all は loopback の交絡で pass を降格するが、TEST-NET の判定はその影響を受けない。
        inner = {"positive_ok": True, "guard": {"unavailable": "n/a"}, "attempts": {"udp_direct": True},
                 "attempt_evidence": {"udp_direct": "errno"}}
        loop = judge.judge_all("appcontainer", inner, {k: 0 for k in CONTROL}, CONTROL,
                               loopback_confound=judge.LOOPBACK_EXEMPT_UNMEASURABLE_REASON)
        self.assertEqual(loop["udp_direct"]["verdict"], judge.UNOBSERVABLE)
        v = judge.judge_testnet("appcontainer", observation=_obs(), positive_ok=True, origin_ok=True, binary_match=True)
        self.assertEqual(v["testnet_udp"]["verdict"], judge.PASS)

    def test_existing_route_lists_are_unchanged(self) -> None:
        self.assertEqual(judge.routes_for_mode("baseline"), judge.LISTENER_ROUTES)
        self.assertEqual(judge.routes_for_mode("appcontainer"), judge.LISTENER_ROUTES + judge.STRUCTURAL_ROUTES)
        self.assertFalse(set(judge.TESTNET_ROUTES) & set(judge.routes_for_mode("appcontainer")))
        out = judge.judge_all("appcontainer", _inner(), {k: 0 for k in CONTROL}, CONTROL, loopback_confound=None)
        self.assertFalse(any(k.startswith("testnet") for k in out))


class TestnetPagesTest(unittest.TestCase):
    PORTS = testnet.testnet_ports("baseline")

    def test_pages_use_the_testnet_literal_and_per_route_ports(self) -> None:
        pages = fixtures.build_testnet_pages(self.PORTS)
        self.assertEqual(set(pages), {"/testnet_img", "/testnet_fetch", "/testnet_ws"})
        self.assertIn("http://192.0.2.1:47301/img", pages["/testnet_img"])
        self.assertIn("http://192.0.2.1:47302/fetch", pages["/testnet_fetch"])
        self.assertIn("ws://192.0.2.1:47303/ws", pages["/testnet_ws"])
        self.assertEqual(fixtures.testnet_redirect_location(self.PORTS), "http://192.0.2.1:47300/redirected")
        self.assertEqual(set(fixtures.TESTNET_ROUTE_PATH), set(testnet.TESTNET_TCP_ROUTES))
        for body in pages.values():
            self.assertNotIn("beacon", body)

    def test_bad_ports_are_rejected(self) -> None:
        with self.assertRaises(KeyError):
            fixtures.build_testnet_pages({"testnet_img": 47301})
        with self.assertRaises(ValueError):
            fixtures.build_testnet_pages({**self.PORTS, "testnet_img": "47301"})
        with self.assertRaises(ValueError):
            fixtures.testnet_redirect_location({**self.PORTS, "testnet_redirect": 80})

    def test_destination_guard_rejects_out_of_range_hosts(self) -> None:
        with self.assertRaises(ValueError):
            fixtures._checked_testnet("fetch('http://10.0.0.1:47301/x')")
        with self.assertRaises(ValueError):
            fixtures._checked_testnet("no destination here")

    def test_existing_pages_do_not_mention_testnet_destinations(self) -> None:
        ports = {k: 2001 + i for i, k in enumerate(("redirect", "img", "fetch", "ws", "internal", "udp_direct"))}
        for literal in (False, True):
            pages = fixtures.build_pages(ports, literal=literal)
            self.assertFalse(any("192.0.2." in b.replace(judge.IP_DNS_STANDIN, "") for b in pages.values()))
            self.assertFalse(any(p.startswith("/testnet") for p in pages))
        self.assertNotIn("testnet", fixtures.redirect_location(ports, literal=True))


class InSandboxGateTest(unittest.TestCase):
    def args(self, ports: dict[str, int], testnet_flag: bool) -> argparse.Namespace:
        base = {"redirect": 2001, "img": 2002, "fetch": 2003, "ws": 2004, "internal": 2005, "udp_direct": 2006}
        return argparse.Namespace(mode="appcontainer", host=sys.executable, ports=json.dumps({**base, **ports}),
                                  out=Path("o.json"), page_port=0, testnet=testnet_flag)

    def test_without_the_flag_testnet_ports_are_neither_required_nor_returned(self) -> None:
        got = in_sandbox.validate_args(self.args({}, False))
        self.assertFalse(any(k.startswith("testnet") for k in got))
        got2 = in_sandbox.validate_args(self.args(testnet.testnet_ports("baseline"), False))
        self.assertFalse(any(k.startswith("testnet") for k in got2))

    def test_flag_is_windows_only(self) -> None:
        with self.assertRaises(ValueError):
            with mock.patch.object(in_sandbox.sys, "platform", "linux"):
                in_sandbox.validate_args(self.args(testnet.testnet_ports("appcontainer"), True))

    def test_flag_requires_distinct_valid_ports_on_windows(self) -> None:
        with mock.patch.object(in_sandbox.sys, "platform", "win32"):
            got = in_sandbox.validate_args(self.args(testnet.testnet_ports("appcontainer"), True))
            self.assertEqual({k: got[k] for k in testnet.TESTNET_ROUTES}, testnet.testnet_ports("appcontainer"))
            ports = testnet.testnet_ports("appcontainer")
            for bad in ({}, {**ports, "testnet_img": 80}, {**ports, "testnet_ws": "x"}, {**ports, "testnet_ws": ports["testnet_img"]}):
                with self.assertRaises(ValueError, msg=str(bad)):
                    in_sandbox.validate_args(self.args(bad, True))

    def test_testnet_udp_probe_validates_the_destination_before_sending(self) -> None:
        with mock.patch.object(in_sandbox, "probe_udp", return_value="sent") as send:
            self.assertEqual(in_sandbox.probe_testnet_udp(47320), "sent")
            send.assert_called_once_with("192.0.2.1", 47320)
            with self.assertRaises(ValueError):
                in_sandbox.probe_testnet_udp(80)
            self.assertEqual(send.call_count, 1)

    def test_cli_flag_exists_and_defaults_off(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            with mock.patch.object(in_sandbox, "run") as run, mock.patch.object(in_sandbox, "validate_args") as va:
                va.return_value = {}
                run.return_value = {"positive_ok": True}
                in_sandbox.main(["--mode", "baseline", "--host", sys.executable, "--ports", "{}",
                                 "--out", str(Path(td) / "o.json")])
                self.assertFalse(va.call_args.args[0].testnet)
                in_sandbox.main(["--mode", "baseline", "--host", sys.executable, "--ports", "{}",
                                 "--out", str(Path(td) / "o.json"), "--testnet"])
                self.assertTrue(va.call_args.args[0].testnet)


class MeasureGateTest(unittest.TestCase):
    def test_validate_testnet_observe(self) -> None:
        measure.validate_testnet_observe(False, "linux", ["baseline"], False)
        measure.validate_testnet_observe(True, "win32", ["baseline", "appcontainer"], True)
        for plat, modes, exempt in (("linux", ["baseline", "appcontainer"], True), ("darwin", ["baseline", "seatbelt"], True),
                                    ("win32", ["baseline"], True), ("win32", ["baseline", "appcontainer"], False)):
            with self.assertRaises(ValueError, msg=f"{plat} {modes} {exempt}"):
                measure.validate_testnet_observe(True, plat, modes, exempt)

    def test_main_rejects_the_flag_on_a_non_windows_host(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            self.assertEqual(measure.main(["--host", sys.executable, "--out", td, "--appcontainer-testnet-observe"]), 2)

    def test_build_argv_adds_testnet_only_when_requested(self) -> None:
        kw: dict[str, Any] = dict(host=Path("C:/h.exe"), ports={"img": 2001}, out_dir=Path("C:/o"), hosts_file=Path("hosts"),
                                  image=None, page_port=4000, platform_name="win32")
        plain = measure.build_argv("baseline", **kw)
        self.assertNotIn("--testnet", plain)
        self.assertIn("--testnet", measure.build_argv("baseline", testnet_observe_on=True, **kw))
        self.assertEqual(plain, measure.build_argv("baseline", testnet_observe_on=False, **kw))

    def test_run_argv_default_timeout_is_unchanged(self) -> None:
        with mock.patch.object(measure, "_run_argv", return_value=(0, "")) as ra:
            measure.run_argv(["x"])
            ra.assert_called_once_with(["x"], measure.RUN_TIMEOUT_S)
            measure.run_argv(["x"], timeout_s=measure.RUN_TIMEOUT_S + measure.TESTNET_EXTRA_S)
            self.assertEqual(ra.call_args.args[1], 360)


@unittest.skipUnless(_can_bind_alias(), "127.0.0.2 is not bindable on this host")
class AppContainerTestnetModeTest(unittest.TestCase):
    def _run(self, session: FakeSession, **kw: Any) -> dict[str, Any]:
        recs = {k: _Rec() for k in PORTS}
        with tempfile.TemporaryDirectory() as td:
            return measure.run_appcontainer_mode(host=Path("C:/h.exe"), ports=PORTS, recs=recs, out_dir=Path(td),
                                                 page_port=4000, launcher=lambda **_: session, **kw)

    def test_flag_is_passed_and_timeout_extended_only_when_requested(self) -> None:
        seen: list[tuple[list[str], int | None]] = []
        s = FakeSession(main=_main(_inner()))
        orig = s.run

        def spy(script: str, args: list[str], **k: Any) -> dict[str, Any]:
            if script.endswith("in_sandbox.py"):
                seen.append((args, k.get("timeout_s")))
            return orig(script, args, **k)

        s.run = spy  # type: ignore[method-assign]
        self._run(s)
        self.assertNotIn("--testnet", seen[-1][0])
        self.assertIsNone(seen[-1][1])
        s2 = FakeSession(main=_main(_inner()))
        orig2 = s2.run

        def spy2(script: str, args: list[str], **k: Any) -> dict[str, Any]:
            if script.endswith("in_sandbox.py"):
                seen.append((args, k.get("timeout_s")))
            return orig2(script, args, **k)

        s2.run = spy2  # type: ignore[method-assign]
        self._run(s2, testnet_observe_on=True)
        self.assertIn("--testnet", seen[-1][0])
        self.assertEqual(seen[-1][1], measure.appcontainer.DEFAULT_TIMEOUT_S + measure.TESTNET_EXTRA_S)


class FinishAndSummaryTest(unittest.TestCase):
    def _results(self) -> dict[str, Any]:
        return {"meta": {"date_utc": "t", "kernel": "k", "host_sha256": "h", "host_version": "v", "proxy_env_present": False},
                "modes": {"baseline": {"outer_counts": CONTROL},
                          "appcontainer": {"outer_counts": {k: 0 for k in CONTROL}, "inner": _inner(),
                                           "appcontainer": {"profile": {"sid": SID}, "cleanup": {}}}}}

    def test_finish_collects_with_the_profile_sid_and_cleans_up(self) -> None:
        events = [netevent(DROP, "192.0.2.1", testnet.testnet_ports("appcontainer")[r]) for r in testnet.TESTNET_TCP_ROUTES]
        frames = [syn(5000 + i, testnet.testnet_ports("baseline")[r]) for i, r in enumerate(testnet.TESTNET_TCP_ROUTES)]
        fake = FakeWindows(pcap_frames=frames, events=events)
        obs = observer(fake)
        obs.start()
        out = measure.finish_testnet_observe(obs, self._results())
        self.assertEqual(out["cleanup"], GOOD_CLEANUP)
        self.assertIsNone(out["unavailable"])
        v = judge.judge_testnet("appcontainer", observation=out, positive_ok=True, origin_ok=True, binary_match=True)
        self.assertEqual(v["testnet_redirect"]["verdict"], judge.PASS)

    def test_finish_cleans_up_even_when_collection_raises(self) -> None:
        fake = FakeWindows()
        obs = observer(fake)
        obs.start()
        with mock.patch.object(obs, "stop_and_collect", side_effect=RuntimeError("boom")):
            out = measure.finish_testnet_observe(obs, self._results())
        self.assertIn("RuntimeError", out["unavailable"])
        self.assertEqual(out["cleanup"], GOOD_CLEANUP)
        self.assertFalse(fake.pm_running or fake.pm_filters or fake.netevents)

    def test_finish_reports_a_cleanup_exception_as_incomplete(self) -> None:
        obs = SimpleNamespace(unavailable=None, scaffold={}, diag={}, stop_and_collect=lambda sid: {"unavailable": None},
                              cleanup=mock.Mock(side_effect=OSError("x")))
        out = measure.finish_testnet_observe(obs, self._results())  # type: ignore[arg-type]
        self.assertEqual(measure.admin_restore_failures({"modes": {}, "testnet_observe": out}),
                         ["testnet_observe: pktmon_stopped is not true",
                          "testnet_observe: pktmon_filters_removed is not true",
                          "testnet_observe: netevents_restored is not true",
                          "testnet_observe: raw_removed is not true"])

    def test_raw_removed_failure_is_reflected_in_admin_restore_failures(self) -> None:
        out = {"cleanup": dict(GOOD_CLEANUP, raw_removed=False)}
        self.assertEqual(measure.admin_restore_failures({"modes": {}, "testnet_observe": out}),
                         ["testnet_observe: raw_removed is not true"])

    def test_record_testnet_cleanup_does_not_touch_appcontainer_cleanup(self) -> None:
        data = {"appcontainer": {"cleanup": {}}}
        measure.record_testnet_cleanup(data, GOOD_CLEANUP)
        self.assertEqual(data["appcontainer"]["cleanup"], {})
        self.assertEqual(data["appcontainer"]["testnet_cleanup"], GOOD_CLEANUP)
        self.assertIsNot(data["appcontainer"]["testnet_cleanup"], GOOD_CLEANUP)
        self.assertIn("not recorded", measure.appcontainer_cleanup_reason(data) or "")
        bare: dict[str, Any] = {}
        measure.record_testnet_cleanup(bare, GOOD_CLEANUP)
        self.assertEqual(bare, {"appcontainer": {"testnet_cleanup": GOOD_CLEANUP}})

    def test_admin_restore_failures_only_when_observed(self) -> None:
        self.assertEqual(measure.admin_restore_failures({"modes": {}}), [])
        ok = {"modes": {}, "testnet_observe": {"cleanup": GOOD_CLEANUP}}
        self.assertEqual(measure.admin_restore_failures(ok), [])
        bad = {"modes": {}, "testnet_observe": {"cleanup": {**GOOD_CLEANUP, "netevents_restored": False}}}
        self.assertEqual(measure.admin_restore_failures(bad), ["testnet_observe: netevents_restored is not true"])

    def test_summary_renders_the_testnet_table_and_scaffold(self) -> None:
        res = self._results()
        ac = res["modes"]["appcontainer"]
        ac["verdicts"] = {}
        ac["testnet_verdicts"] = judge.judge_testnet("appcontainer", observation=_obs(), positive_ok=True, origin_ok=True, binary_match=True)
        res["testnet_observe"] = {"unavailable": None, "scaffold": {"services_running": True}, "cleanup": GOOD_CLEANUP,
                                  "pktmon": {"routes": {"unattributed": {"packets": {"tcp_syn": 1}}}, "diag": {"frames": 5}},
                                  "wfp": {"routes": {}, "diag": {}}, "diag": {"pktmon_start_rc": 0}}
        text = measure.summarize(res)
        self.assertIn("TEST-NET-1 routes", text)
        self.assertIn("| testnet_redirect | pass |", text)
        self.assertIn("TEST-NET-1 observation", text)
        self.assertIn("unattributed", text)
        # 既存の profile 行は従来どおり。TEST-NET の節には SID の値を出さない。
        self.assertNotIn(SID, text.split("TEST-NET-1 routes", 1)[1])
        json.dumps(res)

    def test_summary_without_testnet_is_unchanged(self) -> None:
        res = self._results()
        res["modes"]["appcontainer"]["verdicts"] = {}
        self.assertNotIn("TEST-NET", measure.summarize(res))

    def test_caveat_split_keeps_the_combined_behaviour(self) -> None:
        data = {"appcontainer": {"preflight": None, "cleanup": {"profile_deleted": False}}}
        self.assertEqual(measure.appcontainer_confound_reason(data), judge.LOOPBACK_PREFLIGHT_MISSING_REASON)
        self.assertIn("cleanup was incomplete", measure.appcontainer_cleanup_reason(data) or "")
        self.assertEqual(measure.appcontainer_caveat(data), judge.LOOPBACK_PREFLIGHT_MISSING_REASON)
        data["appcontainer"]["preflight"] = {"nocap": {"probe": {}, "outer_tcp": 0, "outer_udp": 0},
                                             "netcap": {"probe": {}, "outer_tcp": 1, "outer_udp": 0}}
        self.assertIsNone(measure.appcontainer_confound_reason(data))
        self.assertIn("cleanup was incomplete", measure.appcontainer_caveat(data) or "")

    def test_empty_appcontainer_cleanup_is_not_masked_by_testnet_cleanup(self) -> None:
        # session 生成に失敗して cleanup が空でも、testnet の片付け結果（すべて True）で未記録が隠れない（Issue #1515）。
        data = {"appcontainer": {"cleanup": {}, "testnet_cleanup": dict(GOOD_CLEANUP)}}
        self.assertIn("not recorded", measure.appcontainer_cleanup_reason(data) or "")

    def test_testnet_cleanup_failure_gives_a_reason(self) -> None:
        bad = dict(GOOD_CLEANUP, netevents_restored=False)
        data = {"appcontainer": {"cleanup": {"profile_deleted": True}, "testnet_cleanup": bad}}
        self.assertIn("TEST-NET observation cleanup", measure.appcontainer_cleanup_reason(data) or "")
        data["appcontainer"]["testnet_cleanup"] = dict(GOOD_CLEANUP)
        self.assertIsNone(measure.appcontainer_cleanup_reason(data))


class TestnetOriginOkTest(unittest.TestCase):
    def test_requires_ok_and_page_host(self) -> None:
        good = {"testnet_origin_load": {"ok": True, "final_host": "pages.test"}}
        self.assertTrue(measure.testnet_origin_ok(good))
        for bad in (None, {}, {"testnet_origin_load": None},
                    {"testnet_origin_load": {"ok": False, "kind": "error"}},
                    {"testnet_origin_load": {"ok": True, "final_host": "127.0.0.1"}},
                    {"testnet_origin_load": {"ok": 1, "final_host": "pages.test"}}):
            self.assertFalse(measure.testnet_origin_ok(bad), bad)


if __name__ == "__main__":
    unittest.main()
