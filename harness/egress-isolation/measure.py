#!/usr/bin/env python3
"""Servo ホストの egress 隔離を Linux・macOS・Windows で実測する外側の計測ランナー。

役割・責務境界:
    Issue #1377（TASK-38・`RENDER-3`・`SEC-2`・`PLUG-1`・`XOS-1`）。親判断 #1003 の比較設計
    `docs/design/servo-host-egress-isolation.md` §6 の共通観測方法を実装する。方式の採否は決めず、
    測定条件つきの事実（各経路の pass / fail / unobservable と観測手段）を JSON と Markdown に出す。

動作:
    1. 外側（ホストの netns）に許可外の宛先の listener（loopback のダミー）を立てる。
    2. モードごとに内側ランナー（`in_sandbox.py`）を bwrap / docker の固定 argv で起動する。
       baseline は `--unshare-net` だけを外した同条件（陽性対照）。docker-none は `--network none`。
    3. 外側の受信件数と内側の結果を `judge.py` で突き合わせる。観測できなければ不合格（fail-closed）。

OS 別のモード（#1387）:
    Linux は bwrap / docker。macOS は `sandbox-exec`（Seatbelt。`seatbelt.py`）で、試行の観測はサンドボックスの
    拒否ログ。Windows は baseline と、capability なしの AppContainer（`appcontainer.py`。ctypes。Issue #1396 で
    オーナー承認済み）で内側ランナーを起動する `appcontainer`。AppContainer は loopback に出られないため、
    Servo を使わない preflight（`appcontainer_probe.py`）で loopback 遮断と egress 遮断を切り分け、
    切り分けられない経路は理由つきの `unobservable` にする。macOS・Windows にはプロセス単位の監視が無い。
    Issue #1401: `--appcontainer-loopback-exempt`（opt-in。`xos-measure.yml` の Windows ジョブだけが付ける。要管理者）で
    ハーネスが作ったプロファイルの SID に `CheckNetIsolation LoopbackExempt` を付けて preflight を取り直し、解除する
    （オーナー承認 2026-11-01）。capability なしでも外側に届いたら、loopback 宛ての listener では測れないとして
    理由つきの `unobservable` にする。

安全上の規則:
    - 宛先は 127.0.0.0/8 のダミーだけ。実在のアドレスへ向けたプローブは、隔離下の内側で
      lo 以外の経路が無いことを確かめた後にのみ行う（`in_sandbox.py`）。
    - argv は定数リストで `shell=False`。`--host` は実行可能ファイルの絶対パス、`--docker-image` は正規表現で検証。
    - ログ・結果に URL のクエリ・本文・プロキシ系環境変数の値を載せない（有無だけ）。

終了コード: 0（計測が完走。経路判定の合否は結果 JSON を見る）／1（計測の失敗）／2（引数不正）。
"""

from __future__ import annotations

import argparse
import contextvars
from collections import Counter
import json
import os
import platform
import re
import select
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path
from typing import Any

_HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(_HERE))
import judge  # noqa: E402
import appcontainer  # noqa: E402
import listeners  # noqa: E402
import seatbelt  # noqa: E402

RUN_TIMEOUT_S = 240
DOCKER_IMAGE_RE = re.compile(r"^[a-z0-9][a-z0-9._/:@-]{0,127}$")
DOCKER_HOST_PATH = "/opt/fandhe/bin/fandhe-browser-servo-host"


def fmt_rc(rc: Any) -> str:
    """終了コードを 10 進と 16 進で示す。0xC0000409 は Rust の panic=abort も含む fail-fast（#1413）。"""
    if not isinstance(rc, int) or isinstance(rc, bool) or rc < 256:
        return str(rc)
    note = "; fail-fast, includes Rust panic=abort" if rc & 0xFFFFFFFF == 0xC0000409 else ""
    return f"{rc} (0x{rc & 0xFFFFFFFF:08X}{note})"


def validate_host(path: str) -> Path:
    p = Path(path)
    if not p.is_absolute() or not p.is_file() or not os.access(p, os.X_OK):
        raise ValueError("--host must be an absolute path to an executable file")
    return p


def validate_image(image: str) -> str:
    if not DOCKER_IMAGE_RE.match(image):
        raise ValueError("--docker-image has an unexpected format")
    return image


def start_outer_listeners(diag: bool = False) -> tuple[dict[str, Any], dict[str, int]]:
    """外側の許可外宛先（127.0.0.2/127.0.0.3）の listener を立て、(listener 辞書, ポート辞書) を返す。

    `diag=True`（macOS のときだけ呼び出し側が渡す。Issue #1415）は redirect の診断経路の listener を足す。
    診断経路は判定に使わない（`judge.routes_for_mode` に入れない）。
    """
    recs: dict[str, Any] = {}
    if diag:
        for r in fixtures_diag_routes():
            recs[r] = listeners.TcpRecorder(r, judge.IP_BLOCKED).start()
    for r in ("redirect", "img", "fetch", "ws"):
        recs[r] = listeners.TcpRecorder(r, judge.IP_BLOCKED).start()
    recs["internal"] = listeners.TcpRecorder("internal", judge.IP_INTERNAL_STANDIN).start()
    recs["udp_direct"] = listeners.UdpRecorder("udp_direct", judge.IP_BLOCKED).start()
    return recs, {k: v.port for k, v in recs.items()}


def fixtures_diag_routes() -> tuple[str, ...]:
    """診断経路名（`page_templates.DIAG_ROUTES`）。循環 import を避けて遅延 import する。"""
    import page_templates

    return page_templates.DIAG_ROUTES


def resolv_bind_args(resolv_file: Path) -> list[str]:
    """`/etc/resolv.conf` の実体の位置に固定の resolv.conf を重ねる bwrap 引数。

    `/run` を空の tmpfs にするとシンボリックリンク（例: ../run/systemd/resolve/stub-resolv.conf）が切れ、
    DNS の失敗が netns と無関係になる。実体の位置に非ループバックの固定 nameserver を置いて、
    失敗が netns 由来かを内側で判別できるようにする（両モード共通）。実体が /etc か /run 以外なら何もしない。
    """
    dest = os.path.realpath("/etc/resolv.conf")
    if dest.startswith("/run/"):
        return ["--dir", os.path.dirname(dest), "--ro-bind", str(resolv_file), dest]
    if dest.startswith("/etc/"):
        return ["--ro-bind", str(resolv_file), dest]
    return []


def ensure_loopback_aliases() -> None:
    """127.0.0.2 / 127.0.0.3 に bind できることを確かめる（できなければ ValueError）。

    macOS の lo0 は既定で 127.0.0.1 だけを持つ。ハーネスは sudo を使わないため、案内だけ出して止める。
    """
    for ip in (judge.IP_BLOCKED, judge.IP_INTERNAL_STANDIN):
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        try:
            s.bind((ip, 0))
        except OSError as exc:
            raise ValueError(
                f"cannot bind {ip} ({type(exc).__name__}); on macOS run: sudo ifconfig lo0 alias {ip} up") from exc
        finally:
            s.close()


def mac_version() -> str | None:
    """`sw_vers -productVersion`（記録のみ）。取得できなければ None。"""
    try:
        done = subprocess.run(["/usr/bin/sw_vers", "-productVersion"], capture_output=True, text=True,  # noqa: S603
                              timeout=20, check=False)
    except (OSError, subprocess.SubprocessError):
        return None
    return done.stdout.strip()[:40] or None


def free_port() -> int:
    """空きの loopback ポートを 1 つ確保して返す（Seatbelt のプロファイルが起動前にポートを要るため）。"""
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        s.bind((judge.IP_ALLOWED, 0))
        return int(s.getsockname()[1])
    finally:
        s.close()


def build_argv(mode: str, *, host: Path, ports: dict[str, int], out_dir: Path, hosts_file: Path,
               image: str | None, resolv_file: Path | None = None, name: str | None = None,
               page_port: int = 0, platform_name: str | None = None) -> list[str]:
    """内側ランナーを起動する固定 argv。ポートは int を文字列化したものだけが入る。"""
    inner_args = ["--mode", mode, "--ports", json.dumps(ports, sort_keys=True)]
    plat = platform_name or sys.platform
    if plat == "darwin" and all(r in ports for r in fixtures_diag_routes()):
        inner_args.append("--diag-routes")  # redirect の診断（Issue #1415。判定には使わない）
    if not plat.startswith("linux"):
        # macOS・Windows: 内側ランナーを（Seatbelt があれば包んで）直接起動する。
        inner_args += ["--page-port", str(page_port)]
        runner = [sys.executable, "-I", "-B", str(_HERE / "in_sandbox.py"),
                  "--host", str(host), "--out", str(out_dir / f"{mode}.json"), *inner_args]
        if mode == "baseline":
            return ([seatbelt.SANDBOX_EXEC, "-p", seatbelt.BASELINE_PROFILE, *runner]
                    if plat == "darwin" else runner)
        if mode == "seatbelt" and plat == "darwin":
            return [seatbelt.SANDBOX_EXEC, "-p", seatbelt.isolated_profile(page_port), *runner]
        raise ValueError(f"mode {mode} is not runnable on {plat}")
    if mode in ("baseline", "bwrap-netns"):
        argv = ["bwrap", "--die-with-parent"]
        if mode == "bwrap-netns":
            argv += ["--unshare-net"]
        argv += ["--dev-bind", "/", "/",
                 # /run を空の tmpfs にし、ホスト側 resolver（systemd-resolved・nscd 等）の Unix ソケット経由で
                 # netns の外へ名前解決が出る経路を断つ（baseline と同条件）。
                 "--tmpfs", "/run",
                 *(resolv_bind_args(resolv_file) if resolv_file is not None else []),
                 "--ro-bind", str(hosts_file), "/etc/hosts", "--",
                 sys.executable, "-I", "-B", str(_HERE / "in_sandbox.py"),
                 "--host", str(host), "--out", str(out_dir / f"{mode}.json"), *inner_args]
        return argv
    if mode == "docker-none":
        if image is None:
            raise ValueError("docker-none needs --docker-image")
        argv = ["docker", "run", "--rm", "--network", "none",
                # Windows には getuid が無い（Linux コンテナの出力先はコンテナ既定のユーザーで書く）。
                *(["--user", f"{os.getuid()}:{os.getgid()}"] if hasattr(os, "getuid") else []),
                "--tmpfs", "/tmp", *(["--name", name] if name else [])]
        for name, ip in (("pages.test", judge.IP_ALLOWED), (judge.HOST_BLOCKED, judge.IP_BLOCKED),
                         (judge.HOST_INTERNAL_STANDIN, judge.IP_INTERNAL_STANDIN)):
            argv += ["--add-host", f"{name}:{ip}"]
        argv += ["-v", f"{_HERE.parent}:/harness:ro", "-v", f"{out_dir}:/out", "--entrypoint", "python3", image,
                 "-I", "-B", "/harness/egress-isolation/in_sandbox.py", "--host", DOCKER_HOST_PATH,
                 "--out", f"/out/{mode}.json", *inner_args]
        return argv
    raise ValueError(f"unknown mode: {mode}")


def docker_image_id(image: str) -> str | None:
    """実測に使ったイメージの ID（digest）。取得できなければ None（記録のみ。判定は内側の SHA256 で行う）。"""
    try:
        out = subprocess.run(["docker", "image", "inspect", "--format", "{{.Id}}", image],  # noqa: S603,S607
                             capture_output=True, text=True, timeout=30, check=False)
    except (OSError, subprocess.SubprocessError):
        return None
    return out.stdout.strip()[:100] if out.returncode == 0 else None


def remove_container(name: str) -> None:
    """名前つきコンテナを強制削除する（ローカルの SIGKILL では daemon 管理のコンテナが止まらないため）。

    存在しない場合の失敗は無視する（`--rm` で既に消えている通常ケース）。
    """
    try:
        subprocess.run(["docker", "rm", "-f", name], stdin=subprocess.DEVNULL,  # noqa: S603,S607
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=60, check=False)
    except (OSError, subprocess.SubprocessError):
        pass


def run_argv(argv: list[str], container: str | None = None) -> tuple[int | None, str]:
    """argv を実行する（シェル不使用・タイムアウトつき・プロセスグループごと終了）。

    `container` を渡すと、正常終了・タイムアウト・例外のいずれでも finally で `docker rm -f` する。
    """
    try:
        return _run_argv(argv)
    finally:
        if container:
            remove_container(container)


def _run_argv(argv: list[str]) -> tuple[int | None, str]:
    windows = sys.platform == "win32"
    kwargs: dict[str, Any] = (
        {"creationflags": subprocess.CREATE_NEW_PROCESS_GROUP} if windows else {"start_new_session": True})
    proc = subprocess.Popen(  # noqa: S603
        argv, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, **kwargs,
    )
    try:
        _, err = proc.communicate(timeout=RUN_TIMEOUT_S)
        return proc.returncode, err.decode("utf-8", errors="replace")[-400:]
    except subprocess.TimeoutExpired:
        if windows:
            # プロセスツリーごと終了する（PID は int。固定 argv）。
            subprocess.run(["taskkill", "/PID", str(int(proc.pid)), "/T", "/F"],  # noqa: S603,S607
                           stdin=subprocess.DEVNULL, capture_output=True, timeout=30, check=False)
        else:
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        proc.communicate()
        return None, "timed out"


def _signal_log_group(proc: Any, sig: int) -> None:
    """`start_new_session=True` で起動した `log show` 系のプロセスグループ全体へシグナルを送る。

    子は `start_new_session=True` で自前のグループを持つ（pgid == proc.pid）。グループ送信が拒否・失敗した場合は親だけへ送る。
    """
    try:
        os.killpg(proc.pid, sig)
        return
    except (OSError, AttributeError, TypeError):
        pass
    try:
        proc.send_signal(sig)
    except (OSError, AttributeError):
        pass


def _stop_log_proc(proc: Any) -> None:
    """`log show` の子プロセスを止める。グループへ SIGTERM を先に送り、期限後に SIGKILL する（Issue #1402）。"""
    if hasattr(os, "killpg") and hasattr(proc, "pid"):
        try:
            _signal_log_group(proc, signal.SIGTERM)
            proc.wait(timeout=5)
        except (subprocess.TimeoutExpired, OSError):
            _signal_log_group(proc, signal.SIGKILL)
        return
    try:
        proc.terminate()
        proc.wait(timeout=5)
    except (subprocess.TimeoutExpired, OSError):
        try:
            proc.kill()
        except OSError:
            pass


class _TimedLineReader:
    """パイプの fd を `select` で期限つきに読む行リーダー。期限を過ぎたら EOF 扱い（b""）にして `timed_out` を立てる。

    子の `log` が残ってパイプが閉じなくても `collect_denies` の読み取りループが戻れるようにする（Issue #1402）。
    """

    def __init__(self, stream: Any, deadline: float) -> None:
        self._fd = stream.fileno()
        self._deadline = deadline
        self._buf = b""
        self._eof = False
        self.timed_out = False

    def readline(self, limit: int = -1) -> bytes:
        while True:
            nl = self._buf.find(b"\n")
            end = nl + 1 if nl >= 0 else (len(self._buf) if self._eof else -1)
            if limit > 0 and (end < 0 or end > limit) and len(self._buf) >= limit:
                end = limit
            if end >= 0:
                line, self._buf = self._buf[:end], self._buf[end:]
                return line
            remain = self._deadline - time.monotonic()
            if remain <= 0:
                self.timed_out = True
                return b""
            ready, _, _ = select.select([self._fd], [], [], min(remain, 1.0))
            if not ready:
                continue
            chunk = os.read(self._fd, 65536)
            if not chunk:
                self._eof = True
            else:
                self._buf += chunk


# run_mode が collect_denies の呼び出しの間だけ設定する、この run の既知の宛先 ("ip:port") 一覧。
# collect_denies の呼び出し形（start, pids）を変えずに渡すための文脈（既存の差し替え・呼び出し側を壊さない）。
_KNOWN_DESTS: contextvars.ContextVar[tuple[str, ...] | None] = contextvars.ContextVar("known_dests", default=None)


def collect_denies(start: str, pids: frozenset[int] | None = None, *,
                   dests: Any = None, diagnostic: bool = False) -> tuple[dict[str, int], dict[str, Any]]:
    """macOS のサンドボックス拒否ログを逐次読んで集計する。(宛先 -> 件数, 取得状況)。読めなければ空と理由。

    `pids` は対象ホストとその子孫の pid。拒否ログの `proc(<pid>)` が含まれる行だけを数える（無関係な
    プロセスの同宛先の拒否を試行の根拠にしない）。

    出力を全量メモリに載せず、1 行ずつ上限つきで読む。行数・総バイト数が上限に達したら
    プロセスを止めて unobservable（available=False）にする（リソース枯渇の防止）。

    `dests` はこの run の既知の宛先 ("ip:port") 一覧（dest_counts のキー）。macOS 26 のワイルドカード形式
    （`network-outbound <語>:*:<port>`）は、pid が `pids` に含まれ、かつポートが `dests` で一意のときだけその宛先へ
    帰属させる（ポート重複・未知ポートは帰属しない。`seatbelt.PortMap`）。None なら文脈 `_KNOWN_DESTS`（run_mode が設定）を使い、それも無ければ帰属させない。
    """
    if dests is None:
        dests = _KNOWN_DESTS.get()
    port_map = seatbelt.PortMap.from_dests(dests) if dests is not None else None
    try:
        argv = seatbelt.log_show_argv(start, diagnostic=diagnostic)
        proc = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,  # noqa: S603
                                stderr=subprocess.DEVNULL, start_new_session=True)
    except (OSError, subprocess.SubprocessError, ValueError) as exc:
        return {}, {"available": False, "reason": type(exc).__name__}
    timer = threading.Timer(180, _stop_log_proc, args=(proc,))
    # タイマーで止めても子 log がパイプを握り続ける場合に備え、読み取り自体にも期限を付ける（無期限ブロックの防止）。
    try:
        reader: Any = _TimedLineReader(proc.stdout, time.monotonic() + 185)
    except (AttributeError, OSError, ValueError, TypeError):
        reader = proc.stdout
    timer.start()
    counts: Counter[str] = Counter()
    diag = seatbelt.DenyDiagnostics()
    lines = total = 0
    over = False
    try:
        assert proc.stdout is not None
        while True:
            raw = reader.readline(seatbelt.MAX_LINE_LEN + 1)
            if not raw:
                break
            total += len(raw)
            if not raw.endswith(b"\n") and len(raw) > seatbelt.MAX_LINE_LEN:
                # 長すぎる行は残りを捨てる（メモリに溜めない）。
                while True:
                    rest = reader.readline(seatbelt.MAX_LINE_LEN)
                    total += len(rest)
                    if not rest or rest.endswith(b"\n") or total > seatbelt.MAX_SCAN_BYTES:
                        break
                lines += 1
            else:
                lines += 1
                dest = seatbelt.scan_line(raw.decode("utf-8", errors="replace").rstrip("\r\n"), pids, diag, port_map)
                if dest and sum(counts.values()) < seatbelt.MAX_LOG_LINES:
                    counts[dest] += 1
            if lines > seatbelt.MAX_SCAN_LINES or total > seatbelt.MAX_SCAN_BYTES:
                over = True
                break
    finally:
        timer.cancel()
        if over:
            _stop_log_proc(proc)
        else:
            # 通常の EOF では子プロセスの終了処理が済むまで期限付きで待つ（直後の poll() が None でも
            # 正常終了した log show を kill して集計済みの拒否記録を捨てない）。超過時だけ kill する。
            try:
                proc.wait(timeout=30)
            except subprocess.TimeoutExpired:
                _stop_log_proc(proc)
        proc.wait()
        if proc.stdout:
            proc.stdout.close()
    if over:
        return {}, {"available": False, "reason": "log show output exceeded scan limit"}
    if getattr(reader, "timed_out", False):
        return {}, {"available": False, "reason": "log show timed out"}
    if proc.returncode != 0:
        return {}, {"available": False, "reason": f"log show rc={proc.returncode}"}
    return dict(counts), {"available": True, "deny_lines": sum(counts.values()), "diagnostics": diag.as_dict()}


def run_appcontainer_mode(*, host: Path, ports: dict[str, int], recs: dict[str, Any], out_dir: Path,
                          page_port: int, launcher: Any = None, loopback_exempt: bool = False) -> dict[str, Any]:
    """AppContainer モード（Windows。Issue #1396）。preflight（capability の対照）→ 本体の順に同じ容器で実行する。

    `launcher` は `appcontainer.run_in_appcontainer_session` と同じ呼び出し規約のファクトリ（自己テストで偽物を渡す）。
    起動・複製・片付けの失敗は `unavailable` に理由を入れて全経路を `unobservable` にする。成功・失敗・タイムアウトの
    いずれでも `with` が容器・ハンドル・複製先を片付け、その結果を `appcontainer.cleanup` に残す。
    """
    make = launcher or appcontainer.run_in_appcontainer_session
    data: dict[str, Any] = {
        "mode": "appcontainer", "exit_code": None, "binary_sha256": None, "binary_version": None,
        "stderr_tail": "", "inner": {}, "outer_counts": {k: 0 for k in recs},
        "appcontainer": {"preflight": None, "profile": None, "staging": None, "cleanup": {}, "launcher_error": None,
                         "loopback_exempt": {"requested": loopback_exempt}},
    }
    info = data["appcontainer"]
    # 経路用の listener とは分ける（経路の受信件数を汚さない）。
    pre_tcp = listeners.TcpRecorder("preflight_tcp", judge.IP_BLOCKED).start()
    pre_udp = listeners.UdpRecorder("preflight_udp", judge.IP_BLOCKED).start()
    session = None
    try:
        session = make(host=host, harness_dir=_HERE.parent, loopback_exempt=loopback_exempt)
        with session:
            info["profile"] = {"name": getattr(session, "name", None), "sid": getattr(session, "sid_string", None)}
            info["staging"] = getattr(session, "staging_mode", None)
            preflight: dict[str, Any] = {}
            for variant, caps in (("nocap", ()), ("netcap", appcontainer.NETWORK_CAPABILITIES)):
                t0, u0 = pre_tcp.count, pre_udp.count
                res = session.run("egress-isolation/appcontainer_probe.py",
                                  ["--tcp-port", str(pre_tcp.port), "--udp-port", str(pre_udp.port),
                                   "--out", session.out_path(f"preflight-{variant}.json")],
                                  capabilities=caps, result_name=f"preflight-{variant}.json")
                preflight[variant] = {
                    "probe": res.get("result_json"), "exit_code": res.get("exit_code"),
                    "timed_out": res.get("timed_out"), "error": res.get("error"),
                    "outer_tcp": pre_tcp.count - t0, "outer_udp": pre_udp.count - u0}
            info["preflight"] = preflight
            before = {k: r.count for k, r in recs.items()}
            res = session.run("egress-isolation/in_sandbox.py",
                              ["--mode", "appcontainer", "--host", session.host_path,
                               "--ports", json.dumps(ports, sort_keys=True),
                               "--out", session.out_path("appcontainer.json"), "--page-port", str(page_port)],
                              capabilities=(), result_name="appcontainer.json")
            data["outer_counts"] = {k: r.count - before[k] for k, r in recs.items()}
            data["exit_code"] = res.get("exit_code")
            data["stderr_tail"] = res.get("stderr_tail") or ""
            inner = res.get("result_json")
            if res.get("exit_code") in (0, 2) and isinstance(inner, dict):
                data["inner"] = inner
            if res.get("error"):
                info["launcher_error"] = res["error"]
            if res.get("timed_out"):
                info["launcher_error"] = "in-container runner timed out"
    except (appcontainer.AppContainerError, OSError, ValueError) as exc:
        info["launcher_error"] = f"{type(exc).__name__}: {str(exc)[:200]}"
    finally:
        pre_tcp.stop()
        pre_udp.stop()
        info["cleanup"] = dict(getattr(session, "cleanup", {}) or {})
        info["loopback_exempt"] = dict(getattr(session, "loopback_exempt_state", None)
                                       or {"requested": loopback_exempt})
    inner = data["inner"]
    data["binary_sha256"] = inner.get("host_sha256")
    data["binary_version"] = inner.get("host_version")
    reason = None
    if info["launcher_error"]:
        reason = f"AppContainer launcher failed: {info['launcher_error']}"
    elif not inner:
        reason = f"in-container runner produced no result (exit code {data['exit_code']})"
    if reason is not None:
        if sum(data["outer_counts"].values()) == 0:
            data["unavailable"] = reason
        else:
            # 外側で受信があったときは、理由つきの unobservable で fail を隠さない（judge が受信を先に fail にする）。
            inner = data["inner"] = {**inner, "fatal": reason, "positive_ok": False}
    if inner:
        (out_dir / "appcontainer.json").write_text(json.dumps(inner, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return data


def appcontainer_caveat(data: dict[str, Any]) -> str | None:
    """appcontainer の pass を妨げる理由（loopback の交絡、または片付けの不完全）。なければ None。"""
    info = data.get("appcontainer") or {}
    exempt = info.get("loopback_exempt")
    requested = isinstance(exempt, dict) and exempt.get("requested") is True
    confound = judge.appcontainer_confound(info.get("preflight"), loopback_exempt=requested)
    if confound is not None:
        return confound
    cleanup = info.get("cleanup") or {}
    if not cleanup or any(v is False for v in cleanup.values()):
        return "AppContainer cleanup was incomplete or not recorded; result is not trusted"
    return None


def run_mode(mode: str, *, host: Path, ports: dict[str, int], recs: dict[str, Any], out_dir: Path,
             hosts_file: Path, image: str | None, resolv_file: Path | None = None, page_port: int = 0,
             launcher: Any = None, loopback_exempt: bool = False) -> dict[str, Any]:
    if mode == "appcontainer":
        return run_appcontainer_mode(host=host, ports=ports, recs=recs, out_dir=out_dir, page_port=page_port,
                                     launcher=launcher, loopback_exempt=loopback_exempt)
    before = {k: r.count for k, r in recs.items()}
    # docker はコンテナ名を付けて、タイムアウト・例外時にも確実に後始末する（名前は pid 由来の固定形式）。
    name = f"egress-iso-{os.getpid()}" if mode == "docker-none" else None
    argv = build_argv(mode, host=host, ports=ports, out_dir=out_dir, hosts_file=hosts_file, image=image,
                      resolv_file=resolv_file, name=name, page_port=page_port)
    denies: dict[str, int] = {}
    status: dict[str, Any] = {}
    pids: frozenset[int] = frozenset()
    inner: dict[str, Any] = {}
    diag_dests: dict[str, str] = {}
    info_debug: dict[str, Any] | None = None
    started = time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(time.time() - 2))
    rc, err = run_argv(argv, container=name)
    outer = {k: r.count - before[k] for k, r in recs.items()}
    result_path = out_dir / f"{mode}.json"
    # 内側ランナーが正常終了（0）または隔離 abort（2。理由つきの JSON を書く）した場合の今回の結果だけを使う。
    # それ以外の失敗は空 = 判定不能（合格にしない）。--out は空ディレクトリ検査済みで古い結果は混ざらない。
    if rc in (0, 2) and result_path.is_file():
        try:
            inner = json.loads(result_path.read_text(encoding="utf-8"))
        except ValueError:
            inner = {}
    if mode == "seatbelt" and inner:
        # 共有 loopback のため内側の写し listener が無い。TCP の試行は拒否ログ（宛先 ip:port）で観測する。
        raw_pids = inner.get("host_pids")
        pids = frozenset(p for p in raw_pids if isinstance(p, int) and not isinstance(p, bool)) \
            if isinstance(raw_pids, list) else frozenset()
        # 診断経路（Issue #1415）の宛先も既知の宛先に入れる（入れないとワイルドカード形式が未知ポート扱いになる）。
        diag_dests = {r: f"{judge.IP_BLOCKED}:{ports[r]}" for r in fixtures_diag_routes() if r in ports}
        all_dests = (*(f"{judge.IP_INTERNAL_STANDIN if r == 'internal' else judge.IP_BLOCKED}:{ports[r]}"
                       for r in ("redirect", "img", "fetch", "ws", "internal")),
                     f"{judge.IP_BLOCKED}:{ports['udp_direct']}", *diag_dests.values())
        if pids:
            token = _KNOWN_DESTS.set(all_dests)
            try:
                denies, status = collect_denies(started, pids)
                if (status.get("available") and diag_dests
                        and denies.get(f"{judge.IP_BLOCKED}:{ports['redirect']}", 0) == 0):
                    # D5: redirect の拒否行が 0 件のときだけ、取得レベルを広げて読み直す（件数は補助情報。根拠にしない）。
                    info_denies, info_status = collect_denies(started, pids, diagnostic=True)
                    info_debug = ({d: info_denies.get(d, 0) for d in all_dests}
                                  if info_status.get("available") else {"unavailable": info_status.get("reason")})
            finally:
                _KNOWN_DESTS.reset(token)
        else:
            # 対象 PID を特定できないときは帰属を確認できないため、拒否ログを試行の根拠にしない（unobservable）。
            denies, status = {}, {"available": False, "reason": "host pid not recorded"}
    data = {"mode": mode, "exit_code": rc, "binary_sha256": inner.get("host_sha256"),
            "binary_version": inner.get("host_version"), "stderr_tail": err if rc != 0 else "", "inner": inner,
            "outer_counts": outer}
    if mode == "seatbelt" and inner:
        dest = {r: f"{judge.IP_INTERNAL_STANDIN if r == 'internal' else judge.IP_BLOCKED}:{ports[r]}"
                for r in ("redirect", "img", "fetch", "ws", "internal")}
        udp = f"{judge.IP_BLOCKED}:{ports['udp_direct']}"
        # 正解の分かっている拒否（内側ランナー自身の UDP プローブ）を拒否ログが取りこぼしているなら、
        # 拒否ログは TCP の試行の根拠にしない（fail-closed。ビーコンは補助情報のみで根拠にしない。Issue #1395）。
        probe_ok, probe_why = seatbelt.self_check(denies, (inner.get("probes") or {}).get("udp_direct"), udp)
        if not probe_ok and status.get("available"):
            status = {**status, "available": False, "reason": probe_why}
        mk = (status.get("diagnostics") or {}).get("match_kind") or {}
        wildcard_routes = sorted(r for r, d in (*dest.items(), ("udp_direct", udp))
                                 if status.get("available") and (mk.get(d) or {}).get(seatbelt.MATCH_KIND_WILDCARD))
        observed = dict(denies)  # 診断用に記録する生の件数（根拠に使うかは status で決まる）
        if not status.get("available"):
            denies = {}
        # 拒否ログの結果を、内側ランナーの試行記録（UDP の errno 由来）へ OR で合成する。ビーコンは根拠にしない。
        inner["attempts"], inner["attempt_evidence"] = seatbelt.compose_attempts(
            inner.get("attempts") or {}, inner.get("attempt_evidence") or {}, denies, dest, udp)
        data["deny_log"] = {**status, "dest_counts": {d: observed.get(d, 0) for d in (*dest.values(), udp)},
                            "wildcard_routes": wildcard_routes}
        if diag_dests:
            # 診断経路の宛先の件数は判定用の dest_counts と分けて出す（Issue #1415。根拠に使うのは availableのとき）。
            data["deny_log"]["diagnostic_dest_counts"] = {r: observed.get(d, 0) for r, d in diag_dests.items()}
            by_dest = {d: r for r, d in (*dest.items(), ("udp_direct", udp), *diag_dests.items())}
            un = (status.get("diagnostics") or {}).get("unattributed_network_dests") or {}
            data["deny_log"]["unattributed_routes"] = {by_dest.get(d, d): n for d, n in un.items()}
            if info_debug is not None:
                data["deny_log"]["diagnostic_info_debug_dest_counts"] = info_debug
    return data


_SAMPLE_KEYS = ("unmatched_deny_samples", "pid_excluded_samples")


def _without_samples(dl: Any) -> Any:
    """deny_log から診断サンプル本文を除いた写し。サマリーの 1 行表示用。"""
    if not isinstance(dl, dict):
        return dl
    out = dict(dl)
    for key in ("diagnostics",):
        if isinstance(out.get(key), dict):
            out[key] = {k: v for k, v in out[key].items() if k not in _SAMPLE_KEYS}
    return out


def _deny_sample_block(dl: dict[str, Any]) -> list[str]:
    """診断サンプルを text のコードブロックで出す（表・箇条書きを壊さない。バッククォートは置換する）。"""
    out: list[str] = []
    diag = dl.get("diagnostics")
    if not isinstance(diag, dict):
        return out
    for key in _SAMPLE_KEYS:
        samples = diag.get(key) or []
        dropped = diag.get(f"{key}_dropped", 0)
        if not samples and not dropped:
            continue
        out += ["", f"deny log samples ({key}; {len(samples)} shown, {dropped} dropped)", "", "```text"]
        out += [str(x).replace("`", "'") for x in samples]
        out.append("```")
    return out


def redirect_notes(data: dict[str, Any]) -> dict[str, str]:
    """redirect が unobservable のときに理由へ足す診断の事実（Issue #1415）。診断が無ければ空。判定は変えない。"""
    inner = data.get("inner") or {}
    dl = data.get("deny_log") or {}
    dc = dl.get("diagnostic_dest_counts")
    if not isinstance(dc, dict):
        return {}
    kind = lambda r: ((inner.get("loads") or {}).get(r) or (inner.get("diag_loads") or {}).get(r) or {}).get("kind")  # noqa: E731
    ms = inner.get("load_elapsed_ms") or {}
    parts = [f"diagnostics (not verdicts): deny rows late-redirect={dc.get('diag_redirect_late', 0)}, "
             f"direct-nav={dc.get('diag_nav_direct', 0)}",
             f"redirect load kind={kind('redirect')} in {ms.get('redirect')} ms"]
    if dl.get("unattributed_routes"):
        parts.append(f"unattributed deny rows by route={dl['unattributed_routes']}")
    if "diagnostic_info_debug_dest_counts" in dl:
        parts.append(f"info/debug re-read={dl['diagnostic_info_debug_dest_counts']}")
    return {"redirect": "; ".join(parts)}


def redirect_diag_block(mode: str, data: dict[str, Any], outer: dict[str, int]) -> list[str]:
    """redirect 診断の表（Issue #1415。判定ではない）。診断経路が無いモードは空。WARNING は隔離モードの外側受信。"""
    inner = data.get("inner") or {}
    routes = ("redirect", *fixtures_diag_routes())
    if not any(r in outer for r in fixtures_diag_routes()):
        return []
    dl = data.get("deny_log") or {}
    deny = dict(dl.get("diagnostic_dest_counts") or {})
    ms = inner.get("load_elapsed_ms") or {}
    alive = inner.get("host_alive_after_load") or {}
    loads = {**(inner.get("loads") or {}), **(inner.get("diag_loads") or {})}
    out = ["", "redirect diagnostics (not verdicts)", "",
           "| route | outer | deny | load kind | elapsed_ms | host alive after failed load |",
           "| ----- | ----- | ---- | --------- | ---------- | ---------------------------- |"]
    for r in routes:
        dv = deny.get(r, "-") if r != "redirect" else (dl.get("dest_counts") or {}).get("redirect", "-")
        out.append(f"| {r} | {outer.get(r, '-')} | {dv} | {(loads.get(r) or {}).get('kind', 'ok' if r in loads else '-')} "
                   f"| {ms.get(r, '-')} | {alive.get(r, '-')} |")
    if mode in judge.ISOLATED_MODES:
        for r in fixtures_diag_routes():
            if outer.get(r, 0) >= 1:
                out.append(f"- WARNING: diagnostic route {r} has {outer[r]} outer receipt(s) in an isolated mode; "
                           "review for a possible real leak")
    for key in ("unattributed_routes", "diagnostic_info_debug_dest_counts"):
        if key in dl:
            out.append(f"- {key}: {dl[key]}")
    return out


def summarize(results: dict[str, Any]) -> str:
    meta = results["meta"]
    lines = [f"# egress isolation measurement ({meta.get('platform_name', 'Linux')})", ""]
    lines.append(f"- date (UTC): {meta['date_utc']}")
    lines.append(f"- kernel: {meta['kernel']}")
    lines.append(f"- host binary sha256: {meta['host_sha256']}")
    lines.append(f"- host version: {meta['host_version']}")
    lines.append(f"- proxy env present: {meta['proxy_env_present']} (values are never recorded)")
    lines.append("")
    base = results["modes"].get("baseline", {})
    lines += ["## baseline control (outer receipts per route)", ""]
    lines.append("| route | outer receipts |")
    lines.append("| ----- | -------------- |")
    for r, c in sorted(base.get("outer_counts", {}).items()):
        lines.append(f"| {r} | {c} |")
    lines += redirect_diag_block("baseline", base, base.get("outer_counts", {}))
    for mode, data in results["modes"].items():
        if mode == "baseline":
            continue
        inner = data.get("inner", {})
        lines += ["", f"## {mode}", ""]
        lines.append(f"- exit code: {data.get('exit_code')}; positive_ok: {inner.get('positive_ok')}; "
                     f"selftest_rc: {fmt_rc(inner.get('selftest_rc'))}; aborted: {inner.get('aborted', False)}")
        if "host_exit_rc" in inner:
            lines.append(f"- host exit after shutdown: {fmt_rc(inner.get('host_exit_rc'))}; "
                         f"timed out: {inner.get('host_exit_timed_out', False)}")
        for key in ("selftest_stderr_tail", "host_stderr_tail"):
            tail = inner.get(key)
            if tail:
                safe = tail.replace("```", "'" * 3).replace("::", ": :")
                lines += ["", f"{key} (last 2 KiB at most):", "", "```text", safe, "```"]
        lines.append(f"- guard: {inner.get('guard')}")
        if data.get("unavailable"):
            lines.append(f"- mode unavailable: {data['unavailable']}")
        ac = data.get("appcontainer")
        if ac:
            lines.append(f"- appcontainer: profile={ac.get('profile')}; staging={ac.get('staging')}; "
                         f"launcher_error={ac.get('launcher_error')}")
            lines.append(f"- appcontainer cleanup: {ac.get('cleanup')}")
            lines.append(f"- loopback exemption: {ac.get('loopback_exempt')}")
            pre = ac.get("preflight") or {}
            lines += ["", "preflight (loopback confound check; outer = receipts at the preflight listeners)", "",
                      "| variant | loopback_self | tcp_outer | udp_outer | outer tcp | outer udp |",
                      "| ------- | ------------- | --------- | --------- | --------- | --------- |"]
            for variant in ("nocap", "netcap"):
                e = pre.get(variant) or {}
                p = e.get("probe") or {}
                lines.append(f"| {variant} | {p.get('loopback_self')} | {p.get('tcp_outer')} | {p.get('udp_outer')} | "
                             f"{e.get('outer_tcp')} | {e.get('outer_udp')} |")
            lines.append("")
            lines.append(f"- loopback confound: {judge.appcontainer_confound(pre, loopback_exempt=bool((ac.get('loopback_exempt') or {}).get('requested'))) or 'none'}")
        if inner.get("fatal"):
            lines.append(f"- in-sandbox fatal: {inner['fatal']}")
        if data.get("deny_log"):
            dl = data["deny_log"]
            lines.append(f"- sandbox deny log: {_without_samples(dl)}")
            lines += _deny_sample_block(dl)
        if inner.get("beacons") is not None:
            lines.append(f"- failure-path beacons (route -> count): {inner['beacons']}")
        if inner.get("attempt_evidence"):
            lines.append(f"- attempt evidence (route -> source): {inner['attempt_evidence']}")
        lines.append(f"- isolation precheck: {inner.get('isolation_precheck')}")
        lines.append(f"- binary sha256 (inside sandbox): {data.get('binary_sha256')}; "
                     f"matches baseline: {data.get('binary_match')}")
        if data.get("image_id"):
            lines.append(f"- docker image id: {data['image_id']}")
        lines += ["", "| route | verdict | outer | control | method | reason |", "| --- | --- | --- | --- | --- | --- |"]
        for r, v in data.get("verdicts", {}).items():
            lines.append(f"| {r} | {v['verdict']} | {v['outer_count']} | {v['control_count']} | {v['method']} | {v['reason']} |")
        # unobservable に降格しても外側の受信は本物の漏えいかもしれない（Issue #1401 監査）。目立たせる。
        for r, v in data.get("verdicts", {}).items():
            if v["verdict"] == judge.UNOBSERVABLE and v["outer_count"] >= 1:
                lines.append(f"- WARNING: route {r} is unobservable but has {v['outer_count']} outer receipt(s); "
                             "review for a possible real leak")
        lines += redirect_diag_block(mode, data, data.get("outer_counts", {}))
    return "\n".join(lines) + "\n"


def validate_loopback_exempt(requested: bool, plat: str, modes: list[str]) -> None:
    """`--appcontainer-loopback-exempt` は Windows の appcontainer モードがあるときだけ受け付ける（Issue #1401）。"""
    if requested and (plat != "win32" or "appcontainer" not in modes):
        raise ValueError("--appcontainer-loopback-exempt needs win32 and the appcontainer mode")


def _unavailable_deny_log_reason(data: dict[str, Any]) -> str | None:
    """拒否ログが使えなかったときの理由（judge の reason へ足す。使えた・無いときは None）。"""
    dl = data.get("deny_log") or {}
    return dl.get("reason") if dl and not dl.get("available") else None


DEFAULT_MODES = {"linux": ("baseline", "bwrap-netns"), "darwin": ("baseline", "seatbelt"),
                 "win32": ("baseline", "appcontainer")}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Measure Servo host egress isolation (Linux, macOS, Windows).")
    parser.add_argument("--host", required=True, help="Absolute path to fandhe-browser-servo-host.")
    parser.add_argument("--modes", default=None,
                        help="Comma separated modes allowed on this OS (baseline first; default per OS: linux baseline,bwrap-netns / darwin baseline,seatbelt / win32 baseline,appcontainer): linux baseline,bwrap-netns,docker-none; "
                        "darwin baseline,seatbelt; win32 baseline,appcontainer.")
    parser.add_argument("--docker-image", help="Image containing the host at " + DOCKER_HOST_PATH)
    parser.add_argument("--out", required=True, type=Path, help="Existing empty directory for results.")
    parser.add_argument("--appcontainer-loopback-exempt", action="store_true",
                        help="Opt-in (needs administrator; used only by the xos-measure Windows job): add "
                        "CheckNetIsolation LoopbackExempt for the harness-created AppContainer SID and remove it afterwards.")
    args = parser.parse_args(argv)
    try:
        plat = "linux" if sys.platform.startswith("linux") else sys.platform
        allowed = judge.PLATFORM_MODES.get(plat)
        if allowed is None:
            raise ValueError(f"unsupported platform: {sys.platform}")
        host = validate_host(args.host)
        modes = args.modes.split(",") if args.modes else list(DEFAULT_MODES[plat])
        if not modes or modes[0] != "baseline" or any(m not in allowed for m in modes):
            raise ValueError("--modes must start with baseline and use known modes")
        validate_loopback_exempt(args.appcontainer_loopback_exempt, plat, modes)
        image = validate_image(args.docker_image) if args.docker_image else None
        if "docker-none" in modes and image is None:
            raise ValueError("docker-none needs --docker-image")
        if plat == "darwin":
            ensure_loopback_aliases()
        if not args.out.is_dir():
            raise ValueError("--out must be an existing directory")
        if any(args.out.iterdir()):
            raise ValueError("--out must be an empty directory (stale results must not be reused)")
    except ValueError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2

    out_dir = args.out.resolve()
    # 所有者のみ書き込み可。docker は --user でホストの UID/GID を指定するため全ユーザー書き込みは不要
    # （別ユーザーによる結果 JSON の差し替え・symlink 先行作成を防ぐ）。
    os.chmod(out_dir, 0o700)
    with tempfile.TemporaryDirectory(prefix="egress-hosts-") as td:
        hosts_file = Path(td) / "hosts"
        hosts_file.write_text(judge.HOSTS_FILE, encoding="utf-8")
        resolv_file = Path(td) / "resolv.conf"
        resolv_file.write_text(judge.RESOLV_CONF, encoding="utf-8")
        recs, ports = start_outer_listeners(diag=sys.platform == "darwin")
        results: dict[str, Any] = {"modes": {}}
        try:
            version = subprocess.run([str(host), "--version"], capture_output=True, text=True,  # noqa: S603
                                     timeout=20, check=False).stdout.strip()[:80]
            import hashlib

            sha = hashlib.sha256(host.read_bytes()).hexdigest()
            import datetime

            results["meta"] = {
                "date_utc": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
                "kernel": platform.release(), "platform_name": platform.platform(),
                "sw_vers": mac_version() if sys.platform == "darwin" else None, "host_sha256": sha, "host_version": version,
                "proxy_env_present": any(k.lower().endswith("_proxy") for k in os.environ),
            }
            for mode in modes:
                # モードごとにポートを取り直す（close 直後の再 bind が EADDRINUSE になるのを避ける）。
                page_port = 0 if sys.platform.startswith("linux") else free_port()
                results["modes"][mode] = run_mode(mode, host=host, ports=ports, recs=recs, out_dir=out_dir,
                                                  hosts_file=hosts_file, image=image,
                                                  resolv_file=resolv_file, page_port=page_port,
                                                  loopback_exempt=args.appcontainer_loopback_exempt)
                if mode == "docker-none" and image is not None:
                    results["modes"][mode]["image_id"] = docker_image_id(image)
        finally:
            for r in recs.values():
                r.stop()
    control = results["modes"]["baseline"]["outer_counts"]
    base_sha = results["modes"]["baseline"].get("binary_sha256")
    for mode, data in results["modes"].items():
        if mode in judge.ISOLATED_MODES:
            # 陽性対照（baseline）と同一バイナリでなければ、対照が当該モードの根拠にならない。
            data["binary_match"] = bool(base_sha) and data.get("binary_sha256") == base_sha
            data["verdicts"] = judge.judge_all(mode, data["inner"], data["outer_counts"], control,
                                               binary_match=data["binary_match"],
                                               mode_unavailable=data.get("unavailable"),
                                               loopback_confound=appcontainer_caveat(data),
                                               deny_log_reason=_unavailable_deny_log_reason(data),
                                               wildcard_routes=frozenset(
                                                   (data.get("deny_log") or {}).get("wildcard_routes") or ()),
                                               route_notes=redirect_notes(data))
    return finalize(results, out_dir)


# 管理者操作（Windows のループバック免除）の解除が不成立だったときの終了コード。
# 判定（pass / fail / unobservable）では変えない。2 は引数エラーで使用済み（Issue #1401 の監査）。
EXIT_ADMIN_RESTORE_FAILED = 3


def admin_restore_failures(results: dict[str, Any]) -> list[str]:
    """管理者操作の解除が不成立のモードと内容を返す（空なら成立。要求されなかった操作は対象外）。

    Windows: ループバック免除を要求した実行で、解除の rc 失敗・解除後も一覧に残る/確認不能。
    """
    failures: list[str] = []
    for mode, data in (results.get("modes") or {}).items():
        ac = data.get("appcontainer") or {}
        if (ac.get("loopback_exempt") or {}).get("requested") is True:
            cleanup = ac.get("cleanup") or {}
            for key in ("loopback_exempt_removed", "loopback_exempt_absent_after_remove"):
                if key in cleanup and cleanup[key] is not True:
                    failures.append(f"{mode}: {key} is not true")
    return failures


def finalize(results: dict[str, Any], out_dir: Path) -> int:
    """結果 JSON と summary を書き終えてから、解除の不成立だけを終了コードに反映する。"""
    (out_dir / "result.json").write_text(json.dumps(results, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    summary = summarize(results)
    (out_dir / "summary.md").write_text(summary, encoding="utf-8")
    print(summary)
    failures = admin_restore_failures(results)
    for f in failures:
        print(f"error: administrator operation not undone ({f})", file=sys.stderr)
    return EXIT_ADMIN_RESTORE_FAILED if failures else 0


if __name__ == "__main__":
    sys.exit(main())
