#!/usr/bin/env python3
"""Servo ホストの egress 隔離を Linux で実測する外側の計測ランナー。

役割・責務境界:
    Issue #1377（TASK-38・`RENDER-3`・`SEC-2`・`PLUG-1`・`XOS-1`）。親判断 #1003 の比較設計
    `docs/design/servo-host-egress-isolation.md` §6 の共通観測方法を実装する。方式の採否は決めず、
    測定条件つきの事実（各経路の pass / fail / unobservable と観測手段）を JSON と Markdown に出す。

動作:
    1. 外側（ホストの netns）に許可外の宛先の listener（loopback のダミー）を立てる。
    2. モードごとに内側ランナー（`in_sandbox.py`）を bwrap / docker の固定 argv で起動する。
       baseline は `--unshare-net` だけを外した同条件（陽性対照）。docker-none は `--network none`。
    3. 外側の受信件数と内側の結果を `judge.py` で突き合わせる。観測できなければ不合格（fail-closed）。

安全上の規則:
    - 宛先は 127.0.0.0/8 のダミーだけ。実在のアドレスへ向けたプローブは、隔離下の内側で
      lo 以外の経路が無いことを確かめた後にのみ行う（`in_sandbox.py`）。
    - argv は定数リストで `shell=False`。`--host` は実行可能ファイルの絶対パス、`--docker-image` は正規表現で検証。
    - ログ・結果に URL のクエリ・本文・プロキシ系環境変数の値を載せない（有無だけ）。

終了コード: 0（計測が完走。経路判定の合否は結果 JSON を見る）／1（計測の失敗）／2（引数不正）。
"""

from __future__ import annotations

import argparse
import json
import os
import re
import signal
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any

_HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(_HERE))
import judge  # noqa: E402
import listeners  # noqa: E402

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


def build_argv(mode: str, *, host: Path, ports: dict[str, int], out_dir: Path, hosts_file: Path,
               image: str | None) -> list[str]:
    """内側ランナーを起動する固定 argv。ポートは int を文字列化したものだけが入る。"""
    inner_args = ["--mode", mode, "--ports", json.dumps(ports, sort_keys=True)]
    if mode in ("baseline", "bwrap-netns"):
        argv = ["bwrap", "--die-with-parent"]
        if mode == "bwrap-netns":
            argv.append("--unshare-net")
        argv += ["--dev-bind", "/", "/", "--ro-bind", str(hosts_file), "/etc/hosts", "--",
                 sys.executable, "-I", "-B", str(_HERE / "in_sandbox.py"),
                 "--host", str(host), "--out", str(out_dir / f"{mode}.json"), *inner_args]
        return argv
    if mode == "docker-none":
        if image is None:
            raise ValueError("docker-none needs --docker-image")
        argv = ["docker", "run", "--rm", "--network", "none", "--user", f"{os.getuid()}:{os.getgid()}",
                "--tmpfs", "/tmp"]
        for name, ip in (("pages.test", judge.IP_ALLOWED), (judge.HOST_BLOCKED, judge.IP_BLOCKED),
                         (judge.HOST_INTERNAL_STANDIN, judge.IP_INTERNAL_STANDIN)):
            argv += ["--add-host", f"{name}:{ip}"]
        argv += ["-v", f"{_HERE.parent}:/harness:ro", "-v", f"{out_dir}:/out", "--entrypoint", "python3", image,
                 "-I", "-B", "/harness/egress-isolation/in_sandbox.py", "--host", DOCKER_HOST_PATH,
                 "--out", f"/out/{mode}.json", *inner_args]
        return argv
    raise ValueError(f"unknown mode: {mode}")


def run_argv(argv: list[str]) -> tuple[int | None, str]:
    """argv を実行する（シェル不使用・タイムアウトつき・プロセスグループごと終了）。"""
    proc = subprocess.Popen(  # noqa: S603
        argv, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
        start_new_session=True,
    )
    try:
        _, err = proc.communicate(timeout=RUN_TIMEOUT_S)
        return proc.returncode, err.decode("utf-8", errors="replace")[-400:]
    except subprocess.TimeoutExpired:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        proc.communicate()
        return None, "timed out"


def run_mode(mode: str, *, host: Path, ports: dict[str, int], recs: dict[str, Any], out_dir: Path,
             hosts_file: Path, image: str | None) -> dict[str, Any]:
    before = {k: r.count for k, r in recs.items()}
    argv = build_argv(mode, host=host, ports=ports, out_dir=out_dir, hosts_file=hosts_file, image=image)
    rc, err = run_argv(argv)
    outer = {k: r.count - before[k] for k, r in recs.items()}
    result_path = out_dir / f"{mode}.json"
    inner: dict[str, Any] = {}
    # ランナーが正常終了（rc == 0）した場合の今回の結果だけを使う。失敗時は空 = 判定不能（合格にしない）。
    if rc == 0 and result_path.is_file():
        try:
            inner = json.loads(result_path.read_text(encoding="utf-8"))
        except ValueError:
            inner = {}
    return {"mode": mode, "exit_code": rc, "stderr_tail": err if rc != 0 else "", "inner": inner,
            "outer_counts": outer}


def summarize(results: dict[str, Any]) -> str:
    lines = ["# egress isolation measurement (Linux)", ""]
    meta = results["meta"]
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
        lines += ["", "| route | verdict | outer | control | method | reason |", "| --- | --- | --- | --- | --- | --- |"]
        for r, v in data.get("verdicts", {}).items():
            lines.append(f"| {r} | {v['verdict']} | {v['outer_count']} | {v['control_count']} | {v['method']} | {v['reason']} |")
    return "\n".join(lines) + "\n"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Measure Servo host egress isolation on Linux.")
    parser.add_argument("--host", required=True, help="Absolute path to fandhe-browser-servo-host.")
    parser.add_argument("--modes", default="baseline,bwrap-netns",
                        help="Comma separated subset of baseline,bwrap-netns,docker-none (baseline first).")
    parser.add_argument("--docker-image", help="Image containing the host at " + DOCKER_HOST_PATH)
    parser.add_argument("--out", required=True, type=Path, help="Existing empty directory for results.")
    args = parser.parse_args(argv)
    try:
        if not sys.platform.startswith("linux"):
            raise ValueError("Linux only")
        host = validate_host(args.host)
        modes = args.modes.split(",")
        if not modes or modes[0] != "baseline" or any(m not in judge.MODES for m in modes):
            raise ValueError("--modes must start with baseline and use known modes")
        image = validate_image(args.docker_image) if args.docker_image else None
        if "docker-none" in modes and image is None:
            raise ValueError("docker-none needs --docker-image")
        if not args.out.is_dir():
            raise ValueError("--out must be an existing directory")
        if any(args.out.iterdir()):
            raise ValueError("--out must be an empty directory (stale results must not be reused)")
    except ValueError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2

    out_dir = args.out.resolve()
    os.chmod(out_dir, 0o777)  # docker の --user でも書けるように（結果にはクエリ・本文を含まない）
    with tempfile.TemporaryDirectory(prefix="egress-hosts-") as td:
        hosts_file = Path(td) / "hosts"
        hosts_file.write_text(judge.HOSTS_FILE, encoding="utf-8")
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
                "kernel": os.uname().release, "host_sha256": sha, "host_version": version,
                "proxy_env_present": any(k.lower().endswith("_proxy") for k in os.environ),
            }
            for mode in modes:
                results["modes"][mode] = run_mode(mode, host=host, ports=ports, recs=recs, out_dir=out_dir,
                                                  hosts_file=hosts_file, image=image)
        finally:
            for r in recs.values():
                r.stop()
    control = results["modes"]["baseline"]["outer_counts"]
    for mode, data in results["modes"].items():
        if mode in judge.ISOLATED_MODES:
            data["verdicts"] = judge.judge_all(mode, data["inner"], data["outer_counts"], control)
    (out_dir / "result.json").write_text(json.dumps(results, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    summary = summarize(results)
    (out_dir / "summary.md").write_text(summary, encoding="utf-8")
    print(summary)
    return 0


if __name__ == "__main__":
    sys.exit(main())
