#!/usr/bin/env python3
"""egress 隔離手段の可否（Linux・macOS・Windows）を問い合わせる読み取り専用プローブ。

役割・責務境界:
    Issue #1377（TASK-38・`RENDER-3`・`SEC-2`・`XOS-1`）。設計 `docs/design/servo-host-egress-isolation.md`
    §6.2 の Linux の項目（Landlock ABI・netns の作り方ごとの可否・sysctl・Docker 内での挙動）を記録する。
    ルールセットの作成・`landlock_restrict_self` は行わない（承認事項。ABI の問い合わせは NULL・サイズ 0 の 1 回のみ）。

呼び出し文脈: 単体で実行し JSON を標準出力へ出す（`measure.sh` が計測結果と並べて保存する）。
外部コマンドは固定 argv・`shell=False`・タイムアウトつき。出力には errno の種別と終了コードだけを残す。
"""

from __future__ import annotations

import argparse
import ctypes
import errno
import json
import os
import platform
import re
import subprocess
import sys
from pathlib import Path
from typing import Any, Callable

LANDLOCK_CREATE_RULESET_VERSION = 1
# landlock_create_ruleset の syscall 番号は x86_64・aarch64 で共通（それ以外のアーキテクチャでは問い合わせない）。
LANDLOCK_SYSCALL_NR = {"x86_64": 444, "aarch64": 444}
SYSCTLS = (
    "kernel/apparmor_restrict_unprivileged_userns",
    "kernel/unprivileged_userns_clone",
    "user/max_user_namespaces",
    "net/ipv4/ip_unprivileged_port_start",
)
DOCKER_IMAGE_RE = re.compile(r"^[a-z0-9][a-z0-9._/:@-]{0,127}$")

Runner = Callable[[list[str]], tuple[int | None, str]]
# 標準出力も受け取る実行関数 (終了コード, stdout, stderr の末尾)。コンテナ内の問い合わせ結果を構造化して取るために使う。
OutRunner = Callable[[list[str]], tuple[int | None, str, str]]
# `docker run` 自身が使う終了コード（125: docker の失敗、126: 実行不可、127: コマンドなし）。コンテナ内の結果とは別扱いにする。
DOCKER_OWN_EXIT_CODES = (125, 126, 127)


def default_runner(argv: list[str]) -> tuple[int | None, str]:
    """固定 argv を実行し (終了コード, stderr の末尾) を返す。見つからない・タイムアウトは (None, 理由)。"""
    try:
        done = subprocess.run(argv, capture_output=True, text=True, timeout=30, check=False)  # noqa: S603
    except FileNotFoundError:
        return None, "not installed"
    except subprocess.TimeoutExpired:
        return None, "timed out"
    return done.returncode, done.stderr.strip()[-200:]


def default_out_runner(argv: list[str]) -> tuple[int | None, str, str]:
    """固定 argv を実行し (終了コード, stdout, stderr の末尾) を返す。見つからない・タイムアウトは (None, "", 理由)。"""
    try:
        done = subprocess.run(argv, capture_output=True, text=True, timeout=30, check=False)  # noqa: S603
    except FileNotFoundError:
        return None, "", "not installed"
    except subprocess.TimeoutExpired:
        return None, "", "timed out"
    return done.returncode, done.stdout, done.stderr.strip()[-200:]


def classify(rc: int | None, err: str) -> str:
    """終了コードと stderr から結果の種別（ok / EPERM / ENOSYS 等 / not installed）を返す。"""
    if rc == 0:
        return "ok"
    if rc is None:
        return err
    if rc == 127:
        return "not installed"
    low = err.lower()
    for token, name in (("operation not permitted", "EPERM"), ("permission denied", "EACCES"),
                        ("function not implemented", "ENOSYS"), ("invalid argument", "EINVAL")):
        if token in low:
            return name
    return f"failed(rc={rc})"


def landlock_abi(syscall: Callable[..., int] | None = None, machine: str | None = None) -> dict[str, Any]:
    """Landlock の ABI バージョンを問い合わせる（ルールセットは作らない）。"""
    machine = machine or platform.machine()
    info: dict[str, Any] = {"machine": machine}
    nr = LANDLOCK_SYSCALL_NR.get(machine)
    try:
        lsm = Path("/sys/kernel/security/lsm").read_text(encoding="ascii").strip()
        info["lsm_has_landlock"] = "landlock" in lsm.split(",")
    except OSError:
        info["lsm_has_landlock"] = None
    if nr is None:
        info["abi"] = None
        info["note"] = "unsupported architecture for the query"
        return info
    if syscall is None:
        libc = ctypes.CDLL(None, use_errno=True)
        syscall = libc.syscall
    ctypes.set_errno(0)
    ret = syscall(nr, None, 0, LANDLOCK_CREATE_RULESET_VERSION)
    if ret < 0:
        err = ctypes.get_errno()
        info["abi"] = None
        info["errno"] = errno.errorcode.get(err, str(err))
    else:
        info["abi"] = int(ret)
    return info


def read_sysctls(root: Path = Path("/proc/sys")) -> dict[str, str | None]:
    out: dict[str, str | None] = {}
    for key in SYSCTLS:
        try:
            out[key.replace("/", ".")] = (root / key).read_text(encoding="ascii").strip()[:16]
        except OSError:
            out[key.replace("/", ".")] = None
    return out


def netns_capabilities(run: Runner = default_runner) -> dict[str, str]:
    """netns の作り方ごとの可否。bwrap の ns 内で CAP_NET_ADMIN が使えるか（`ip addr add`）も確かめる。"""
    cases = {
        "unshare -rn": ["unshare", "-rn", "true"],
        "unshare -n": ["unshare", "-n", "true"],
        "bwrap --unshare-net": ["bwrap", "--dev-bind", "/", "/", "--unshare-net", "true"],
        "bwrap --unshare-net + ip addr add (CAP_NET_ADMIN)": [
            "bwrap", "--dev-bind", "/", "/", "--unshare-net", "ip", "addr", "add", "10.0.0.2/24", "dev", "lo"],
    }
    return {name: classify(*run(argv)) for name, argv in cases.items()}


def parse_landlock_json(rc: int | None, stdout: str, stderr: str) -> str:
    """コンテナ内の Landlock ABI 問い合わせ結果（標準出力の JSON 1 行）を判定語彙の文字列にする。

    終了コードは docker run 自身の失敗（125・126・127 等）の判別にだけ使い、Landlock の errno とは混ぜない。
    docker の失敗・出力なし・壊れた JSON・想定外の形はすべて `unobservable`（fail-closed。judge の語彙に合わせる）。
    """
    if rc is None:
        return f"unobservable: {stderr}"
    if rc in DOCKER_OWN_EXIT_CODES or rc != 0:
        return f"unobservable: docker run exited with rc={rc}"
    lines = [ln for ln in stdout.splitlines() if ln.strip()]
    if len(lines) != 1:
        return "unobservable: no single JSON line on stdout"
    try:
        data = json.loads(lines[0])
    except ValueError:
        return "unobservable: malformed JSON on stdout"
    if not isinstance(data, dict):
        return "unobservable: unexpected JSON shape"
    ret, err = data.get("ret"), data.get("errno")
    if isinstance(ret, int) and not isinstance(ret, bool) and ret >= 0 and err == 0:
        return "ok"
    if isinstance(ret, int) and not isinstance(ret, bool) and ret < 0 and isinstance(err, int) and err > 0:
        return f"errno {errno.errorcode.get(err, err)}"
    return "unobservable: unexpected JSON shape"


def docker_probe(image: str, run: Runner = default_runner, run_out: OutRunner = default_out_runner) -> dict[str, str]:
    """`--network none` の Docker 内で unshare・bwrap・Landlock の問い合わせがどうなるかを記録する。"""
    if not DOCKER_IMAGE_RE.match(image):
        raise ValueError("unexpected image name format")
    base = ["docker", "run", "--rm", "--network", "none", "--entrypoint"]
    code = (
        "import ctypes,json;l=ctypes.CDLL(None,use_errno=True);r=l.syscall(444,None,0,1);"
        "print(json.dumps({'ret':r,'errno':ctypes.get_errno() if r<0 else 0}))"
    )
    out = {
        "unshare -n": classify(*run([*base, "unshare", image, "-n", "true"])),
        "bwrap --unshare-net": classify(*run([*base, "bwrap", image, "--dev-bind", "/", "/", "--unshare-net", "true"])),
    }
    out["landlock ABI query"] = parse_landlock_json(*run_out([*base, "python3", image, "-c", code]))
    return out


def tool_versions(run_text: Callable[[list[str]], str] | None = None) -> dict[str, str]:
    def _run(argv: list[str]) -> str:
        try:
            done = subprocess.run(argv, capture_output=True, text=True, timeout=20, check=False)  # noqa: S603
        except (FileNotFoundError, subprocess.TimeoutExpired):
            return "unavailable"
        return (done.stdout or done.stderr).strip().splitlines()[0][:80] if (done.stdout or done.stderr).strip() else ""

    run_text = run_text or _run
    return {"bwrap": run_text(["bwrap", "--version"]), "docker": run_text(["docker", "--version"])}


def collect(image: str | None = None) -> dict[str, Any]:
    result: dict[str, Any] = {
        "kernel": os.uname().release,
        "machine": platform.machine(),
        "landlock": landlock_abi(),
        "sysctl": read_sysctls(),
        "netns": netns_capabilities(),
        "tools": tool_versions(),
    }
    if image:
        result["docker_none"] = docker_probe(image)
    return result


def can_bind(ip: str) -> str:
    """ip に TCP で bind できるか（できれば "ok"、できなければ errno 名）。bind するだけで接続しない。"""
    import socket

    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        sock.bind((ip, 0))
        return "ok"
    except OSError as exc:
        return errno.errorcode.get(exc.errno or 0, "oserror")
    finally:
        sock.close()


def collect_macos(run: Runner = default_runner, run_out: OutRunner = default_out_runner) -> dict[str, Any]:
    """macOS の項目（#1387）。sandbox-exec の可否・OS 版・127.0.0.2/.3 を bind できるか。読み取りのみ。"""
    _, ver, _ = run_out(["/usr/bin/sw_vers", "-productVersion"])
    return {
        "platform": "darwin",
        "machine": platform.machine(),
        "sw_vers": ver.strip()[:40],
        "sandbox_exec_allow_default": classify(*run(
            ["/usr/bin/sandbox-exec", "-p", "(version 1)(allow default)", "/usr/bin/true"])),
        "bind_127.0.0.2": can_bind("127.0.0.2"),
        "bind_127.0.0.3": can_bind("127.0.0.3"),
    }


def collect_windows(run: Runner = default_runner, run_out: OutRunner = default_out_runner) -> dict[str, Any]:
    """Windows の項目（#1387）。昇格状態・CheckNetIsolation の有無と一覧表示の終了コード・OS ビルド。読み取りのみ。

    loopback 例外の設定・ファイアウォール規則の変更は行わない。runner は管理者で動くため、
    「非管理者で loopback 例外を作れるか」には答えられない（設計 §6.2）。
    """
    rc, out, err = run_out(["whoami", "/groups"])
    elevated = None if rc != 0 else ("S-1-16-12288" in out)
    return {
        "platform": "win32",
        "machine": platform.machine(),
        "build": platform.version()[:40],
        "high_integrity_level": elevated,
        "CheckNetIsolation LoopbackExempt -s": classify(*run(["CheckNetIsolation.exe", "LoopbackExempt", "-s"])),
        "whoami_error": err if rc != 0 else "",
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Read-only probe of Linux egress isolation primitives.")
    parser.add_argument("--docker-image", help="Optional image for the --network none probe.")
    args = parser.parse_args(argv)
    try:
        if sys.platform == "darwin":
            result = collect_macos()
        elif sys.platform == "win32":
            result = collect_windows()
        elif sys.platform.startswith("linux"):
            result = collect(args.docker_image)
        else:
            print(f"error: unsupported platform: {sys.platform}", file=sys.stderr)
            return 2
    except ValueError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
