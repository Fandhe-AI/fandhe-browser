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
from collections import Counter
import json
import os
import platform
import re
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


def validate_host(path: str) -> Path:
    p = Path(path)
    if not p.is_absolute() or not p.is_file() or not os.access(p, os.X_OK):
        raise ValueError("--host must be an absolute path to an executable file")
    return p


def validate_image(image: str) -> str:
    if not DOCKER_IMAGE_RE.match(image):
        raise ValueError("--docker-image has an unexpected format")
    return image


def start_outer_listeners() -> tuple[dict[str, Any], dict[str, int]]:
    """外側の許可外宛先（127.0.0.2/127.0.0.3）の listener を立て、(listener 辞書, ポート辞書) を返す。"""
    recs: dict[str, Any] = {}
    for r in ("redirect", "img", "fetch", "ws"):
        recs[r] = listeners.TcpRecorder(r, judge.IP_BLOCKED).start()
    recs["internal"] = listeners.TcpRecorder("internal", judge.IP_INTERNAL_STANDIN).start()
    recs["udp_direct"] = listeners.UdpRecorder("udp_direct", judge.IP_BLOCKED).start()
    return recs, {k: v.port for k, v in recs.items()}


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


def _stop_log_proc(proc: Any) -> None:
    """`log show` 系の子プロセスを止める。SIGTERM（sudo は子の log へ中継する）を先に送り、期限後に SIGKILL する。

    sudo への SIGKILL は子の `log` に伝わらずパイプが開いたままになるため、この順にする（Issue #1402）。
    """
    try:
        proc.terminate()
        proc.wait(timeout=5)
    except (subprocess.TimeoutExpired, OSError):
        try:
            proc.kill()
        except OSError:
            pass


def collect_denies(start: str, pids: frozenset[int] | None = None, *,
                   sudo: bool = False) -> tuple[dict[str, int], dict[str, Any]]:
    """macOS のサンドボックス拒否ログを逐次読んで集計する。(宛先 -> 件数, 取得状況)。読めなければ空と理由。

    `pids` は対象ホストとその子孫の pid。拒否ログの `proc(<pid>)` が含まれる行だけを数える（無関係な
    プロセスの同宛先の拒否を試行の根拠にしない）。

    出力を全量メモリに載せず、1 行ずつ上限つきで読む。行数・総バイト数が上限に達したら
    プロセスを止めて unobservable（available=False）にする（リソース枯渇の防止）。

    `sudo=True` は opt-in（Issue #1402。xos-measure の macOS ジョブのみ）。`sudo -n log show --info --debug` を使う。
    """
    try:
        argv = seatbelt.log_show_argv(start, sudo=sudo)
        proc = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,  # noqa: S603
                                stderr=subprocess.DEVNULL, start_new_session=True)
    except (OSError, subprocess.SubprocessError, ValueError) as exc:
        return {}, {"available": False, "reason": type(exc).__name__}
    timer = threading.Timer(180, _stop_log_proc, args=(proc,))
    timer.start()
    counts: Counter[str] = Counter()
    diag = seatbelt.DenyDiagnostics()
    lines = total = 0
    over = False
    try:
        assert proc.stdout is not None
        while True:
            raw = proc.stdout.readline(seatbelt.MAX_LINE_LEN + 1)
            if not raw:
                break
            total += len(raw)
            if not raw.endswith(b"\n") and len(raw) > seatbelt.MAX_LINE_LEN:
                # 長すぎる行は残りを捨てる（メモリに溜めない）。
                while True:
                    rest = proc.stdout.readline(seatbelt.MAX_LINE_LEN)
                    total += len(rest)
                    if not rest or rest.endswith(b"\n") or total > seatbelt.MAX_SCAN_BYTES:
                        break
                lines += 1
            else:
                lines += 1
                dest = seatbelt.scan_line(raw.decode("utf-8", errors="replace").rstrip("\r\n"), pids, diag)
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
    if proc.returncode != 0:
        return {}, {"available": False, "reason": f"log show rc={proc.returncode}" + (" (sudo)" if sudo else "")}
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
             launcher: Any = None, loopback_exempt: bool = False, sudo_log: str | None = None,
             log_runner: Any = None) -> dict[str, Any]:
    """`sudo_log` は seatbelt だけで効く opt-in（None / "show" / "config"。Issue #1402）。"""
    if mode == "appcontainer":
        return run_appcontainer_mode(host=host, ports=ports, recs=recs, out_dir=out_dir, page_port=page_port,
                                     launcher=launcher, loopback_exempt=loopback_exempt)
    before = {k: r.count for k, r in recs.items()}
    # docker はコンテナ名を付けて、タイムアウト・例外時にも確実に後始末する（名前は pid 由来の固定形式）。
    name = f"egress-iso-{os.getpid()}" if mode == "docker-none" else None
    argv = build_argv(mode, host=host, ports=ports, out_dir=out_dir, hosts_file=hosts_file, image=image,
                      resolv_file=resolv_file, name=name, page_port=page_port)
    use_sudo = mode == "seatbelt" and sudo_log in ("show", "config")
    cfg = seatbelt.SandboxLogConfig(runner=log_runner) if mode == "seatbelt" and sudo_log == "config" else None
    denies: dict[str, int] = {}
    status: dict[str, Any] = {}
    unpriv: dict[str, Any] | None = None
    pids: frozenset[int] = frozenset()
    inner: dict[str, Any] = {}
    # log config を上げた場合は、内側の実行・log show のどこで失敗しても finally で必ず戻す（Issue #1402）。
    try:
        if cfg is not None:
            cfg.__enter__()
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
            if pids:
                denies, status = collect_denies(started, pids, sudo=use_sudo)
                if use_sudo:
                    # 権限の差かレベルの差かを切り分ける比較用（診断カウンタだけ。試行の根拠には使わない）。
                    _, ustat = collect_denies(started, pids)
                    unpriv = {k: ustat.get(k) for k in ("available", "reason", "deny_lines", "diagnostics")
                              if k in ustat}
            else:
                # 対象 PID を特定できないときは帰属を確認できないため、拒否ログを試行の根拠にしない（unobservable）。
                denies, status = {}, {"available": False, "reason": "host pid not recorded"}
    finally:
        if cfg is not None:
            cfg.__exit__(None, None, None)
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
        if cfg is not None and not cfg.state.get("all_restored") and status.get("available"):
            # 片付けが不完全なら拒否ログを根拠にしない（pass にしない。udp の errno 根拠は残る）。
            status = {**status, "available": False, "reason": "log config not restored"}
        observed = dict(denies)  # 診断用に記録する生の件数（根拠に使うかは status で決まる）
        if not status.get("available"):
            denies = {}
        # 拒否ログの結果を、内側ランナーの試行記録（UDP の errno 由来）へ OR で合成する。ビーコンは根拠にしない。
        inner["attempts"], inner["attempt_evidence"] = seatbelt.compose_attempts(
            inner.get("attempts") or {}, inner.get("attempt_evidence") or {}, denies, dest, udp)
        data["deny_log"] = {**status, "dest_counts": {d: observed.get(d, 0) for d in (*dest.values(), udp)},
                            "sudo": use_sudo,
                            "log_show_flags": "sudo -n, --info --debug" if use_sudo else "default",
                            "log_config": cfg.state if cfg is not None else None,
                            "unprivileged": unpriv}
    return data


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
    for mode, data in results["modes"].items():
        if mode == "baseline":
            continue
        inner = data.get("inner", {})
        lines += ["", f"## {mode}", ""]
        lines.append(f"- exit code: {data.get('exit_code')}; positive_ok: {inner.get('positive_ok')}; "
                     f"selftest_rc: {inner.get('selftest_rc')}; aborted: {inner.get('aborted', False)}")
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
            lines.append(f"- sandbox deny log: {dl}")
            if dl.get("sudo") or dl.get("log_config"):
                lc = dl.get("log_config") or {}
                lines.append(f"- deny log opt-in (Issue #1402): sudo={dl.get('sudo')}, flags={dl.get('log_show_flags')}, "
                             f"log config all_restored={lc.get('all_restored')}, unprivileged diagnostics={dl.get('unprivileged')}")
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
    return "\n".join(lines) + "\n"


def validate_loopback_exempt(requested: bool, plat: str, modes: list[str]) -> None:
    """`--appcontainer-loopback-exempt` は Windows の appcontainer モードがあるときだけ受け付ける（Issue #1401）。"""
    if requested and (plat != "win32" or "appcontainer" not in modes):
        raise ValueError("--appcontainer-loopback-exempt needs win32 and the appcontainer mode")


def _unavailable_deny_log_reason(data: dict[str, Any]) -> str | None:
    """拒否ログが使えなかったときの理由（judge の reason へ足す。使えた・無いときは None）。"""
    dl = data.get("deny_log") or {}
    return dl.get("reason") if dl and not dl.get("available") else None


def validate_seatbelt_sudo_log(value: str | None, plat: str, modes: list[str]) -> None:
    """`--seatbelt-sudo-log` は macOS の seatbelt モードがあるときだけ受け付ける（Issue #1402）。"""
    if value is None:
        return
    if value not in ("show", "config"):
        raise ValueError("--seatbelt-sudo-log must be show or config")
    if plat != "darwin" or "seatbelt" not in modes:
        raise ValueError("--seatbelt-sudo-log needs darwin and the seatbelt mode")


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
    parser.add_argument("--seatbelt-sudo-log", choices=("show", "config"), default=None,
                        help="Opt-in (macOS seatbelt only; used only by the xos-measure macOS job on a throwaway runner): "
                        "show = read the deny log with `sudo -n log show --info --debug`; config = show plus raise the "
                        "sandbox subsystems' log level with `sudo log config` and reset it afterwards (Issue #1402).")
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
        validate_seatbelt_sudo_log(args.seatbelt_sudo_log, plat, modes)
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
        recs, ports = start_outer_listeners()
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
                "seatbelt_sudo_log": args.seatbelt_sudo_log,
            }
            for mode in modes:
                # モードごとにポートを取り直す（close 直後の再 bind が EADDRINUSE になるのを避ける）。
                page_port = 0 if sys.platform.startswith("linux") else free_port()
                results["modes"][mode] = run_mode(mode, host=host, ports=ports, recs=recs, out_dir=out_dir,
                                                  hosts_file=hosts_file, image=image,
                                                  resolv_file=resolv_file, page_port=page_port,
                                                  loopback_exempt=args.appcontainer_loopback_exempt,
                                                  sudo_log=args.seatbelt_sudo_log)
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
                                               deny_log_reason=_unavailable_deny_log_reason(data))
    (out_dir / "result.json").write_text(json.dumps(results, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    summary = summarize(results)
    (out_dir / "summary.md").write_text(summary, encoding="utf-8")
    print(summary)
    return 0


if __name__ == "__main__":
    sys.exit(main())
