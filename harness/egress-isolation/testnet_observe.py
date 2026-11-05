#!/usr/bin/env python3
"""Windows のランナー側で TEST-NET-1 宛てのパケットを観測する（pktmon と WFP net events）。

役割・責務境界:
    Issue #1515（TASK-38・`RENDER-3`・`XOS-1`・`SEC-2`・予備実験 #1427）。AppContainer の中から TEST-NET-1 へ試した経路が
    「NIC から出たか」を `pktmon`（etl2pcap で pcapng にして `testnet_parse` で解析）で、「遮断された試行が起きたか」を
    WFP の net events（`netsh wfp set options netevents=on` -> 試行 -> `netsh wfp show netevents`）で観測する。
    採取 1 回で baseline と appcontainer の両方を覆う（管理者操作と片付けを 1 回にするため）。
    判定は `judge.judge_testnet` が行い、ここは観測と片付けだけを担う。解析は `testnet_parse.py`（純粋関数）。

管理者操作（オーナー承認 2026-11-05・#1427。Windows の xos-measure ジョブだけが `--appcontainer-testnet-observe` で呼ぶ）:
    使うのは `pktmon`（filter add / remove / list、start / stop / status、etl2pcap）と `netsh wfp`
    （show options / set options netevents / show netevents / show filters）だけ。`netsh wfp capture` は使わない。
    他人の状態を壊さないため、採取の前に pktmon が動いていない・filter が無いことを確かめ、そうでなければ何もしない
    （理由つきの unavailable）。netevents は元の設定を記録して on にし、終わりに元へ戻して確認する。

外部コマンドの規則: 実行ファイルは `<SystemRoot>\\System32\\` 配下の絶対パス・固定 argv・`shell=False`。動く部分は
    ハーネスが作った一時ディレクトリのパスと定数だけ。標準出力・標準エラーはパイプではなく一時ファイルへ書き（kill 後に孫が
    握るハンドルで読み取りが戻らなくなるのを避ける）、上限つきで読み、timeout を超えたら木ごと止める。

生データ（etl・pcapng・net events の xml・filters の xml）: アーティファクトの外の一時ディレクトリ（`--out` の外）に置き、
    集計後に必ず削除して `raw_removed` に成否を記録する。結果に出すのは宛先で絞った件数・種類と形の診断だけ。

テスト: `run` と `sysroot` を差し替えられるため、Linux でも偽のコマンド実行で順序と失敗時の記録を自己テストできる
    （`test_testnet_observe.py`）。win32 のコマンドは関数の中でだけ組み立てる。
"""

from __future__ import annotations

import json
import shutil
import subprocess
import tempfile
from pathlib import Path
from typing import Any, Callable

import testnet
import testnet_parse as parse

CMD_TIMEOUT_S = 60
SHORT_TIMEOUT_S = 20
MAX_CMD_OUT_BYTES = 4 * 1024 * 1024
MAX_XML_BYTES = 256 * 1024 * 1024
MAX_PCAP_BYTES = 512 * 1024 * 1024
MAX_DIAG_LINES = 12

RunFn = Callable[..., dict[str, Any]]


def _kill_tree(proc: "subprocess.Popen[bytes]", sysroot: str) -> None:
    """taskkill /T /F で子孫ごと止める（孫が継承ハンドルを握ったまま残ることがある）。待ちは短く打ち切る。"""
    exe = sysroot.rstrip("\\/") + "\\System32\\taskkill.exe"
    try:
        killer = subprocess.Popen([exe, "/T", "/F", "/PID", str(int(proc.pid))],  # noqa: S603
                                  stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            killer.wait(timeout=10)
        except subprocess.TimeoutExpired:
            killer.kill()
    except OSError:
        pass
    try:
        proc.kill()
    except OSError:
        pass
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        pass


def run_cmd(args: list[str], *, cwd: Path | None = None, timeout: int = CMD_TIMEOUT_S,
            sysroot: str = "C:\\Windows") -> dict[str, Any]:
    """外部コマンドを実行し {rc, out, error} を返す。例外は呼び出し側へ出さない（timeout は rc=None・error="TimeoutExpired"）。"""
    try:
        with tempfile.TemporaryFile() as fo, tempfile.TemporaryFile() as fe:
            try:
                proc = subprocess.Popen(args, stdin=subprocess.DEVNULL, stdout=fo, stderr=fe,  # noqa: S603
                                        cwd=str(cwd) if cwd else None, shell=False)
            except OSError as exc:
                return {"rc": None, "out": "", "error": type(exc).__name__}
            error = None
            rc: int | None
            try:
                rc = proc.wait(timeout=max(1, timeout))
            except subprocess.TimeoutExpired:
                _kill_tree(proc, sysroot)
                rc, error = None, "TimeoutExpired"
            parts = []
            for f in (fo, fe):
                f.seek(0)
                parts.append(f.read(MAX_CMD_OUT_BYTES))
            return {"rc": rc, "out": b"\n".join(parts).decode("utf-8", errors="replace"), "error": error}
    except OSError as exc:
        return {"rc": None, "out": "", "error": type(exc).__name__}


def _shapes(text: str, limit: int = MAX_DIAG_LINES) -> list[str]:
    """出力の形（数字・IP・GUID・SID の値を潰した先頭行）。空行は除く。"""
    return [parse.shape(x, 100) for x in text.splitlines() if x.strip()][:limit]


class TestnetObserver:
    """pktmon と WFP net events の採取。`start` -> `stop_and_collect` -> `cleanup` の順で使い、cleanup は必ず呼ぶ。

    すべての段の失敗は例外にせず `unavailable`（理由 1 つ）と `diag` に記録する。呼び出し元は `measure.main`
    （モードのループの外側を try/finally で包む）。
    """

    def __init__(self, *, sysroot: str, run: RunFn | None = None, workdir: Path | None = None) -> None:
        self.sysroot = sysroot.rstrip("\\/")
        self._run_impl = run
        self._own_workdir = workdir is None
        self.workdir = workdir or Path(tempfile.mkdtemp(prefix="egress-testnet-raw-"))
        self.etl = self.workdir / "pm.etl"
        self.pcap = self.workdir / "pktmon.pcapng"
        self.unavailable: str | None = None
        self.diag: dict[str, Any] = {}
        self.scaffold: dict[str, Any] = {}
        self.filter_names: dict[str, str] = {}
        self._pm_start_attempted = False
        self._pm_filters_attempted = False
        self._wfp_set_attempted = False
        self._wfp_orig_on: bool | None = None
        self._cleanup_done: dict[str, Any] | None = None

    # --- 外部コマンド ---------------------------------------------------------------------------------------
    def _sys32(self, name: str) -> str:
        return self.sysroot + "\\System32\\" + name

    def _exec(self, args: list[str], timeout: int = CMD_TIMEOUT_S) -> dict[str, Any]:
        if self._run_impl is not None:
            return self._run_impl(args, cwd=self.workdir, timeout=timeout)
        return run_cmd(args, cwd=self.workdir, timeout=timeout, sysroot=self.sysroot)

    def _pm(self, *args: str, timeout: int = CMD_TIMEOUT_S) -> dict[str, Any]:
        return self._exec([self._sys32("pktmon.exe"), *args], timeout)

    def _wfp(self, *args: str, timeout: int = CMD_TIMEOUT_S) -> dict[str, Any]:
        return self._exec([self._sys32("netsh.exe"), "wfp", *args], timeout)

    def _ps_json(self, script: str) -> Any:
        """固定の PowerShell スクリプトを実行して JSON を得る。失敗は {"error": ...}。"""
        exe = self.sysroot + "\\System32\\WindowsPowerShell\\v1.0\\powershell.exe"
        r = self._exec([exe, "-NoProfile", "-NonInteractive", "-Command", f"{script} | ConvertTo-Json -Compress"])
        if r["rc"] != 0:
            return {"error": r["error"] or f"rc={r['rc']}"}
        text = r["out"].strip()
        try:
            return json.loads(text.splitlines()[0]) if text else None
        except ValueError:
            return {"error": "unparsable output"}

    def _fail(self, reason: str) -> None:
        if self.unavailable is None:
            self.unavailable = reason

    # --- 足場の確認（読み取りのみ） --------------------------------------------------------------------------
    def check_scaffold(self) -> None:
        """firewall・BFE・AppContainer 既定の遮断 filter が想定どおりかを確かめる。欠ければ unavailable。"""
        sc = self.scaffold
        profile = self._ps_json("Get-NetConnectionProfile | Select-Object "
                                "@{n='NetworkCategory';e={$_.NetworkCategory.ToString()}}")
        fw = self._ps_json("Get-NetFirewallProfile | Select-Object Name,@{n='Enabled';e={$_.Enabled.ToString()}}")
        svc = self._ps_json("Get-Service mpssvc,bfe | Select-Object Name,@{n='Status';e={$_.Status.ToString()}}")
        cats = [str(x.get("NetworkCategory")) for x in _as_list(profile) if isinstance(x, dict) and "NetworkCategory" in x]
        mapped = {"DomainAuthenticated": "Domain"}
        names = {mapped.get(c, c) for c in cats}
        fw_enabled = {str(x.get("Name")): str(x.get("Enabled")).lower() in ("true", "1")
                      for x in _as_list(fw) if isinstance(x, dict) and "Name" in x}
        sc["network_categories"] = sorted(names)
        sc["firewall_enabled_for_category"] = bool(names) and all(fw_enabled.get(n) is True for n in names)
        running = {str(x.get("Name")).lower(): str(x.get("Status")).lower() == "running"
                   for x in _as_list(svc) if isinstance(x, dict) and "Name" in x}
        sc["services_running"] = running.get("mpssvc") is True and running.get("bfe") is True
        filters = self._snapshot_filters()
        sc["wfp_filters_total"] = filters.get("total")
        sc["wfp_scaffold_filter_matches"] = filters.get("pattern_counts")
        sc["wfp_scaffold_filter_present"] = sum((filters.get("pattern_counts") or {}).values()) >= 1
        if not names:
            self._fail("scaffold: network connection profile not found")
        elif not sc["firewall_enabled_for_category"]:
            self._fail("scaffold: firewall profile for the connection category is not enabled")
        elif not sc["services_running"]:
            self._fail("scaffold: mpssvc/bfe are not both running")
        elif not sc["wfp_scaffold_filter_present"]:
            self._fail("scaffold: AppContainer default outbound block filters not found")

    def _snapshot_filters(self) -> dict[str, Any]:
        f = self.workdir / "filters.xml"
        r = self._wfp("show", "filters", f"file={f}")
        res: dict[str, Any] = {"rc": r["rc"], "total": None, "pattern_counts": {}}
        try:
            if r["rc"] == 0 and f.is_file() and f.stat().st_size <= MAX_XML_BYTES:
                parsed = parse.parse_filters_xml(str(f))
                self.filter_names.update(parsed["names"])
                res.update({"total": parsed["total"], "pattern_counts": parsed["pattern_counts"]})
                if parsed["diag"]:
                    res["diag"] = parsed["diag"]
        except OSError as exc:
            res["error"] = type(exc).__name__
        finally:
            _unlink(f)
        return res

    # --- 開始 ------------------------------------------------------------------------------------------------
    def start(self) -> None:
        """足場と既存状態を確かめ、pktmon の filter 追加・採取開始と netevents=on を行う。失敗は unavailable に残す。"""
        d = self.diag
        self.check_scaffold()
        status = self._pm("status", timeout=SHORT_TIMEOUT_S)
        flist = self._pm("filter", "list", timeout=SHORT_TIMEOUT_S)
        d["pktmon_status_shape"] = _shapes(status["out"])
        d["pktmon_filter_list_shape"] = _shapes(flist["out"])
        running = parse.parse_pktmon_running(status["out"]) if status["rc"] == 0 else None
        empty = parse.parse_pktmon_filters_empty(flist["out"]) if flist["rc"] == 0 else None
        before = self._wfp("show", "options", "optionsfor=netevents", timeout=SHORT_TIMEOUT_S)
        orig = parse.parse_netevents_option(before["out"]) if before["rc"] == 0 else None
        d["wfp_options_before_shape"] = _shapes(before["out"], 6)
        self._wfp_orig_on = orig
        if running is None:
            self._fail("pktmon state could not be determined (status output not recognized)")
        elif running:
            self._fail("pktmon was already running before the measurement (left untouched)")
        elif empty is None:
            self._fail("pktmon filter state could not be determined (filter list output not recognized)")
        elif not empty:
            self._fail("pktmon filters already existed before the measurement (left untouched)")
        elif orig is None:
            self._fail("WFP netevents state could not be determined (show options output not recognized)")
        if self.unavailable is not None:
            return
        d["wfp_netevents_originally_on"] = orig
        self._pm_filters_attempted = True
        add = self._pm("filter", "add", "testnet", "-i", str(testnet.TESTNET_NET), timeout=SHORT_TIMEOUT_S)
        d["pktmon_filter_add_rc"] = add["rc"]
        if add["rc"] != 0:
            self._fail("pktmon filter add failed")
            return
        self._pm_start_attempted = True
        r = self._pm("start", "--capture", "--comp", "all", "--pkt-size", "0", "--file-name", str(self.etl))
        if r["rc"] != 0:
            d["pktmon_start_comp_all_rc"] = r["rc"]
            r = self._pm("start", "--capture", "--file-name", str(self.etl))
        d["pktmon_start_rc"] = r["rc"]
        if r["rc"] != 0:
            self._fail("pktmon start failed")
            return
        self._wfp_set_attempted = True  # 付与を試みたら、部分適用を除外できないため復元も必ず試す
        on = self._wfp("set", "options", "netevents=on")
        d["wfp_set_on_rc"] = on["rc"]
        if on["rc"] != 0:
            self._fail("netsh wfp set options netevents=on failed")

    # --- 停止と集計 ------------------------------------------------------------------------------------------
    def stop_and_collect(self, profile_sid: str | None) -> dict[str, Any]:
        """net events を読み出し、pktmon を止めて pcapng を解析する。宛先で絞った集計だけを返す（生データは返さない）。"""
        d = self.diag
        index = testnet.port_index()
        result: dict[str, Any] = {"pktmon": {"routes": {}, "diag": {}}, "wfp": {"routes": {}, "diag": {}}}
        if self.unavailable is None:
            xml_path = self.workdir / "netevents.xml"
            r = self._wfp("show", "netevents", f"file={xml_path}")
            d["wfp_show_netevents_rc"] = r["rc"]
            try:
                if r["rc"] != 0 or not xml_path.is_file():
                    self._fail("netsh wfp show netevents failed")
                elif xml_path.stat().st_size > MAX_XML_BYTES:
                    self._fail("netevents xml exceeds the size limit")
                else:
                    result["wfp"] = parse.parse_wfp_events(str(xml_path), index, profile_sid)
                    if "parse_error" in result["wfp"]["diag"]:
                        self._fail("netevents xml could not be parsed")
            except OSError:
                self._fail("netevents xml could not be read")
            finally:
                _unlink(xml_path)
            for ent in result["wfp"]["routes"].values():
                ent["filter_names"] = {fid: self.filter_names.get(fid, "?") for fid in ent.get("filters", {})}
        if self._pm_start_attempted:
            stop = self._pm("stop")
            d["pktmon_stop_rc"] = stop["rc"]
            if self.unavailable is None:
                conv = self._pm("etl2pcap", str(self.etl), "--out", str(self.pcap))
                if conv["rc"] != 0 or not self.pcap.exists():
                    d["pktmon_etl2pcap_long_rc"] = conv["rc"]
                    conv = self._pm("etl2pcap", str(self.etl), "-o", str(self.pcap))
                d["pktmon_etl2pcap_rc"] = conv["rc"]
                try:
                    if conv["rc"] != 0 or not self.pcap.exists():
                        self._fail("pktmon etl2pcap failed")
                    elif self.pcap.stat().st_size > MAX_PCAP_BYTES:
                        self._fail("pktmon pcapng exceeds the size limit")
                    else:
                        with self.pcap.open("rb") as f:
                            result["pktmon"] = parse.parse_pcapng(f, index)
                        if "error" in result["pktmon"]["diag"]:
                            self._fail(f"pktmon pcapng could not be parsed ({result['pktmon']['diag']['error']})")
                except OSError:
                    self._fail("pktmon pcapng could not be read")
        return {"unavailable": self.unavailable, "scaffold": self.scaffold, "diag": d,
                "pktmon": result["pktmon"], "wfp": result["wfp"]}

    # --- 片付け ----------------------------------------------------------------------------------------------
    def cleanup(self) -> dict[str, Any]:
        """pktmon の停止・filter の削除、netevents の復元、生データの削除。成否（True が成功）を返す。何度呼んでもよい。

        開始していない操作は対象外（True）。試みた操作は、状態を読み直して確かめられたときだけ True にする。
        """
        if self._cleanup_done is not None:
            return self._cleanup_done
        c: dict[str, Any] = {}
        if self._pm_start_attempted:
            self._pm("stop", timeout=SHORT_TIMEOUT_S)
            st = self._pm("status", timeout=SHORT_TIMEOUT_S)
            c["pktmon_stopped"] = st["rc"] == 0 and parse.parse_pktmon_running(st["out"]) is False
        else:
            c["pktmon_stopped"] = True
        if self._pm_filters_attempted:
            self._pm("filter", "remove", timeout=SHORT_TIMEOUT_S)
            fl = self._pm("filter", "list", timeout=SHORT_TIMEOUT_S)
            c["pktmon_filters_removed"] = fl["rc"] == 0 and parse.parse_pktmon_filters_empty(fl["out"]) is True
        else:
            c["pktmon_filters_removed"] = True
        if self._wfp_set_attempted and self._wfp_orig_on is not None:
            want = "on" if self._wfp_orig_on else "off"
            self._wfp("set", "options", f"netevents={want}", timeout=30)
            after = self._wfp("show", "options", "optionsfor=netevents", timeout=30)
            c["netevents_restored"] = (after["rc"] == 0
                                       and parse.parse_netevents_option(after["out"]) is self._wfp_orig_on)
        else:
            c["netevents_restored"] = not self._wfp_set_attempted
        if self._own_workdir:
            shutil.rmtree(self.workdir, ignore_errors=True)
            c["raw_removed"] = not self.workdir.exists()
        else:
            for f in (self.etl, self.pcap):
                _unlink(f)
            c["raw_removed"] = not (self.etl.exists() or self.pcap.exists())
        self._cleanup_done = c
        return c


def cleanup_reason(cleanup: dict[str, Any] | None) -> str | None:
    """片付けが不完全なら pass を妨げる理由、完全なら None（未実施・欠落も不完全として扱う）。"""
    keys = ("pktmon_stopped", "pktmon_filters_removed", "netevents_restored", "raw_removed")
    if not isinstance(cleanup, dict) or any(cleanup.get(k) is not True for k in keys):
        return "TEST-NET observation cleanup was incomplete or not recorded; result is not trusted"
    return None


def _as_list(value: Any) -> list[Any]:
    if isinstance(value, list):
        return value
    return [value] if value is not None else []


def _unlink(path: Path) -> None:
    try:
        path.unlink()
    except OSError:
        pass
