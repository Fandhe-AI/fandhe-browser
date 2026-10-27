#!/usr/bin/env python3
"""保存済みページ撮影（`--saved-pages`）の前提となるネットワーク隔離を撮影前に確かめる。

役割・責務境界:
    Issue #1281（TASK-37・`RENDER-5`・関連 `MEAS-3`）。#55 のオーナー判断（2026-10-27）で、
    `docker run --network none` のコンテナは RUNBOOK 5.1 の再開条件を満たす隔離として
    認められた。本モジュールは「実際にそのコンテナの中で外部へ出られないこと」と
    「loopback の `pages.test` からページが取れること」を、エンジンを起動する前に
    確かめる。1 項目でも外れる・判定できない場合は `isolated: false`（fail-closed）。

呼び出し文脈:
    - `capture_screenshots.py --saved-pages` が撮影直前にプロセス内で `run_isolation_check`
      を呼び、結果を結果 JSON の `isolation` に埋め込む。外部のレポートファイルは
      読み込まない（古い・差し替えられたレポートを受け付けないため）。
    - `container/run-in-container.sh` が単体 CLI として呼び、`/out/isolation-report.json`
      に記録を残す。
    ネットワーク操作（名前解決・接続・HTTP GET・`/proc/net/dev`）は引数で差し替え可能で、
    自己テスト（`test_isolation_check.py`）は実ソケットを使わない。

セキュリティ上の注意:
    外部接続の試行は TCP の connect のみで、接続できた場合も何も送らず即座に閉じる。
    レポートの `detail` はエラー種別・errno 程度に限り、取得した本文は載せない。
    プロキシ系の環境変数は値を出さず有無だけを記録する。Python 標準ライブラリのみ。
"""

from __future__ import annotations

import argparse
import http.client
import ipaddress
import json
import os
import socket
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable
from urllib.parse import urlparse

REPORT_SCHEMA_VERSION = 1

# 外部到達性の確認対象。いずれも「接続できてはならない」宛先。
EXTERNAL_DNS_NAME = "example.com"
EXTERNAL_IPV4_TARGETS = (("1.1.1.1", 443), ("169.254.169.254", 80))
EXTERNAL_IPV6_TARGETS = (("2606:4700:4700::1111", 443),)
CONNECT_TIMEOUT_SEC = 3.0
HTTP_TIMEOUT_SEC = 10.0
# loopback ページの本文読み込み上限（巨大応答による資源消費を防ぐ）。
HTTP_MAX_BODY_BYTES = 8 * 1024 * 1024
PROC_NET_DEV = Path("/proc/net/dev")
PROXY_ENV_KEYS = (
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
)

Resolver = Callable[[str, int], list[str]]
Connector = Callable[[str, int, float], None]
HttpGet = Callable[[str, float, int], tuple[int, int]]


def _default_resolver(host: str, port: int) -> list[str]:
    """名前解決結果の IP 文字列一覧を返す（失敗は `OSError`）。"""
    infos = socket.getaddrinfo(host, port, type=socket.SOCK_STREAM)
    return sorted({str(info[4][0]) for info in infos})


def _default_connector(host: str, port: int, timeout: float) -> None:
    """TCP 接続を試みる（成功時は何も送らず閉じる。失敗は `OSError`）。"""
    sock = socket.create_connection((host, port), timeout=timeout)
    sock.close()


def _default_http_get(url: str, timeout: float, max_bytes: int) -> tuple[int, int]:
    """URL へ GET し `(status, 読み込んだ本文バイト数)` を返す。リダイレクトは辿らない。"""
    parsed = urlparse(url)
    conn = http.client.HTTPConnection(parsed.hostname or "", parsed.port, timeout=timeout)
    try:
        path = parsed.path or "/"
        if parsed.query:
            path += "?" + parsed.query
        conn.request("GET", path, headers={"Connection": "close"})
        resp = conn.getresponse()
        body = resp.read(max_bytes + 1)
        return resp.status, min(len(body), max_bytes + 1)
    finally:
        conn.close()


def _check(name: str, ok: bool, detail: str) -> dict[str, Any]:
    return {"name": name, "ok": ok, "detail": detail}


def _err_detail(exc: BaseException) -> str:
    """例外を種別と errno だけの文字列にする（本文・宛先の詳細は載せない）。"""
    errno_part = f" errno={exc.errno}" if isinstance(exc, OSError) and exc.errno is not None else ""
    return f"{type(exc).__name__}{errno_part}"


def check_interfaces(proc_net_dev_path: Path) -> dict[str, Any]:
    """`/proc/net/dev` に `lo` 以外のインターフェースが無いことを確かめる。"""
    try:
        lines = proc_net_dev_path.read_text(encoding="utf-8").splitlines()
    except OSError as exc:
        return _check("interfaces", False, f"cannot determine interfaces: {_err_detail(exc)}")
    names = []
    for line in lines[2:]:  # 先頭 2 行はヘッダ
        if ":" in line:
            names.append(line.split(":", 1)[0].strip())
    extra = [n for n in names if n != "lo"]
    if not names:
        return _check("interfaces", False, "no interfaces listed (cannot determine)")
    if extra:
        return _check("interfaces", False, f"non-loopback interfaces present: {','.join(extra)}")
    return _check("interfaces", True, "only lo")


def check_external_dns(resolver: Resolver) -> dict[str, Any]:
    """公開 DNS 名の名前解決が失敗することを確かめる。"""
    try:
        resolver(EXTERNAL_DNS_NAME, 443)
    except OSError as exc:
        return _check("external_dns", True, f"resolution failed as expected: {_err_detail(exc)}")
    return _check("external_dns", False, "external DNS name resolved")


def check_external_connect(
    name: str, targets: tuple[tuple[str, int], ...], connector: Connector, timeout: float
) -> dict[str, Any]:
    """外部アドレスへの TCP 接続がすべて失敗することを確かめる。"""
    for host, port in targets:
        try:
            connector(host, port, timeout)
        except OSError:
            continue
        return _check(name, False, f"connection to {host}:{port} succeeded")
    return _check(name, True, f"{len(targets)} target(s) unreachable as expected")


def check_pages_host(resolver: Resolver, host: str, port: int) -> dict[str, Any]:
    """ページ用ホスト名の解決結果がすべて loopback であることを確かめる。"""
    try:
        addrs = resolver(host, port)
    except OSError as exc:
        return _check("pages_host", False, f"cannot resolve pages host: {_err_detail(exc)}")
    if not addrs:
        return _check("pages_host", False, "pages host resolved to no addresses")
    for addr in addrs:
        try:
            if not ipaddress.ip_address(addr).is_loopback:
                return _check("pages_host", False, f"pages host resolves to non-loopback: {addr}")
        except ValueError:
            return _check("pages_host", False, "pages host resolved to an unparsable address")
    return _check("pages_host", True, f"resolves to loopback only ({len(addrs)} address(es))")


def check_loopback_pages(urls: list[str], http_get: HttpGet, timeout: float) -> dict[str, Any]:
    """各ページ URL が 200 で 1 バイト以上の本文を返すことを確かめる。

    404 は「そのページが保存されていない」だけで隔離の不備ではないため、`missing_urls` に
    記録して skipped 扱いにする（全ページが 404 のときは配信が成り立たないので失敗）。
    それ以外の失敗（他のステータス・空本文・接続エラー）は fail-closed。
    """
    missing: list[str] = []
    for url in urls:
        try:
            status, body_len = http_get(url, timeout, HTTP_MAX_BODY_BYTES)
        except (OSError, http.client.HTTPException) as exc:
            return _check("loopback_pages", False, f"GET failed: {_err_detail(exc)}")
        if status == 404:
            missing.append(url)
            continue
        if status != 200:
            return _check("loopback_pages", False, f"unexpected status {status}")
        if body_len < 1:
            return _check("loopback_pages", False, "empty body")
    if len(missing) == len(urls):
        return _check("loopback_pages", False, "no page was served (all returned 404)")
    result = _check(
        "loopback_pages",
        True,
        f"{len(urls) - len(missing)} page(s) returned 200 with a body, {len(missing)} missing (skipped)",
    )
    result["missing_urls"] = missing
    return result


def run_isolation_check(
    page_urls: list[str],
    *,
    pages_host: str = "pages.test",
    pages_port: int = 80,
    resolver: Resolver = _default_resolver,
    connector: Connector = _default_connector,
    http_get: HttpGet = _default_http_get,
    proc_net_dev_path: Path = PROC_NET_DEV,
    connect_timeout: float = CONNECT_TIMEOUT_SEC,
    http_timeout: float = HTTP_TIMEOUT_SEC,
) -> dict[str, Any]:
    """隔離確認を実行し、レポート dict（`schema_version: 1`）を返す。

    すべての項目が ok のときだけ `isolated: true`。ネットワーク操作は引数で差し替え可能。
    """
    checks = [
        check_interfaces(proc_net_dev_path),
        check_external_dns(resolver),
        check_external_connect("external_ipv4", EXTERNAL_IPV4_TARGETS, connector, connect_timeout),
        check_external_connect("external_ipv6", EXTERNAL_IPV6_TARGETS, connector, connect_timeout),
        check_pages_host(resolver, pages_host, pages_port),
        check_loopback_pages(page_urls, http_get, http_timeout),
    ]
    try:
        uid: int | None = os.getuid()
    except AttributeError:  # Windows
        uid = None
    return {
        "schema_version": REPORT_SCHEMA_VERSION,
        "checked_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "isolated": all(c["ok"] for c in checks),
        # 保存されておらず撮影対象から外すページ（`loopback_pages` の 404 分）。
        "missing_urls": list(checks[-1].get("missing_urls", [])),
        "checks": checks,
        "environment": {
            "hostname": socket.gethostname(),
            "uid": uid,
            "python": sys.version.split()[0],
            # 値は出さず有無だけを記録する（秘密情報の混入防止）。
            "proxy_env_set": sorted(k for k in PROXY_ENV_KEYS if os.environ.get(k)),
        },
    }


def main(argv: list[str] | None = None) -> int:
    """CLI。0=隔離確認に合格、1=隔離されていない、2=引数・入力が不正。"""
    # 循環 import を避けるため遅延 import する（capture_screenshots が本モジュールを import する）。
    import capture_screenshots as cs

    parser = argparse.ArgumentParser(description="Verify network isolation before saved-pages capture.")
    parser.add_argument("--sites", type=Path, default=cs.DEFAULT_SAVED_PAGES_SITES)
    parser.add_argument("--report", type=Path, required=True, help="Path of the JSON report to write.")
    try:
        args = parser.parse_args(argv)
    except SystemExit as exc:
        return 2 if exc.code not in (0, None) else 0
    try:
        _viewport, sites = cs.load_sites(args.sites, 1, url_policy=cs.SAVED_PAGES_MODE_NAME)
    except cs.CaptureError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    port = urlparse(sites[0].url).port or 80
    report = run_isolation_check([s.url for s in sites], pages_port=port)
    text = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
    try:
        cs._write_bytes_nofollow(args.report, text.encode("utf-8"))
    except OSError as exc:
        print(f"error: failed to write report: {exc}", file=sys.stderr)
        return 2
    for c in report["checks"]:
        print(f"[{'ok' if c['ok'] else 'NG'}] {c['name']}: {c['detail']}", file=sys.stderr)
    return 0 if report["isolated"] else 1


if __name__ == "__main__":
    sys.exit(main())
