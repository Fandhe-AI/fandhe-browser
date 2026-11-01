"""appcontainer.py・appcontainer_probe.py・AppContainer モードの判定の自己テスト（Issue #1396・TASK-38・`RENDER-3`）。

純粋関数・構造体のサイズ・判定・偽ランチャーを使った `measure.run_appcontainer_mode` は全 OS で実行する。
Win32 API を呼ぶ FFI テスト（属性リストの組み立て）は Windows のみで、他の OS では理由つきで skip する。
プロセスの起動は単体テストでは行わない（実測は `xos-measure.yml` の suite=egress-isolation）。
"""

from __future__ import annotations

import ctypes
import json
import socket
import sys
import tempfile
import unittest
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))
import appcontainer as ac  # noqa: E402
import judge  # noqa: E402
import measure  # noqa: E402

X64 = ctypes.sizeof(ctypes.c_void_p) == 8
SID = "S-1-15-2-1-2-3-4-5-6-7"


def _can_bind_alias() -> bool:
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        s.bind((judge.IP_BLOCKED, 0))
        return True
    except OSError:
        return False
    finally:
        s.close()


class PureFunctionsTest(unittest.TestCase):
    def test_profile_name_is_fixed_format(self) -> None:
        self.assertEqual(ac.profile_name(4242), "fandhe.egressiso.4242")
        for bad in (-1, True, "1", None):
            with self.assertRaises(ValueError):
                ac.profile_name(bad)  # type: ignore[arg-type]

    def test_profile_name_validation(self) -> None:
        self.assertEqual(ac.validate_profile_name("fandhe.egressiso.1"), "fandhe.egressiso.1")
        for bad in ("", "a", "UPPER.case", "x" * 65, "has space", "a;b", "../x", 5):
            with self.assertRaises(ValueError):
                ac.validate_profile_name(bad)

    def test_sid_string_validation(self) -> None:
        self.assertEqual(ac.validate_sid_string(SID), SID)
        for bad in ("S-1-5-32-544", "S-1-15-2-1-2-3", f"{SID}; calc", f"{SID}-8", "", None):
            with self.assertRaises(ValueError):
                ac.validate_sid_string(bad)

    def test_environment_block_is_sorted_and_double_nul_terminated(self) -> None:
        block = ac.build_environment_block({"TMP": "C:\\t", "PYTHONIOENCODING": "utf-8", "SystemRoot": "C:\\W"})
        self.assertEqual(block, "PYTHONIOENCODING=utf-8\0SystemRoot=C:\\W\0TMP=C:\\t\0\0")
        with self.assertRaises(ValueError):
            ac.build_environment_block({"A=B": "x"})
        with self.assertRaises(ValueError):
            ac.build_environment_block({"A": "x\0y"})

    def test_container_env_is_an_allowlist(self) -> None:
        env = ac.container_env("C:\\s\\tmp", "C:\\Windows")
        self.assertEqual(sorted(env), ["FANDHE_EGRESS_APPCONTAINER", "PYTHONIOENCODING", "SystemRoot", "TEMP", "TMP"])
        self.assertNotIn("PATH", env)
        self.assertEqual(sorted(ac.container_env("C:\\s\\tmp", None)),
                         ["FANDHE_EGRESS_APPCONTAINER", "PYTHONIOENCODING", "TEMP", "TMP"])

    def test_container_env_carries_appcontainer_flag_matching_in_sandbox(self) -> None:
        import in_sandbox
        env = ac.container_env("C:\\s\\tmp", "C:\\Windows", {"FANDHE_EGRESS_APPCONTAINER": "spoof"})
        self.assertEqual(env["FANDHE_EGRESS_APPCONTAINER"], "1")
        self.assertEqual((ac.APPCONTAINER_FLAG_ENV, ac.APPCONTAINER_FLAG_VALUE),
                         (in_sandbox.APPCONTAINER_FLAG_ENV, in_sandbox.APPCONTAINER_FLAG_VALUE))

    def test_container_env_copies_host_path_vars_only_when_set(self) -> None:
        host = {"LOCALAPPDATA": "C:\\Users\\u\\AppData\\Local", "USERPROFILE": "C:\\Users\\u", "USERNAME": "u",
                "APPDATA": "", "PATH": "C:\\bin", "HTTP_PROXY": "http://p:1", "HTTPS_PROXY": "http://p:1"}
        env = ac.container_env("C:\\s\\tmp", "C:\\Windows", host)
        self.assertEqual(sorted(env), ["FANDHE_EGRESS_APPCONTAINER", "LOCALAPPDATA", "PYTHONIOENCODING",
                                       "SystemRoot", "TEMP", "TMP", "USERNAME", "USERPROFILE"])
        self.assertEqual(env["LOCALAPPDATA"], "C:\\Users\\u\\AppData\\Local")
        self.assertEqual(env["USERPROFILE"], "C:\\Users\\u")
        self.assertEqual(env["TMP"], "C:\\s\\tmp")
        for banned in ("PATH", "HTTP_PROXY", "HTTPS_PROXY", "APPDATA"):
            self.assertNotIn(banned, env)

    def test_command_line_quotes_with_list2cmdline(self) -> None:
        self.assertEqual(ac.command_line(["C:\\a b\\python.exe", "-I", "x.py", "--p", "4000"]),
                         '"C:\\a b\\python.exe" -I x.py --p 4000')
        for bad in ([], ["a\0b"], [1]):
            with self.assertRaises(ValueError):
                ac.command_line(bad)  # type: ignore[arg-type]

    def test_lib_ignore_excludes_heavy_dirs_only(self) -> None:
        names = ["os.py", "site-packages", "test", "idlelib", "json", "x.pyc", "tkinter", "unittest"]
        self.assertEqual(ac.lib_ignore("", names), {"site-packages", "test", "idlelib", "x.pyc", "tkinter"})

    def test_harness_sources_exclude_tests(self) -> None:
        srcs = ac.harness_sources(Path(__file__).resolve().parent.parent)
        rels = {rel for _, rel in srcs}
        self.assertIn("egress-isolation/in_sandbox.py", rels)
        self.assertIn("egress-isolation/appcontainer_probe.py", rels)
        self.assertIn("render-screenshot/servo_bbox_driver.py", rels)
        self.assertFalse(any(r.rsplit("/", 1)[1].startswith("test_") for r in rels))

    def test_read_capped_rejects_oversize_and_missing(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            small, big = Path(td) / "s.json", Path(td) / "b.json"
            small.write_bytes(b'{"a": 1}')
            big.write_bytes(b"x" * 11)
            self.assertEqual(ac.read_capped(small, 10), b'{"a": 1}')
            with self.assertRaises(ValueError):
                ac.read_capped(big, 10)
            with self.assertRaises(ValueError):
                ac.read_capped(Path(td) / "none.json", 10)
            self.assertEqual(ac.tail_text(big, 4), "xxxx")
            self.assertEqual(ac.tail_text(Path(td) / "none.txt"), "")

    def test_fallback_dir_gets_dacl_grant_and_inherited_low_integrity_label(self) -> None:
        root = Path("C:/t/egress-iso-x")
        sid = "S-1-15-2-1-2-3-4-5-6-7"
        self.assertEqual(ac.icacls_grant_args(root, sid),
                         ["icacls", str(root), "/grant", f"*{sid}:(OI)(CI)(M)"])
        self.assertEqual(ac.icacls_low_integrity_args(root),
                         ["icacls", str(root), "/setintegritylevel", "(OI)(CI)L"])

    def test_appcontainer_folder_staging_gets_explicit_grant_without_low_label(self) -> None:
        # 3.13+ の mkdtemp は継承を切るため、AppContainer 専用フォルダ配下でも明示付与が要る。
        from unittest import mock
        calls: list[list[str]] = []

        def fake_run(args: list[str], **_kw: Any) -> Any:
            calls.append(list(args))
            return mock.Mock(returncode=0)

        with tempfile.TemporaryDirectory() as td:
            sess = ac.AppContainerSession(host=Path(td) / "h.exe", harness_dir=Path(td))
            sess.sid_string = SID
            sess._root = Path(td)
            with mock.patch.object(ac.subprocess, "run", fake_run):
                sess._icacls_grant(low_integrity=False)
            self.assertEqual(calls, [ac.icacls_grant_args(Path(td), SID)])
            self.assertTrue(sess._granted)
            calls.clear()
            with mock.patch.object(ac.subprocess, "run", fake_run):
                sess._icacls_grant()
            self.assertEqual(calls, [ac.icacls_grant_args(Path(td), SID),
                                     ac.icacls_low_integrity_args(Path(td))])

    def test_capability_sids_are_well_known_network_ones(self) -> None:
        self.assertEqual(ac.NETWORK_CAPABILITIES, ("S-1-15-3-1", "S-1-15-3-3"))

    def test_ffi_is_unavailable_off_windows(self) -> None:
        if sys.platform == "win32":
            self.skipTest("Windows has the FFI")
        with self.assertRaises(ac.AppContainerError):
            ac._api()


class IcaclsTimeoutTest(unittest.TestCase):
    def test_timeout_becomes_appcontainer_error(self) -> None:
        import subprocess
        from unittest import mock
        with mock.patch.object(ac.subprocess, "run", side_effect=subprocess.TimeoutExpired("icacls", 60)):
            with self.assertRaises(ac.AppContainerError) as cm:
                ac._run_icacls(["icacls", "x"], "grant")
        self.assertEqual(str(cm.exception), "icacls grant failed: TimeoutExpired")

    def test_oserror_becomes_appcontainer_error(self) -> None:
        from unittest import mock
        with mock.patch.object(ac.subprocess, "run", side_effect=FileNotFoundError("icacls")):
            with self.assertRaises(ac.AppContainerError):
                ac._run_icacls(["icacls", "x"], "setintegritylevel")


@unittest.skipUnless(X64, "struct sizes are asserted for 64-bit pointers")
class DrainJobTest(unittest.TestCase):
    """TerminateJobObject 後の終了待ち（#1396）。Windows 以外でも偽の api で実行できる。"""

    class FakeApi:
        def __init__(self, active: list[int], query_ok: bool = True) -> None:
            self.active, self.query_ok, self.calls = list(active), query_ok, []

        def QueryInformationJobObject(self, job: Any, cls: int, ref: Any, size: int, ret: Any) -> int:
            self.calls.append(("query", cls))
            if not self.query_ok:
                return 0
            ref._obj.ActiveProcesses = self.active.pop(0) if len(self.active) > 1 else self.active[0]
            return 1

    def test_returns_true_once_active_processes_reach_zero(self) -> None:
        api = self.FakeApi([2, 1, 0])
        sleeps: list[float] = []
        ok = ac.drain_job(api, "J", poll_s=0.05, sleep=sleeps.append, clock=lambda: 0.0)
        self.assertTrue(ok)
        self.assertEqual(len(api.calls), 3)
        self.assertEqual(api.calls[0], ("query", 1))
        self.assertEqual(sleeps, [0.05, 0.05])

    def test_timeout_returns_false(self) -> None:
        api = self.FakeApi([1])
        ticks = iter([0.0, 1.0, 2.0, 3.0, 4.0])
        ok = ac.drain_job(api, "J", limit_s=2.5, sleep=lambda _s: None, clock=lambda: next(ticks))
        self.assertFalse(ok)
        self.assertEqual(len(api.calls), 3)

    def test_query_failure_is_fail_closed(self) -> None:
        self.assertFalse(ac.drain_job(self.FakeApi([0], query_ok=False), "J"))

    def test_struct_size_matches_win32(self) -> None:
        self.assertEqual(ctypes.sizeof(ac.JOBOBJECT_BASIC_ACCOUNTING_INFORMATION), 48)


class LaunchTeardownOrderTest(unittest.TestCase):
    """_launch の finally が TerminateJobObject -> 終了待ち -> CloseHandle の順で動くこと（#1396）。"""

    def _run(self, active: list[int]) -> tuple[list[str], dict[str, Any], dict[str, Any]]:
        from unittest import mock
        order: list[str] = []

        class Api:
            def __getattr__(self, name: str) -> Any:
                def call(*args: Any) -> int:
                    order.append(name)
                    if name == "QueryInformationJobObject":
                        args[2]._obj.ActiveProcesses = active.pop(0) if len(active) > 1 else active[0]
                        return 1
                    if name == "WaitForSingleObject":
                        return 0
                    if name == "InitializeProcThreadAttributeList" and args[0] is None:
                        args[3]._obj.value = 64
                        return 0
                    if name == "ResumeThread":
                        return 1
                    return 1
                return call

        class FakeMsvcrt:
            @staticmethod
            def get_osfhandle(fd: int) -> int:
                return fd

        tmp = Path(tempfile.mkdtemp())
        sess = ac.AppContainerSession(host=Path("h.exe"), harness_dir=tmp)
        sess._api = Api()
        sess.staging = tmp
        result: dict[str, Any] = {"exit_code": None, "timed_out": False}
        with mock.patch.dict(sys.modules, {"msvcrt": FakeMsvcrt}), \
                mock.patch.object(ac.os, "set_handle_inheritable", create=True), \
                mock.patch.object(ac.time, "sleep"), \
                mock.patch.object(ac.time, "monotonic", side_effect=iter(range(0, 1000, 5))):
            sess._launch("x.py", [], (), 5, result)
        return order, sess.cleanup, result

    def test_drain_runs_after_terminate_and_before_close(self) -> None:
        order, cleanup, _ = self._run([1, 0])
        i_term = len(order) - 1 - order[::-1].index("TerminateJobObject")
        i_query = order.index("QueryInformationJobObject", i_term)
        i_close = order.index("CloseHandle", i_term)
        self.assertLess(i_term, i_query)
        self.assertLess(i_query, i_close)
        self.assertTrue(cleanup["job_drained"])

    def test_drain_timeout_is_recorded_as_incomplete_and_cleanup_continues(self) -> None:
        order, cleanup, _ = self._run([1])
        self.assertIs(cleanup["job_drained"], False)
        self.assertEqual(order.count("CloseHandle"), 1)  # Linux では proc/thread が NULL のため job のみ
        self.assertTrue(cleanup["handles_closed"])


class StructSizeTest(unittest.TestCase):
    """Microsoft の定義（x64）: STARTUPINFOW 104・STARTUPINFOEXW 112・PROCESS_INFORMATION 24 等。"""

    def test_sizes(self) -> None:
        self.assertEqual(ctypes.sizeof(ac.STARTUPINFOW), 104)
        self.assertEqual(ctypes.sizeof(ac.STARTUPINFOEXW), 112)
        self.assertEqual(ctypes.sizeof(ac.PROCESS_INFORMATION), 24)
        self.assertEqual(ctypes.sizeof(ac.SID_AND_ATTRIBUTES), 16)
        self.assertEqual(ctypes.sizeof(ac.SECURITY_CAPABILITIES), 24)
        self.assertEqual(ctypes.sizeof(ac.JOBOBJECT_BASIC_LIMIT_INFORMATION), 64)
        self.assertEqual(ctypes.sizeof(ac.JOBOBJECT_EXTENDED_LIMIT_INFORMATION), 144)


@unittest.skipUnless(sys.platform == "win32",
                     "AppContainer FFI is Windows-only (CreateAppContainerProfile / UpdateProcThreadAttribute)")
class WindowsFfiTest(unittest.TestCase):
    def _attribute_list_roundtrip(self, caps: tuple[str, ...]) -> None:
        a = ac._api()
        psid = ctypes.c_void_p(None)
        name = ac.profile_name(0)
        self.assertGreaterEqual(a.DeriveAppContainerSidFromAppContainerName(name, ctypes.byref(psid)), 0)
        cap_ptrs: list[ctypes.c_void_p] = []
        try:
            arr = (ac.SID_AND_ATTRIBUTES * max(1, len(caps)))()
            for i, cap in enumerate(caps):
                p = ctypes.c_void_p(None)
                self.assertTrue(a.ConvertStringSidToSidW(cap, ctypes.byref(p)))
                cap_ptrs.append(p)
                arr[i].Sid, arr[i].Attributes = p, ac.SE_GROUP_ENABLED
            sc = ac.SECURITY_CAPABILITIES(psid, ctypes.cast(arr, ctypes.c_void_p) if caps else None, len(caps), 0)
            size = ctypes.c_size_t(0)
            self.assertFalse(a.InitializeProcThreadAttributeList(None, 1, 0, ctypes.byref(size)))
            self.assertEqual(ctypes.get_last_error(), ac.ERROR_INSUFFICIENT_BUFFER)  # type: ignore[attr-defined]
            self.assertGreater(size.value, 0)
            buf = ctypes.create_string_buffer(size.value)
            self.assertTrue(a.InitializeProcThreadAttributeList(buf, 1, 0, ctypes.byref(size)))
            try:
                self.assertTrue(a.UpdateProcThreadAttribute(
                    buf, 0, ac.PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, ctypes.byref(sc),
                    ctypes.sizeof(sc), None, None))
            finally:
                a.DeleteProcThreadAttributeList(buf)
        finally:
            for p in cap_ptrs:
                a.LocalFree(p)
            a.FreeSid(psid)

    def test_attribute_list_without_capabilities(self) -> None:
        self._attribute_list_roundtrip(())

    def test_attribute_list_with_network_capabilities(self) -> None:
        self._attribute_list_roundtrip(ac.NETWORK_CAPABILITIES)

    def test_derived_sid_string_matches_the_appcontainer_format(self) -> None:
        a = ac._api()
        psid = ctypes.c_void_p(None)
        self.assertGreaterEqual(a.DeriveAppContainerSidFromAppContainerName(ac.profile_name(0), ctypes.byref(psid)), 0)
        out = ctypes.c_void_p(None)
        try:
            self.assertTrue(a.ConvertSidToStringSidW(psid, ctypes.byref(out)))
            self.assertEqual(ac.validate_sid_string(ctypes.wstring_at(out)).count("-"), 10)
        finally:
            a.LocalFree(out)
            a.FreeSid(psid)


def _preflight(nocap: tuple[int, int] | None, netcap: tuple[int, int] | None) -> dict[str, Any]:
    def entry(v: tuple[int, int] | None) -> Any:
        return None if v is None else {"probe": {"loopback_self": {"ok": True}}, "outer_tcp": v[0], "outer_udp": v[1]}
    return {"nocap": entry(nocap), "netcap": entry(netcap)}


class ConfoundTest(unittest.TestCase):
    def test_netcap_reaching_outside_means_no_confound(self) -> None:
        self.assertIsNone(judge.appcontainer_confound(_preflight((0, 0), (1, 0))))
        self.assertIsNone(judge.appcontainer_confound(_preflight((0, 0), (0, 2))))

    def test_netcap_not_reaching_is_a_confound(self) -> None:
        self.assertEqual(judge.appcontainer_confound(_preflight((0, 0), (0, 0))), judge.LOOPBACK_CONFOUND_REASON)

    def test_missing_or_malformed_preflight_is_a_confound(self) -> None:
        for bad in (None, {}, "x", _preflight(None, (1, 1)), _preflight((0, 0), None),
                    {"nocap": {"probe": {}, "outer_tcp": True, "outer_udp": 0},
                     "netcap": {"probe": {}, "outer_tcp": 1, "outer_udp": 0}},
                    {"nocap": {"probe": {}, "outer_tcp": -1, "outer_udp": 0},
                     "netcap": {"probe": {}, "outer_tcp": 1, "outer_udp": 0}}):
            self.assertEqual(judge.appcontainer_confound(bad), judge.LOOPBACK_PREFLIGHT_MISSING_REASON)


PORTS = {"redirect": 2001, "img": 2002, "fetch": 2003, "ws": 2004, "internal": 2005, "udp_direct": 2006}
CONTROL = {"redirect": 1, "img": 1, "fetch": 1, "ws": 1, "internal": 1, "udp_direct": 1}
ZERO = {k: 0 for k in CONTROL}


def _inner(**over: Any) -> dict[str, Any]:
    base: dict[str, Any] = {"positive_ok": True, "attempts": {"udp_direct": True},
                            "guard": {"unavailable": "no monitor"}, "attempt_evidence": {"udp_direct": "errno"}}
    base.update(over)
    return base


class JudgeAppContainerTest(unittest.TestCase):
    def test_udp_pass_requires_no_confound(self) -> None:
        v = judge.judge_all("appcontainer", _inner(), ZERO, CONTROL, loopback_confound=None)
        self.assertEqual(v["udp_direct"]["verdict"], judge.PASS)
        self.assertEqual(v["redirect"]["verdict"], judge.UNOBSERVABLE)  # TCP は試行の根拠なし
        self.assertIn("no attempt was observed", v["redirect"]["reason"])
        for structural in judge.STRUCTURAL_ROUTES:
            self.assertEqual(v[structural]["verdict"], judge.UNOBSERVABLE)

    def test_confound_downgrades_pass_to_unobservable_with_reason(self) -> None:
        v = judge.judge_all("appcontainer", _inner(), ZERO, CONTROL, loopback_confound=judge.LOOPBACK_CONFOUND_REASON)
        self.assertEqual(v["udp_direct"]["verdict"], judge.UNOBSERVABLE)
        self.assertEqual(v["udp_direct"]["reason"], judge.LOOPBACK_CONFOUND_REASON)

    def test_default_is_fail_closed(self) -> None:
        v = judge.judge_all("appcontainer", _inner(), ZERO, CONTROL)
        self.assertEqual(v["udp_direct"]["verdict"], judge.UNOBSERVABLE)

    def test_outer_receipt_is_still_a_fail(self) -> None:
        outer = {**ZERO, "udp_direct": 1}
        v = judge.judge_all("appcontainer", _inner(), outer, CONTROL, loopback_confound=judge.LOOPBACK_CONFOUND_REASON)
        self.assertEqual(v["udp_direct"]["verdict"], judge.FAIL)

    def test_positive_failure_reason_mentions_the_confound(self) -> None:
        v = judge.judge_all("appcontainer", _inner(positive_ok=False), ZERO, CONTROL,
                            loopback_confound=judge.LOOPBACK_CONFOUND_REASON)
        self.assertTrue(v["udp_direct"]["reason"].startswith("positive check failed"))
        self.assertIn("loopback isolation blocks", v["udp_direct"]["reason"])

    def test_method_names_the_capability_contrast(self) -> None:
        v = judge.judge_all("appcontainer", _inner(), ZERO, CONTROL, loopback_confound=None)
        self.assertIn("loopback confound checked by capability contrast", v["udp_direct"]["method"])


class FakeSession:
    """`appcontainer.AppContainerSession` と同じ規約の偽物。finally での片付けの呼び出しを記録する。"""

    def __init__(self, *, main: dict[str, Any], cleanup_ok: bool = True, **_kw: Any) -> None:
        self.main, self.cleanup_ok = main, cleanup_ok
        self.name, self.sid_string, self.staging_mode = "fandhe.egressiso.1", SID, "appcontainer-folder"
        self.host_path = "C:\\stage\\host\\h.exe"
        self.cleanup: dict[str, Any] = {}
        self.entered = self.exited = False
        self.calls: list[tuple[str, tuple[str, ...]]] = []

    def __enter__(self) -> "FakeSession":
        self.entered = True
        return self

    def __exit__(self, *exc: object) -> None:
        self.exited = True
        self.cleanup = {"profile_deleted": self.cleanup_ok, "staging_removed": True, "handles_closed": True}

    def out_path(self, name: str) -> str:
        return f"C:\\stage\\out\\{name}"

    def run(self, script: str, args: list[str], *, capabilities: tuple[str, ...] = (),
            result_name: str | None = None, timeout_s: int | None = None) -> dict[str, Any]:
        self.calls.append((script, capabilities))
        if script.endswith("appcontainer_probe.py"):
            return {"exit_code": 0, "timed_out": False, "error": None, "stderr_tail": "",
                    "result_json": {"loopback_self": {"ok": False, "errno": "WSAEACCES"},
                                    "tcp_outer": "WSAEACCES", "udp_outer": "WSAEACCES"}}
        if isinstance(self.main.get("raise"), Exception):
            raise self.main["raise"]
        return self.main


class _Rec:
    def __init__(self) -> None:
        self.count = 0


def _main(result_json: Any = None, exit_code: int | None = 0, timed_out: bool = False) -> dict[str, Any]:
    return {"exit_code": exit_code, "timed_out": timed_out, "error": None, "stderr_tail": "",
            "result_json": result_json}


@unittest.skipUnless(_can_bind_alias(), "127.0.0.2 is not bindable on this host")
class RunAppContainerModeTest(unittest.TestCase):
    def _run(self, session: FakeSession) -> dict[str, Any]:
        recs = {k: _Rec() for k in PORTS}
        with tempfile.TemporaryDirectory() as td:
            return measure.run_appcontainer_mode(
                host=Path("C:/h.exe"), ports=PORTS, recs=recs, out_dir=Path(td), page_port=4000,
                launcher=lambda **_: session)

    def test_success_records_preflight_cleanup_and_inner(self) -> None:
        s = FakeSession(main=_main(_inner(host_sha256="ab" * 32)))
        data = self._run(s)
        self.assertTrue(s.entered and s.exited)
        self.assertEqual([c[1] for c in s.calls], [(), ac.NETWORK_CAPABILITIES, ()])
        self.assertEqual(data["binary_sha256"], "ab" * 32)
        self.assertEqual(data["appcontainer"]["preflight"]["nocap"]["outer_tcp"], 0)
        self.assertEqual(data["appcontainer"]["cleanup"]["profile_deleted"], True)
        self.assertNotIn("unavailable", data)
        # 外側への到達が無い（netcap でも 0 件）ので、loopback 交絡ありとして pass は出ない。
        self.assertEqual(measure.appcontainer_caveat(data), judge.LOOPBACK_CONFOUND_REASON)

    def test_launcher_exception_is_cleaned_up_and_unobservable(self) -> None:
        s = FakeSession(main={"raise": ac.AppContainerError("CreateProcessW failed")})
        data = self._run(s)
        self.assertTrue(s.exited)
        self.assertIn("CreateProcessW failed", data["unavailable"])
        self.assertEqual(data["inner"], {})
        v = judge.judge_all("appcontainer", data["inner"], data["outer_counts"], CONTROL,
                            mode_unavailable=data["unavailable"])
        self.assertTrue(all(x["verdict"] == judge.UNOBSERVABLE and x["reason"] for x in v.values()))

    def test_timeout_is_reported_with_cleanup(self) -> None:
        data = self._run(FakeSession(main=_main(None, exit_code=None, timed_out=True)))
        self.assertIn("timed out", data["unavailable"])
        self.assertTrue(data["appcontainer"]["cleanup"]["handles_closed"])

    def test_no_result_json_is_unobservable_not_pass(self) -> None:
        data = self._run(FakeSession(main=_main(None, exit_code=1)))
        self.assertIn("no result", data["unavailable"])

    def test_incomplete_cleanup_blocks_a_pass(self) -> None:
        data = self._run(FakeSession(main=_main(_inner()), cleanup_ok=False))
        data["appcontainer"]["preflight"]["netcap"]["outer_tcp"] = 1  # 交絡なしを仮定しても片付け失敗が残る
        self.assertIn("cleanup was incomplete", measure.appcontainer_caveat(data) or "")

    def test_summary_renders_preflight_table(self) -> None:
        data = self._run(FakeSession(main=_main(_inner())))
        data["verdicts"] = judge.judge_all("appcontainer", data["inner"], data["outer_counts"], CONTROL,
                                           loopback_confound=measure.appcontainer_caveat(data))
        results = {"meta": {"date_utc": "t", "kernel": "k", "host_sha256": "h", "host_version": "v",
                            "proxy_env_present": False},
                   "modes": {"baseline": {"outer_counts": CONTROL}, "appcontainer": data}}
        text = measure.summarize(results)
        self.assertIn("| nocap |", text)
        self.assertIn("loopback confound:", text)
        json.dumps(results)  # JSON 化できる

SYS_ROOT = "C:\\Windows"
CNI = "C:\\Windows\\System32\\CheckNetIsolation.exe"


class CheckNetIsolationArgsTest(unittest.TestCase):
    """Issue #1401: CheckNetIsolation の argv は固定形式で、SID は検証済みの値だけ。"""

    def test_exact_argv(self) -> None:
        self.assertEqual(ac.checknetisolation_args("add", SID, SYS_ROOT), [CNI, "LoopbackExempt", "-a", f"-p={SID}"])
        self.assertEqual(ac.checknetisolation_args("delete", SID, SYS_ROOT),
                         [CNI, "LoopbackExempt", "-d", f"-p={SID}"])
        self.assertEqual(ac.checknetisolation_args("show", None, SYS_ROOT), [CNI, "LoopbackExempt", "-s"])

    def test_rejects_bad_inputs(self) -> None:
        with self.assertRaises(ValueError):
            ac.checknetisolation_args("reset", SID, SYS_ROOT)
        with self.assertRaises(ValueError):
            ac.checknetisolation_args("add", "S-1-5-32-544", SYS_ROOT)
        with self.assertRaises(ac.AppContainerError):
            ac.checknetisolation_args("add", SID, "Windows")
        with self.assertRaises(ac.AppContainerError):
            ac.checknetisolation_args("show", None, "")


class LoopbackExemptSessionTest(unittest.TestCase):
    """付与・解除の順序と、解除失敗・付与失敗の記録（Issue #1401。Linux 上で subprocess を偽物にして確かめる）。"""

    def _session(self, order: list[str], *, exempt: bool, rc: dict[str, Any] | None = None) -> Any:
        from unittest import mock
        rc = rc or {}
        sess = ac.AppContainerSession(host=Path("h.exe"), harness_dir=Path("."), loopback_exempt=exempt)

        class Api:
            def __getattr__(self, name: str) -> Any:
                def call(*_a: Any) -> int:
                    order.append(name)
                    return 0
                return call

        def create_profile() -> None:
            order.append("create_profile")
            sess.sid_string = SID
            sess._created = True

        def run(args: list[str], **_kw: Any) -> Any:
            flag = args[2]
            order.append(f"cni {flag}")
            behavior = rc.get(flag, 0)
            if isinstance(behavior, Exception):
                raise behavior
            out = SID if flag == "-s" and rc.get("listed", True) else ""

            class Done:
                returncode = behavior if flag != "-s" else 0
                stdout = out.encode()
            return Done()

        sess._api_patch = (mock.patch.object(ac, "_api", return_value=Api()),
                           mock.patch.object(sess, "_create_profile", create_profile),
                           mock.patch.object(sess, "_prepare_staging", lambda: order.append("prepare_staging")),
                           mock.patch.object(ac.subprocess, "run", run),
                           mock.patch.dict(ac.os.environ, {"SystemRoot": SYS_ROOT}))
        return sess

    def _enter_exit(self, sess: Any) -> None:
        for p in sess._api_patch:
            p.start()
        try:
            sess._sid = ctypes.c_void_p(1)
            with sess:
                pass
        finally:
            for p in sess._api_patch:
                p.stop()

    def test_order_add_before_staging_and_remove_before_delete(self) -> None:
        order: list[str] = []
        sess = self._session(order, exempt=True)
        self._enter_exit(sess)
        self.assertEqual(order, ["create_profile", "cni -a", "cni -s", "prepare_staging",
                                 "cni -d", "cni -s", "DeleteAppContainerProfile", "FreeSid"])
        self.assertEqual(sess.loopback_exempt_state,
                         {"requested": True, "add_rc": 0, "listed_after_add": True, "added": True})
        self.assertIs(sess.cleanup["loopback_exempt_removed"], True)
        # 解除後の確認で SID が一覧に残っていれば absent は False（偽物は常に SID を返す）。
        self.assertIs(sess.cleanup["loopback_exempt_absent_after_remove"], False)

    def test_remove_failure_is_recorded_and_profile_still_deleted(self) -> None:
        for bad in (1, __import__("subprocess").TimeoutExpired("x", 60)):
            order: list[str] = []
            sess = self._session(order, exempt=True, rc={"-d": bad})
            self._enter_exit(sess)
            self.assertIs(sess.cleanup["loopback_exempt_removed"], False)
            self.assertIn("DeleteAppContainerProfile", order)
            data = {"appcontainer": {"cleanup": sess.cleanup, "loopback_exempt": sess.loopback_exempt_state,
                                     "preflight": _preflight((0, 0), (1, 0))}}
            self.assertIn("cleanup was incomplete", measure.appcontainer_caveat(data) or "")

    def test_add_failure_raises_and_still_tries_remove_and_profile_delete(self) -> None:
        order: list[str] = []
        sess = self._session(order, exempt=True, rc={"-a": 5})
        with self.assertRaises(ac.AppContainerError):
            self._enter_exit(sess)
        self.assertEqual(order, ["create_profile", "cni -a", "cni -d", "cni -s", "DeleteAppContainerProfile", "FreeSid"])
        self.assertIs(sess.loopback_exempt_state["added"], False)
        self.assertEqual(sess.loopback_exempt_state["add_rc"], 5)

    def test_default_off_never_calls_checknetisolation(self) -> None:
        order: list[str] = []
        sess = self._session(order, exempt=False)
        self._enter_exit(sess)
        self.assertFalse([o for o in order if o.startswith("cni")])
        self.assertEqual(sess.loopback_exempt_state, {"requested": False})
        self.assertFalse([k for k in sess.cleanup if k.startswith("loopback_exempt")])


class LoopbackExemptJudgeTest(unittest.TestCase):
    def test_confound_three_ways(self) -> None:
        self.assertEqual(judge.appcontainer_confound(_preflight((1, 0), (1, 0))),
                         judge.LOOPBACK_EXEMPT_UNMEASURABLE_REASON)
        self.assertIsNone(judge.appcontainer_confound(_preflight((0, 0), (1, 0)), loopback_exempt=True))
        self.assertEqual(judge.appcontainer_confound(_preflight((0, 0), (0, 0)), loopback_exempt=True),
                         judge.LOOPBACK_CONFOUND_EXEMPTED_REASON)
        self.assertEqual(judge.appcontainer_confound(_preflight((0, 0), (0, 0)), loopback_exempt=False),
                         judge.LOOPBACK_CONFOUND_REASON)

    def test_nocap_reach_downgrades_listener_fail_and_keeps_counts(self) -> None:
        v = judge.judge_all("appcontainer", _inner(), {**ZERO, "udp_direct": 1, "fetch": 2}, CONTROL,
                            loopback_confound=judge.LOOPBACK_EXEMPT_UNMEASURABLE_REASON)
        for route, n in (("udp_direct", 1), ("fetch", 2)):
            self.assertEqual(v[route]["verdict"], judge.UNOBSERVABLE)
            self.assertIn(f"outer receipts ({n})", v[route]["reason"])
            self.assertEqual(v[route]["outer_count"], n)

    def test_other_confound_keeps_fail(self) -> None:
        v = judge.judge_all("appcontainer", _inner(), {**ZERO, "fetch": 2}, CONTROL,
                            loopback_confound=judge.LOOPBACK_CONFOUND_EXEMPTED_REASON)
        self.assertEqual(v["fetch"]["verdict"], judge.FAIL)


class LoopbackExemptMeasureTest(unittest.TestCase):
    def test_flag_reaches_launcher_and_is_recorded(self) -> None:
        seen: dict[str, Any] = {}

        def make(**kw: Any) -> FakeSession:
            seen.update(kw)
            return FakeSession(main=_main(_inner()))
        recs = {k: _Rec() for k in PORTS}
        with tempfile.TemporaryDirectory() as td:
            data = measure.run_appcontainer_mode(host=Path("C:/h.exe"), ports=PORTS, recs=recs, out_dir=Path(td),
                                                 page_port=4000, launcher=make, loopback_exempt=True)
        self.assertIs(seen["loopback_exempt"], True)
        self.assertEqual(data["appcontainer"]["loopback_exempt"], {"requested": True})

    def test_validate_flag(self) -> None:
        measure.validate_loopback_exempt(False, "linux", ["baseline"])
        measure.validate_loopback_exempt(True, "win32", ["baseline", "appcontainer"])
        with self.assertRaises(ValueError):
            measure.validate_loopback_exempt(True, "linux", ["baseline", "appcontainer"])
        with self.assertRaises(ValueError):
            measure.validate_loopback_exempt(True, "win32", ["baseline"])


if __name__ == "__main__":
    unittest.main()
