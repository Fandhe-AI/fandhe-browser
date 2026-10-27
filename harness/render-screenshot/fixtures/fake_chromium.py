#!/usr/bin/env python3
"""テスト専用の偽 Chromium（`--remote-debugging-pipe` の CDP を模す）。

`test_chromium_bbox_driver.py` が `chromium_bbox_driver.py` の `--chromium-cmd` として差し込み、
実 Chromium・ネットワークなしで要求列と異常系を検証する（Issue #1288・TASK-37・RENDER-5）。
fd 3 から NUL 終端の JSON を読み、fd 4 へ NUL 終端の JSON を書く（Chromium と同じ向き）。
受け取った argv と要求は `--record` の JSON Lines に書く。実 Chromium の動作の代替や証跡には
ならない（実装済みを装わない。REPAIR-3）。
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from fake_render_host import png_bytes, rect_for  # noqa: E402

SESSION = "SESSION-1"
TARGET = "TARGET-1"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--scenario", default="ok")
    parser.add_argument("--record", default=None)
    args, _unknown = parser.parse_known_args()
    scenario = args.scenario
    reader = os.fdopen(3, "rb", buffering=0)
    writer = os.fdopen(4, "wb", buffering=0)
    record = open(args.record, "a", encoding="utf-8") if args.record else None

    def log(entry: dict) -> None:
        if record:
            record.write(json.dumps(entry) + "\n")
            record.flush()

    def send(obj: dict) -> None:
        writer.write(json.dumps(obj).encode("utf-8") + b"\0")

    def read_message() -> dict | None:
        buf = bytearray()
        while True:
            ch = reader.read(1)
            if not ch:
                return None
            if ch == b"\0":
                return json.loads(buf.decode("utf-8"))
            buf += ch

    log({"kind": "argv", "argv": sys.argv[1:]})
    width, height = 1280, 800
    for arg in sys.argv[1:]:
        if arg.startswith("--window-size="):
            width, height = (int(v) for v in arg.split("=", 1)[1].split(","))
    if scenario == "stderr_flood":
        sys.stderr.write("x" * (1024 * 1024))
        sys.stderr.flush()
    if scenario == "spawn_grandchild_and_hang":
        child = subprocess.Popen(["sleep", "300"])  # 同じプロセスグループ
        if args.record:
            Path(args.record + ".grandchild").write_text(str(child.pid), encoding="utf-8")

    # 外枠 = --window-size。ビューポートは外枠より CHROME_H だけ低い（headless=new を模す）。
    chrome_h = 0 if scenario == "viewport_equals_window" else 85
    win = {"width": width, "height": height}
    navigated_url = "about:blank"
    first = True
    while True:
        msg = read_message()
        if msg is None:
            return 0
        log({"kind": "request", "message": msg})
        method, rid = msg["method"], msg["id"]
        params = msg.get("params") or {}
        if scenario == "spawn_grandchild_and_hang":
            time.sleep(300)
        if first and scenario == "oversized_message":
            writer.write(b"a" * (2 * 1024 * 1024))
            time.sleep(300)
        if first and scenario == "wrong_id":
            send({"id": rid + 100, "result": {}})
            first = False
            continue
        first = False
        reply: dict = {"id": rid}
        if "sessionId" in msg:
            reply["sessionId"] = msg["sessionId"]
        events: list[dict] = []
        if method == "Browser.getVersion":
            reply["result"] = {"product": "HeadlessChrome/130.0.6723.58", "userAgent": "ignored"}
        elif method == "Target.getTargets":
            reply["result"] = {"targetInfos": [{"targetId": TARGET, "type": "page", "url": "about:blank"}]}
        elif method == "Target.attachToTarget":
            reply["result"] = {"sessionId": SESSION}
        elif method == "Page.getLayoutMetrics":
            reply["result"] = {
                "cssLayoutViewport": {"clientWidth": win["width"], "clientHeight": win["height"] - chrome_h}
            }
        elif method == "Browser.getWindowForTarget":
            reply["result"] = {"windowId": 7, "bounds": dict(win)}
        elif method == "Browser.setWindowBounds":
            if scenario != "set_bounds_ignored":
                win["width"] = params["bounds"]["width"]
                win["height"] = params["bounds"]["height"]
            reply["result"] = {}
        elif method == "Page.setLifecycleEventsEnabled":
            reply["result"] = {}
        elif method == "Page.enable":
            reply["result"] = {}
            if scenario == "stale_load":
                # 初期 about:blank の古い load が受信キューに残る状況を模す。
                events.append({"method": "Page.lifecycleEvent", "params": {"name": "load", "loaderId": "OLD"}, "sessionId": SESSION})
            if scenario == "event_flood":
                for _ in range(2000):
                    events.append({"method": "Page.frameStartedLoading", "params": {}, "sessionId": SESSION})
        elif method == "Page.navigate":
            navigated_url = params["url"]
            reply["result"] = {"frameId": "F", "loaderId": "L1"}
            if scenario == "navigate_error":
                reply["result"]["errorText"] = "net::ERR_FAILED"
            elif scenario == "target_crashed":
                events.append({"method": "Inspector.targetCrashed", "params": {}, "sessionId": SESSION})
            elif scenario not in ("no_load_event", "stale_load"):
                events.append({"method": "Page.lifecycleEvent", "params": {"name": "load", "loaderId": "L1"}, "sessionId": SESSION})
        elif method == "Runtime.evaluate":
            expr = params["expression"]
            if "document.fonts.ready" in expr:
                if scenario == "fonts_hang":
                    time.sleep(300)
                reply["result"] = {"result": {"type": "boolean", "value": True}}
            elif "document.readyState" in expr:
                reply["result"] = {"result": {"type": "boolean", "value": False}}
            elif "getBoundingClientRect" in expr:
                selector = json.loads(expr.rsplit("})(", 1)[1][:-1])
                if scenario == "eval_exception":
                    reply["result"] = {"result": {"type": "object"}, "exceptionDetails": {"text": "boom"}}
                elif selector == "missing":
                    reply["result"] = {"result": {"type": "object", "value": {"found": False}}}
                elif selector == "bad[":
                    reply["result"] = {"result": {"type": "object", "value": {"error": "invalid_selector"}}}
                elif scenario == "nonfinite_rect":
                    value = {"found": True, "x": None, "y": 0, "width": 1, "height": 1}
                    reply["result"] = {"result": {"type": "object", "value": value}}
                else:
                    value = {"found": True, **rect_for(selector)}
                    reply["result"] = {"result": {"type": "object", "value": value}}
            else:
                reply["result"] = {"result": {"type": "undefined"}}
        elif method == "Page.captureScreenshot":
            if scenario == "bad_png_size":
                data = base64.b64encode(png_bytes(100, 100)).decode()
            elif scenario == "bad_base64":
                data = "!!!not-base64!!!"
            else:
                data = base64.b64encode(png_bytes(win["width"], win["height"] - chrome_h)).decode()
            reply["result"] = {"data": data}
        elif method == "Target.getTargetInfo":
            reply["result"] = {"targetInfo": {"targetId": TARGET, "url": navigated_url}}
        elif method == "Browser.close":
            if scenario == "close_without_response":
                return 0
            reply["result"] = {}
            send(reply)
            return 0
        else:
            reply["error"] = {"code": -32601, "message": "not found"}
        send(reply)
        for event in events:
            send(event)


if __name__ == "__main__":
    sys.exit(main())
