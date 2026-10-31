#!/usr/bin/env python3
"""テスト専用の偽 Servo ホスト（egress 隔離ハーネスの自己テスト用）。

`test_in_sandbox.py` が一時ディレクトリへコピーし、隣の `fake_host.config.json`
（`{"connect": [["127.0.0.2", 12345], ...]}`）を読む。`load` を受けるたびに（`/ok` を除き）
設定された宛先へ TCP で接続して要求行を 1 つ送る。`--selftest` は終了コード 0。
実ホストの動作の代替や証跡にはならない（実装済みを装わない。`REPAIR-3`）。
Issue #1377・TASK-38・`RENDER-3`。
"""

from __future__ import annotations

import json
import socket
import struct
import sys
from pathlib import Path

HELLO = {
    "v": 1,
    "event": "hello",
    "data": {
        "host_version": "0.0.0",
        "servo_version": "0.0.0",
        "protocol": {"min": 1, "max": 1},
        "capabilities": ["load", "screenshot", "visibility", "bounding_box", "shutdown"],
        "limits": {"request_frame_bytes": 65536, "response_frame_bytes": 1048576, "screenshot_frame_bytes": 25165824},
    },
}


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
    if "--selftest" in sys.argv:
        return 0
    config_path = Path(__file__).with_name("fake_host.config.json")
    connect = json.loads(config_path.read_text(encoding="utf-8")).get("connect", [])
    send(HELLO)
    while True:
        message = read_frame()
        if message is None:
            return 0
        rid, method, params = message["id"], message["method"], message.get("params", {})
        if method == "shutdown":
            send({"v": 1, "id": rid, "result": {}})
            return 0
        if method == "load":
            if not params["url"].split("?", 1)[0].endswith("/ok"):
                for ip, port in connect:
                    try:
                        with socket.create_connection((ip, port), timeout=1) as conn:
                            conn.sendall(b"GET /x?secret=1 HTTP/1.1\r\nHost: x\r\n\r\n")
                    except OSError:
                        pass
            send({"v": 1, "id": rid, "result": {"final_url": params["url"], "status": 200}})
        else:
            send({"v": 1, "id": rid, "error": {"kind": "unsupported", "message": "x"}})


if __name__ == "__main__":
    sys.exit(main())
