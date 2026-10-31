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
    拒否ログ。Windows は baseline のみ実測し、`appcontainer` は起動に FFI（ctypes）が要るため承認事項として
    保留し、全経路を理由つきの `unobservable` で出す（設計 §7）。macOS・Windows にはプロセス単位の監視が無い。

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
import time
from pathlib import Path
from typing import Any

_HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(_HERE))
import judge  # noqa: E402
import listeners  # noqa: E402
import seatbelt  # noqa: E402

APPCONTAINER_UNAVAILABLE = (
    "AppContainer launcher needs FFI (CreateProcessW with SECURITY_CAPABILITIES via ctypes); "
    "approval item (design section 7); not implemented"
)

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
        argv = ["docker", "run", "--rm", "--network", "none", "--user", f"{os.getuid()}:{os.getgid()}",
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


def collect_denies(start: str) -> tuple[dict[str, int], dict[str, Any]]:
    """macOS のサンドボックス拒否ログを集計する。(宛先 -> 件数, 取得状況)。読めなければ空と理由。"""
    try:
        done = subprocess.run(seatbelt.log_show_argv(start), stdin=subprocess.DEVNULL,  # noqa: S603
                              capture_output=True, text=True, timeout=180, check=False)
    except (OSError, subprocess.SubprocessError, ValueError) as exc:
        return {}, {"available": False, "reason": type(exc).__name__}
    if done.returncode != 0:
        return {}, {"available": False, "reason": f"log show rc={done.returncode}"}
    counts = seatbelt.parse_denies(done.stdout)
    return dict(counts), {"available": True, "deny_lines": sum(counts.values())}


def run_mode(mode: str, *, host: Path, ports: dict[str, int], recs: dict[str, Any], out_dir: Path,
             hosts_file: Path, image: str | None, resolv_file: Path | None = None, page_port: int = 0) -> dict[str, Any]:
    if mode == "appcontainer":
        return {"mode": mode, "exit_code": None, "binary_sha256": None, "binary_version": None,
                "stderr_tail": "", "inner": {}, "outer_counts": {k: 0 for k in recs},
                "unavailable": APPCONTAINER_UNAVAILABLE}
    before = {k: r.count for k, r in recs.items()}
    # docker はコンテナ名を付けて、タイムアウト・例外時にも確実に後始末する（名前は pid 由来の固定形式）。
    name = f"egress-iso-{os.getpid()}" if mode == "docker-none" else None
    argv = build_argv(mode, host=host, ports=ports, out_dir=out_dir, hosts_file=hosts_file, image=image,
                      resolv_file=resolv_file, name=name, page_port=page_port)
    started = time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(time.time() - 2))
    rc, err = run_argv(argv, container=name)
    outer = {k: r.count - before[k] for k, r in recs.items()}
    result_path = out_dir / f"{mode}.json"
    inner: dict[str, Any] = {}
    # 内側ランナーが正常終了（0）または隔離 abort（2。理由つきの JSON を書く）した場合の今回の結果だけを使う。
    # それ以外の失敗は空 = 判定不能（合格にしない）。--out は空ディレクトリ検査済みで古い結果は混ざらない。
    if rc in (0, 2) and result_path.is_file():
        try:
            inner = json.loads(result_path.read_text(encoding="utf-8"))
        except ValueError:
            inner = {}
    data = {"mode": mode, "exit_code": rc, "binary_sha256": inner.get("host_sha256"),
            "binary_version": inner.get("host_version"), "stderr_tail": err if rc != 0 else "", "inner": inner,
            "outer_counts": outer}
    if mode == "seatbelt" and inner:
        # 共有 loopback のため内側の写し listener が無い。TCP の試行は拒否ログ（宛先 ip:port）で観測する。
        denies, status = collect_denies(started)
        dest = {r: f"{judge.IP_INTERNAL_STANDIN if r == 'internal' else judge.IP_BLOCKED}:{ports[r]}"
                for r in ("redirect", "img", "fetch", "ws", "internal")}
        attempts = dict(inner.get("attempts") or {})
        for route, seen in seatbelt.attempts_from_denies(
                Counter(denies), dest).items():
            attempts[route] = seen
        udp = f"{judge.IP_BLOCKED}:{ports['udp_direct']}"
        attempts["udp_direct"] = bool(attempts.get("udp_direct")) or denies.get(udp, 0) >= 1
        inner["attempts"] = attempts
        data["deny_log"] = {**status, "dest_counts": {d: denies.get(d, 0) for d in (*dest.values(), udp)}}
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
        if data.get("deny_log"):
            lines.append(f"- sandbox deny log: {data['deny_log']}")
        lines.append(f"- isolation precheck: {inner.get('isolation_precheck')}")
        lines.append(f"- binary sha256 (inside sandbox): {data.get('binary_sha256')}; "
                     f"matches baseline: {data.get('binary_match')}")
        if data.get("image_id"):
            lines.append(f"- docker image id: {data['image_id']}")
        lines += ["", "| route | verdict | outer | control | method | reason |", "| --- | --- | --- | --- | --- | --- |"]
        for r, v in data.get("verdicts", {}).items():
            lines.append(f"| {r} | {v['verdict']} | {v['outer_count']} | {v['control_count']} | {v['method']} | {v['reason']} |")
    return "\n".join(lines) + "\n"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Measure Servo host egress isolation (Linux, macOS, Windows).")
    parser.add_argument("--host", required=True, help="Absolute path to fandhe-browser-servo-host.")
    parser.add_argument("--modes", default="baseline,bwrap-netns",
                        help="Comma separated modes allowed on this OS (baseline first): linux baseline,bwrap-netns,docker-none; "
                        "darwin baseline,seatbelt; win32 baseline,appcontainer.")
    parser.add_argument("--docker-image", help="Image containing the host at " + DOCKER_HOST_PATH)
    parser.add_argument("--out", required=True, type=Path, help="Existing empty directory for results.")
    args = parser.parse_args(argv)
    try:
        plat = "linux" if sys.platform.startswith("linux") else sys.platform
        allowed = judge.PLATFORM_MODES.get(plat)
        if allowed is None:
            raise ValueError(f"unsupported platform: {sys.platform}")
        host = validate_host(args.host)
        modes = args.modes.split(",")
        if not modes or modes[0] != "baseline" or any(m not in allowed for m in modes):
            raise ValueError("--modes must start with baseline and use known modes")
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
        page_port = 0 if sys.platform.startswith("linux") else free_port()
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
                results["modes"][mode] = run_mode(mode, host=host, ports=ports, recs=recs, out_dir=out_dir,
                                                  hosts_file=hosts_file, image=image,
                                                  resolv_file=resolv_file, page_port=page_port)
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
                                               mode_unavailable=data.get("unavailable"))
    (out_dir / "result.json").write_text(json.dumps(results, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    summary = summarize(results)
    (out_dir / "summary.md").write_text(summary, encoding="utf-8")
    print(summary)
    return 0


if __name__ == "__main__":
    sys.exit(main())
