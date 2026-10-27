#!/usr/bin/env python3
"""テスト専用の偽 Servo ホスト（`--stdio` のフレームプロトコルを話す）。

`test_servo_bbox_driver.py` が `servo_bbox_driver.py` の `--host-cmd` として差し込み、
実ホスト・ネットワークなしで要求列と異常系を検証する（Issue #1282・TASK-37・RENDER-5）。
hello はゴールデン fixture（`crates/fandhe-browser-render/tests/fixtures/protocol/hello-ok.json`）を
そのまま送る。受け取った要求は `--record` の JSON Lines に書く。実ホストの動作の代替や
証跡にはならない（実装済みを装わない。REPAIR-3）。
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import struct
import sys
import time
import zlib
from pathlib import Path

FIXTURE = (
    Path(__file__).resolve().parents[3]
    / "crates/fandhe-browser-render/tests/fixtures/protocol/hello-ok.json"
)


def png_bytes(width: int, height: int) -> bytes:
    def chunk(kind: bytes, data: bytes) -> bytes:
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)

    raw = b"".join(b"\x00" + bytes([200, 200, 200]) * width for _ in range(height))
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw))
        + chunk(b"IEND", b"")
    )


def rect_for(selector: str) -> dict[str, float]:
    """セレクタごとに決定的な矩形（テストが同じ式で期待値を作る）。"""
    return {"x": float(len(selector)), "y": 2.5, "width": 100.0 + len(selector), "height": 20.0}


def send(obj: dict) -> None:
    body = json.dumps(obj).encode("utf-8")
    sys.stdout.buffer.write(struct.pack("<I", len(body)) + body)
    sys.stdout.buffer.flush()


def read_frame() -> dict | None:
    header = sys.stdin.buffer.read(4)
    if len(header) < 4:
        return None
    (length,) = struct.unpack("<I", header)
    return json.loads(sys.stdin.buffer.read(length).decode("utf-8"))


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--scenario", default="ok")
    parser.add_argument("--record", type=Path, required=True)
    parser.add_argument("--stdio", action="store_true")
    args = parser.parse_args()
    scenario = args.scenario

    def record(entry: dict) -> None:
        with args.record.open("a", encoding="utf-8") as fh:
            fh.write(json.dumps(entry) + "\n")

    record({"kind": "env", "env": dict(os.environ)})

    hello = json.loads(FIXTURE.read_text(encoding="utf-8"))["message"]
    if scenario == "no_hello":
        time.sleep(60)
        return 0
    if scenario == "missing_capability":
        hello["data"]["capabilities"].remove("bounding_box")
    if scenario == "bad_protocol_version":
        hello["data"]["protocol"] = {"min": 2, "max": 2}
    send(hello)
    if scenario == "stderr_flood":
        # stderr を大量に書いても駆動側が詰まらず、保存量が有界であることの検証用。
        for _ in range(80):
            sys.stderr.write("x" * 65536)
            sys.stderr.flush()

    while True:
        message = read_frame()
        if message is None:
            return 0
        record({"kind": "request", "message": message})
        rid, method, params = message["id"], message["method"], message.get("params", {})
        if method == "shutdown":
            send({"v": 1, "id": rid, "result": {}})
            return 0
        if method == "load":
            if scenario == "close_without_response":
                return 0
            if scenario == "load_failed":
                send({"v": 1, "id": rid, "error": {"kind": "load_failed", "message": "x"}})
            elif scenario == "result_and_error":
                send({"v": 1, "id": rid, "result": {"final_url": params["url"]}, "error": {"kind": "internal", "message": "x"}})
            elif scenario == "wrong_id":
                send({"v": 1, "id": rid + 100, "result": {"final_url": params["url"]}})
            else:
                send({"v": 1, "id": rid, "result": {"final_url": params["url"], "status": 200}})
        elif method == "screenshot":
            if scenario == "oversized_frame":
                sys.stdout.buffer.write(struct.pack("<I", 200 * 1024 * 1024))
                sys.stdout.buffer.flush()
                time.sleep(60)
                return 0
            width = 640 if scenario == "bad_png" else 1280
            data = base64.b64encode(png_bytes(1280, 800)).decode("ascii")
            send({"v": 1, "id": rid, "result": {"format": "png", "width": width, "height": 800, "data_base64": data}})
        elif method == "bounding_box":
            selector = params["selector"]
            if scenario == "query_timeout":
                send({"v": 1, "id": rid, "error": {"kind": "timeout", "message": "x"}})
            elif scenario == "not_found_one" and selector == "#missing":
                send({"v": 1, "id": rid, "error": {"kind": "not_found", "message": "x"}})
            elif scenario == "nonfinite_bbox":
                send({"v": 1, "id": rid, "result": {**rect_for(selector), "width": -1}})
            else:
                send({"v": 1, "id": rid, "result": rect_for(selector), "unknown_extra": True})
        else:
            send({"v": 1, "id": rid, "error": {"kind": "unsupported", "message": "x"}})


if __name__ == "__main__":
    sys.exit(main())
