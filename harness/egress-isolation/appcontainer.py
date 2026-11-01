#!/usr/bin/env python3
"""Windows の AppContainer で内側ランナーを起動するランチャー（ctypes。標準ライブラリのみ）。

役割・責務境界:
    Issue #1396（TASK-38・`RENDER-3`・`SEC-2`・`XOS-1`。親判断 #1003・設計 `docs/design/servo-host-egress-isolation.md` §3.3・§7）。
    `measure.py` の Windows `appcontainer` モードが、capability を持たない AppContainer の中で
    `in_sandbox.py`（ひいては Servo ホスト）を起動するための Win32 FFI を閉じ込める。
    オーナー承認（2026-11-01）の範囲は「`harness/egress-isolation/` の Python 計測ハーネスに限る ctypes の Win32 FFI」。
    本体 crate（Rust）への FFI・`unsafe` の追加、依存の追加、管理者権限が要る操作（`CheckNetIsolation` 等）は範囲外で、行わない。

方式:
    - AppContainer プロファイルを作り、`PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` で `CreateProcessW` する。
      容器から読めるのは AppContainer 専用フォルダ（`GetAppContainerFolderPath`）だけなので、Python ランタイム・
      ハーネス・ホスト（exe と同階層の DLL）をそこへ複製して使う。toolcache・チェックアウト・`D:\\sr` の ACL は変えない。
      専用フォルダのパスが取れない場合だけ、ハーネスが作った一時ディレクトリに `icacls` で当該 SID の権限と継承する low integrity ラベルを付与し、
      finally で除去する（フォールバック）。
    - プロセスは一時停止で作って Job（`KILL_ON_JOB_CLOSE`）に入れてから再開する。タイムアウト・失敗時もツリーごと止まる。
    - 環境変数は許可リストのみ（`SystemRoot`・`TMP`・`TEMP`・`PYTHONIOENCODING`。`PATH`・`*_PROXY` は渡さない。`SEC-2`）。
    - プロファイル・ハンドル・属性リスト・SID・複製先は入れ子の `try/finally` で必ず片付け、各段の成否を `cleanup` に記録する。
      片付けの失敗は握りつぶさず記録し、判定を合格側へ倒さない。

非 Windows: 構造体・純粋関数は import できる（自己テスト用）。Win32 API の読み込みは Windows のときだけ関数内で行う。
呼び出し文脈: `measure.py`（`run_appcontainer_mode`）、`test_appcontainer.py`。
"""

from __future__ import annotations

import ctypes
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any

PROFILE_NAME_RE = re.compile(r"^[a-z0-9][a-z0-9._-]{1,63}$")
SID_RE = re.compile(r"^S-1-15-2(-\d+){7}$")
CAPABILITY_SID_RE = re.compile(r"^S-1-15-3-\d{1,3}$")
# capability SID（well-known）。preflight の対照でのみ使う。本体の実行では 1 件も付けない。
CAP_INTERNET_CLIENT = "S-1-15-3-1"
CAP_PRIVATE_NETWORK = "S-1-15-3-3"
NETWORK_CAPABILITIES = (CAP_INTERNET_CLIENT, CAP_PRIVATE_NETWORK)

MAX_RESULT_BYTES = 1024 * 1024
STDERR_TAIL_BYTES = 400
DEFAULT_TIMEOUT_S = 240
ENV_ALLOWLIST = ("SystemRoot",)

PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES = 0x00020009
PROC_THREAD_ATTRIBUTE_HANDLE_LIST = 0x00020002
EXTENDED_STARTUPINFO_PRESENT = 0x00080000
CREATE_SUSPENDED = 0x00000004
CREATE_UNICODE_ENVIRONMENT = 0x00000400
CREATE_NO_WINDOW = 0x08000000
STARTF_USESTDHANDLES = 0x00000100
SE_GROUP_ENABLED = 0x00000004
JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE = 0x00002000
JobObjectExtendedLimitInformation = 9
WAIT_OBJECT_0 = 0
WAIT_TIMEOUT = 0x102
ERROR_INSUFFICIENT_BUFFER = 122
HRESULT_ALREADY_EXISTS = -2147024713  # HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS) = 0x800700B7

# 複製から除くディレクトリ名（実行に不要で大きいもの）。
LIB_EXCLUDED_DIRS = frozenset({"site-packages", "test", "tests", "idlelib", "tkinter", "ensurepip",
                               "turtledemo", "lib2to3", "__pycache__"})


class AppContainerError(RuntimeError):
    """ランチャーの失敗（`measure.py` がモード全体を理由つきの `unobservable` にする）。"""


# --- 構造体（固定幅。x64 のサイズは test_appcontainer.py が確かめる） ----------------------------------
class STARTUPINFOW(ctypes.Structure):
    _fields_ = [
        ("cb", ctypes.c_uint32), ("lpReserved", ctypes.c_void_p), ("lpDesktop", ctypes.c_void_p),
        ("lpTitle", ctypes.c_void_p), ("dwX", ctypes.c_uint32), ("dwY", ctypes.c_uint32),
        ("dwXSize", ctypes.c_uint32), ("dwYSize", ctypes.c_uint32), ("dwXCountChars", ctypes.c_uint32),
        ("dwYCountChars", ctypes.c_uint32), ("dwFillAttribute", ctypes.c_uint32), ("dwFlags", ctypes.c_uint32),
        ("wShowWindow", ctypes.c_uint16), ("cbReserved2", ctypes.c_uint16), ("lpReserved2", ctypes.c_void_p),
        ("hStdInput", ctypes.c_void_p), ("hStdOutput", ctypes.c_void_p), ("hStdError", ctypes.c_void_p),
    ]


class STARTUPINFOEXW(ctypes.Structure):
    _fields_ = [("StartupInfo", STARTUPINFOW), ("lpAttributeList", ctypes.c_void_p)]


class PROCESS_INFORMATION(ctypes.Structure):  # noqa: N801
    _fields_ = [("hProcess", ctypes.c_void_p), ("hThread", ctypes.c_void_p),
                ("dwProcessId", ctypes.c_uint32), ("dwThreadId", ctypes.c_uint32)]


class SID_AND_ATTRIBUTES(ctypes.Structure):  # noqa: N801
    _fields_ = [("Sid", ctypes.c_void_p), ("Attributes", ctypes.c_uint32)]


class SECURITY_CAPABILITIES(ctypes.Structure):  # noqa: N801
    _fields_ = [("AppContainerSid", ctypes.c_void_p), ("Capabilities", ctypes.c_void_p),
                ("CapabilityCount", ctypes.c_uint32), ("Reserved", ctypes.c_uint32)]


class JOBOBJECT_BASIC_LIMIT_INFORMATION(ctypes.Structure):  # noqa: N801
    _fields_ = [("PerProcessUserTimeLimit", ctypes.c_int64), ("PerJobUserTimeLimit", ctypes.c_int64),
                ("LimitFlags", ctypes.c_uint32), ("MinimumWorkingSetSize", ctypes.c_size_t),
                ("MaximumWorkingSetSize", ctypes.c_size_t), ("ActiveProcessLimit", ctypes.c_uint32),
                ("Affinity", ctypes.c_size_t), ("PriorityClass", ctypes.c_uint32),
                ("SchedulingClass", ctypes.c_uint32)]


class IO_COUNTERS(ctypes.Structure):  # noqa: N801
    _fields_ = [(n, ctypes.c_uint64) for n in ("ReadOperationCount", "WriteOperationCount", "OtherOperationCount",
                                               "ReadTransferCount", "WriteTransferCount", "OtherTransferCount")]


class JOBOBJECT_EXTENDED_LIMIT_INFORMATION(ctypes.Structure):  # noqa: N801
    _fields_ = [("BasicLimitInformation", JOBOBJECT_BASIC_LIMIT_INFORMATION), ("IoInfo", IO_COUNTERS),
                ("ProcessMemoryLimit", ctypes.c_size_t), ("JobMemoryLimit", ctypes.c_size_t),
                ("PeakProcessMemoryUsed", ctypes.c_size_t), ("PeakJobMemoryUsed", ctypes.c_size_t)]


# --- 純粋関数（全 OS で自己テストできる） -------------------------------------------------------------
def validate_profile_name(name: object) -> str:
    """プロファイル名の検証（3〜64 文字の小文字英数字と `.` `_` `-`）。外部入力から作らず固定形式で組み立てる。"""
    if not isinstance(name, str) or not PROFILE_NAME_RE.match(name):
        raise ValueError("invalid AppContainer profile name")
    return name


def profile_name(pid: int) -> str:
    """実行ごとのプロファイル名 `fandhe.egressiso.<pid>`。"""
    if isinstance(pid, bool) or not isinstance(pid, int) or pid < 0:
        raise ValueError("pid must be a non-negative int")
    return validate_profile_name(f"fandhe.egressiso.{pid}")


def icacls_grant_args(root: Path, sid: str) -> list[str]:
    """フォールバックの専用ディレクトリへ AppContainer SID の Modify（継承）を付与する `icacls` 引数。"""
    return ["icacls", str(root), "/grant", f"*{validate_sid_string(sid)}:(OI)(CI)(M)"]


def icacls_low_integrity_args(root: Path) -> list[str]:
    """専用ディレクトリへ子に継承する low integrity ラベル（S-1-16-4096）を付ける `icacls` 引数。"""
    return ["icacls", str(root), "/setintegritylevel", "(OI)(CI)L"]


def validate_sid_string(sid: object) -> str:
    """AppContainer SID 文字列（`S-1-15-2-` + 7 要素）の検証。`icacls` へ渡す前の必須検査。"""
    if not isinstance(sid, str) or not SID_RE.match(sid):
        raise ValueError("invalid AppContainer SID string")
    return sid


def build_environment_block(env: dict[str, str]) -> str:
    """名前でソートし、`NAME=VALUE\\0` を連ねて二重 NUL で終える環境ブロック（UTF-16 にする前の文字列）。"""
    parts = []
    for name in sorted(env):
        if "=" in name or "\0" in name or "\0" in env[name]:
            raise ValueError("invalid environment entry")
        parts.append(f"{name}={env[name]}\0")
    return "".join(parts) + "\0"


def container_env(staging_tmp: str, system_root: str | None) -> dict[str, str]:
    """容器へ渡す環境変数（許可リスト）。`PATH`・`*_PROXY`・ユーザー由来の変数は渡さない（`SEC-2`）。"""
    env = {"TMP": staging_tmp, "TEMP": staging_tmp, "PYTHONIOENCODING": "utf-8"}
    if system_root:
        env["SystemRoot"] = system_root
    return env


def command_line(argv: list[str]) -> str:
    """`CreateProcessW` のコマンドライン。argv は検証済みの絶対パス・固定引数・int ポートだけを受け取る。"""
    if not argv or any((not isinstance(a, str)) or "\0" in a for a in argv):
        raise ValueError("invalid argv")
    return subprocess.list2cmdline(argv)


def lib_ignore(_dir: str, names: list[str]) -> set[str]:
    """`shutil.copytree` の ignore。標準ライブラリの複製から不要なディレクトリを除く。"""
    return {n for n in names if n in LIB_EXCLUDED_DIRS or n.endswith(".pyc")}


def harness_sources(harness_dir: Path) -> list[tuple[Path, str]]:
    """複製するハーネスの `.py`（テストを除く）。(元パス, harness_dir からの相対パス)。"""
    out: list[tuple[Path, str]] = []
    for sub in ("egress-isolation", "render-screenshot"):
        for p in sorted((harness_dir / sub).glob("*.py")):
            if p.name.startswith("test_"):
                continue
            out.append((p, f"{sub}/{p.name}"))
    return out


def read_capped(path: Path, cap: int = MAX_RESULT_BYTES) -> bytes:
    """結果ファイルを上限つきで読む。上限超過・読めないときは ValueError（巨大ファイルでメモリを使わない）。"""
    try:
        size = path.stat().st_size
        if size > cap:
            raise ValueError("result file exceeds the size cap")
        with path.open("rb") as f:
            data = f.read(cap + 1)
    except OSError as exc:
        raise ValueError(f"result file unreadable ({type(exc).__name__})") from exc
    if len(data) > cap:
        raise ValueError("result file exceeds the size cap")
    return data


def tail_text(path: Path, n: int = STDERR_TAIL_BYTES) -> str:
    """ファイル末尾 n バイトだけを読む。読めなければ空文字列。"""
    try:
        with path.open("rb") as f:
            f.seek(0, os.SEEK_END)
            f.seek(max(0, f.tell() - n))
            return f.read(n).decode("utf-8", errors="replace")
    except OSError:
        return ""


# --- Win32 API（Windows のみ。関数内で読み込む） --------------------------------------------------------
def _api() -> Any:
    if sys.platform != "win32":
        raise AppContainerError("AppContainer is available on Windows only")
    from types import SimpleNamespace

    k32 = ctypes.WinDLL("kernel32", use_last_error=True)  # type: ignore[attr-defined]
    adv = ctypes.WinDLL("advapi32", use_last_error=True)  # type: ignore[attr-defined]
    usr = ctypes.WinDLL("userenv", use_last_error=True)  # type: ignore[attr-defined]
    ole = ctypes.WinDLL("ole32", use_last_error=True)  # type: ignore[attr-defined]
    vp, u32, i32, wp = ctypes.c_void_p, ctypes.c_uint32, ctypes.c_int32, ctypes.c_wchar_p
    pvp = ctypes.POINTER(ctypes.c_void_p)

    def sig(fn: Any, args: list[Any], res: Any) -> Any:
        fn.argtypes, fn.restype = args, res
        return fn

    return SimpleNamespace(
        CreateAppContainerProfile=sig(usr.CreateAppContainerProfile, [wp, wp, wp, vp, u32, pvp], i32),
        DeriveAppContainerSidFromAppContainerName=sig(usr.DeriveAppContainerSidFromAppContainerName,
                                                      [wp, pvp], i32),
        DeleteAppContainerProfile=sig(usr.DeleteAppContainerProfile, [wp], i32),
        GetAppContainerFolderPath=sig(usr.GetAppContainerFolderPath, [wp, pvp], i32),
        ConvertSidToStringSidW=sig(adv.ConvertSidToStringSidW, [vp, pvp], i32),
        ConvertStringSidToSidW=sig(adv.ConvertStringSidToSidW, [wp, pvp], i32),
        FreeSid=sig(adv.FreeSid, [vp], vp),
        LocalFree=sig(k32.LocalFree, [vp], vp),
        CoTaskMemFree=sig(ole.CoTaskMemFree, [vp], None),
        InitializeProcThreadAttributeList=sig(
            k32.InitializeProcThreadAttributeList, [vp, u32, u32, ctypes.POINTER(ctypes.c_size_t)], i32),
        UpdateProcThreadAttribute=sig(
            k32.UpdateProcThreadAttribute, [vp, u32, ctypes.c_size_t, vp, ctypes.c_size_t, vp, vp], i32),
        DeleteProcThreadAttributeList=sig(k32.DeleteProcThreadAttributeList, [vp], None),
        CreateProcessW=sig(k32.CreateProcessW, [wp, vp, vp, vp, i32, u32, vp, wp, vp, vp], i32),
        CreateJobObjectW=sig(k32.CreateJobObjectW, [vp, vp], vp),
        SetInformationJobObject=sig(k32.SetInformationJobObject, [vp, i32, vp, u32], i32),
        AssignProcessToJobObject=sig(k32.AssignProcessToJobObject, [vp, vp], i32),
        ResumeThread=sig(k32.ResumeThread, [vp], u32),
        WaitForSingleObject=sig(k32.WaitForSingleObject, [vp, u32], u32),
        GetExitCodeProcess=sig(k32.GetExitCodeProcess, [vp, ctypes.POINTER(ctypes.c_uint32)], i32),
        TerminateJobObject=sig(k32.TerminateJobObject, [vp, u32], i32),
        CloseHandle=sig(k32.CloseHandle, [vp], i32),
        TerminateProcess=sig(k32.TerminateProcess, [vp, u32], i32),
    )


def _wstr_from_ptr(ptr: Any) -> str:
    return ctypes.wstring_at(ptr)


class AppContainerSession:
    """1 つの AppContainer プロファイルと複製先（staging）を持ち、同じ容器で複数回起動できる文脈。

    `measure.py` が preflight（capability 0 件・あり）と本体の実行を同じプロファイル・同じ複製先で行う。
    `__exit__` が必ず片付け、結果は `cleanup` に残る。
    """

    def __init__(self, *, host: Path, harness_dir: Path, timeout_s: int = DEFAULT_TIMEOUT_S) -> None:
        self.host = host
        self.harness_dir = harness_dir
        self.timeout_s = timeout_s
        self.name = profile_name(os.getpid())
        self.sid_string: str | None = None
        self.staging_mode: str | None = None
        self.staging: Path | None = None
        self.cleanup: dict[str, Any] = {}
        self._api: Any = None
        self._sid = ctypes.c_void_p(None)
        self._created = False
        self._granted = False
        self._root: Path | None = None  # 片付けで丸ごと削除する複製先の根

    # 複製先の中のパス（容器内で使う）。
    @property
    def python_path(self) -> str:
        return str(self._need_staging() / "py" / "python.exe")

    @property
    def host_path(self) -> str:
        return str(self._need_staging() / "host" / self.host.name)

    def script_path(self, rel: str) -> str:
        return str(self._need_staging() / "harness" / rel)

    def out_path(self, name: str) -> str:
        if not re.fullmatch(r"[a-z0-9_.-]{1,64}", name):
            raise ValueError("invalid result name")
        return str(self._need_staging() / "out" / name)

    def _need_staging(self) -> Path:
        if self.staging is None:
            raise AppContainerError("staging is not prepared")
        return self.staging

    def __enter__(self) -> "AppContainerSession":
        self._api = _api()
        try:
            self._create_profile()
            self._prepare_staging()
        except BaseException:
            self.__exit__(None, None, None)
            raise
        return self

    def __exit__(self, *exc: object) -> None:
        c = self.cleanup
        if self._root is not None:
            shutil.rmtree(self._root, ignore_errors=True)
            c["staging_removed"] = not self._root.exists()
        if self._granted and self.sid_string:
            c["acl_removed"] = self._icacls_remove()
        if self._created and self._api is not None:
            hr = self._api.DeleteAppContainerProfile(self.name)
            c["profile_deleted"] = hr >= 0
        if self._sid.value and self._api is not None:
            self._api.FreeSid(self._sid)
            self._sid = ctypes.c_void_p(None)
            c["sid_freed"] = True

    def _create_profile(self) -> None:
        a = self._api
        hr = a.CreateAppContainerProfile(self.name, "fandhe egress isolation", "egress isolation measurement",
                                         None, 0, ctypes.byref(self._sid))
        if hr == HRESULT_ALREADY_EXISTS:
            # 過去の強制終了の残骸。削除して作り直す。
            a.DeleteAppContainerProfile(self.name)
            self._sid = ctypes.c_void_p(None)
            hr = a.CreateAppContainerProfile(self.name, "fandhe egress isolation", "egress isolation measurement",
                                             None, 0, ctypes.byref(self._sid))
        if hr < 0:
            raise AppContainerError(f"CreateAppContainerProfile failed hr=0x{hr & 0xFFFFFFFF:08x}")
        self._created = True
        out = ctypes.c_void_p(None)
        if not a.ConvertSidToStringSidW(self._sid, ctypes.byref(out)):
            raise AppContainerError("ConvertSidToStringSidW failed")
        try:
            self.sid_string = validate_sid_string(_wstr_from_ptr(out))
        finally:
            a.LocalFree(out)

    def _prepare_staging(self) -> None:
        a = self._api
        assert self.sid_string is not None
        path = ctypes.c_void_p(None)
        hr = a.GetAppContainerFolderPath(self.sid_string, ctypes.byref(path))
        base: Path | None = None
        if hr >= 0 and path.value:
            try:
                base = Path(_wstr_from_ptr(path))
            finally:
                a.CoTaskMemFree(path)
        if base is not None and base.is_dir():
            self.staging_mode = "appcontainer-folder"
            self._root = Path(tempfile.mkdtemp(prefix="egress-", dir=base))
            # Python 3.13 以降の mkdtemp は mode=0o700 で継承を切った DACL を付けるため、
            # 親（AppContainer 専用フォルダ）の権限を継承せず容器が複製物を読めない。明示的に付与する。
            # 親フォルダは容器の integrity で書けるので low ラベルは付けない。
            self._icacls_grant(low_integrity=False)
        else:
            # フォールバック: ハーネスが作った一時ディレクトリへ当該 SID の権限だけを付与する。
            self.staging_mode = "icacls-grant"
            self._root = Path(tempfile.mkdtemp(prefix="egress-iso-"))
            self._icacls_grant()
        self.staging = self._root
        for sub in ("tmp", "out", "host", "py", "harness"):
            (self._root / sub).mkdir()
        self._copy_files()
        self.cleanup["staging_mode"] = self.staging_mode

    def _icacls_grant(self, *, low_integrity: bool = True) -> None:
        sid = validate_sid_string(self.sid_string)
        assert self._root is not None
        done = subprocess.run(icacls_grant_args(self._root, sid),  # noqa: S603,S607
                              stdin=subprocess.DEVNULL, capture_output=True, timeout=60, check=False)
        if done.returncode != 0:
            raise AppContainerError("icacls grant failed")
        self._granted = True
        if not low_integrity:
            return
        # 通常の一時ディレクトリは medium integrity で、low integrity の AppContainer からは DACL の許可があっても
        # Mandatory Integrity Control に書き込みを拒否される。子へ継承する low ラベルを付ける。
        done = subprocess.run(icacls_low_integrity_args(self._root),  # noqa: S603,S607
                              stdin=subprocess.DEVNULL, capture_output=True, timeout=60, check=False)
        if done.returncode != 0:
            raise AppContainerError("icacls setintegritylevel failed")

    def _icacls_remove(self) -> bool:
        sid = validate_sid_string(self.sid_string)
        assert self._root is not None
        try:
            done = subprocess.run(["icacls", str(self._root), "/remove", f"*{sid}"],  # noqa: S603,S607
                                  stdin=subprocess.DEVNULL, capture_output=True, timeout=60, check=False)
        except (OSError, subprocess.SubprocessError):
            return False
        return done.returncode == 0 or not self._root.exists()

    def _copy_files(self) -> None:
        assert self._root is not None
        base = Path(sys.base_prefix)
        py = self._root / "py"
        copied = 0
        for pattern in ("python.exe", "python3*.dll", "vcruntime*.dll"):
            for p in base.glob(pattern):
                shutil.copy2(p, py / p.name)
                copied += 1
        if not (py / "python.exe").is_file():
            raise AppContainerError("python.exe was not found under sys.base_prefix")
        for sub in ("DLLs", "Lib"):
            if (base / sub).is_dir():
                shutil.copytree(base / sub, py / sub, ignore=lib_ignore)
        for src, rel in harness_sources(self.harness_dir):
            dest = self._root / "harness" / rel
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(src, dest)
        hostdir = self.host.parent
        shutil.copy2(self.host, self._root / "host" / self.host.name)
        for dll in sorted(hostdir.glob("*.dll")):
            shutil.copy2(dll, self._root / "host" / dll.name)
        self.cleanup["staged_host_sha256"] = hashlib.sha256(
            (self._root / "host" / self.host.name).read_bytes()).hexdigest()

    def run(self, script_rel: str, script_args: list[str], *, capabilities: tuple[str, ...] = (),
            result_name: str | None = None, timeout_s: int | None = None) -> dict[str, Any]:
        """容器内で `python -I -B <script> <args>` を実行する。構造化 dict を返す（失敗は `error` に入る）。"""
        for cap in capabilities:
            if not CAPABILITY_SID_RE.match(cap):
                raise ValueError("invalid capability SID")
        result: dict[str, Any] = {"exit_code": None, "timed_out": False, "stderr_tail": "", "result_json": None,
                                  "error": None, "capabilities": list(capabilities)}
        try:
            self._launch(script_rel, script_args, capabilities, timeout_s or self.timeout_s, result)
        except AppContainerError as exc:
            result["error"] = str(exc)[:200]
        if result_name:
            try:
                result["result_json"] = json.loads(read_capped(Path(self.out_path(result_name))))
            except (ValueError, OSError):
                result["result_json"] = None
        return result

    def _launch(self, script_rel: str, script_args: list[str], capabilities: tuple[str, ...],
                timeout_s: int, result: dict[str, Any]) -> None:
        a = self._api
        staging = self._need_staging()
        argv = [self.python_path, "-I", "-B", self.script_path(script_rel), *script_args]
        cmd = ctypes.create_unicode_buffer(command_line(argv))
        env_block = ctypes.create_unicode_buffer(
            build_environment_block(container_env(str(staging / "tmp"), os.environ.get("SystemRoot"))), )
        stderr_path = staging / "stderr.txt"
        keep: list[Any] = []  # ctypes の参照を属性リストの生存期間中保持する
        files: list[Any] = []
        cap_sids: list[Any] = []
        attr_list = ctypes.create_string_buffer(0)
        attr_ready = False
        pi = PROCESS_INFORMATION()
        h = {"job": None, "proc": None, "thread": None}
        try:
            fin = open(os.devnull, "rb")  # noqa: SIM115
            fout = open(os.devnull, "wb")  # noqa: SIM115
            ferr = open(stderr_path, "wb")  # noqa: SIM115
            files += [fin, fout, ferr]
            import msvcrt  # type: ignore[import-not-found]

            handles = []
            for f in files:
                hnd = msvcrt.get_osfhandle(f.fileno())  # type: ignore[attr-defined]
                os.set_handle_inheritable(hnd, True)  # type: ignore[attr-defined]
                handles.append(hnd)
            handle_arr = (ctypes.c_void_p * len(handles))(*handles)
            caps_arr = (SID_AND_ATTRIBUTES * max(1, len(capabilities)))()
            for i, cap in enumerate(capabilities):
                psid = ctypes.c_void_p(None)
                if not a.ConvertStringSidToSidW(cap, ctypes.byref(psid)):
                    raise AppContainerError("ConvertStringSidToSidW failed")
                cap_sids.append(psid)
                caps_arr[i].Sid = psid
                caps_arr[i].Attributes = SE_GROUP_ENABLED
            sc = SECURITY_CAPABILITIES(self._sid, ctypes.cast(caps_arr, ctypes.c_void_p) if capabilities else None,
                                       len(capabilities), 0)
            keep += [handle_arr, caps_arr, sc]
            size = ctypes.c_size_t(0)
            a.InitializeProcThreadAttributeList(None, 2, 0, ctypes.byref(size))
            if size.value == 0:
                raise AppContainerError("InitializeProcThreadAttributeList size query failed")
            attr_list = ctypes.create_string_buffer(size.value)
            if not a.InitializeProcThreadAttributeList(attr_list, 2, 0, ctypes.byref(size)):
                raise AppContainerError("InitializeProcThreadAttributeList failed")
            attr_ready = True
            if not a.UpdateProcThreadAttribute(attr_list, 0, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
                                               ctypes.byref(sc), ctypes.sizeof(sc), None, None):
                raise AppContainerError("UpdateProcThreadAttribute(SECURITY_CAPABILITIES) failed")
            if not a.UpdateProcThreadAttribute(attr_list, 0, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                                               ctypes.byref(handle_arr), ctypes.sizeof(handle_arr), None, None):
                raise AppContainerError("UpdateProcThreadAttribute(HANDLE_LIST) failed")
            si = STARTUPINFOEXW()
            si.StartupInfo.cb = ctypes.sizeof(si)
            si.StartupInfo.dwFlags = STARTF_USESTDHANDLES
            si.StartupInfo.hStdInput, si.StartupInfo.hStdOutput, si.StartupInfo.hStdError = handles
            si.lpAttributeList = ctypes.cast(attr_list, ctypes.c_void_p)
            job = a.CreateJobObjectW(None, None)
            if not job:
                raise AppContainerError("CreateJobObjectW failed")
            h["job"] = job
            info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION()
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            if not a.SetInformationJobObject(job, JobObjectExtendedLimitInformation, ctypes.byref(info),
                                             ctypes.sizeof(info)):
                raise AppContainerError("SetInformationJobObject failed")
            flags = (EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT
                     | CREATE_NO_WINDOW)
            ok = a.CreateProcessW(self.python_path, ctypes.cast(cmd, ctypes.c_void_p), None, None, 1, flags,
                                  ctypes.cast(env_block, ctypes.c_void_p), str(staging), ctypes.byref(si),
                                  ctypes.byref(pi))
            if not ok:
                raise AppContainerError(f"CreateProcessW failed winerror={ctypes.get_last_error()}")  # type: ignore[attr-defined]
            h["proc"], h["thread"] = pi.hProcess, pi.hThread
            if not a.AssignProcessToJobObject(job, pi.hProcess):
                a.TerminateProcess(pi.hProcess, 1)
                raise AppContainerError("AssignProcessToJobObject failed")
            if a.ResumeThread(pi.hThread) == 0xFFFFFFFF:
                raise AppContainerError("ResumeThread failed")
            wait = a.WaitForSingleObject(pi.hProcess, int(timeout_s * 1000))
            if wait == WAIT_TIMEOUT:
                result["timed_out"] = True
                a.TerminateJobObject(job, 1)
                a.WaitForSingleObject(pi.hProcess, 10000)
            code = ctypes.c_uint32(0)
            if a.GetExitCodeProcess(pi.hProcess, ctypes.byref(code)):
                result["exit_code"] = None if result["timed_out"] else int(code.value)
        finally:
            c = self.cleanup
            if h["job"]:
                a.TerminateJobObject(h["job"], 1)
            closed = True
            for key in ("proc", "thread", "job"):
                if h[key] and not a.CloseHandle(h[key]):
                    closed = False
            if attr_ready:
                a.DeleteProcThreadAttributeList(attr_list)
            for psid in cap_sids:
                a.LocalFree(psid)
            for f in files:
                try:
                    f.close()
                except OSError:
                    closed = False
            c["handles_closed"] = bool(c.get("handles_closed", True)) and closed
            result["stderr_tail"] = tail_text(stderr_path) if result.get("exit_code") != 0 else ""
            del keep


def run_in_appcontainer_session(**kwargs: Any) -> AppContainerSession:
    """`measure.py` が `launcher=` を省略したときの既定ファクトリ。"""
    return AppContainerSession(**kwargs)
