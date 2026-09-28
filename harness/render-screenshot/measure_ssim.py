#!/usr/bin/env python3
"""TASK-37.2（ビヘイビア `RENDER-5`。関連: `MEAS-3`）/ MS-1 対応。

`capture_screenshots.py`（#53・TASK-37.1）が書き出した `capture-result.json` と
両エンジンの PNG を入力とし、サイトごとに以下 2 つを数値で算出する。

- SSIM（構造的類似度。Wang et al. 2004・7x7 一様窓・母分散/母共分散）
- 境界ボックス一致率（エンジンごとに別途用意する `<engine>/<site_id>.bboxes.json`
  を読み、4 辺それぞれのずれが viewport 寸法の ±5% 以内かで判定）

RENDER-5 の判定基準（代表 5 サイト以上で SSIM 0.90 以上・境界ボックス一致率
80% 以上）と機械的に比較した結果を `<capture-dir>/measure-result.json` へ
書き出す。**この比較は閾値との機械的な比較に過ぎず、実機（Linux）で撮影した
PNG に対する RENDER-5 / MEAS-3 の最終合否判定ではない**（それは #55・
TASK-37.h1 で人間が担当する。REPAIR-3: 実装済みの判定機能を装わない）。

範囲外（本スクリプトが担わないもの。詳細は README「スコープ外・申し送り」）:

- 境界ボックスの抽出処理そのもの（Chromium の CDP `DOM.getBoxModel` 等・
  Servo 側の API。TASK-36/38 の成果次第）
- ±5%・80% という閾値の解釈と、SSIM 算出パラメータの確定（#55 で確認する）
- 16bit・パレット・インターレース PNG への対応

同じ `harness/render-screenshot/` ディレクトリの `capture_screenshots.py`
（以下 `cs`）が定義する定数・ヘルパーの一部（`SITE_ID_RE`・`MAX_PNG_*`・
`MIN_VIEWPORT`/`MAX_VIEWPORT`・`PNG_SIGNATURE`・`_write_bytes_nofollow` 等）を
再利用する。`_write_bytes_nofollow` は先頭が `_` の非公開関数だが、これは
crate 間の公開 API 契約ではなく同一 harness ディレクトリ内でのヘルパー再利用
であり、symlink 追随防止の実装を 2 箇所で別々に持たないための意図した再利用
である。
"""

from __future__ import annotations

import argparse
import json
import math
import re
import struct
import sys
import zlib
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

_THIS_DIR = Path(__file__).resolve().parent
if str(_THIS_DIR) not in sys.path:
    sys.path.insert(0, str(_THIS_DIR))
import capture_screenshots as cs  # noqa: E402  (同ディレクトリの兄弟モジュール)

# --- 定数 -------------------------------------------------------------------

RESULT_SCHEMA_VERSION = 1
REFERENCE_ENGINE = "chromium"
TARGET_ENGINE = "servo"

# RENDER-5 の判定基準（PoC-6 成功基準）。#55 で実機測定に基づき確認・確定する。
SSIM_MIN = 0.90
BBOX_RATE_MIN = 0.80
BBOX_TOLERANCE = 0.05
DEFAULT_MIN_SITES = 5

# 非信頼入力（capture-result.json・bbox JSON）のサイズ上限。無制限確保による
# DoS を防ぐ（security.md OWASP A04・coding-rust.md「長さ・件数の上限検証」）。
MAX_CAPTURE_RESULT_BYTES = 4 * 1024 * 1024
MAX_BBOX_FILE_BYTES = 1 * 1024 * 1024
MAX_BBOX_ELEMENTS = 100
MAX_BBOX_VALUE_ABS = 1e6
BBOX_ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$")

# 純 Python で現実的な時間に収まる画素数の上限（§2.5）。capture 側は
# 10000x10000 まで許すが、本スクリプトの SSIM 計算は O(画素数) の純 Python
# ループのため、既定 viewport（1280x800）に十分な余裕を持たせつつ実行時間が
# 破綻しない範囲に制限する。
MAX_SSIM_PIXELS = 2560 * 1600

SSIM_WINDOW = 7
_SSIM_K1 = 0.01
_SSIM_K2 = 0.03
_SSIM_L = 255
_SSIM_C1 = (_SSIM_K1 * _SSIM_L) ** 2
_SSIM_C2 = (_SSIM_K2 * _SSIM_L) ** 2

# color_type -> 1 ピクセルあたりのチャンネル数。§2.7 の対応範囲（8bit・
# 非インターレース・color type 0/2/4/6 のみ）に対応する。
_SUPPORTED_CHANNELS = {0: 1, 2: 3, 4: 2, 6: 4}


class MeasureError(Exception):
    """本スクリプト固有のエラーの基底クラス。"""


class CaptureResultError(MeasureError):
    """`capture-result.json` の読み込み・検証に失敗した場合に送出する。"""


class PngDecodeError(MeasureError):
    """PNG のデコードに失敗した場合に送出する。"""


class BboxError(MeasureError):
    """境界ボックス JSON（`<engine>/<site_id>.bboxes.json`）の読み込み・検証に失敗した場合に送出する。"""


# --- capture-result.json の読み込み -----------------------------------------


def _resolve_capture_png(capture_dir: Path, png_rel: Any) -> Path:
    """`captures[].png`（`capture_dir` からの相対パス）を検証して絶対パスへ解決する。

    パストラバーサル・symlink 経由での capture-dir 外参照を拒否する
    （security.md「プロファイル境界」と同じ考え方を capture-dir に適用）。
    """
    if not isinstance(png_rel, str) or not png_rel:
        raise CaptureResultError(f"'png' must be a non-empty string: {png_rel!r}")
    candidate = Path(png_rel)
    if candidate.is_absolute():
        raise CaptureResultError(f"'png' must be a relative path: {png_rel}")
    if any(part == ".." for part in candidate.parts):
        raise CaptureResultError(f"'png' must not contain '..': {png_rel}")
    full = capture_dir / candidate
    if full.is_symlink():
        raise CaptureResultError(f"refusing to follow symlink: {full}")
    resolved_capture_dir = capture_dir.resolve()
    if not full.resolve().is_relative_to(resolved_capture_dir):
        raise CaptureResultError(f"'png' escapes capture-dir: {png_rel}")
    return full


def load_capture_result(
    capture_dir: Path,
) -> tuple[dict[str, int], list[dict[str, Any]], list[dict[str, Any]]]:
    """`<capture_dir>/capture-result.json` を読み込み検証する。

    `capture_screenshots.write_result`（#53）が書いた JSON を入力契約とする。
    戻り値は `(viewport, pairs, skipped)`。`pairs` は両エンジンとも
    `status == "ok"` だったサイト（`site_id` 昇順）、`skipped` はペアに
    ならなかったサイト（理由付き・`site_id` 昇順）。
    ファイル全体が壊れている（JSON 不正・schema 不一致・viewport 不正）場合は
    `CaptureResultError` を送出し、呼び出し側（`main`）は終了コード 2 とする。
    個々の capture エントリの不備はそのサイトを `skipped` にするだけで、
    全体の読み込みは継続する（fail-closed だが 1 サイトの異常で全体を止めない）。
    """
    result_path = capture_dir / "capture-result.json"
    if result_path.is_symlink():
        raise CaptureResultError(f"refusing to follow symlink: {result_path}")
    try:
        size = result_path.stat().st_size
    except OSError as exc:
        raise CaptureResultError(f"failed to stat capture-result.json: {exc}") from exc
    if size > MAX_CAPTURE_RESULT_BYTES:
        raise CaptureResultError(
            f"capture-result.json exceeds the {MAX_CAPTURE_RESULT_BYTES} byte limit"
        )
    try:
        text = result_path.read_text(encoding="utf-8")
    except OSError as exc:
        raise CaptureResultError(f"failed to read capture-result.json: {exc}") from exc
    try:
        payload = json.loads(text)
    except json.JSONDecodeError as exc:
        raise CaptureResultError(f"invalid JSON in capture-result.json: {exc}") from exc

    if not isinstance(payload, dict) or payload.get("schema_version") != 1:
        raise CaptureResultError("capture-result.json: unsupported or missing schema_version")

    viewport = payload.get("viewport")
    if not isinstance(viewport, dict):
        raise CaptureResultError("capture-result.json: 'viewport' must be an object")
    vw = viewport.get("width")
    vh = viewport.get("height")
    if (
        not isinstance(vw, int)
        or isinstance(vw, bool)
        or not isinstance(vh, int)
        or isinstance(vh, bool)
        or not (cs.MIN_VIEWPORT <= vw <= cs.MAX_VIEWPORT)
        or not (cs.MIN_VIEWPORT <= vh <= cs.MAX_VIEWPORT)
    ):
        raise CaptureResultError("capture-result.json: invalid viewport width/height")
    viewport_out: dict[str, int] = {"width": vw, "height": vh}

    captures = payload.get("captures")
    if not isinstance(captures, list):
        raise CaptureResultError("capture-result.json: 'captures' must be a list")
    if len(captures) > cs.MAX_SITES * 2:
        raise CaptureResultError("capture-result.json: too many capture entries")

    by_site: dict[str, dict[str, dict[str, Any]]] = {}
    for entry in captures:
        if not isinstance(entry, dict):
            continue
        site_id = entry.get("site_id")
        engine = entry.get("engine")
        status = entry.get("status")
        # `site_id`/`engine` を辞書キーに使う前に文字列であることを確認する
        # （unhashable な値だと main 全体が落ちる。#53 レビュー指摘と同種）。
        if not isinstance(site_id, str) or not cs.SITE_ID_RE.match(site_id):
            continue
        if not isinstance(engine, str) or not isinstance(status, str):
            continue
        by_site.setdefault(site_id, {})[engine] = entry

    pairs: list[dict[str, Any]] = []
    skipped: list[dict[str, Any]] = []
    for site_id, by_engine in sorted(by_site.items()):
        ref_entry = by_engine.get(REFERENCE_ENGINE)
        tgt_entry = by_engine.get(TARGET_ENGINE)
        ref_ok = ref_entry is not None and ref_entry.get("status") == "ok"
        tgt_ok = tgt_entry is not None and tgt_entry.get("status") == "ok"
        if not (ref_ok and tgt_ok):
            ref_status = ref_entry.get("status") if ref_entry else "missing"
            tgt_status = tgt_entry.get("status") if tgt_entry else "missing"
            skipped.append(
                {
                    "site_id": site_id,
                    "reason": f"{REFERENCE_ENGINE}={ref_status}, {TARGET_ENGINE}={tgt_status}",
                }
            )
            continue
        try:
            ref_png = _resolve_capture_png(capture_dir, ref_entry.get("png"))
            tgt_png = _resolve_capture_png(capture_dir, tgt_entry.get("png"))
        except CaptureResultError as exc:
            skipped.append({"site_id": site_id, "reason": str(exc)})
            continue
        pairs.append({"site_id": site_id, "chromium_png": ref_png, "servo_png": tgt_png})

    return viewport_out, pairs, skipped


# --- PNG デコード（グレースケール輝度への変換） -------------------------------


def _read_png_chunks(path: Path) -> tuple[int, int, int, int, bytes]:
    """PNG のチャンク構造を検証しつつ IHDR フィールドと展開済み画素データを取り出す。

    `capture_screenshots.read_png_size`（撮影直後に一度検証済み）とは独立した
    2 回目の検証であり、1 回目の検証結果を信用しない（TOCTOU: 検証後にファイルが
    差し替えられている可能性があるため。coding-rust.md「外部入力」方針）。
    シグネチャ・チャンク長・CRC・チャンク数上限（`cs.MAX_PNG_CHUNKS`）・
    展開後サイズ上限（`cs.MAX_PNG_RAW_BYTES`）を自前で再確認する。

    戻り値は `(width, height, color_type, channels, unfiltered_pixels)`。
    `unfiltered_pixels` はフィルタバイトを除去した生ピクセルデータ
    （行ごとに `width * channels` バイト）。
    """
    try:
        size = path.stat().st_size
    except OSError as exc:
        raise PngDecodeError(f"failed to stat PNG file: {path}: {exc}") from exc
    if size > cs.MAX_PNG_BYTES:
        raise PngDecodeError(f"PNG file exceeds the {cs.MAX_PNG_BYTES} byte limit: {path}")
    if path.is_symlink():
        raise PngDecodeError(f"refusing to follow symlink: {path}")
    try:
        data = path.read_bytes()
    except OSError as exc:
        raise PngDecodeError(f"failed to read PNG file: {path}: {exc}") from exc

    if len(data) < len(cs.PNG_SIGNATURE) or data[: len(cs.PNG_SIGNATURE)] != cs.PNG_SIGNATURE:
        raise PngDecodeError(f"invalid PNG signature: {path}")

    width: int | None = None
    height: int | None = None
    bit_depth: int | None = None
    color_type: int | None = None
    interlace: int | None = None
    idat_chunks: list[bytes] = []
    offset = len(cs.PNG_SIGNATURE)
    chunk_count = 0
    seen_iend = False
    is_first = True
    while offset < len(data):
        chunk_count += 1
        if chunk_count > cs.MAX_PNG_CHUNKS:
            raise PngDecodeError(f"too many PNG chunks (> {cs.MAX_PNG_CHUNKS}): {path}")
        if offset + 8 > len(data):
            raise PngDecodeError(f"truncated PNG chunk header: {path}")
        (length,) = struct.unpack(">I", data[offset : offset + 4])
        ctype = data[offset + 4 : offset + 8]
        d_start = offset + 8
        d_end = d_start + length
        crc_end = d_end + 4
        if crc_end > len(data):
            raise PngDecodeError(f"truncated PNG chunk {ctype!r}: {path}")
        cdata = data[d_start:d_end]
        (stored_crc,) = struct.unpack(">I", data[d_end:crc_end])
        if (zlib.crc32(ctype + cdata) & 0xFFFFFFFF) != stored_crc:
            raise PngDecodeError(f"CRC mismatch in {ctype!r} chunk (corrupted PNG): {path}")

        if is_first and ctype != b"IHDR":
            raise PngDecodeError(f"first chunk after signature must be IHDR, got {ctype!r}: {path}")
        is_first = False

        if ctype == b"IHDR":
            if width is not None:
                raise PngDecodeError(f"duplicate IHDR chunk: {path}")
            if length != 13:
                raise PngDecodeError(f"IHDR chunk length must be exactly 13, got {length}: {path}")
            (width,) = struct.unpack(">I", cdata[0:4])
            (height,) = struct.unpack(">I", cdata[4:8])
            bit_depth = cdata[8]
            color_type = cdata[9]
            compression = cdata[10]
            filter_method = cdata[11]
            interlace = cdata[12]
            if width == 0 or height == 0:
                raise PngDecodeError(f"PNG has zero width or height: {path}")
            if compression != 0:
                raise PngDecodeError(f"unsupported PNG compression method: {path}")
            if filter_method != 0:
                raise PngDecodeError(f"unsupported PNG filter method: {path}")
            if interlace not in (0, 1):
                raise PngDecodeError(f"unsupported PNG interlace method: {path}")
        elif ctype == b"IDAT":
            if width is None:
                raise PngDecodeError(f"IDAT chunk before IHDR: {path}")
            idat_chunks.append(cdata)
        elif ctype == b"IEND":
            if length != 0:
                raise PngDecodeError(f"IEND chunk must be empty, got length {length}: {path}")
            seen_iend = True
            offset = crc_end
            break
        else:
            if ctype[0:1].isupper():
                raise PngDecodeError(f"unknown critical chunk {ctype!r}: {path}")
        offset = crc_end

    if width is None or height is None or bit_depth is None or color_type is None or interlace is None:
        raise PngDecodeError(f"missing IHDR chunk: {path}")
    if not seen_iend:
        raise PngDecodeError(f"missing IEND chunk (truncated PNG): {path}")
    if offset != len(data):
        raise PngDecodeError(f"trailing data after IEND chunk: {path}")
    if not idat_chunks:
        raise PngDecodeError(f"missing IDAT chunk (no pixel data): {path}")

    # §2.7: 対応範囲は 8bit・非インターレース・color type 0/2/4/6 のみ。
    # 16bit・パレット（type 3）・Adam7 は明示的に拒否する（黙って通さない）。
    if bit_depth != 8 or interlace != 0 or color_type not in _SUPPORTED_CHANNELS:
        raise PngDecodeError(
            f"unsupported PNG format for measure_ssim (bit_depth={bit_depth}, "
            f"color_type={color_type}, interlace={interlace}); only 8-bit "
            f"non-interlaced grayscale/RGB(+alpha) is supported: {path}"
        )
    channels = _SUPPORTED_CHANNELS[color_type]
    row_len = width * channels
    expected_raw = height * (1 + row_len)
    if expected_raw > cs.MAX_PNG_RAW_BYTES:
        raise PngDecodeError(
            f"IHDR declares a decompressed size exceeding the {cs.MAX_PNG_RAW_BYTES} "
            f"byte limit: {path}"
        )

    decompressor = zlib.decompressobj()
    decoded = bytearray()
    for chunk in idat_chunks:
        remaining = chunk
        while remaining:
            piece = decompressor.decompress(remaining, 65536)
            decoded += piece
            if len(decoded) > expected_raw:
                raise PngDecodeError(f"IDAT decompresses larger than the IHDR-derived size: {path}")
            remaining = decompressor.unconsumed_tail
    while not decompressor.eof:
        allowance = expected_raw - len(decoded) + 1
        if allowance <= 0:
            raise PngDecodeError(f"IDAT decompresses larger than the IHDR-derived size: {path}")
        piece = decompressor.decompress(b"", allowance)
        if not piece:
            raise PngDecodeError(f"truncated compressed stream (unexpected end of IDAT): {path}")
        decoded += piece
        if len(decoded) > expected_raw:
            raise PngDecodeError(f"IDAT decompresses larger than the IHDR-derived size: {path}")
    if len(decoded) != expected_raw:
        raise PngDecodeError(
            f"decompressed size mismatch (got {len(decoded)}, expected {expected_raw}): {path}"
        )

    pixels = _unfilter_scanlines(bytes(decoded), width, height, channels, path)
    return width, height, color_type, channels, pixels


def _paeth_predictor(a: int, b: int, c: int) -> int:
    """PNG 仕様の Paeth 予測子。`_unfilter_scanlines` のフィルタ 4 から呼ばれる。"""
    p = a + b - c
    pa = abs(p - a)
    pb = abs(p - b)
    pc = abs(p - c)
    if pa <= pb and pa <= pc:
        return a
    if pb <= pc:
        return b
    return c


def _unfilter_scanlines(raw: bytes, width: int, height: int, channels: int, path: Path) -> bytes:
    """展開済みバイト列（フィルタバイト付き走査行の連なり）を逆フィルタして画素データへ戻す。

    bit depth 8 前提のため 1 ピクセルあたりのバイト数（bpp）はそのまま
    `channels` に等しい（§2.7）。フィルタタイプ 0〜4（None/Sub/Up/Average/
    Paeth）に対応する。
    """
    row_len = width * channels
    stride = row_len + 1
    if len(raw) != stride * height:
        raise PngDecodeError(f"unexpected decompressed size for {width}x{height}: {path}")
    bpp = channels
    out = bytearray(row_len * height)
    prev = bytearray(row_len)
    pos = 0
    for y in range(height):
        filt = raw[pos]
        row = raw[pos + 1 : pos + 1 + row_len]
        cur = bytearray(row_len)
        if filt == 0:
            cur[:] = row
        elif filt == 1:
            for i in range(row_len):
                left = cur[i - bpp] if i >= bpp else 0
                cur[i] = (row[i] + left) & 0xFF
        elif filt == 2:
            for i in range(row_len):
                cur[i] = (row[i] + prev[i]) & 0xFF
        elif filt == 3:
            for i in range(row_len):
                left = cur[i - bpp] if i >= bpp else 0
                cur[i] = (row[i] + ((left + prev[i]) >> 1)) & 0xFF
        elif filt == 4:
            for i in range(row_len):
                left = cur[i - bpp] if i >= bpp else 0
                up = prev[i]
                upleft = prev[i - bpp] if i >= bpp else 0
                cur[i] = (row[i] + _paeth_predictor(left, up, upleft)) & 0xFF
        else:
            raise PngDecodeError(f"invalid scanline filter type {filt} (corrupted PNG): {path}")
        out[y * row_len : (y + 1) * row_len] = cur
        prev = cur
        pos += stride
    return bytes(out)


def decode_png_gray(path: Path) -> tuple[int, int, list[int]]:
    """PNG を読み、輝度（0〜255 の整数）の行優先配列へ変換する。

    グレースケール化は BT.601 の整数近似 `Y = (299R+587G+114B+500)//1000`
    （§2.5）。アルファチャンネルを持つ場合は不透明な白（255）の上に合成して
    から輝度を計算する（ブラウザのスクリーンショットは通常不透明）。
    """
    width, height, color_type, channels, pixels = _read_png_chunks(path)
    row_len = width * channels
    gray = [0] * (width * height)
    for y in range(height):
        row_off = y * row_len
        out_off = y * width
        for x in range(width):
            p = row_off + x * channels
            if color_type == 0:
                gray[out_off + x] = pixels[p]
            elif color_type == 2:
                r, g, b = pixels[p], pixels[p + 1], pixels[p + 2]
                gray[out_off + x] = (299 * r + 587 * g + 114 * b + 500) // 1000
            elif color_type == 4:
                g0, a = pixels[p], pixels[p + 1]
                gray[out_off + x] = (g0 * a + 255 * (255 - a)) // 255
            else:  # color_type == 6 (RGBA)
                r, g, b, a = pixels[p], pixels[p + 1], pixels[p + 2], pixels[p + 3]
                r2 = (r * a + 255 * (255 - a)) // 255
                g2 = (g * a + 255 * (255 - a)) // 255
                b2 = (b * a + 255 * (255 - a)) // 255
                gray[out_off + x] = (299 * r2 + 587 * g2 + 114 * b2 + 500) // 1000
    return width, height, gray


# --- SSIM --------------------------------------------------------------------


def _ssim_from_sums(
    sa: float, sb: float, saa: float, sbb: float, sab: float, n: int
) -> float:
    """7x7 窓の統計量（画素値の和・自乗和・積和）から 1 窓分の SSIM 値を計算する。"""
    mean_a = sa / n
    mean_b = sb / n
    var_a = saa / n - mean_a * mean_a
    var_b = sbb / n - mean_b * mean_b
    cov_ab = sab / n - mean_a * mean_b
    numerator = (2 * mean_a * mean_b + _SSIM_C1) * (2 * cov_ab + _SSIM_C2)
    denominator = (mean_a * mean_a + mean_b * mean_b + _SSIM_C1) * (var_a + var_b + _SSIM_C2)
    return numerator / denominator


def compute_ssim(w: int, h: int, a: list[int], b: list[int]) -> float:
    """`a`・`b`（幅 `w`・高さ `h` の輝度配列。行優先）の mean SSIM を計算する。

    Wang et al. 2004。7x7 一様窓・パディングなし（valid 窓のみ）・母分散/母共分散、
    K1=0.01・K2=0.03・L=255（§2.5）。窓ごとの統計量は列方向のスライディング和
    （高さ方向に 7 行分）を保ち、その上で横方向にもスライディング和を取ることで
    O(width*height) で計算する（画像全体の積分画像は持たない。メモリは幅に
    比例する定数で済む）。
    """
    win = SSIM_WINDOW
    if w < win or h < win:
        raise ValueError(f"image smaller than the {win}px SSIM window: {w}x{h}")
    n = win * win

    def idx(y: int, x: int) -> int:
        return y * w + x

    col_a = [0] * w
    col_b = [0] * w
    col_aa = [0] * w
    col_bb = [0] * w
    col_ab = [0] * w

    for y in range(win):
        for x in range(w):
            va = a[idx(y, x)]
            vb = b[idx(y, x)]
            col_a[x] += va
            col_b[x] += vb
            col_aa[x] += va * va
            col_bb[x] += vb * vb
            col_ab[x] += va * vb

    total = 0.0
    count = 0
    num_row_windows = h - win + 1
    for wy in range(num_row_windows):
        if wy > 0:
            out_y = wy - 1
            in_y = wy + win - 1
            for x in range(w):
                va_out = a[idx(out_y, x)]
                vb_out = b[idx(out_y, x)]
                va_in = a[idx(in_y, x)]
                vb_in = b[idx(in_y, x)]
                col_a[x] += va_in - va_out
                col_b[x] += vb_in - vb_out
                col_aa[x] += va_in * va_in - va_out * va_out
                col_bb[x] += vb_in * vb_in - vb_out * vb_out
                col_ab[x] += va_in * vb_in - va_out * vb_out

        s_a = sum(col_a[0:win])
        s_b = sum(col_b[0:win])
        s_aa = sum(col_aa[0:win])
        s_bb = sum(col_bb[0:win])
        s_ab = sum(col_ab[0:win])
        total += _ssim_from_sums(s_a, s_b, s_aa, s_bb, s_ab, n)
        count += 1
        for wx in range(1, w - win + 1):
            out_x = wx - 1
            in_x = wx + win - 1
            s_a += col_a[in_x] - col_a[out_x]
            s_b += col_b[in_x] - col_b[out_x]
            s_aa += col_aa[in_x] - col_aa[out_x]
            s_bb += col_bb[in_x] - col_bb[out_x]
            s_ab += col_ab[in_x] - col_ab[out_x]
            total += _ssim_from_sums(s_a, s_b, s_aa, s_bb, s_ab, n)
            count += 1

    return total / count


def _measure_ssim_pair(reference_png: Path, target_png: Path) -> dict[str, Any]:
    """基準（Chromium）・対象（Servo）の PNG ペアから SSIM を算出する。

    寸法不一致・デコード失敗・SSIM 窓に満たない画像・画素数上限超過は
    例外を外へ漏らさず `status: "error"` として返す（1 サイトの異常で
    実行全体を止めないため。§2.6 の設計制約）。
    """
    try:
        rw, rh, ref_gray = decode_png_gray(reference_png)
        tw, th, tgt_gray = decode_png_gray(target_png)
    except PngDecodeError as exc:
        return {"status": "error", "value": None, "passed": False, "reason": str(exc)}
    if (rw, rh) != (tw, th):
        return {
            "status": "error",
            "value": None,
            "passed": False,
            "reason": f"dimension mismatch: {REFERENCE_ENGINE} {rw}x{rh} vs {TARGET_ENGINE} {tw}x{th}",
        }
    if rw < SSIM_WINDOW or rh < SSIM_WINDOW:
        return {
            "status": "error",
            "value": None,
            "passed": False,
            "reason": f"image smaller than the {SSIM_WINDOW}px SSIM window: {rw}x{rh}",
        }
    if rw * rh > MAX_SSIM_PIXELS:
        return {
            "status": "error",
            "value": None,
            "passed": False,
            "reason": f"pixel count {rw * rh} exceeds the {MAX_SSIM_PIXELS} limit",
        }
    value = compute_ssim(rw, rh, ref_gray, tgt_gray)
    return {"status": "measured", "value": value, "passed": value >= SSIM_MIN, "reason": None}


# --- 境界ボックス比較 ----------------------------------------------------------


def _reject_non_finite_constant(token: str) -> float:
    """`json.loads(parse_constant=...)` に渡し、NaN/Infinity トークンを拒否する。"""
    raise BboxError(f"NaN/Infinity is not allowed in bbox JSON: {token}")


def load_bboxes(path: Path) -> dict[str, dict[str, float]] | None:
    """`<capture-dir>/<engine>/<site_id>.bboxes.json` を読み込み検証する。

    契約: `{"schema_version": 1, "elements": [{"id": ..., "x": ..., "y": ...,
    "width": ..., "height": ...}, ...]}`。ファイルが存在しない場合は `None`
    を返す（呼び出し側は `not_measured` として扱う）。symlink は拒否し、
    数値は `math.isfinite` と絶対値上限で検証する（生のトークンを Fraction /
    Decimal へ渡さない。指数表記による DoS を防ぐ。§2.6）。
    """
    if not path.exists():
        return None
    if path.is_symlink():
        raise BboxError(f"refusing to follow symlink: {path}")
    try:
        size = path.stat().st_size
    except OSError as exc:
        raise BboxError(f"failed to stat bbox file: {path}: {exc}") from exc
    if size > MAX_BBOX_FILE_BYTES:
        raise BboxError(f"bbox file exceeds the {MAX_BBOX_FILE_BYTES} byte limit: {path}")
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as exc:
        raise BboxError(f"failed to read bbox file: {path}: {exc}") from exc
    try:
        payload = json.loads(text, parse_constant=_reject_non_finite_constant)
    except (json.JSONDecodeError, ValueError) as exc:
        # 巨大な整数リテラル（`1e999999999` を整数として書いた場合等）は
        # `json` の桁数上限で `ValueError` になることがあるため、
        # `JSONDecodeError` と合わせて `BboxError` に変換する（§2.6）。
        raise BboxError(f"invalid bbox JSON: {path}: {exc}") from exc

    if not isinstance(payload, dict):
        raise BboxError(f"bbox JSON must be an object: {path}")
    if payload.get("schema_version") != 1:
        raise BboxError(f"unsupported or missing bbox schema_version: {path}")
    elements = payload.get("elements")
    if not isinstance(elements, list):
        raise BboxError(f"bbox JSON 'elements' must be a list: {path}")
    if len(elements) > MAX_BBOX_ELEMENTS:
        raise BboxError(
            f"bbox element count {len(elements)} exceeds the {MAX_BBOX_ELEMENTS} limit: {path}"
        )

    result: dict[str, dict[str, float]] = {}
    for element in elements:
        if not isinstance(element, dict):
            raise BboxError(f"bbox element must be an object: {path}")
        eid = element.get("id")
        if not isinstance(eid, str) or not BBOX_ID_RE.match(eid):
            raise BboxError(f"invalid bbox element id: {element.get('id')!r}: {path}")
        if eid in result:
            raise BboxError(f"duplicate bbox element id: {eid}: {path}")
        values: dict[str, float] = {}
        for key in ("x", "y", "width", "height"):
            raw_value = element.get(key)
            # bool は int のサブクラスなので isinstance(v, (int, float)) より先に弾く。
            if isinstance(raw_value, bool) or not isinstance(raw_value, (int, float)):
                raise BboxError(f"bbox element {eid!r}: {key} must be a number: {path}")
            value = float(raw_value)
            if not math.isfinite(value) or abs(value) > MAX_BBOX_VALUE_ABS:
                raise BboxError(f"bbox element {eid!r}: {key} out of range: {path}")
            values[key] = value
        if values["width"] < 0 or values["height"] < 0:
            raise BboxError(f"bbox element {eid!r}: width/height must be >= 0: {path}")
        result[eid] = values
    return result


def compare_bboxes(
    reference: dict[str, dict[str, float]] | None,
    target: dict[str, dict[str, float]] | None,
    viewport: dict[str, int],
) -> dict[str, Any]:
    """基準（Chromium）の境界ボックスを基準に、対象（Servo）との一致率を計算する。

    要素 1 件の一致判定は 4 辺（x, x+width, y, y+height）それぞれのずれが
    viewport 寸法（幅は x 軸、高さは y 軸）の ±5% 以内かで行う。ちょうど 5% は
    一致とし、浮動小数の誤差を `1e-9` の加算で吸収する（§2.4）。一致率は
    一致した要素数 / 基準側の要素数（Servo 側に無い id は不一致）。
    """
    if reference is None:
        return {
            "status": "not_measured",
            "matched": 0,
            "total": 0,
            "rate": None,
            "passed": False,
            "elements": [],
            "warnings": [],
            "reason": f"{REFERENCE_ENGINE} bbox file is missing",
        }
    if not reference:
        return {
            "status": "error",
            "matched": 0,
            "total": 0,
            "rate": None,
            "passed": False,
            "elements": [],
            "warnings": [],
            "reason": f"{REFERENCE_ENGINE} bbox has zero elements",
        }

    warnings: list[str] = []
    target = target or {}
    if not target:
        warnings.append(f"{TARGET_ENGINE} bbox file is missing or empty; treating all elements as unmatched")

    vw = viewport["width"]
    vh = viewport["height"]
    tol_x = vw * BBOX_TOLERANCE
    tol_y = vh * BBOX_TOLERANCE

    matched = 0
    elements: list[dict[str, Any]] = []
    for eid, ref_box in sorted(reference.items()):
        tgt_box = target.get(eid)
        if tgt_box is None:
            elements.append(
                {"id": eid, "max_deviation_ratio": None, "within_tolerance": False, "missing_in_target": True}
            )
            continue
        dx1 = abs(tgt_box["x"] - ref_box["x"])
        dx2 = abs((tgt_box["x"] + tgt_box["width"]) - (ref_box["x"] + ref_box["width"]))
        dy1 = abs(tgt_box["y"] - ref_box["y"])
        dy2 = abs((tgt_box["y"] + tgt_box["height"]) - (ref_box["y"] + ref_box["height"]))
        within = dx1 <= tol_x + 1e-9 and dx2 <= tol_x + 1e-9 and dy1 <= tol_y + 1e-9 and dy2 <= tol_y + 1e-9
        ratio_x = max(dx1, dx2) / vw
        ratio_y = max(dy1, dy2) / vh
        elements.append(
            {
                "id": eid,
                "max_deviation_ratio": max(ratio_x, ratio_y),
                "within_tolerance": within,
                "missing_in_target": False,
            }
        )
        if within:
            matched += 1

    extra_ids = sorted(set(target) - set(reference))
    if extra_ids:
        warnings.append(
            f"{TARGET_ENGINE} has {len(extra_ids)} element id(s) not present in {REFERENCE_ENGINE} "
            f"(ignored): {extra_ids}"
        )
    if not (5 <= len(reference) <= 10):
        warnings.append(f"reference has {len(reference)} elements; PoC-6 expects 5-10")

    total = len(reference)
    rate = matched / total
    return {
        "status": "measured",
        "matched": matched,
        "total": total,
        "rate": rate,
        "passed": rate >= BBOX_RATE_MIN,
        "elements": elements,
        "warnings": warnings,
        "reason": None,
    }


# --- サイト単位の計測・出力 ----------------------------------------------------


def measure_site(
    site_id: str, capture_dir: Path, chromium_png: Path, servo_png: Path, viewport: dict[str, int]
) -> dict[str, Any]:
    """1 サイトについて SSIM と境界ボックス一致率をそれぞれ独立に算出する。

    片方が `error`/`not_measured` でも、もう片方は独立して出力する。
    サイト合格は `ssim.passed and bbox.passed`（両方 `measured` かつ閾値以上）
    の場合のみで、`not_measured`・`error` は不合格として扱う（fail-closed）。
    """
    ssim_result = _measure_ssim_pair(chromium_png, servo_png)

    ref_bbox_path = capture_dir / REFERENCE_ENGINE / f"{site_id}.bboxes.json"
    tgt_bbox_path = capture_dir / TARGET_ENGINE / f"{site_id}.bboxes.json"
    try:
        ref_bboxes = load_bboxes(ref_bbox_path)
        tgt_bboxes = load_bboxes(tgt_bbox_path)
    except BboxError as exc:
        bbox_result: dict[str, Any] = {
            "status": "error",
            "matched": 0,
            "total": 0,
            "rate": None,
            "passed": False,
            "elements": [],
            "warnings": [],
            "reason": str(exc),
        }
    else:
        bbox_result = compare_bboxes(ref_bboxes, tgt_bboxes, viewport)

    passed = (
        ssim_result["status"] == "measured"
        and ssim_result["passed"]
        and bbox_result["status"] == "measured"
        and bbox_result["passed"]
    )
    return {
        "site_id": site_id,
        "status": "measured",
        "reason": None,
        "ssim": ssim_result,
        "bbox": bbox_result,
        "passed": passed,
    }


def write_result(capture_dir: Path, result: dict[str, Any]) -> Path:
    """計測結果を `<capture_dir>/measure-result.json` へ書き出す。

    `--out-dir` を使い回す再実行で本ファイルが外部ファイルへの symlink に
    差し替えられていた場合の上書きを防ぐため、`cs._write_bytes_nofollow` で
    追随せずに書き込む（`capture_screenshots.write_result` と同じ防御。
    モジュール docstring に記載のとおり意図した非公開ヘルパーの再利用）。
    """
    result_path = capture_dir / "measure-result.json"
    if result_path.is_symlink():
        raise CaptureResultError(f"refusing to overwrite existing symlink: {result_path}")
    text = json.dumps(result, ensure_ascii=False, indent=2, allow_nan=False) + "\n"
    try:
        cs._write_bytes_nofollow(result_path, text.encode("utf-8"))  # noqa: SLF001
    except OSError as exc:
        raise CaptureResultError(f"failed to write measure-result.json: {result_path}: {exc}") from exc
    return result_path


# --- CLI ----------------------------------------------------------------------


def _min_sites_type(raw: str) -> int:
    try:
        value = int(raw)
    except ValueError as exc:
        raise argparse.ArgumentTypeError(f"--min-sites must be an integer: {raw}") from exc
    if not (1 <= value <= cs.MAX_SITES):
        raise argparse.ArgumentTypeError(f"--min-sites must be in [1, {cs.MAX_SITES}]: {value}")
    return value


def build_arg_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=(
            "Compare Servo vs Chromium screenshots by SSIM and bounding-box overlap "
            "(TASK-37.2, RENDER-5); reads capture-result.json written by capture_screenshots.py."
        )
    )
    parser.add_argument(
        "--capture-dir",
        required=True,
        type=Path,
        help="directory containing capture-result.json (the --out-dir of capture_screenshots.py)",
    )
    parser.add_argument(
        "--min-sites",
        type=_min_sites_type,
        default=DEFAULT_MIN_SITES,
        help=f"minimum number of passing site pairs required for exit code 0 (default: {DEFAULT_MIN_SITES})",
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_arg_parser()
    args = parser.parse_args(argv)
    capture_dir: Path = args.capture_dir

    if not capture_dir.is_dir():
        print(f"error: --capture-dir is not a directory: {capture_dir}", file=sys.stderr)
        return 2

    try:
        viewport, pairs, skipped = load_capture_result(capture_dir)
    except CaptureResultError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2

    for entry in skipped:
        print(f"warning: site {entry['site_id']!r} skipped: {entry['reason']}", file=sys.stderr)

    sites_result: list[dict[str, Any]] = []
    for pair in pairs:
        site_result = measure_site(
            pair["site_id"], capture_dir, pair["chromium_png"], pair["servo_png"], viewport
        )
        for warning in site_result["bbox"].get("warnings", []):
            print(f"warning: site {pair['site_id']!r}: {warning}", file=sys.stderr)
        sites_result.append(site_result)
    for entry in skipped:
        sites_result.append(
            {
                "site_id": entry["site_id"],
                "status": "skipped",
                "reason": entry["reason"],
                "ssim": None,
                "bbox": None,
                "passed": False,
            }
        )
    sites_result.sort(key=lambda item: item["site_id"])

    passing = sum(1 for item in sites_result if item.get("passed"))
    verdict = "meets_threshold" if passing >= args.min_sites else "below_threshold"

    result: dict[str, Any] = {
        "schema_version": RESULT_SCHEMA_VERSION,
        "generated_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "reference_engine": REFERENCE_ENGINE,
        "target_engine": TARGET_ENGINE,
        "viewport": viewport,
        "thresholds": {
            "ssim_min": SSIM_MIN,
            "bbox_rate_min": BBOX_RATE_MIN,
            "bbox_tolerance": BBOX_TOLERANCE,
            "min_sites": args.min_sites,
        },
        "ssim_method": {
            "window": SSIM_WINDOW,
            "window_shape": "uniform",
            "padding": "none",
            "covariance": "population",
            "luma": "bt601-int",
            "alpha": "composite-over-white",
        },
        "sites": sites_result,
        "summary": {
            "pair_count": len(pairs),
            "passing_sites": passing,
            "verdict": verdict,
            "note": "mechanical threshold comparison; not a hardware-verified result (see issue #55)",
        },
    }

    try:
        write_result(capture_dir, result)
    except CaptureResultError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2

    print(f"{'site_id':<28} {'ssim':>8} {'bbox':>10} {'rate':>7} {'passed':>7}")
    for item in sites_result:
        ssim = item.get("ssim") or {}
        bbox = item.get("bbox") or {}
        ssim_str = f"{ssim.get('value'):.4f}" if ssim.get("value") is not None else "-"
        bbox_str = f"{bbox.get('matched')}/{bbox.get('total')}" if bbox.get("total") else "-"
        rate = bbox.get("rate")
        rate_str = f"{rate:.2f}" if rate is not None else "-"
        print(f"{item['site_id']:<28} {ssim_str:>8} {bbox_str:>10} {rate_str:>7} {str(item.get('passed')):>7}")
    print(
        f"pair_count={len(pairs)} passing_sites={passing} min_sites={args.min_sites} verdict={verdict}"
    )

    return 0 if passing >= args.min_sites else 1


if __name__ == "__main__":
    sys.exit(main())
