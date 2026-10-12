#!/usr/bin/env python3
"""テスト専用の偽エンジン。スナップショットのサブリソースを `--proxy` 経由で取得する。

`test_capture_screenshots.py` が、検証プロキシ経由の撮影（RENDER-5 / TASK-37）の
自己テストに使う。実 Servo の代わりであり、実 Servo が全通信をプロキシへ流すことの
証跡にはならない（その確認は #55 の実機作業。REPAIR-3）。

モード:
    proxied: スナップショットの `src` / `href` を `--proxy` 経由で取得し、結果を
             `--report` へ JSON で書く（既定）
    silent:  何も取得せず PNG だけ出力する（プロキシ未使用の fail-closed 検証用）
    direct:  `--direct-connect HOST:PORT` へプロキシを介さず直接接続して保持する
             （送信先ソケット監視の検証用）
"""

from __future__ import annotations

import argparse
import json
import os
import re
import socket
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from fake_engine import write_minimal_png  # noqa: E402

REF_RE = re.compile(r"""(?:src|href)\s*=\s*["']([^"']+)["']""", re.IGNORECASE)
MAX_REFS = 50
ENV_KEYS = ["http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY", "no_proxy", "NO_PROXY", "all_proxy", "ALL_PROXY"]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--mode", choices=["proxied", "silent", "direct"], default="proxied")
    parser.add_argument("--proxy", default=None)
    parser.add_argument("--html", type=Path, default=None)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--width", type=int, default=1280)
    parser.add_argument("--height", type=int, default=800)
    parser.add_argument("--report", type=Path, default=None)
    parser.add_argument("--dump-env", type=Path, default=None)
    parser.add_argument("--direct-connect", default=None)
    parser.add_argument("--hold-ms", type=int, default=600)
    args = parser.parse_args(argv)

    if args.dump_env is not None:
        args.dump_env.write_text(json.dumps({k: os.environ.get(k) for k in ENV_KEYS}), encoding="utf-8")

    results: list[dict[str, object]] = []
    if args.mode == "proxied" and args.proxy and args.html:
        opener = urllib.request.build_opener(
            urllib.request.ProxyHandler({"http": args.proxy, "https": args.proxy})
        )
        text = args.html.read_text(encoding="utf-8", errors="replace")
        for ref in REF_RE.findall(text)[:MAX_REFS]:
            if not ref.startswith(("http://", "https://")):
                continue
            try:
                with opener.open(ref, timeout=5) as resp:
                    results.append({"url": ref, "status": resp.status})
            except urllib.error.HTTPError as exc:
                results.append({"url": ref, "status": exc.code})
            except (urllib.error.URLError, OSError) as exc:
                results.append({"url": ref, "status": None, "error": str(exc)})
    elif args.mode == "direct" and args.direct_connect:
        host, _, port = args.direct_connect.rpartition(":")
        with socket.create_connection((host, int(port)), timeout=5):
            time.sleep(args.hold_ms / 1000)

    if args.report is not None:
        args.report.write_text(json.dumps(results), encoding="utf-8")
    args.out.parent.mkdir(parents=True, exist_ok=True)
    write_minimal_png(args.out, args.width, args.height)
    return 0


if __name__ == "__main__":
    sys.exit(main())
