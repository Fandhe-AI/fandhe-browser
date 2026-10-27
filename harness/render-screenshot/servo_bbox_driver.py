#!/usr/bin/env python3
"""Servo ホストの `bounding_box` を stdio で呼び、PNG と境界ボックス JSON を書き出す駆動スクリプト。

役割（Issue #1282・TASK-37・`RENDER-5`・`MEAS-3`）:
    #55 の境界ボックス一致率には Servo 側の要素矩形が要るが、cli・CDP からは
    ホストの `bounding_box` を呼べない。2026-10-27 のオーナー判断で本体（cli・render・
    servo-host）は変えず、ハーネス側でホスト（`fandhe-browser-servo-host --stdio`）を直接駆動する。
    プロトコルは `docs/design/render-host-protocol.schema.json`（`RENDER-3`）に従い、
    hello 受信 -> `load` -> `screenshot` -> `bounding_box` x N -> `shutdown` の順に送る。

呼び出し文脈:
    - `capture_screenshots.py --saved-pages` の `--servo-cmd` として差し込む（`{url}`・`{out}` を受ける）。
      同じホストセッションで撮った PNG を `{out}` へ書き、その SHA-256 を持つ
      `<stem>.bboxes.json` を隣へ書く。`measure_ssim.py` は PNG と bbox の対応を
      `png_sha256` で照合する（`_bbox_png_binding_mismatch`）ため、cli `screenshot` の
      別撮影の PNG とは紐付けられず、PNG も本スクリプトが撮る必要がある。
    - 単体でも実行できる（`--assume-isolated` が必須）。
    - 使い方・信頼境界は README「`servo_bbox_driver.py`」と RUNBOOK §6.1 を参照。

信頼境界:
    - URL は `capture_screenshots.check_saved_pages_url`（`http://pages.test:<port>` のみ）を共有する。
    - ホストはシェルを介さず `[host, "--stdio"]` で起動し、環境変数は
      `crates/fandhe-browser-render/src/host_process/env.rs` と同じ許可リストに限る（`SEC-2`）。
    - ホストの応答は untrusted として検証する（id・result/error の排他・型・範囲・フレーム長）。
      未知フィールドは無視する（`REPAIR-4`）。
    - UA・ヘッダ・ビューポート等のパラメータは送らない（スキーマにも無い。`SEC-2`）。

未実装（実装済みを装わない。`REPAIR-3`）:
    - Chromium 側の bbox 抽出と、対象要素（セレクタ）の選定は範囲外（#55）。
    - 実ホストでの動作は未検証。偽ホスト（`fixtures/fake_render_host.py`）によるオフライン検証のみ。

終了コード: 0（PNG を書いた）／1（ホスト・プロトコル・ページ読み込みの失敗）／2（引数・入力・URL 規則の違反）。
"""

from __future__ import annotations

import argparse
import base64
import binascii
import hashlib
import json
import math
import os
import queue
import re
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import threading
import unicodedata
from dataclasses import dataclass
from pathlib import Path
from typing import Any

_THIS_DIR = Path(__file__).resolve().parent
if str(_THIS_DIR) not in sys.path:
    sys.path.insert(0, str(_THIS_DIR))
import capture_screenshots as cs  # noqa: E402  (同ディレクトリの兄弟モジュール)
import measure_ssim as ms  # noqa: E402

# --- 定数 -------------------------------------------------------------------

PROTOCOL_VERSION = 1
# ホストの hello（`limits`）と照合する前のクライアント側上限。確保前に長さ接頭辞を照合する。
HELLO_FRAME_MAX = 16 * 1024
RESPONSE_FRAME_MAX = 1024 * 1024
SCREENSHOT_FRAME_MAX = 24 * 1024 * 1024
REQUEST_FRAME_MAX = 64 * 1024
# 待ち時間（hello の `limits` と同じ値。余裕分を足して使う）。
HANDSHAKE_TIMEOUT_S = 10.0
SCREENSHOT_TIMEOUT_S = 20.0
QUERY_TIMEOUT_S = 5.0
SHUTDOWN_GRACE_S = 3.0
TIMEOUT_SLACK_S = 5.0
LOAD_TIMEOUT_MAX_MS = 60_000
MAX_DIMENSION = 16_384
REQUIRED_CAPABILITIES = ("load", "screenshot", "bounding_box", "shutdown")
STDERR_TAIL_BYTES = 4096
MAX_SELECTORS_FILE_BYTES = 1 * 1024 * 1024
MAX_SITES = 50
MAX_SELECTOR_BYTES = 1024
VERSION_STRING_MAX = 64
# 要素単位の失敗として記録して続行するエラー種別。それ以外はサイト全体の失敗にする。
ELEMENT_ERROR_KINDS = ("not_found", "invalid_params")
TOOL_NAME = "servo_bbox_driver"


class UsageError(Exception):
    """引数・入力ファイル・URL 規則の違反（終了コード 2）。"""


class HostError(Exception):
    """ホスト・プロトコル・ページ読み込みの失敗（終了コード 1）。"""


class RpcError(Exception):
    """ホストが返した `error` 応答。要素単位の失敗かどうかは `kind` で判断する。"""

    def __init__(self, kind: str) -> None:
        super().__init__(kind)
        self.kind = kind


# --- セレクタ入力 -------------------------------------------------------------


def validate_selector(selector: Any) -> str:
    """ホストの `validate_selector`（servo-host/src/layout.rs）と同じ規則を先に適用する。"""
    if not isinstance(selector, str):
        raise UsageError("selector must be a string")
    # 単独サロゲート等は UTF-8 にできないため先に弾く。
    size = len(selector.encode("utf-8")) if _encodable(selector) else -1
    if size < 1 or size > MAX_SELECTOR_BYTES:
        raise UsageError(f"selector must be 1..{MAX_SELECTOR_BYTES} UTF-8 bytes")
    for ch in selector:
        if unicodedata.category(ch) == "Cc" or ch in (" ", " "):
            raise UsageError("selector must not contain control characters")
    return selector


def _encodable(text: str) -> bool:
    try:
        text.encode("utf-8")
    except UnicodeEncodeError:
        return False
    return True


def load_selectors(path: Path) -> dict[str, list[dict[str, str]]]:
    """セレクタ入力ファイルを読み、`site_id` -> `[{id, selector}]` を返す（fail-closed）。"""
    try:
        if path.is_symlink():
            raise UsageError(f"refusing to follow symlink: {path}")
        with path.open("rb") as fh:
            raw = fh.read(MAX_SELECTORS_FILE_BYTES + 1)
    except OSError as exc:
        raise UsageError(f"failed to read selectors file: {path}: {exc}") from exc
    if len(raw) > MAX_SELECTORS_FILE_BYTES:
        raise UsageError(f"selectors file exceeds the {MAX_SELECTORS_FILE_BYTES} byte limit")
    try:
        payload = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, ValueError, RecursionError) as exc:
        raise UsageError(f"invalid selectors JSON: {exc}") from exc
    if not isinstance(payload, dict) or set(payload) != {"schema_version", "sites"}:
        raise UsageError("selectors JSON must be an object with exactly schema_version and sites")
    if payload["schema_version"] != 1 or isinstance(payload["schema_version"], bool):
        raise UsageError("unsupported selectors schema_version")
    sites = payload["sites"]
    if not isinstance(sites, list) or len(sites) > MAX_SITES:
        raise UsageError(f"sites must be a list of at most {MAX_SITES} entries")
    result: dict[str, list[dict[str, str]]] = {}
    for site in sites:
        if not isinstance(site, dict) or set(site) != {"site_id", "elements"}:
            raise UsageError("each site must be an object with exactly site_id and elements")
        site_id = site["site_id"]
        if not isinstance(site_id, str) or not site_id:
            raise UsageError("site_id must be a non-empty string")
        if site_id in result:
            raise UsageError(f"duplicate site_id: {site_id}")
        elements = site["elements"]
        if not isinstance(elements, list) or len(elements) > ms.MAX_BBOX_ELEMENTS:
            raise UsageError(f"elements must be a list of at most {ms.MAX_BBOX_ELEMENTS} entries")
        seen: set[str] = set()
        checked: list[dict[str, str]] = []
        for element in elements:
            if not isinstance(element, dict) or set(element) != {"id", "selector"}:
                raise UsageError("each element must be an object with exactly id and selector")
            eid = element["id"]
            if not isinstance(eid, str) or not ms.BBOX_ID_RE.fullmatch(eid):
                raise UsageError(f"invalid element id: {eid!r}")
            if eid in seen:
                raise UsageError(f"duplicate element id: {eid}")
            seen.add(eid)
            checked.append({"id": eid, "selector": validate_selector(element["selector"])})
        result[site_id] = checked
    return result


# --- ホストの起動 -------------------------------------------------------------


def build_host_env(workdir: Path, platform: str | None = None) -> dict[str, str]:
    """ホストへ渡す環境変数（許可リストのみ）を作る。

    `crates/fandhe-browser-render/src/host_process/env.rs` と同じ集合を、実行ごとの
    一時ディレクトリ配下へ向ける。`PATH`・`LANG`・`TZ`・`*_PROXY`・`LD_*`・`DYLD_*` は渡さない
    （識別面・注入経路。`SEC-2`・PROF）。
    """
    platform = sys.platform if platform is None else platform
    home = workdir / "home"
    tmp = workdir / "tmp"
    env: dict[str, str] = {}
    passthrough: tuple[str, ...] = ()
    if platform.startswith("win"):
        env.update({"TMP": str(tmp), "TEMP": str(tmp)})
        passthrough = ("SystemRoot",)
    else:
        env.update({"HOME": str(home), "TMPDIR": str(tmp)})
        if platform.startswith("linux"):
            for name, sub in (
                ("XDG_CONFIG_HOME", "xdg-config"),
                ("XDG_CACHE_HOME", "xdg-cache"),
                ("XDG_DATA_HOME", "xdg-data"),
            ):
                env[name] = str(workdir / sub)
            passthrough = ("FONTCONFIG_FILE", "FONTCONFIG_PATH")
    for name in passthrough:
        value = os.environ.get(name)
        if value is not None:
            env[name] = value
    return env


# --- フレーム入出力 -----------------------------------------------------------


class HostChannel:
    """ホストの stdio フレーム（u32 LE 長 + UTF-8 JSON）を扱う。

    読み取りは専用スレッドとキューで行い、タイムアウトを付ける（Windows のパイプは
    select できないため）。長さ接頭辞は確保前に `max_frame` と照合する。
    """

    def __init__(self, proc: subprocess.Popen[bytes]) -> None:
        self._proc = proc
        self._queue: queue.Queue[tuple[str, Any]] = queue.Queue()
        self.max_frame = HELLO_FRAME_MAX
        self.request_max = REQUEST_FRAME_MAX
        self._thread = threading.Thread(target=self._read_loop, daemon=True)
        self._thread.start()

    def _read_exact(self, size: int) -> bytes | None:
        stream = self._proc.stdout
        assert stream is not None
        data = stream.read(size)
        if len(data) < size:
            return None
        return data

    def _read_loop(self) -> None:
        try:
            while True:
                header = self._read_exact(4)
                if header is None:
                    self._queue.put(("eof", None))
                    return
                (length,) = struct.unpack("<I", header)
                if length > self.max_frame:
                    self._queue.put(("error", f"frame length {length} exceeds the {self.max_frame} byte limit"))
                    return
                body = self._read_exact(length)
                if body is None:
                    self._queue.put(("error", "host closed the pipe in the middle of a frame"))
                    return
                self._queue.put(("frame", body))
        except (OSError, ValueError) as exc:
            self._queue.put(("error", f"failed to read from host: {exc}"))

    def recv(self, timeout: float) -> Any:
        try:
            kind, value = self._queue.get(timeout=timeout)
        except queue.Empty as exc:
            raise HostError(f"timed out waiting for the host ({timeout:.0f}s)") from exc
        if kind == "eof":
            raise HostError("host closed stdout before responding")
        if kind == "error":
            raise HostError(str(value))
        try:
            return json.loads(value.decode("utf-8"))
        except (UnicodeDecodeError, ValueError, RecursionError) as exc:
            raise HostError(f"host sent an invalid JSON frame: {exc}") from exc

    def send(self, message: dict[str, Any]) -> None:
        body = json.dumps(message, separators=(",", ":"), ensure_ascii=False).encode("utf-8")
        if len(body) > self.request_max:
            raise HostError(f"request frame exceeds the {self.request_max} byte limit")
        stream = self._proc.stdin
        assert stream is not None
        try:
            stream.write(struct.pack("<I", len(body)) + body)
            stream.flush()
        except (OSError, ValueError) as exc:
            raise HostError(f"failed to write to host: {exc}") from exc


@dataclass
class HostInfo:
    host_version: str
    servo_version: str
    response_max: int = RESPONSE_FRAME_MAX
    screenshot_max: int = SCREENSHOT_FRAME_MAX
    request_max: int = REQUEST_FRAME_MAX


def _is_int(value: Any) -> bool:
    return isinstance(value, int) and not isinstance(value, bool)


def validate_hello(message: Any) -> HostInfo:
    """hello イベントを検証する（`v`・`event`・プロトコル範囲・必須 capability）。"""
    if not isinstance(message, dict) or message.get("v") != PROTOCOL_VERSION or message.get("event") != "hello":
        raise HostError("first frame is not a protocol v1 hello event")
    data = message.get("data")
    if not isinstance(data, dict):
        raise HostError("hello data must be an object")
    protocol = data.get("protocol")
    if not isinstance(protocol, dict) or not _is_int(protocol.get("min")) or not _is_int(protocol.get("max")):
        raise HostError("hello protocol range is missing")
    if not (protocol["min"] <= PROTOCOL_VERSION <= protocol["max"]):
        raise HostError(f"host does not support protocol {PROTOCOL_VERSION}")
    capabilities = data.get("capabilities")
    if not isinstance(capabilities, list):
        raise HostError("hello capabilities must be a list")
    missing = [c for c in REQUIRED_CAPABILITIES if c not in capabilities]
    if missing:
        raise HostError(f"host lacks required capabilities: {', '.join(missing)}")
    versions: list[str] = []
    for key in ("host_version", "servo_version"):
        value = data.get(key)
        if not isinstance(value, str) or not value or len(value) > VERSION_STRING_MAX:
            raise HostError(f"hello {key} must be a non-empty string of at most {VERSION_STRING_MAX} chars")
        versions.append(value)
    info = HostInfo(host_version=versions[0], servo_version=versions[1])
    limits = data.get("limits")
    if not isinstance(limits, dict):
        raise HostError("hello limits must be an object")
    for key, attr in (
        ("response_frame_bytes", "response_max"),
        ("screenshot_frame_bytes", "screenshot_max"),
        ("request_frame_bytes", "request_max"),
    ):
        declared = limits.get(key)
        if not _is_int(declared) or declared < 1:
            raise HostError(f"hello limits.{key} must be a positive integer")
        # 申告値が大きくてもクライアント側の上限を超えて確保しない。
        setattr(info, attr, min(declared, getattr(info, attr)))
    return info


def call(
    chan: HostChannel,
    next_id: list[int],
    method: str,
    params: dict[str, Any] | None,
    timeout: float,
    *,
    max_frame: int,
) -> dict[str, Any]:
    """要求を 1 件送り、対応する応答の `result` を返す。`error` 応答は `RpcError`。"""
    request_id = next_id[0]
    next_id[0] += 1
    message: dict[str, Any] = {"v": PROTOCOL_VERSION, "id": request_id, "method": method}
    if params is not None:
        message["params"] = params
    # 応答待ちに入る前に上限を設定する（読み取りスレッドが長さ接頭辞を照合するため）。
    chan.max_frame = max_frame
    chan.send(message)
    response = chan.recv(timeout)
    if not isinstance(response, dict) or response.get("v") != PROTOCOL_VERSION:
        raise HostError(f"{method}: response is not a protocol v1 envelope")
    if response.get("id") != request_id or not _is_int(response.get("id")):
        raise HostError(f"{method}: response id does not match request id {request_id}")
    has_result = "result" in response
    has_error = "error" in response
    if has_result == has_error:
        raise HostError(f"{method}: response must have exactly one of result and error")
    if has_error:
        error = response["error"]
        kind = error.get("kind") if isinstance(error, dict) else None
        if not isinstance(kind, str):
            raise HostError(f"{method}: malformed error response")
        raise RpcError(kind)
    result = response["result"]
    if not isinstance(result, dict):
        raise HostError(f"{method}: result must be an object")
    return result


def validate_bbox_result(result: dict[str, Any]) -> dict[str, float]:
    """`bounding_box` の結果を検証する（有限の数値・width/height は 0 以上・絶対値上限）。"""
    values: dict[str, float] = {}
    for key in ("x", "y", "width", "height"):
        raw = result.get(key)
        if isinstance(raw, bool) or not isinstance(raw, (int, float)):
            raise HostError(f"bounding_box: {key} must be a number")
        try:
            value = float(raw)
        except OverflowError as exc:
            raise HostError(f"bounding_box: {key} out of range") from exc
        if not math.isfinite(value) or abs(value) > ms.MAX_BBOX_VALUE_ABS:
            raise HostError(f"bounding_box: {key} out of range")
        values[key] = value
    if values["width"] < 0 or values["height"] < 0:
        raise HostError("bounding_box: width/height must be >= 0")
    return values


def decode_screenshot(result: dict[str, Any], scratch: Path) -> bytes:
    """`screenshot` の結果を検証して PNG バイト列を返す（寸法は IHDR と照合）。"""
    if result.get("format") != "png":
        raise HostError("screenshot: format must be png")
    width, height = result.get("width"), result.get("height")
    for label, dim in (("width", width), ("height", height)):
        if not _is_int(dim) or not (1 <= dim <= MAX_DIMENSION):
            raise HostError(f"screenshot: {label} must be an integer in [1, {MAX_DIMENSION}]")
    encoded = result.get("data_base64")
    if not isinstance(encoded, str):
        raise HostError("screenshot: data_base64 must be a string")
    try:
        data = base64.b64decode(encoded, validate=True)
    except (binascii.Error, ValueError) as exc:
        raise HostError(f"screenshot: invalid base64: {exc}") from exc
    if not data.startswith(b"\x89PNG\r\n\x1a\n"):
        raise HostError("screenshot: data is not a PNG")
    probe = scratch / "probe.png"
    probe.write_bytes(data)
    try:
        actual = cs.read_png_size(probe)
    except cs.CaptureError as exc:
        raise HostError(f"screenshot: invalid PNG: {exc}") from exc
    finally:
        probe.unlink(missing_ok=True)
    if actual != (width, height):
        raise HostError(f"screenshot: PNG size {actual} does not match the reported {(width, height)}")
    return data


@dataclass
class DriveResult:
    png: bytes
    elements: list[dict[str, Any]]
    errors: list[dict[str, str]]
    final_url_matches: bool
    info: HostInfo
    graceful: bool = False


def drive(
    chan: HostChannel, url: str, elements: list[dict[str, str]], load_timeout_ms: int | None, scratch: Path
) -> DriveResult:
    """hello 受信から `shutdown` までの一連の要求列を実行する。"""
    info = validate_hello(chan.recv(HANDSHAKE_TIMEOUT_S))
    chan.request_max = info.request_max
    next_id = [1]
    params: dict[str, Any] = {"url": url}
    load_wait = (30_000 if load_timeout_ms is None else load_timeout_ms) / 1000 + TIMEOUT_SLACK_S
    if load_timeout_ms is not None:
        params["timeout_ms"] = load_timeout_ms
    try:
        loaded = call(chan, next_id, "load", params, load_wait, max_frame=info.response_max)
    except RpcError as exc:
        raise HostError(f"load failed: {exc.kind}") from exc
    try:
        shot = call(
            chan,
            next_id,
            "screenshot",
            {"full_page": False},
            SCREENSHOT_TIMEOUT_S + TIMEOUT_SLACK_S,
            max_frame=info.screenshot_max,
        )
    except RpcError as exc:
        raise HostError(f"screenshot failed: {exc.kind}") from exc
    png = decode_screenshot(shot, scratch)
    boxes: list[dict[str, Any]] = []
    errors: list[dict[str, str]] = []
    for element in elements:
        try:
            result = call(
                chan,
                next_id,
                "bounding_box",
                {"selector": element["selector"]},
                QUERY_TIMEOUT_S + TIMEOUT_SLACK_S,
                max_frame=info.response_max,
            )
        except RpcError as exc:
            if exc.kind in ELEMENT_ERROR_KINDS:
                errors.append({"id": element["id"], "kind": exc.kind})
                continue
            raise HostError(f"bounding_box failed for {element['id']}: {exc.kind}") from exc
        boxes.append({"id": element["id"], **validate_bbox_result(result)})
    # 結果はここまでで揃っている。shutdown の失敗は警告にとどめ、出力済みの結果は有効とする。
    graceful = False
    try:
        call(chan, next_id, "shutdown", None, SHUTDOWN_GRACE_S + TIMEOUT_SLACK_S, max_frame=info.response_max)
        graceful = True
    except (HostError, RpcError) as exc:
        print(f"warning: shutdown did not complete cleanly: {exc}", file=sys.stderr)
    return DriveResult(
        png=png,
        elements=boxes,
        errors=errors,
        final_url_matches=loaded.get("final_url") == url,
        info=info,
        graceful=graceful,
    )


# --- 出力 ---------------------------------------------------------------------


def bbox_path_for(out: Path) -> Path:
    return out.with_suffix(".bboxes.json")


def write_outputs(out: Path, result: DriveResult) -> None:
    """PNG -> bboxes の順に書く。どちらかが失敗したら両方消す。"""
    bboxes = bbox_path_for(out)
    payload = {
        "schema_version": 1,
        "png_sha256": hashlib.sha256(result.png).hexdigest(),
        "elements": result.elements,
        "coordinate_space": "viewport_css_px",
        "errors": result.errors,
        "source": {
            "tool": TOOL_NAME,
            "host_version": result.info.host_version,
            "servo_version": result.info.servo_version,
            "final_url_matches": result.final_url_matches,
        },
    }
    text = json.dumps(payload, indent=2, allow_nan=False) + "\n"
    try:
        cs._write_bytes_nofollow(out, result.png)
        cs._write_bytes_nofollow(bboxes, text.encode("utf-8"))
    except OSError as exc:
        remove_outputs(out)
        raise HostError(f"failed to write outputs: {exc}") from exc


def remove_outputs(out: Path) -> None:
    for path in (out, bbox_path_for(out)):
        try:
            path.unlink(missing_ok=True)
        except OSError:
            pass


# --- 実行 ---------------------------------------------------------------------


def _stderr_tail(path: Path) -> str:
    try:
        with path.open("rb") as fh:
            fh.seek(0, os.SEEK_END)
            size = fh.tell()
            fh.seek(max(0, size - STDERR_TAIL_BYTES))
            return fh.read().decode("utf-8", errors="replace")
    except OSError:
        return ""


def _stop_host(proc: subprocess.Popen[bytes], graceful: bool) -> None:
    """ホストを止める。正常終了時は猶予を待ち、それ以外と猶予超過は kill する。"""
    for stream in (proc.stdin,):
        try:
            if stream is not None:
                stream.close()
        except OSError:
            pass
    if graceful:
        try:
            proc.wait(timeout=SHUTDOWN_GRACE_S)
            return
        except subprocess.TimeoutExpired:
            pass
    if proc.poll() is None:
        proc.kill()
    try:
        proc.wait(timeout=SHUTDOWN_GRACE_S)
    except subprocess.TimeoutExpired:
        pass
    # stdout は閉じない（読み取りスレッドが保持するバッファのロックと競合しうる。プロセス終了で EOF になる）。


def run_host(host_cmd: list[str], url: str, elements: list[dict[str, str]], load_timeout_ms: int | None) -> DriveResult:
    """ホストを `--stdio` で起動して駆動する。一時ディレクトリは成否に関わらず削除する。"""
    workdir = Path(tempfile.mkdtemp(prefix="servo-bbox-driver-"))
    try:
        for sub in ("home", "tmp", "xdg-config", "xdg-cache", "xdg-data"):
            (workdir / sub).mkdir()
        env = build_host_env(workdir)
        stderr_path = workdir / "host-stderr.log"
        with stderr_path.open("wb") as stderr_file:
            try:
                proc = subprocess.Popen(  # noqa: S603  (argv 固定・シェル不使用)
                    [*host_cmd, "--stdio"],
                    stdin=subprocess.PIPE,
                    stdout=subprocess.PIPE,
                    stderr=stderr_file,
                    env=env,
                )
            except OSError as exc:
                raise HostError(f"failed to launch the host: {exc}") from exc
            graceful = False
            try:
                chan = HostChannel(proc)
                scratch = workdir / "tmp"
                result = drive(chan, url, elements, load_timeout_ms, scratch)
                graceful = result.graceful
                return result
            except HostError:
                tail = _stderr_tail(stderr_path)
                if tail:
                    print(f"host stderr (tail):\n{tail}", file=sys.stderr)
                raise
            finally:
                _stop_host(proc, graceful)
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


def _load_timeout_type(raw: str) -> int:
    try:
        value = int(raw)
    except ValueError as exc:
        raise argparse.ArgumentTypeError(f"invalid int value: {raw!r}") from exc
    if not (1 <= value <= LOAD_TIMEOUT_MAX_MS):
        raise argparse.ArgumentTypeError(f"--load-timeout-ms must be in [1, {LOAD_TIMEOUT_MAX_MS}]")
    return value


def build_arg_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Drive the Servo host over stdio: load, screenshot and bounding_box for each selector."
    )
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--host-bin", help="path to fandhe-browser-servo-host")
    group.add_argument("--host-cmd", help="host argv as a JSON array (test use; --stdio is appended)")
    parser.add_argument("--selectors", type=Path, required=True, help="selectors JSON (schema_version 1, sites[])")
    parser.add_argument("--site-id", help="site id in the selectors file (default: stem of OUT_PNG)")
    parser.add_argument("--load-timeout-ms", type=_load_timeout_type, default=None)
    parser.add_argument(
        "--assume-isolated",
        action="store_true",
        help="declare that this runs inside the --network none measurement container",
    )
    parser.add_argument("url", help="http://pages.test:<port>/... page URL")
    parser.add_argument("out_png", type=Path, help="output PNG path (bboxes go next to it as <stem>.bboxes.json)")
    return parser


def _host_command(args: argparse.Namespace) -> list[str]:
    if args.host_bin:
        return [os.path.abspath(args.host_bin)]
    try:
        parsed = json.loads(args.host_cmd)
    except ValueError as exc:
        raise UsageError(f"--host-cmd must be a JSON array of strings: {exc}") from exc
    if not isinstance(parsed, list) or not parsed or not all(isinstance(x, str) for x in parsed):
        raise UsageError("--host-cmd must be a non-empty JSON array of strings")
    return parsed


def _prepare(args: argparse.Namespace) -> tuple[list[str], list[dict[str, str]], Path]:
    if not args.assume_isolated:
        raise UsageError("--assume-isolated is required (run inside the --network none container)")
    try:
        cs.check_saved_pages_url(args.url)
    except cs.SiteListError as exc:
        raise UsageError(str(exc)) from exc
    out = args.out_png
    if out.suffix != ".png":
        raise UsageError("OUT_PNG must end with .png")
    if out.parent.is_symlink():
        raise UsageError(f"refusing to write into a symlinked directory: {out.parent}")
    if not out.parent.is_dir():
        raise UsageError(f"output directory does not exist: {out.parent}")
    sites = load_selectors(args.selectors)
    site_id = args.site_id or out.stem
    elements = sites.get(site_id)
    if elements is None:
        print(
            f"warning: no selectors for site_id {site_id!r}; writing the PNG only "
            "(bbox will be not_measured)",
            file=sys.stderr,
        )
        elements = []
    return _host_command(args), elements, out


def main(argv: list[str] | None = None) -> int:
    args = build_arg_parser().parse_args(argv)
    try:
        host_cmd, elements, out = _prepare(args)
    except UsageError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    # 前回の出力を先に消す（失敗時に古い PNG・bbox が今回の結果として残らないようにする）。
    remove_outputs(out)
    previous = None
    try:
        # capture_screenshots の SIGTERM を後始末（ホストの kill）へ流す。
        previous = signal.signal(signal.SIGTERM, _raise_keyboard_interrupt)
    except ValueError:
        pass
    try:
        result = run_host(host_cmd, args.url, elements, args.load_timeout_ms)
        write_outputs(out, result)
    except HostError as exc:
        remove_outputs(out)
        print(f"error: {exc}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        remove_outputs(out)
        print("error: interrupted", file=sys.stderr)
        return 1
    finally:
        if previous is not None:
            signal.signal(signal.SIGTERM, previous)
    print(f"wrote {out} and {bbox_path_for(out)} ({len(result.elements)} elements)", file=sys.stderr)
    return 0


def _raise_keyboard_interrupt(signum: int, frame: Any) -> None:
    raise KeyboardInterrupt


if __name__ == "__main__":
    sys.exit(main())
