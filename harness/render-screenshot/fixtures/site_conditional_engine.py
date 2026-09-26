#!/usr/bin/env python3
"""テスト専用の偽エンジン: サイトごとに成功/失敗を切り替えられる版。

`fake_engine.py` は実行全体で 1 つの `--mode` しか持てず、両エンジンが
異なるサイト集合で成功するケース（Servo が 1〜5、Chromium が 2〜6 で成功等）を
再現できない。本スクリプトは `--fail-sites`（カンマ区切りの site_id）に含まれる
サイトだけ失敗させ、それ以外は最小の PNG を書き出す。`capture_screenshots.py`
の「両エンジンで共通して "ok" だったサイト数」の判定（codex P1）を検証する
`test_capture_screenshots.py` からのみ使う。実機での撮影可否には関係しない
（REPAIR-3）。
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from fake_engine import write_minimal_png  # noqa: E402


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--width", type=int, default=1280)
    parser.add_argument("--height", type=int, default=800)
    parser.add_argument("--fail-sites", default="")
    args = parser.parse_args(argv)

    site_id = args.out.stem
    fail_sites = {s for s in args.fail_sites.split(",") if s}
    if site_id in fail_sites:
        print(f"site_conditional_engine: simulated failure for {site_id}", file=sys.stderr)
        return 1

    args.out.parent.mkdir(parents=True, exist_ok=True)
    write_minimal_png(args.out, args.width, args.height)
    return 0


if __name__ == "__main__":
    sys.exit(main())
