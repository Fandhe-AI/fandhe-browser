"""testnet_observe.py の自己テスト（Issue #1515）。偽のコマンド実行で、開始・停止・片付けの順序と失敗時の記録を確かめる。

実 Windows・pktmon・netsh は使わない。偽物は pktmon の状態（動作中・filter）と WFP の netevents 設定を持ち、
片付けが状態を元へ戻したかを観測できる。
"""

from __future__ import annotations

import json
import struct
import sys
import unittest
import unittest.mock
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))
import judge  # noqa: E402
import testnet  # noqa: E402
import testnet_observe as to  # noqa: E402
from test_testnet_parse import DROP, NIC, SID, block, epb, eth, events_xml, idb, ipv4, netevent, shb  # noqa: E402

# run 42000257965（Windows Server 2025）の実機の出力の形。偽物もこの形で返し、パーサーの実機追従を回帰で守る。
PM_RUNNING_OUT = ("Collected Data:\n    Packet counters, packet capture\nCapture Type:\n    All packets\n"
                  "Monitored Components:\n    Network adapters\nPacket Filters:\n"
                  "    # Name    IP Address\n    - ----    ----------\n    1 testnet 192.0.2.0/24\n"
                  "Logger Parameters:\n    Logger name:        PktMon\n")
PM_STOPPED_OUT = "Packet Monitor is not running."
PM_FILTERS_OUT = ("Packet Filters:\n    # Name    IP Address\n    - ----    ----------\n"
                  "    1 testnet 192.0.2.0/24\n")
PM_NO_FILTERS_OUT = "Packet Filters:\n    None\n"

FILTERS_XML = ("<wfpdiag><filters><item><filterId>100</filterId><displayData>"
               "<name>UWP Default Outbound Block Rule</name></displayData></item></filters></wfpdiag>")


class FakeWindows:
    """pktmon / netsh / powershell の偽物。引数は呼び出しごとに `calls` へ残す。"""

    def __init__(self, **over: Any) -> None:
        self.calls: list[list[str]] = []
        self.pm_running = over.get("pm_running", False)
        self.pm_filters = over.get("pm_filters", False)
        self.netevents = over.get("netevents", False)
        self.fail = set(over.get("fail", ()))        # 失敗させる操作名
        self.restore_fail = over.get("restore_fail", False)  # off への set が効かない（復元の失敗）
        self.firewall = over.get("firewall", "True")
        self.status_out = over.get("status_out")
        self.pcap_frames: list[bytes] = over.get("pcap_frames", [])
        self.events: list[str] = over.get("events", [])
        self.bad_pcap = over.get("bad_pcap", False)
        self.pcap_bytes: bytes | None = over.get("pcap_bytes")
        self.stop_fail_first = over.get("stop_fail_first", False)  # 最初の stop だけ失敗（再試行は成功）
        # 診断コマンドの失敗の注入。採取前の status / filter list は開始の条件なので、採取中（pm_running）だけに効かせる。
        self.diag_raise = over.get("diag_raise", False)
        self.diag_rc_fail = over.get("diag_rc_fail", False)
        self.status_fail_after_stop = over.get("status_fail_after_stop", False)  # stop 後の status だけ rc=1
        self._stopped = False

    def _rc(self, name: str, out: str = "") -> dict[str, Any]:
        return {"rc": 1 if name in self.fail else 0, "out": out, "error": None}

    def __call__(self, args: list[str], *, cwd: Path | None = None, timeout: int = 60) -> dict[str, Any]:
        self.calls.append(args)
        exe = args[0].rsplit("\\", 1)[-1].lower()
        rest = args[1:]
        if exe == "powershell.exe":
            script = args[-1]
            if "Get-NetConnectionProfile" in script:
                return self._rc("ps", '{"NetworkCategory":"Public"}')
            if "Get-NetFirewallProfile" in script:
                return self._rc("ps", json.dumps([{"Name": "Public", "Enabled": self.firewall}]))
            return self._rc("ps", json.dumps([{"Name": "mpssvc", "Status": "Running"}, {"Name": "bfe", "Status": "Running"}]))
        if exe == "pktmon.exe":
            return self._pktmon(rest)
        if exe == "netsh.exe":
            return self._netsh(rest)
        raise AssertionError(f"unexpected executable: {args[0]}")

    def _pktmon(self, rest: list[str]) -> dict[str, Any]:
        sub = rest[0]
        if sub in ("status", "filter") and self.pm_running and (sub == "status" or rest[1] == "list"):
            if self.diag_raise:
                raise RuntimeError("injected diagnostic failure")
            if self.diag_rc_fail:
                return {"rc": 1, "out": "", "error": None}
        if sub == "status":
            if self.status_fail_after_stop and self._stopped:
                return {"rc": 1, "out": "", "error": None}
            return self._rc("status", self.status_out or (PM_RUNNING_OUT if self.pm_running else PM_STOPPED_OUT))
        if sub == "filter":
            op = rest[1]
            if op == "list":
                return self._rc("filter-list", PM_FILTERS_OUT if self.pm_filters else PM_NO_FILTERS_OUT)
            if op == "add":
                if "filter-add" not in self.fail:
                    self.pm_filters = True
                return self._rc("filter-add")
            if op == "remove":
                if "filter-remove" not in self.fail:
                    self.pm_filters = False
                return self._rc("filter-remove")
        if sub == "start":
            if "start" not in self.fail:
                self.pm_running = True
            return self._rc("start")
        if sub == "stop":
            self._stopped = True
            if self.stop_fail_first:
                self.stop_fail_first = False
                return {"rc": 1, "out": "", "error": None}
            if "stop" not in self.fail:
                self.pm_running = False
            return self._rc("stop")
        if sub == "etl2pcap":
            if "etl2pcap" not in self.fail:
                data = self.pcap_bytes if self.pcap_bytes is not None else b"\x01\x02" if self.bad_pcap else shb() + idb(1) + b"".join(epb(f) for f in self.pcap_frames)
                Path(rest[-1]).write_bytes(data)
            return self._rc("etl2pcap")
        raise AssertionError(f"unexpected pktmon args: {rest}")

    def _netsh(self, rest: list[str]) -> dict[str, Any]:
        assert rest[0] == "wfp", rest
        if rest[1:3] == ["show", "options"]:
            return self._rc("show-options", "Net events: ON" if self.netevents else "Net events: OFF")
        if rest[1:3] == ["set", "options"]:
            want = rest[3].split("=", 1)[1] == "on"
            if "set-options" not in self.fail and not (self.restore_fail and not want):
                self.netevents = want
            return self._rc("set-options")
        if rest[1:3] == ["show", "netevents"]:
            if "show-netevents" not in self.fail:
                Path(rest[3].split("=", 1)[1]).write_text(events_xml_text(self.events), encoding="utf-8")
            return self._rc("show-netevents")
        if rest[1:3] == ["show", "filters"]:
            Path(rest[3].split("=", 1)[1]).write_text(FILTERS_XML, encoding="utf-8")
            return self._rc("show-filters")
        raise AssertionError(f"unexpected netsh args: {rest}")

    def names(self) -> list[str]:
        out = []
        for a in self.calls:
            exe = a[0].rsplit("\\", 1)[-1].lower()
            out.append(" ".join([exe, *a[1:3]]) if exe != "powershell.exe" else "powershell")
        return out


def events_xml_text(items: list[str]) -> str:
    return events_xml(*items).getvalue().decode()


def observer(fake: FakeWindows) -> to.TestnetObserver:
    return to.TestnetObserver(sysroot="C:\\Windows", run=fake)


def syn(sport: int, dport: int) -> bytes:
    return eth(ipv4(NIC, sport, "192.0.2.1", dport, "tcp_syn"))


class HappyPathTest(unittest.TestCase):
    def test_collects_counts_restores_state_and_removes_raw_data(self) -> None:
        fake = FakeWindows(
            pcap_frames=[syn(5000, 47300), syn(5000, 47300), syn(5001, 47301)],          # baseline のみ NIC に出た
            events=[netevent(DROP, "192.0.2.1", 47316, fid="100"), netevent(DROP, "192.0.2.1", 47317, fid="100")])
        obs = observer(fake)
        raw = obs.workdir
        obs.start()
        self.assertIsNone(obs.unavailable)
        self.assertTrue(fake.pm_running and fake.pm_filters and fake.netevents)
        result = obs.stop_and_collect(SID)
        cleanup = obs.cleanup()
        self.assertIsNone(result["unavailable"])
        self.assertEqual(result["pktmon"]["routes"]["baseline/testnet_redirect"]["packets"], {"tcp_syn": 2})
        self.assertEqual(result["pktmon"]["routes"]["baseline/testnet_redirect"]["unique_flows"], {"tcp_syn": 1})
        self.assertEqual(result["wfp"]["routes"]["appcontainer/testnet_redirect"]["package_sid_match"], 1)
        self.assertEqual(result["wfp"]["routes"]["appcontainer/testnet_redirect"]["filter_names"],
                         {"100": "UWP Default Outbound Block Rule"})
        self.assertEqual(cleanup, {"pktmon_stopped": True, "pktmon_filters_removed": True,
                                   "netevents_restored": True, "raw_removed": True})
        self.assertFalse(raw.exists())
        self.assertFalse(fake.pm_running or fake.pm_filters or fake.netevents)
        self.assertIsNone(to.cleanup_reason(cleanup))
        json.dumps(result)

    def test_shape_diagnostics_are_recorded_and_reach_the_summary(self) -> None:
        # Issue #1515: 次の実測で実機の出力形を確かめるための診断。判定には使わず、cleanup 後の diag も summary に届く。
        import measure  # noqa: PLC0415
        fake = FakeWindows(pcap_frames=[syn(5000, 47300)], events=[netevent(DROP, "192.0.2.1", 47316, fid="100")])
        obs = observer(fake)
        obs.start()
        measure_obs = measure.finish_testnet_observe(obs, {"modes": {}})
        for key in ("pktmon_status_running_shape", "pktmon_filter_list_after_add_shape",
                    "pktmon_status_after_stop_shape", "pktmon_filter_list_after_remove_shape"):
            self.assertIsInstance(measure_obs["diag"].get(key), list, key)
        self.assertEqual(measure_obs["diag"]["pktmon_filter_list_after_remove_shape"],
                         ["Packet Filters:", "None"])
        text = "\n".join(measure._testnet_observe_block(measure_obs))
        self.assertIn("pktmon_filter_list_after_remove_shape", text)

    def test_end_to_end_judgement_pass_and_fail(self) -> None:
        # 陰性（appcontainer）は NIC 0 件・SID つき drop あり、陽性（baseline）は NIC に出た -> 全 TCP 経路 pass。
        frames = [syn(5000 + i, testnet.testnet_ports("baseline")[r]) for i, r in enumerate(testnet.TESTNET_TCP_ROUTES)]
        events = [netevent(DROP, "192.0.2.1", testnet.testnet_ports("appcontainer")[r]) for r in testnet.TESTNET_TCP_ROUTES]
        fake = FakeWindows(pcap_frames=frames, events=events)
        obs = observer(fake)
        obs.start()
        result = obs.stop_and_collect(SID)
        cleanup = obs.cleanup()
        verdicts = judge.judge_testnet("appcontainer", observation=result, positive_ok=True, origin_ok=True, binary_match=True,
                                       cleanup_incomplete=to.cleanup_reason(cleanup))
        for r in testnet.TESTNET_TCP_ROUTES:
            self.assertEqual(verdicts[r]["verdict"], judge.PASS, verdicts[r]["reason"])
        # UDP は baseline の陽性パケットも drop も無いので pass にしない。
        self.assertEqual(verdicts["testnet_udp"]["verdict"], judge.UNOBSERVABLE)
        # 陰性でパケットが出たら fail。
        leak = frames + [syn(6000, testnet.testnet_ports("appcontainer")["testnet_img"])]
        fake2 = FakeWindows(pcap_frames=leak, events=events)
        o2 = observer(fake2)
        o2.start()
        r2 = o2.stop_and_collect(SID)
        o2.cleanup()
        v2 = judge.judge_testnet("appcontainer", observation=r2, positive_ok=True, origin_ok=True, binary_match=True)
        self.assertEqual(v2["testnet_img"]["verdict"], judge.FAIL)
        self.assertEqual(v2["testnet_redirect"]["verdict"], judge.PASS)

    def test_only_approved_commands_are_used(self) -> None:
        fake = FakeWindows()
        obs = observer(fake)
        obs.start()
        obs.stop_and_collect(SID)
        obs.cleanup()
        allowed_pktmon = {"status", "filter", "start", "stop", "etl2pcap"}
        for a in fake.calls:
            exe = a[0].rsplit("\\", 1)[-1].lower()
            self.assertTrue(a[0].startswith("C:\\Windows\\System32\\"), a[0])
            if exe == "pktmon.exe":
                self.assertIn(a[1], allowed_pktmon)
            elif exe == "netsh.exe":
                self.assertEqual(a[1], "wfp")
                self.assertIn(a[2], {"show", "set"})
                self.assertNotIn("capture", a)
            else:
                self.assertEqual(exe, "powershell.exe")


class DiagnosticFailureTest(unittest.TestCase):
    """診断（形の記録）の失敗が観測と片付けを止めないこと。判定に使う値の算出は fail-closed のまま。"""

    SHAPE_KEYS = ("pktmon_status_running_shape", "pktmon_filter_list_after_add_shape")

    def test_diag_command_exception_does_not_stop_the_observation(self) -> None:
        fake = FakeWindows(diag_raise=True, pcap_frames=[syn(5000, 47300)])
        obs = observer(fake)
        obs.start()
        self.assertIsNone(obs.unavailable)
        for key in self.SHAPE_KEYS:
            self.assertEqual(obs.diag[key], ["unavailable: RuntimeError"], key)
        result = obs.stop_and_collect(SID)
        cleanup = obs.cleanup()
        self.assertIsNone(result["unavailable"])
        self.assertEqual(result["pktmon"]["routes"]["baseline/testnet_redirect"]["packets"], {"tcp_syn": 1})
        self.assertTrue(all(cleanup.values()))

    def test_diag_command_rc_failure_does_not_stop_the_observation(self) -> None:
        fake = FakeWindows(diag_rc_fail=True)
        obs = observer(fake)
        obs.start()
        self.assertIsNone(obs.unavailable)
        for key in self.SHAPE_KEYS:
            self.assertIsInstance(obs.diag[key], list, key)
        result = obs.stop_and_collect(SID)
        cleanup = obs.cleanup()
        self.assertIsNone(result["unavailable"])
        self.assertTrue(all(cleanup.values()))

    def test_shape_exception_in_cleanup_does_not_stop_the_cleanup(self) -> None:
        fake = FakeWindows()
        obs = observer(fake)
        obs.start()
        obs.stop_and_collect(SID)
        with unittest.mock.patch.object(to, "_shapes", side_effect=ValueError("boom")):
            cleanup = obs.cleanup()
        self.assertEqual(cleanup, {"pktmon_stopped": True, "pktmon_filters_removed": True,
                                   "netevents_restored": True, "raw_removed": True})
        self.assertEqual(obs.diag["pktmon_status_after_stop_shape"], ["unavailable: ValueError"])
        self.assertEqual(obs.diag["pktmon_filter_list_after_remove_shape"], ["unavailable: ValueError"])

    def test_status_failure_after_stop_is_not_reported_as_stopped(self) -> None:
        fake = FakeWindows(status_fail_after_stop=True)
        obs = observer(fake)
        obs.start()
        obs.stop_and_collect(SID)
        cleanup = obs.cleanup()
        self.assertFalse(cleanup["pktmon_stopped"])
        self.assertIsNotNone(to.cleanup_reason(cleanup))

    def test_short_frame_to_testnet_makes_the_observation_unavailable(self) -> None:
        short = eth(ipv4(NIC, 1, "192.0.2.1", 2, "udp"))[:14 + 20 + 4]
        fake = FakeWindows(pcap_frames=[syn(5000, 47300), short],
                           events=[netevent(DROP, "192.0.2.1", 47316, fid="100")])
        obs = observer(fake)
        obs.start()
        result = obs.stop_and_collect(SID)
        obs.cleanup()
        self.assertEqual(result["unavailable"],
                         "pktmon pcapng could not be parsed (short frame to testnet could not be decoded)")
        self.assertEqual(result["pktmon"]["diag"]["short_frames"], 1)

    def test_short_frame_outside_testnet_is_counted_without_making_the_observation_unavailable(self) -> None:
        short = eth(ipv4(NIC, 1, "198.51.100.7", 2, "udp"))[:14 + 20 + 4]
        leak_port = testnet.testnet_ports("appcontainer")["testnet_img"]
        fake = FakeWindows(pcap_frames=[syn(5000, 47300), short, syn(6000, leak_port)],
                           events=[netevent(DROP, "192.0.2.1", 47316, fid="100")])
        obs = observer(fake)
        obs.start()
        result = obs.stop_and_collect(SID)
        obs.cleanup()
        self.assertIsNone(result["unavailable"])
        # 同じ pcap の陰性パケット（appcontainer の NIC 出力）は従来どおり漏えいとして fail になる。
        verdicts = judge.judge_testnet("appcontainer", observation=result, positive_ok=True, binary_match=True)
        self.assertEqual(verdicts["testnet_img"]["verdict"], judge.FAIL)
        self.assertEqual(result["pktmon"]["diag"]["short_frames"], 1)
        self.assertEqual(result["pktmon"]["routes"]["baseline/testnet_redirect"]["packets"], {"tcp_syn": 1})


class PreExistingStateTest(unittest.TestCase):
    def test_running_pktmon_is_left_untouched(self) -> None:
        fake = FakeWindows(pm_running=True)
        obs = observer(fake)
        obs.start()
        self.assertIn("already running", obs.unavailable or "")
        result = obs.stop_and_collect(SID)
        cleanup = obs.cleanup()
        self.assertIsNotNone(result["unavailable"])
        self.assertTrue(fake.pm_running)  # 他人の採取を止めない
        names = fake.names()
        self.assertNotIn("pktmon.exe stop", names)
        self.assertNotIn("pktmon.exe start", names)
        self.assertFalse(any(a[1:3] == ["filter", "remove"] for a in fake.calls))
        self.assertTrue(all(cleanup.values()))

    def test_existing_filters_are_not_removed(self) -> None:
        fake = FakeWindows(pm_filters=True)
        obs = observer(fake)
        obs.start()
        obs.stop_and_collect(SID)
        cleanup = obs.cleanup()
        self.assertIn("filters already existed", obs.unavailable or "")
        self.assertTrue(fake.pm_filters)
        self.assertFalse(any(a[1:3] == ["filter", "remove"] for a in fake.calls))
        self.assertTrue(all(cleanup.values()))

    def test_unrecognized_state_output_is_fail_closed(self) -> None:
        fake = FakeWindows(status_out="???")
        obs = observer(fake)
        obs.start()
        self.assertIn("could not be determined", obs.unavailable or "")
        self.assertFalse(any(a[1] == "start" for a in fake.calls if a[0].endswith("pktmon.exe")))
        obs.cleanup()

    def test_netevents_originally_on_is_restored_to_on(self) -> None:
        fake = FakeWindows(netevents=True)
        obs = observer(fake)
        obs.start()
        obs.stop_and_collect(SID)
        self.assertTrue(obs.cleanup()["netevents_restored"])
        self.assertTrue(fake.netevents)


class FailureHandlingTest(unittest.TestCase):
    def test_start_failure_still_cleans_up(self) -> None:
        fake = FakeWindows(fail={"start"})
        obs = observer(fake)
        obs.start()
        self.assertEqual(obs.unavailable, "pktmon start failed")
        result = obs.stop_and_collect(SID)
        cleanup = obs.cleanup()
        self.assertEqual(result["unavailable"], "pktmon start failed")
        self.assertEqual(cleanup["pktmon_filters_removed"], True)
        self.assertFalse(fake.pm_filters)
        self.assertFalse(fake.netevents)  # netevents=on は start の失敗で行っていない

    def test_capture_is_limited_to_nics(self) -> None:
        fake = FakeWindows()
        obs = observer(fake)
        obs.start()
        starts = [a for a in fake.calls if a[1:2] == ["start"]]
        self.assertEqual(len(starts), 1)
        self.assertEqual(starts[0][starts[0].index("--comp") + 1], "nics")
        obs.cleanup()

    def test_wfp_failure_still_parses_pcap_and_leak_fails(self) -> None:
        port = testnet.testnet_ports("appcontainer")["testnet_redirect"]
        for failing in ({"set-options"}, {"show-netevents"}):
            fake = FakeWindows(fail=failing, pcap_frames=[syn(5000, port)])
            obs = observer(fake)
            obs.start()
            result = obs.stop_and_collect(SID)
            self.assertIsNotNone(result["unavailable"])
            self.assertEqual(result["pktmon"]["routes"]["appcontainer/testnet_redirect"]["packets"], {"tcp_syn": 1})
            verdicts = judge.judge_testnet("appcontainer", observation=result, positive_ok=True, origin_ok=True, binary_match=True,
                                           cleanup_incomplete=to.cleanup_reason(obs.cleanup()))
            self.assertEqual(verdicts["testnet_redirect"]["verdict"], judge.FAIL)

    def test_filter_add_failure_is_unavailable(self) -> None:
        fake = FakeWindows(fail={"filter-add"})
        obs = observer(fake)
        obs.start()
        self.assertEqual(obs.unavailable, "pktmon filter add failed")
        obs.cleanup()

    def test_netevents_set_failure_is_unavailable_and_restore_is_attempted(self) -> None:
        fake = FakeWindows(fail={"set-options"})
        obs = observer(fake)
        obs.start()
        self.assertIn("netevents=on failed", obs.unavailable or "")
        obs.stop_and_collect(SID)
        cleanup = obs.cleanup()
        self.assertTrue(any(a[1:4] == ["wfp", "set", "options"] for a in fake.calls))
        self.assertFalse(fake.pm_running or fake.pm_filters)
        self.assertIn("netevents_restored", cleanup)

    def test_failed_restore_is_not_reported_as_restored(self) -> None:
        fake = FakeWindows(restore_fail=True)
        obs = observer(fake)
        obs.start()
        obs.stop_and_collect(SID)
        cleanup = obs.cleanup()
        self.assertTrue(fake.netevents)  # 元は off だったのに on のまま残った
        self.assertFalse(cleanup["netevents_restored"])
        self.assertIsNotNone(to.cleanup_reason(cleanup))

    def test_failed_stop_is_not_reported_as_stopped(self) -> None:
        fake = FakeWindows(fail={"stop"})
        obs = observer(fake)
        obs.start()
        obs.stop_and_collect(SID)
        cleanup = obs.cleanup()
        self.assertFalse(cleanup["pktmon_stopped"])
        self.assertIsNotNone(to.cleanup_reason(cleanup))

    def test_stop_failure_is_unavailable_even_if_cleanup_stop_succeeds(self) -> None:
        fake = FakeWindows(stop_fail_first=True, pcap_frames=[syn(5000, 47300)])
        obs = observer(fake)
        obs.start()
        result = obs.stop_and_collect(SID)
        cleanup = obs.cleanup()
        self.assertTrue(cleanup["pktmon_stopped"])  # cleanup の再停止は成功する
        self.assertIsNone(to.cleanup_reason(cleanup))
        self.assertEqual(result["unavailable"], "pktmon stop failed")
        self.assertEqual(result["diag"]["pktmon_stop_rc"], 1)

    def test_stop_failure_keeps_captured_packets_for_leak_evidence(self) -> None:
        fake = FakeWindows(stop_fail_first=True, pcap_frames=[syn(5000, 47300)])
        obs = observer(fake)
        obs.start()
        result = obs.stop_and_collect(SID)
        obs.cleanup()
        self.assertEqual(result["pktmon"]["routes"]["baseline/testnet_redirect"]["packets"], {"tcp_syn": 1})

    def test_failed_filter_remove_is_not_reported_as_removed(self) -> None:
        fake = FakeWindows(fail={"filter-remove"})
        obs = observer(fake)
        obs.start()
        obs.stop_and_collect(SID)
        self.assertFalse(obs.cleanup()["pktmon_filters_removed"])

    def test_etl2pcap_failure_and_bad_pcap_are_unavailable(self) -> None:
        for kw, text in (({"fail": {"etl2pcap"}}, "etl2pcap failed"), ({"bad_pcap": True}, "could not be parsed")):
            fake = FakeWindows(**kw)
            obs = observer(fake)
            obs.start()
            result = obs.stop_and_collect(SID)
            obs.cleanup()
            self.assertIn(text, result["unavailable"] or "")

    def test_undefined_ifid_and_caplen_mismatch_are_unavailable_and_keep_leak_evidence(self) -> None:
        good = epb(syn(5000, 47300))
        frame = syn(5001, 47301)
        bad_ifid = block(6, struct.pack("<IIIII", 7, 0, 0, len(frame), len(frame)) + frame)
        bad_caplen = block(6, struct.pack("<IIIII", 0, 0, 0, len(frame) + 64, len(frame) + 64) + frame)
        for blk, text in ((bad_ifid, "undefined interface"), (bad_caplen, "caplen")):
            fake = FakeWindows(pcap_bytes=shb() + idb(1) + good + blk)
            obs = observer(fake)
            obs.start()
            result = obs.stop_and_collect(SID)
            obs.cleanup()
            self.assertIn(text, result["pktmon"]["diag"]["error"])
            self.assertIn("could not be parsed", result["unavailable"] or "")
            self.assertEqual(result["pktmon"]["routes"]["baseline/testnet_redirect"]["packets"], {"tcp_syn": 1})

    def test_truncated_undecodable_frame_makes_the_observation_unavailable(self) -> None:
        # Issue #1515: ヘッダの途中で切れたフレームは diag.error を経て unavailable に繋がる。
        frame = syn(5001, 47301)
        cut = block(6, struct.pack("<IIIII", 0, 0, 0, 24, len(frame)) + frame[:24])
        fake = FakeWindows(pcap_bytes=shb() + idb(1) + epb(syn(5000, 47300)) + cut)
        obs = observer(fake)
        obs.start()
        result = obs.stop_and_collect(SID)
        obs.cleanup()
        self.assertEqual(result["pktmon"]["diag"]["truncated_frames"], 1)
        self.assertIn("truncated frame could not be decoded", result["unavailable"] or "")

    def test_short_blocks_make_the_observation_unavailable(self) -> None:
        # Issue #1515: 必須ヘッダに満たない IDB・EPB・SPB は無言で捨てず unavailable にする。
        for name, short in (("IDB", block(1, b"\x01\x00")), ("EPB", block(6, b"\x00" * 12)), ("SPB", block(3, b""))):
            fake = FakeWindows(pcap_bytes=shb() + idb(1) + short + epb(syn(5000, 47300)))
            obs = observer(fake)
            obs.start()
            result = obs.stop_and_collect(SID)
            obs.cleanup()
            self.assertIn(f"{name} block shorter", result["unavailable"] or "", name)

    def test_show_netevents_failure_is_unavailable(self) -> None:
        fake = FakeWindows(fail={"show-netevents"})
        obs = observer(fake)
        obs.start()
        result = obs.stop_and_collect(SID)
        obs.cleanup()
        self.assertIn("show netevents failed", result["unavailable"] or "")

    def test_firewall_disabled_scaffold_is_unavailable(self) -> None:
        fake = FakeWindows(firewall="False")
        obs = observer(fake)
        obs.start()
        self.assertIn("firewall profile", obs.unavailable or "")
        self.assertFalse(fake.pm_running)
        obs.cleanup()

    def test_unavailable_makes_every_route_unobservable_but_leak_still_fails(self) -> None:
        obs_dict = {"unavailable": "pktmon start failed", "pktmon": {"routes": {}}, "wfp": {"routes": {}}}
        v = judge.judge_testnet("appcontainer", observation=obs_dict, positive_ok=True, origin_ok=True, binary_match=True)
        self.assertTrue(all(x["verdict"] == judge.UNOBSERVABLE for x in v.values()))
        leak = {"unavailable": "netsh wfp show netevents failed",
                "pktmon": {"routes": {"appcontainer/testnet_fetch": {"packets": {"tcp_syn": 1}, "unique_flows": {"tcp_syn": 1}}}},
                "wfp": {"routes": {}}}
        v2 = judge.judge_testnet("appcontainer", observation=leak, positive_ok=True, origin_ok=True, binary_match=True)
        self.assertEqual(v2["testnet_fetch"]["verdict"], judge.FAIL)

    def test_cleanup_is_idempotent(self) -> None:
        fake = FakeWindows()
        obs = observer(fake)
        obs.start()
        obs.stop_and_collect(SID)
        first = obs.cleanup()
        n = len(fake.calls)
        self.assertEqual(obs.cleanup(), first)
        self.assertEqual(len(fake.calls), n)

    def test_cleanup_reason_requires_every_key_true(self) -> None:
        full = {"pktmon_stopped": True, "pktmon_filters_removed": True, "netevents_restored": True, "raw_removed": True}
        self.assertIsNone(to.cleanup_reason(full))
        for k in full:
            self.assertIsNotNone(to.cleanup_reason({**full, k: False}))
            partial = dict(full)
            del partial[k]
            self.assertIsNotNone(to.cleanup_reason(partial))
        self.assertIsNotNone(to.cleanup_reason(None))


class RunCmdTest(unittest.TestCase):
    def test_run_cmd_reports_missing_executable_without_raising(self) -> None:
        r = to.run_cmd(["/nonexistent/definitely-missing-binary"])
        self.assertIsNone(r["rc"])
        self.assertEqual(r["out"], "")

    def test_run_cmd_captures_output_and_rc(self) -> None:
        r = to.run_cmd([sys.executable, "-c", "print('hi'); import sys; sys.exit(3)"])
        self.assertEqual(r["rc"], 3)
        self.assertIn("hi", r["out"])


if __name__ == "__main__":
    unittest.main()
