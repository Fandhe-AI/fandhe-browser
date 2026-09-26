#!/usr/bin/env python3
"""テスト専用の偽レンダリングエンジン（Servo/Chromium の実バイナリの代わり）。

`test_capture_screenshots.py` の結合テストが `capture_screenshots.py` へ
`--servo-cmd` / `--chromium-cmd` としてこのスクリプトを差し込むことで、
ネットワークにも実エンジンにも依存せず「両エンジン x 複数サイトの PNG と
結果 JSON が出る」経路をオフラインで検証する（TASK-37.1 / RENDER-5）。
実機での撮影の代替や証跡にはならない（実装済みを装わない。REPAIR-3）。
"""

from __future__ import annotations

import argparse
import struct
import subprocess
import sys
import time
import zlib
from pathlib import Path


def _chunk(chunk_type: bytes, data: bytes) -> bytes:
    return (
        struct.pack(">I", len(data))
        + chunk_type
        + data
        + struct.pack(">I", zlib.crc32(chunk_type + data) & 0xFFFFFFFF)
    )


def write_minimal_png(path: Path, width: int, height: int) -> None:
    """指定サイズの単色 PNG を標準ライブラリ（struct・zlib）だけで書き出す。"""
    ihdr = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)  # 8bit, RGB
    pixel_row = bytes([200, 200, 200] * width)  # 3 bytes/px, no alpha
    raw = b"".join(bytes([0]) + pixel_row for _ in range(height))  # filter byte 0 per row
    idat = zlib.compress(raw)
    png = (
        b"\x89PNG\r\n\x1a\n"
        + _chunk(b"IHDR", ihdr)
        + _chunk(b"IDAT", idat)
        + _chunk(b"IEND", b"")
    )
    path.write_bytes(png)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument(
        "--mode", choices=["ok", "fail", "sleep", "garbage", "spawn-grandchild"], default="ok"
    )
    parser.add_argument("--width", type=int, default=1280)
    parser.add_argument("--height", type=int, default=800)
    # 実エンジン（Chromium 等）の `{url}` 直接ナビゲーションテンプレートを模した
    # テスト（直接ナビゲーション向け SSRF チェックの検証）から渡されることが
    # あるが、この偽エンジンは実際には何も取得しないため値は使わず無視する。
    parser.add_argument("--url", default=None)
    # `{html_path}` と `{url}` を両方使うテンプレート（codex P0 再指摘: 直接
    # ナビゲーションの許可リスト検査が `{html_path}` の有無で素通りされないかの
    # 検証）から渡されることがある。同じく値は使わず無視する。
    parser.add_argument("--html", default=None)
    # `capture_one` が起動するローカル転送プロキシの URL（`{proxy}`）。この
    # 偽エンジンは実際には何も取得しないため値は使わず無視するが、
    # `fake_engine_template()` はこの引数を渡すことでテンプレートに `{proxy}`
    # を含める（`--allow-unproxied-engine` 無しでも `capture_one` の fail-closed
    # チェックを通過させるため。codex P0 再指摘）。
    parser.add_argument("--proxy", default=None)
    # `--mode spawn-grandchild` 専用: 孫プロセスの PID をここへ書き出す
    # （タイムアウト後にプロセスツリーごと終了しているかをテストから
    # 確認するため。Cursor Medium: `subprocess.run(timeout=)` は直接の子しか
    # kill しない問題の再現・修正確認）。
    parser.add_argument("--pid-file", type=Path, default=None)
    args = parser.parse_args(argv)

    if args.mode == "fail":
        print("fake_engine: simulated failure", file=sys.stderr)
        return 1

    if args.mode == "sleep":
        # capture_screenshots の --timeout-sec より十分長く待たせ、subprocess の
        # timeout による強制終了（status=timeout）を確実に発生させる。
        time.sleep(30)
        return 0

    if args.mode == "spawn-grandchild":
        # 実エンジン（Chromium 等）がレンダラー・GPU プロセス等の子孫を持つ
        # 状況を模す。孫プロセス自体も長時間 sleep し、`--pid-file` へ自分の
        # PID を書き出してからこのプロセス自身も長時間 sleep する
        # （タイムアウトで直接の子だけが kill されると孫が生き残ってしまう）。
        grandchild = subprocess.Popen(  # noqa: S603
            [sys.executable, "-c", "import time; time.sleep(60)"]
        )
        if args.pid_file is not None:
            args.pid_file.write_text(str(grandchild.pid), encoding="utf-8")
        time.sleep(30)
        grandchild.wait()
        return 0

    args.out.parent.mkdir(parents=True, exist_ok=True)

    if args.mode == "garbage":
        args.out.write_bytes(b"not a png")
        return 0

    write_minimal_png(args.out, args.width, args.height)
    return 0


if __name__ == "__main__":
    sys.exit(main())
