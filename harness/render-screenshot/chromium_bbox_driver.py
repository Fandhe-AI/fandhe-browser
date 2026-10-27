#!/usr/bin/env python3
"""Chromium を `--remote-debugging-pipe` の CDP で駆動し、PNG と境界ボックス JSON を書き出す駆動スクリプト。

役割（Issue #1288・TASK-37・`RENDER-5`・`MEAS-3`）:
    #55 の境界ボックス一致率は Chromium 側の要素矩形（reference）と Servo 側（target）を比べる。
    Servo 側は `servo_bbox_driver.py`（#1282）が書くが、Chromium 側の取得手段がリポに無かったため
    全サイトが bbox `not_measured` になっていた。本スクリプトが Chromium 側を同じ契約
    （`<stem>.bboxes.json`・`png_sha256`・`coordinate_space: viewport_css_px`）で書く。

呼び出し文脈:
    - `capture_screenshots.py --saved-pages` の `--chromium-cmd` として差し込む（`{chromium_bin}`・
      `{width}`・`{height}`・`{url}`・`{out}` を受ける）。`container/run-in-container.sh` が
      `BBOX_SELECTORS` 指定時に組み立てる。
    - `measure_ssim.py` は PNG と bbox の対応を `png_sha256` で照合する（`_bbox_png_binding_mismatch`）。
      そのため `--screenshot` の別撮影ではなく、同じセッションで CDP `Page.captureScreenshot` の PNG を撮る
      （オーナー判断 2026-10-27・#1288）。
    - 単体でも実行できる（`--assume-isolated` が必須）。使い方・信頼境界は README「`chromium_bbox_driver.py`」。

撮影時点（オーナー判断 2026-10-27）:
    load（`Page.navigate` の `loaderId` に紐付く `Page.lifecycleEvent`）-> `document.fonts.ready` -> `requestAnimationFrame` 1 回の後に撮る。

座標の意味論:
    Servo ホストと同じ JS 関数式（`servo-host/src/layout.rs` の `selector_prelude!()` +
    `BOUNDING_BOX_FN`）を `Runtime.evaluate` で評価する。`querySelector` の最初の一致・
    `getBoundingClientRect`・ビューポート相対の CSS px で、`DOM.getBoxModel`（ページ座標の quad）は使わない。
    両者の一致は `test_chromium_bbox_driver.py` のパリティテストが検査する。

信頼境界:
    - URL は `capture_screenshots.check_saved_pages_url`（`http://pages.test:<port>` のみ）を共有する。
    - Chromium はシェルを介さず argv のリストで起動し、TCP のデバッグポートは開かない（pipe のみ）。
      フラグは `SAVED_PAGES_CHROMIUM_TEMPLATE` 由来で、UA・ヘッダ・デバイスメトリクス・locale は
      フラグでも CDP でも上書きしない（`SEC-2`）。環境変数は親をそのまま継承する（SSIM の reference の
      撮影条件を `capture_one` の既存経路と揃えるため）。
    - CDP の応答は untrusted として検証する（id・sessionId・result/error の排他・型・範囲・長さ）。
      未知フィールドは無視する（`REPAIR-4`）。出力にページ由来の文字列は載せない。

未実装（実装済みを装わない。`REPAIR-3`）:
    - 実 Chromium・実 Servo ホストでの動作は未検証。偽 Chromium（`fixtures/fake_chromium.py`）による
      オフライン検証のみ（#55 で人間が確認する）。
    - POSIX のみ（pipe の fd 3・4 を使う）。測定はコンテナ（Linux）で行うため Windows は対象外。

終了コード: 0（PNG を書いた）／1（Chromium・CDP・ページ読み込みの失敗）／2（引数・入力・URL 規則の違反）。
"""

from __future__ import annotations

import argparse
import base64
import binascii
import json
import os
import queue
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any

_THIS_DIR = Path(__file__).resolve().parent
if str(_THIS_DIR) not in sys.path:
    sys.path.insert(0, str(_THIS_DIR))
import capture_screenshots as cs  # noqa: E402  (同ディレクトリの兄弟モジュール)
import servo_bbox_driver as svd  # noqa: E402  (セレクタ入力・出力書き込み・検証を共有する)

try:  # POSIX のみ。非 POSIX は main で終了コード 2 にする。
    import fcntl
except ImportError:  # pragma: no cover
    fcntl = None  # type: ignore[assignment]

# --- 定数 -------------------------------------------------------------------

TOOL_NAME = "chromium_bbox_driver"
RESPONSE_MESSAGE_MAX = 1024 * 1024
SCREENSHOT_MESSAGE_MAX = 24 * 1024 * 1024
REQUEST_MESSAGE_MAX = 64 * 1024
# 受信キューの上限。CDP は要求ごとの応答の間にイベントが挟まるため、Servo より余裕を持たせる。
MAX_PENDING_MESSAGES = 256
MAX_PENDING_BYTES = 2 * SCREENSHOT_MESSAGE_MAX
STARTUP_TIMEOUT_S = 10.0
FONTS_TIMEOUT_S = 10.0
SCREENSHOT_TIMEOUT_S = 20.0
QUERY_TIMEOUT_S = 5.0
URL_TIMEOUT_S = 10.0
SHUTDOWN_GRACE_S = 3.0
TIMEOUT_SLACK_S = 5.0
SEND_TIMEOUT_S = 10.0
DEFAULT_LOAD_TIMEOUT_MS = 30_000
LOAD_TIMEOUT_MAX_MS = svd.LOAD_TIMEOUT_MAX_MS
MAX_DIMENSION = svd.MAX_DIMENSION
DEFAULT_WIDTH = 1280
DEFAULT_HEIGHT = 800
VERSION_RE_PATTERN = r"[A-Za-z]+/[0-9]+(\.[0-9]+){0,3}"
PNG_MAGIC = b"\x89PNG\r\n\x1a\n"
# CDP の `Page.captureScreenshot` に任せず、固定の引数だけを送る（UA 等は一切触らない）。
CHROMIUM_FIXED_FLAGS = ("--remote-debugging-pipe",)

# `servo-host/src/layout.rs` の `selector_prelude!()` と `BOUNDING_BOX_FN` を連結した JS 関数式。
# Servo 側と座標の意味論を揃えるため文字列を完全一致させる（パリティテストが layout.rs と比較する）。
BOUNDING_BOX_FN = (
    "(function(sel){\n"
    "var el;\n"
    'try { el = document.querySelector(sel); } catch (e) { return {error: "invalid_selector"}; }\n'
    "if (el === null) { return {found: false}; }\n"
    "var r = el.getBoundingClientRect();\n"
    "return {found: true, x: r.left, y: r.top, width: r.width, height: r.height};\n"
    "})"
)
# フォント読み込みと次の 1 フレームを待つ式（オーナー判断 1）。
FONTS_AND_FRAME_EXPR = (
    "document.fonts.ready.then(function(){"
    "return new Promise(function(resolve){requestAnimationFrame(function(){resolve(true);});});})"
)
READY_STATE_EXPR = 'document.readyState === "complete" && location.href !== "about:blank"'


class CdpTimeout(svd.HostError):
    """応答・イベントの待ちがタイムアウトした（`HostError` の一種。load 待ちの保険判定で区別する）。"""


class CdpRpcError(Exception):
    """Chromium が返した CDP `error` 応答。コードだけを保持し、メッセージ文字列は持たない。"""

    def __init__(self, code: int | None) -> None:
        super().__init__(f"cdp_error({code})")
        self.code = code


# --- 起動 ---------------------------------------------------------------------


def build_chromium_argv(prefix: list[str], user_data_dir: str, width: int, height: int) -> list[str]:
    """`SAVED_PAGES_CHROMIUM_TEMPLATE` から起動引数を作る（フラグを手書きで複製しない）。

    `--screenshot={out}` と `{url}` を除き、`--remote-debugging-pipe` と初期 URL `about:blank` を足す。
    先頭要素（`{chromium_bin}`）は `prefix`（`--chromium-bin` か `--chromium-cmd`）に置き換える。
    """
    template = [a for a in cs.SAVED_PAGES_CHROMIUM_TEMPLATE if a not in ("--screenshot={out}", "{url}")]
    flags = cs.expand_template(
        template[1:],
        {"user_data_dir": user_data_dir, "width": str(width), "height": str(height)},
    )
    return [*prefix, *flags, *CHROMIUM_FIXED_FLAGS, "about:blank"]


def _make_child_setup(read_fd: int, write_fd: int):
    """子プロセスで pipe を fd 3（Chromium が読む）・fd 4（Chromium が書く）へ移す `preexec_fn`。

    先に fd 10 以上へ複製してから `dup2` する（親の fd がたまたま 3・4 でも衝突しない順序）。
    """

    def setup() -> None:
        high_r = fcntl.fcntl(read_fd, fcntl.F_DUPFD, 10)
        high_w = fcntl.fcntl(write_fd, fcntl.F_DUPFD, 10)
        os.dup2(high_r, 3)
        os.dup2(high_w, 4)
        # F_DUPFD の複製は close-on-exec でない。残すと Chromium が自分の読む pipe の書き込み端を
        # 保持して EOF にならないため閉じる（元の fd は os.pipe 由来の非継承で exec 時に閉じる）。
        for fd in (high_r, high_w):
            if fd not in (3, 4):
                os.close(fd)

    return setup


# --- CDP 通信 -----------------------------------------------------------------


class CdpChannel:
    """Chromium の pipe（NUL 区切り JSON）を扱う。読み取りは専用スレッドとキューで行う。

    NUL が来る前のバッファ長にも上限をかける（無制限確保の防止）。
    """

    def __init__(self, to_chromium: Any, from_chromium: Any) -> None:
        self._out = to_chromium
        self._in = from_chromium
        self._queue: queue.Queue[tuple[str, Any]] = queue.Queue()
        self._lock = threading.Lock()
        self._pending = 0
        self._pending_bytes = 0
        self.max_message = RESPONSE_MESSAGE_MAX
        self.next_id = 1
        self.session: str | None = None
        self.loaded_loaders: set[str] = set()
        self._thread = threading.Thread(target=self._read_loop, daemon=True)
        self._thread.start()

    def _put_message(self, body: bytes) -> bool:
        if len(body) > self.max_message:
            self._queue.put(("error", f"message length {len(body)} exceeds the {self.max_message} byte limit"))
            return False
        with self._lock:
            if self._pending + 1 > MAX_PENDING_MESSAGES or self._pending_bytes + len(body) > MAX_PENDING_BYTES:
                self._queue.put(("error", "chromium sent more unsolicited messages than allowed"))
                return False
            self._pending += 1
            self._pending_bytes += len(body)
        self._queue.put(("msg", body))
        return True

    def _read_loop(self) -> None:
        buf = bytearray()
        try:
            while True:
                chunk = self._in.read(65536)
                if not chunk:
                    self._queue.put(("eof", None))
                    return
                buf += chunk
                while True:
                    end = buf.find(b"\0")
                    if end < 0:
                        break
                    body = bytes(buf[:end])
                    del buf[: end + 1]
                    if not self._put_message(body):
                        return
                if len(buf) > self.max_message:
                    self._queue.put(("error", f"message exceeds the {self.max_message} byte limit before its terminator"))
                    return
        except (OSError, ValueError) as exc:
            self._queue.put(("error", f"failed to read from chromium: {exc}"))

    def recv(self, timeout: float) -> Any:
        try:
            kind, value = self._queue.get(timeout=max(timeout, 0.0))
        except queue.Empty as exc:
            raise CdpTimeout(f"timed out waiting for chromium ({timeout:.0f}s)") from exc
        if kind == "eof":
            raise svd.HostError("chromium closed the pipe before responding")
        if kind == "error":
            raise svd.HostError(str(value))
        with self._lock:
            self._pending -= 1
            self._pending_bytes -= len(value)
        try:
            return json.loads(value.decode("utf-8"))
        except (UnicodeDecodeError, ValueError, RecursionError) as exc:
            raise svd.HostError(f"chromium sent an invalid JSON message: {exc}") from exc

    def send(self, message: dict[str, Any], timeout: float) -> None:
        """要求を送る。書き込みが詰まっても `timeout` で打ち切る（呼び出し側が kill する）。"""
        body = json.dumps(message, separators=(",", ":"), ensure_ascii=True).encode("ascii") + b"\0"
        if len(body) > REQUEST_MESSAGE_MAX:
            raise svd.HostError(f"request exceeds the {REQUEST_MESSAGE_MAX} byte limit")
        failure: list[BaseException] = []

        def _write() -> None:
            try:
                self._out.write(body)
                self._out.flush()
            except (OSError, ValueError) as exc:
                failure.append(exc)

        writer = threading.Thread(target=_write, daemon=True)
        writer.start()
        writer.join(min(SEND_TIMEOUT_S, timeout))
        if writer.is_alive():
            raise svd.HostError("timed out writing to chromium")
        if failure:
            raise svd.HostError(f"failed to write to chromium: {failure[0]}")

    def close(self) -> None:
        """送信側を閉じ、読み取りスレッドが EOF で終わるのを待ってから受信側を閉じる（Chromium 終了後に呼ぶ）。"""
        for stream in (self._out,):
            try:
                stream.close()
            except (OSError, ValueError):
                pass
        self._thread.join(timeout=1.0)
        if not self._thread.is_alive():
            try:
                self._in.close()
            except (OSError, ValueError):
                pass

    def _handle_event(self, message: dict[str, Any]) -> None:
        method = message.get("method")
        if not isinstance(method, str):
            raise svd.HostError("chromium sent a message that is neither a response nor an event")
        mine = self.session is not None and message.get("sessionId") == self.session
        params = message.get("params")
        if method == "Page.lifecycleEvent" and mine and isinstance(params, dict):
            # load 完了は loaderId で対象ナビゲーションに紐付ける（about:blank の古いイベントと区別する）。
            loader = params.get("loaderId")
            if params.get("name") == "load" and isinstance(loader, str) and len(self.loaded_loaders) < 1024:
                self.loaded_loaders.add(loader)
        elif method == "Inspector.targetCrashed" and mine:
            raise svd.HostError("the page crashed")
        elif method == "Target.detachedFromTarget" and isinstance(params, dict) and (
            self.session is not None and params.get("sessionId") == self.session
        ):
            raise svd.HostError("the page target was detached")
        # それ以外のイベントは捨てる。

    def call(
        self,
        method: str,
        params: dict[str, Any] | None = None,
        *,
        timeout: float,
        session: bool = False,
        max_message: int = RESPONSE_MESSAGE_MAX,
    ) -> dict[str, Any]:
        """要求を 1 件送り、対応する応答の `result` を返す。`error` 応答は `CdpRpcError`。"""
        deadline = time.monotonic() + timeout
        request_id = self.next_id
        self.next_id += 1
        message: dict[str, Any] = {"id": request_id, "method": method}
        if params is not None:
            message["params"] = params
        if session:
            if self.session is None:
                raise svd.HostError(f"{method}: no page session")
            message["sessionId"] = self.session
        expected_session = self.session if session else None
        self.max_message = max_message
        self.send(message, timeout)
        while True:
            # イベントが続く間も絶対期限を毎周確認する（期限後に届いた応答を成功として受理しない）。
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise CdpTimeout(f"{method}: timed out waiting for the response")
            reply = self.recv(remaining)
            if not isinstance(reply, dict):
                raise svd.HostError(f"{method}: message is not an object")
            if "id" not in reply:
                self._handle_event(reply)
                continue
            if isinstance(reply["id"], bool) or reply["id"] != request_id:
                raise svd.HostError(f"{method}: response id does not match request id {request_id}")
            if reply.get("sessionId") != expected_session:
                raise svd.HostError(f"{method}: response sessionId does not match the request")
            has_result, has_error = "result" in reply, "error" in reply
            if has_result == has_error:
                raise svd.HostError(f"{method}: response must have exactly one of result and error")
            if has_error:
                error = reply["error"]
                code = error.get("code") if isinstance(error, dict) else None
                raise CdpRpcError(code if isinstance(code, int) and not isinstance(code, bool) else None)
            result = reply["result"]
            if not isinstance(result, dict):
                raise svd.HostError(f"{method}: result must be an object")
            return result

    def wait_for_load(self, loader_id: str, timeout: float) -> bool:
        """`loader_id` の `Page.lifecycleEvent`（name=load）を待つ。届かなければ False（呼び出し側が readyState で確認する）。"""
        deadline = time.monotonic() + timeout
        while loader_id not in self.loaded_loaders:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return False
            try:
                message = self.recv(remaining)
            except CdpTimeout:
                return False
            if not isinstance(message, dict) or "id" in message:
                raise svd.HostError("chromium sent an unexpected response while waiting for load")
            self._handle_event(message)
        return True


def evaluate(chan: CdpChannel, expression: str, timeout: float, *, await_promise: bool = False) -> Any:
    """`Runtime.evaluate`（ページの world・値渡し）を実行して値を返す。例外は失敗。"""
    params: dict[str, Any] = {"expression": expression, "returnByValue": True}
    if await_promise:
        params["awaitPromise"] = True
    try:
        result = chan.call("Runtime.evaluate", params, timeout=timeout, session=True)
    except CdpRpcError as exc:
        raise svd.HostError(f"Runtime.evaluate failed: {exc}") from exc
    if "exceptionDetails" in result:
        raise svd.HostError("Runtime.evaluate raised an exception in the page")
    remote = result.get("result")
    if not isinstance(remote, dict) or "value" not in remote:
        raise svd.HostError("Runtime.evaluate returned no value")
    return remote["value"]


def decode_screenshot(result: dict[str, Any], scratch: Path, width: int, height: int) -> bytes:
    """`Page.captureScreenshot` の結果を検証して PNG バイト列を返す（寸法は IHDR と照合）。"""
    encoded = result.get("data")
    if not isinstance(encoded, str):
        raise svd.HostError("screenshot: data must be a string")
    try:
        data = base64.b64decode(encoded, validate=True)
    except (binascii.Error, ValueError) as exc:
        raise svd.HostError(f"screenshot: invalid base64: {exc}") from exc
    if not data.startswith(PNG_MAGIC):
        raise svd.HostError("screenshot: data is not a PNG")
    probe = scratch / "probe.png"
    probe.write_bytes(data)
    try:
        actual = cs.read_png_size(probe)
    except cs.CaptureError as exc:
        raise svd.HostError(f"screenshot: invalid PNG: {exc}") from exc
    finally:
        probe.unlink(missing_ok=True)
    if actual != (width, height):
        raise svd.HostError(f"screenshot: PNG size {actual} does not match the requested {(width, height)}")
    return data


def _layout_viewport(chan: CdpChannel, timeout: float) -> tuple[int, int]:
    """`Page.getLayoutMetrics` から実ビューポート（CSS px）の寸法を取る。"""
    metrics = chan.call("Page.getLayoutMetrics", timeout=timeout, session=True)
    viewport = metrics.get("cssLayoutViewport")
    if not isinstance(viewport, dict):
        raise svd.HostError("Page.getLayoutMetrics: cssLayoutViewport must be an object")
    dims = []
    for key in ("clientWidth", "clientHeight"):
        value = viewport.get(key)
        if isinstance(value, bool) or not isinstance(value, int) or not 0 < value <= 100_000:
            raise svd.HostError(f"Page.getLayoutMetrics: {key} must be a positive integer")
        dims.append(value)
    return dims[0], dims[1]


def fit_viewport_to_window_size(chan: CdpChannel, target_id: str, width: int, height: int, timeout: float) -> None:
    """実ビューポートを要求寸法に合わせる（RENDER-5 の同条件比較）。

    `--window-size` は外枠の寸法で、headless=new ではページのビューポートと一致しない。
    差分だけ `Browser.setWindowBounds`（通常のウィンドウ操作）で外枠を広げ縮め、`Emulation.*` は使わない。
    """
    current = _layout_viewport(chan, timeout)
    if current == (width, height):
        return
    window = chan.call("Browser.getWindowForTarget", {"targetId": target_id}, timeout=timeout)
    window_id = window.get("windowId")
    bounds = window.get("bounds")
    if isinstance(window_id, bool) or not isinstance(window_id, int) or not isinstance(bounds, dict):
        raise svd.HostError("Browser.getWindowForTarget: unexpected result")
    outer = []
    for key, delta in (("width", width - current[0]), ("height", height - current[1])):
        value = bounds.get(key)
        if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
            raise svd.HostError(f"Browser.getWindowForTarget: bounds.{key} must be a positive integer")
        target = value + delta
        if not 0 < target <= 100_000:
            raise svd.HostError("cannot fit the viewport to the requested size")
        outer.append(target)
    try:
        chan.call(
            "Browser.setWindowBounds",
            {"windowId": window_id, "bounds": {"windowState": "normal", "width": outer[0], "height": outer[1]}},
            timeout=timeout,
        )
    except CdpRpcError as exc:
        raise svd.HostError(f"Browser.setWindowBounds failed: {exc}") from exc
    if _layout_viewport(chan, timeout) != (width, height):
        raise svd.HostError(f"viewport size does not match the requested {(width, height)} after resizing the window")


def interpret_bbox_value(value: Any) -> dict[str, Any]:
    """`BOUNDING_BOX_FN` の戻り値を解釈する。`{"kind": ...}`（要素単位の失敗）または `{"box": ...}`。"""
    if not isinstance(value, dict):
        raise svd.HostError("bounding box script returned a non-object")
    if value.get("error") == "invalid_selector":
        return {"kind": "invalid_params"}
    found = value.get("found")
    if found is False:
        return {"kind": "not_found"}
    if found is not True:
        raise svd.HostError("bounding box script returned an unexpected shape")
    box = svd.validate_bbox_result(value)
    # -0.0 を 0.0 に正規化する（ホストの `+ 0.0` と同じ）。
    return {"box": {key: number + 0.0 for key, number in box.items()}}


@dataclass
class DriveResult:
    png: bytes
    elements: list[dict[str, Any]]
    errors: list[dict[str, str]]
    final_url_matches: bool
    chromium_version: str | None
    graceful: bool = False


def drive(
    chan: CdpChannel,
    url: str,
    elements: list[dict[str, str]],
    load_timeout_ms: int | None,
    scratch: Path,
    width: int,
    height: int,
) -> DriveResult:
    """`Browser.getVersion` から `Browser.close` までの一連の CDP 要求列を実行する。"""
    startup = STARTUP_TIMEOUT_S + TIMEOUT_SLACK_S
    version_result = chan.call("Browser.getVersion", timeout=startup)
    product = version_result.get("product")
    version = product if isinstance(product, str) and len(product) <= svd.VERSION_STRING_MAX and re.fullmatch(VERSION_RE_PATTERN, product) else None

    target_id = None
    for attempt in range(10):
        targets = chan.call("Target.getTargets", timeout=startup).get("targetInfos")
        if not isinstance(targets, list):
            raise svd.HostError("Target.getTargets: targetInfos must be a list")
        pages = [t for t in targets if isinstance(t, dict) and t.get("type") == "page"]
        if pages:
            if len(pages) != 1:
                raise svd.HostError(f"expected exactly one page target, found {len(pages)}")
            target_id = pages[0].get("targetId")
            break
        time.sleep(0.2)
    if not isinstance(target_id, str) or not target_id:
        raise svd.HostError("no page target is available")
    attached = chan.call("Target.attachToTarget", {"targetId": target_id, "flatten": True}, timeout=startup)
    session = attached.get("sessionId")
    if not isinstance(session, str) or not session:
        raise svd.HostError("Target.attachToTarget: sessionId must be a non-empty string")
    chan.session = session
    chan.call("Page.enable", timeout=startup, session=True)
    chan.call("Page.setLifecycleEventsEnabled", {"enabled": True}, timeout=startup, session=True)
    fit_viewport_to_window_size(chan, target_id, width, height, startup)

    load_wait = (DEFAULT_LOAD_TIMEOUT_MS if load_timeout_ms is None else load_timeout_ms) / 1000 + TIMEOUT_SLACK_S
    try:
        navigated = chan.call("Page.navigate", {"url": url}, timeout=load_wait, session=True)
    except CdpRpcError as exc:
        raise svd.HostError(f"navigate failed: {exc}") from exc
    if "errorText" in navigated:
        raise svd.HostError("navigate failed: the page could not be loaded")
    loader_id = navigated.get("loaderId")
    if not isinstance(loader_id, str) or not loader_id:
        raise svd.HostError("navigate failed: loaderId is missing")
    if not chan.wait_for_load(loader_id, load_wait):
        if evaluate(chan, READY_STATE_EXPR, QUERY_TIMEOUT_S + TIMEOUT_SLACK_S) is not True:
            raise svd.HostError("timed out waiting for the load event")

    if evaluate(chan, FONTS_AND_FRAME_EXPR, FONTS_TIMEOUT_S + TIMEOUT_SLACK_S, await_promise=True) is not True:
        raise svd.HostError("fonts/frame wait did not complete")

    try:
        shot = chan.call(
            "Page.captureScreenshot",
            {"format": "png", "captureBeyondViewport": False, "fromSurface": True},
            timeout=SCREENSHOT_TIMEOUT_S + TIMEOUT_SLACK_S,
            session=True,
            max_message=SCREENSHOT_MESSAGE_MAX,
        )
    except CdpRpcError as exc:
        raise svd.HostError(f"screenshot failed: {exc}") from exc
    png = decode_screenshot(shot, scratch, width, height)

    boxes: list[dict[str, Any]] = []
    errors: list[dict[str, str]] = []
    for element in elements:
        literal = json.dumps(svd.validate_selector(element["selector"]), ensure_ascii=True)
        value = evaluate(chan, f"{BOUNDING_BOX_FN}({literal})", QUERY_TIMEOUT_S + TIMEOUT_SLACK_S)
        outcome = interpret_bbox_value(value)
        if "kind" in outcome:
            errors.append({"id": element["id"], "kind": outcome["kind"]})
        else:
            boxes.append({"id": element["id"], **outcome["box"]})

    try:
        info = chan.call("Target.getTargetInfo", {"targetId": target_id}, timeout=URL_TIMEOUT_S + TIMEOUT_SLACK_S)
        target_info = info.get("targetInfo")
        final_matches = isinstance(target_info, dict) and target_info.get("url") == url
    except CdpRpcError:
        final_matches = False

    # 結果はここまでで揃っている。close の失敗は警告にとどめ、出力済みの結果は有効とする。
    graceful = False
    try:
        chan.call("Browser.close", timeout=SHUTDOWN_GRACE_S + TIMEOUT_SLACK_S)
        graceful = True
    except (svd.HostError, CdpRpcError) as exc:
        print(f"warning: Browser.close did not complete cleanly: {exc}", file=sys.stderr)
    return DriveResult(
        png=png,
        elements=boxes,
        errors=errors,
        final_url_matches=final_matches,
        chromium_version=version,
        graceful=graceful,
    )


# --- プロセス管理 ---------------------------------------------------------------


def _kill_group(pgid: int) -> None:
    """Chromium のプロセスグループ（レンダラー・GPU・zygote を含む）を SIGKILL する。"""
    try:
        os.killpg(pgid, signal.SIGKILL)
    except (ProcessLookupError, PermissionError):
        pass


# 見張りプロセスの本体（`python -I -c` で実行する）。argv: 読み取り fd・Chromium のグループ ID・一時ディレクトリ。
# ドライバーが握る書き込み端が閉じられたとき（EOF）に、先に "x"（正常終了の合図）が届いていなければ
# ドライバーが SIGKILL 等で異常終了したとみなし、Chromium のグループを kill して一時ディレクトリを消す。
_WATCHDOG_SRC = """
import os, shutil, signal, sys, time
fd, pgid, workdir = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
try:
    data = os.read(fd, 1)
except OSError:
    data = b""
if data != b"x":
    try:
        os.killpg(pgid, signal.SIGKILL)
    except OSError:
        pass
    time.sleep(0.5)
    shutil.rmtree(workdir, ignore_errors=True)
"""


def _start_watchdog(pgid: int, workdir: Path) -> tuple[int, subprocess.Popen[bytes]]:
    """ドライバーの異常終了（SIGKILL を含む）でも Chromium のグループと一時ディレクトリを回収する見張りを起動する。

    Chromium は `start_new_session=True` で別グループにあり、capture_screenshots.py がドライバーのグループへ
    送る SIGKILL では死なず、`finally` も走らない。見張りは自分の別セッションで動くため巻き込まれず、
    pipe の EOF（書き込み端を握るドライバーの死）を契機に回収する。戻り値は (書き込み端 fd, 見張り)。
    """
    wd_r, wd_w = os.pipe()
    try:
        wd = subprocess.Popen(  # noqa: S603  (argv 固定・シェル不使用)
            [sys.executable, "-I", "-c", _WATCHDOG_SRC, str(wd_r), str(pgid), str(workdir)],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            close_fds=True,
            pass_fds=(wd_r,),
            start_new_session=True,
        )
    except OSError:
        os.close(wd_w)
        raise
    finally:
        os.close(wd_r)
    return wd_w, wd


def _disarm_watchdog(wd_w: int, wd: subprocess.Popen[bytes]) -> None:
    """正常な後始末が済んだ合図（"x"）を送って見張りを止め、reap する。"""
    try:
        os.write(wd_w, b"x")
    except OSError:
        pass
    os.close(wd_w)
    try:
        wd.wait(timeout=5)
    except subprocess.TimeoutExpired:
        wd.kill()
        wd.wait()


def _stop_chromium(proc: subprocess.Popen[bytes], graceful: bool) -> None:
    """Chromium を止める。正常時は猶予を待ち、いずれの場合も最後にグループごと kill して reap する。"""
    pgid = proc.pid  # start_new_session=True のため pid == pgid
    if graceful:
        try:
            proc.wait(timeout=SHUTDOWN_GRACE_S)
        except subprocess.TimeoutExpired:
            pass
    _kill_group(pgid)
    try:
        proc.wait(timeout=SHUTDOWN_GRACE_S)
    except subprocess.TimeoutExpired:
        pass


def run_chromium(
    prefix: list[str],
    url: str,
    elements: list[dict[str, str]],
    load_timeout_ms: int | None,
    width: int,
    height: int,
) -> DriveResult:
    """Chromium を pipe CDP で起動して駆動する。一時ディレクトリは成否に関わらず削除する。"""
    workdir = Path(tempfile.mkdtemp(prefix="chromium-bbox-driver-"))
    try:
        profile = workdir / "profile"
        scratch = workdir / "tmp"
        profile.mkdir()
        scratch.mkdir()
        argv = build_chromium_argv(prefix, str(profile), width, height)
        to_r, to_w = os.pipe()  # driver -> chromium（Chromium は fd 3 から読む）
        from_r, from_w = os.pipe()  # chromium -> driver（Chromium は fd 4 へ書く）
        proc: subprocess.Popen[bytes] | None = None
        try:
            proc = subprocess.Popen(  # noqa: S603  (argv 固定・シェル不使用)
                argv,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
                close_fds=False,
                start_new_session=True,
                preexec_fn=_make_child_setup(to_r, from_w),  # noqa: PLW1509
            )
        except OSError as exc:
            raise svd.HostError(f"failed to launch chromium: {exc}") from exc
        finally:
            # 子に渡した端を閉じる。親が使うのは to_w・from_r（起動に失敗したら両方閉じる）。
            os.close(to_r)
            os.close(from_w)
            if proc is None:
                os.close(to_w)
                os.close(from_r)
        assert proc.stderr is not None
        try:
            wd_w, watchdog = _start_watchdog(proc.pid, workdir)
        except OSError as exc:
            _stop_chromium(proc, False)
            raise svd.HostError(f"failed to start watchdog: {exc}") from exc
        stderr_tail = svd.StderrTail(proc.stderr)
        graceful = False
        stopped = False
        # 読み取りスレッドは Popen の後に起動する（preexec_fn はスレッドがあると安全でないため）。
        chan = CdpChannel(os.fdopen(to_w, "wb", buffering=0), os.fdopen(from_r, "rb", buffering=0))
        try:
            result = drive(chan, url, elements, load_timeout_ms, scratch, width, height)
            graceful = result.graceful
            return result
        except (svd.HostError, CdpRpcError) as exc:
            _stop_chromium(proc, False)
            stopped = True
            tail = stderr_tail.text()
            if tail:
                print(f"chromium stderr (tail):\n{tail}", file=sys.stderr)
            if isinstance(exc, CdpRpcError):
                raise svd.HostError(str(exc)) from exc
            raise
        finally:
            if not stopped:
                _stop_chromium(proc, graceful)
            chan.close()
            # グループを kill して reap した後に解除する（解除前に死んだ場合は見張りが回収する）。
            _disarm_watchdog(wd_w, watchdog)
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


# --- CLI ----------------------------------------------------------------------


def _dimension_type(raw: str) -> int:
    try:
        value = int(raw)
    except ValueError as exc:
        raise argparse.ArgumentTypeError(f"invalid int value: {raw!r}") from exc
    if not (1 <= value <= MAX_DIMENSION):
        raise argparse.ArgumentTypeError(f"value must be in [1, {MAX_DIMENSION}]")
    return value


def build_arg_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Drive Chromium over --remote-debugging-pipe: load, screenshot and bounding boxes for each selector."
    )
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--chromium-bin", help="path to the chromium executable")
    group.add_argument("--chromium-cmd", help="chromium launcher argv as a JSON array (test use; flags are appended)")
    parser.add_argument("--selectors", type=Path, required=True, help="selectors JSON (shared with servo_bbox_driver.py)")
    parser.add_argument("--site-id", help="site id in the selectors file (default: stem of OUT_PNG)")
    parser.add_argument("--width", type=_dimension_type, default=DEFAULT_WIDTH)
    parser.add_argument("--height", type=_dimension_type, default=DEFAULT_HEIGHT)
    parser.add_argument("--load-timeout-ms", type=svd._load_timeout_type, default=None)
    parser.add_argument(
        "--assume-isolated",
        action="store_true",
        help="declare that this runs inside the --network none measurement container",
    )
    parser.add_argument("url", help="http://pages.test:<port>/... page URL")
    parser.add_argument("out_png", type=Path, help="output PNG path (bboxes go next to it as <stem>.bboxes.json)")
    return parser


def _chromium_prefix(args: argparse.Namespace) -> list[str]:
    if args.chromium_bin:
        return [args.chromium_bin]
    try:
        parsed = json.loads(args.chromium_cmd)
    except ValueError as exc:
        raise svd.UsageError(f"--chromium-cmd must be a JSON array of strings: {exc}") from exc
    if not isinstance(parsed, list) or not parsed or not all(isinstance(x, str) for x in parsed):
        raise svd.UsageError("--chromium-cmd must be a non-empty JSON array of strings")
    return parsed


def _prepare(args: argparse.Namespace) -> tuple[list[str], list[dict[str, str]] | None, Path]:
    if not args.assume_isolated:
        raise svd.UsageError("--assume-isolated is required (run inside the --network none container)")
    try:
        cs.check_saved_pages_url(args.url)
    except cs.SiteListError as exc:
        raise svd.UsageError(str(exc)) from exc
    out = args.out_png
    if out.suffix != ".png":
        raise svd.UsageError("OUT_PNG must end with .png")
    if out.parent.is_symlink():
        raise svd.UsageError(f"refusing to write into a symlinked directory: {out.parent}")
    if not out.parent.is_dir():
        raise svd.UsageError(f"output directory does not exist: {out.parent}")
    sites = svd.load_selectors(args.selectors)
    site_id = args.site_id or out.stem
    elements = sites.get(site_id)
    if elements is None:
        # bbox JSON は出さない（空 elements は計測済みの 0 要素と読まれるため。write_bbox_outputs 参照）。
        print(
            f"warning: no selectors for site_id {site_id!r}; writing the PNG only (bbox will be not_measured)",
            file=sys.stderr,
        )
    return _chromium_prefix(args), elements, out


def main(argv: list[str] | None = None) -> int:
    args = build_arg_parser().parse_args(argv)
    if os.name != "posix" or fcntl is None:
        print("error: unsupported platform (POSIX only)", file=sys.stderr)
        return 2
    try:
        prefix, elements, out = _prepare(args)
    except svd.UsageError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    # 前回の出力を先に消す（失敗時に古い PNG・bbox が今回の結果として残らないようにする）。
    svd.remove_outputs(out)
    previous = None
    try:
        # capture_screenshots の SIGTERM を後始末（Chromium のグループ kill）へ流す。
        previous = signal.signal(signal.SIGTERM, svd._raise_keyboard_interrupt)
    except ValueError:
        pass
    try:
        result = run_chromium(prefix, args.url, elements or [], args.load_timeout_ms, args.width, args.height)
        svd.write_bbox_outputs(
            out,
            result.png,
            result.elements,
            result.errors,
            {
                "tool": TOOL_NAME,
                "chromium_version": result.chromium_version,
                "final_url_matches": result.final_url_matches,
            },
            write_bboxes=elements is not None,
        )
    except svd.HostError as exc:
        svd.remove_outputs(out)
        print(f"error: {exc}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        svd.remove_outputs(out)
        print("error: interrupted", file=sys.stderr)
        return 1
    finally:
        if previous is not None:
            signal.signal(signal.SIGTERM, previous)
    if elements is None:
        print(f"wrote {out} (no bboxes)", file=sys.stderr)
    else:
        print(f"wrote {out} and {svd.bbox_path_for(out)} ({len(result.elements)} elements)", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
