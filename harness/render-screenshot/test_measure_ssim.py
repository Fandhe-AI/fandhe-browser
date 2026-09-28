"""measure_ssim.py のユニットテスト・結合テスト（RENDER-5 / MEAS-3・TASK-37.2）。

`capture_screenshots.py` と同様、実機（Servo・Chromium）には依存せず、
合成 PNG・ダミーの bbox JSON を使ったオフラインテストで完結する。
実機での測定・RENDER-5 / MEAS-3 の最終合否判定は #55（TASK-37.h1）の範囲。
"""

from __future__ import annotations

import json
import math
import os
import random
import struct
import sys
import tempfile
import unittest
import zlib
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))

import capture_screenshots as cs  # noqa: E402
import measure_ssim as ms  # noqa: E402


# --- テスト用 PNG ビルダ -------------------------------------------------------


def _chunk(ctype: bytes, data: bytes) -> bytes:
    return struct.pack(">I", len(data)) + ctype + data + struct.pack(">I", zlib.crc32(ctype + data) & 0xFFFFFFFF)


def _filter_row(cur: list[int], prev: list[int], bpp: int, filter_type: int) -> bytes:
    """1 走査行（フィルタ適用前）を PNG のスキャンラインフィルタで符号化する。"""
    if filter_type == 0:
        return bytes(cur)
    out = bytearray(len(cur))
    if filter_type == 1:
        for i in range(len(cur)):
            left = cur[i - bpp] if i >= bpp else 0
            out[i] = (cur[i] - left) & 0xFF
    elif filter_type == 2:
        for i in range(len(cur)):
            out[i] = (cur[i] - prev[i]) & 0xFF
    elif filter_type == 3:
        for i in range(len(cur)):
            left = cur[i - bpp] if i >= bpp else 0
            out[i] = (cur[i] - ((left + prev[i]) >> 1)) & 0xFF
    elif filter_type == 4:
        for i in range(len(cur)):
            left = cur[i - bpp] if i >= bpp else 0
            up = prev[i]
            upleft = prev[i - bpp] if i >= bpp else 0
            out[i] = (cur[i] - ms._paeth_predictor(left, up, upleft)) & 0xFF
    else:
        raise ValueError(f"unsupported filter type for test PNG builder: {filter_type}")
    return bytes(out)


def build_png(
    width: int,
    height: int,
    pixel_fn,
    *,
    color_type: int = 2,
    bit_depth: int = 8,
    interlace: int = 0,
    filter_types: tuple[int, ...] = (0,),
    corrupt_ihdr_length: int | None = None,
    extra_idat_suffix: bytes = b"",
    trailing_garbage: bytes = b"",
) -> bytes:
    """テスト用に IHDR・1 個の IDAT・IEND から成る合成 PNG バイト列を組み立てる。

    `pixel_fn(x, y)` は `color_type` に応じたチャンネル数のタプルを返す
    （0: (gray,)、2: (r,g,b)、4: (gray,alpha)、6: (r,g,b,alpha)）。
    `filter_types` は行ごとに順番に（足りなければ繰り返し）適用するスキャンライン
    フィルタで、5 種類すべて（0〜4）を含めれば往復検証になる。
    """
    channels = {0: 1, 2: 3, 4: 2, 6: 4}[color_type]
    raw = bytearray()
    prev = [0] * (width * channels)
    for y in range(height):
        cur: list[int] = []
        for x in range(width):
            cur.extend(pixel_fn(x, y))
        ft = filter_types[y % len(filter_types)]
        raw.append(ft)
        raw.extend(_filter_row(cur, prev, channels, ft))
        prev = cur
    compressed = zlib.compress(bytes(raw)) + extra_idat_suffix
    ihdr_len = 13 if corrupt_ihdr_length is None else corrupt_ihdr_length
    ihdr = struct.pack(">IIBBBBB", width, height, bit_depth, color_type, 0, 0, interlace)
    if corrupt_ihdr_length is not None:
        ihdr = ihdr[: max(0, corrupt_ihdr_length)].ljust(corrupt_ihdr_length, b"\x00") if corrupt_ihdr_length >= 0 else ihdr
        ihdr_chunk = struct.pack(">I", len(ihdr)) + b"IHDR" + ihdr + struct.pack(">I", zlib.crc32(b"IHDR" + ihdr) & 0xFFFFFFFF)
    else:
        ihdr_chunk = _chunk(b"IHDR", ihdr)
    body = ihdr_chunk + _chunk(b"IDAT", compressed) + _chunk(b"IEND", b"")
    return cs.PNG_SIGNATURE + body + trailing_garbage


def rgb_pixel(x: int, y: int) -> tuple[int, int, int]:
    return (x * 17 % 256, y * 23 % 256, (x + y) * 5 % 256)


def gray_pixel(x: int, y: int) -> tuple[int]:
    return ((x * 11 + y * 7) % 256,)


class DecodePngGrayTest(unittest.TestCase):
    """RENDER-5 / TASK-37.2: `decode_png_gray`・`_read_png_chunks`（PNG デコード）。"""

    def test_roundtrip_all_filter_types_rgb(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "a.png"
            path.write_bytes(build_png(11, 13, rgb_pixel, color_type=2, filter_types=(0, 1, 2, 3, 4)))
            w, h, gray = ms.decode_png_gray(path)
            self.assertEqual((w, h), (11, 13))
            for y in range(h):
                for x in range(w):
                    r, g, b = rgb_pixel(x, y)
                    expected = (299 * r + 587 * g + 114 * b + 500) // 1000
                    self.assertEqual(gray[y * w + x], expected)

    def test_roundtrip_grayscale(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "a.png"
            path.write_bytes(build_png(9, 7, gray_pixel, color_type=0, filter_types=(0, 1, 2, 3, 4)))
            w, h, gray = ms.decode_png_gray(path)
            for y in range(h):
                for x in range(w):
                    self.assertEqual(gray[y * w + x], gray_pixel(x, y)[0])

    def test_alpha_composited_over_white_rgba(self) -> None:
        def pfa(x: int, y: int) -> tuple[int, int, int, int]:
            return (200, 100, 50, (x * 30 + y * 10) % 256)

        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "a.png"
            path.write_bytes(build_png(8, 9, pfa, color_type=6, filter_types=(0, 1, 2, 3, 4)))
            w, h, gray = ms.decode_png_gray(path)
            for y in range(h):
                for x in range(w):
                    r, g, b, a = pfa(x, y)
                    r2 = (r * a + 255 * (255 - a)) // 255
                    g2 = (g * a + 255 * (255 - a)) // 255
                    b2 = (b * a + 255 * (255 - a)) // 255
                    expected = (299 * r2 + 587 * g2 + 114 * b2 + 500) // 1000
                    self.assertEqual(gray[y * w + x], expected)

    def test_alpha_composited_over_white_gray_alpha(self) -> None:
        def pf(x: int, y: int) -> tuple[int, int]:
            return (60, (x * 25 + y * 5) % 256)

        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "a.png"
            path.write_bytes(build_png(8, 8, pf, color_type=4, filter_types=(0, 1, 2, 3, 4)))
            w, h, gray = ms.decode_png_gray(path)
            for y in range(h):
                for x in range(w):
                    g0, a = pf(x, y)
                    expected = (g0 * a + 255 * (255 - a)) // 255
                    self.assertEqual(gray[y * w + x], expected)

    def test_rejects_16bit(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "a.png"
            path.write_bytes(build_png(8, 8, rgb_pixel, color_type=2, bit_depth=16))
            with self.assertRaises(ms.PngDecodeError):
                ms.decode_png_gray(path)

    def test_rejects_palette(self) -> None:
        # `build_png` は測定対象の color type (0/2/4/6) しか作れないため、
        # パレット PNG（color type 3）はここで直接組み立てる。
        width, height = 8, 8
        plte = bytes([0, 0, 0, 255, 255, 255, 255, 0, 0, 0, 255, 0])  # 4 エントリ
        raw = bytearray()
        for _ in range(height):
            raw.append(0)
            raw.extend(bytes([x % 4 for x in range(width)]))
        ihdr = struct.pack(">IIBBBBB", width, height, 8, 3, 0, 0, 0)
        data = (
            cs.PNG_SIGNATURE
            + _chunk(b"IHDR", ihdr)
            + _chunk(b"PLTE", plte)
            + _chunk(b"IDAT", zlib.compress(bytes(raw)))
            + _chunk(b"IEND", b"")
        )
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "a.png"
            path.write_bytes(data)
            with self.assertRaises(ms.PngDecodeError):
                ms.decode_png_gray(path)

    def test_rejects_interlaced(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "a.png"
            path.write_bytes(build_png(8, 8, rgb_pixel, color_type=2, interlace=1))
            with self.assertRaises(ms.PngDecodeError):
                ms.decode_png_gray(path)

    def test_rejects_crc_corruption(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "a.png"
            data = bytearray(build_png(8, 8, rgb_pixel, color_type=2))
            # IDAT のペイロード中の 1 バイトを反転させ CRC 不一致を起こす。
            data[40] ^= 0xFF
            path.write_bytes(bytes(data))
            with self.assertRaises(ms.PngDecodeError):
                ms.decode_png_gray(path)

    def test_rejects_bad_ihdr_length(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "a.png"
            path.write_bytes(build_png(8, 8, rgb_pixel, color_type=2, corrupt_ihdr_length=12))
            with self.assertRaises(ms.PngDecodeError):
                ms.decode_png_gray(path)

    def test_rejects_missing_iend(self) -> None:
        ihdr = struct.pack(">IIBBBBB", 4, 4, 8, 2, 0, 0, 0)
        data = cs.PNG_SIGNATURE + _chunk(b"IHDR", ihdr) + _chunk(b"IDAT", zlib.compress(b"\x00" * (4 * (1 + 4 * 3))))
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "a.png"
            path.write_bytes(data)
            with self.assertRaises(ms.PngDecodeError):
                ms.decode_png_gray(path)

    def test_rejects_trailing_garbage_after_iend(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "a.png"
            path.write_bytes(build_png(4, 4, rgb_pixel, color_type=2, trailing_garbage=b"\x00"))
            with self.assertRaises(ms.PngDecodeError):
                ms.decode_png_gray(path)

    def test_rejects_pixel_count_over_ssim_limit_before_decompression(self) -> None:
        # RENDER-5 / codex・Bugbot 指摘（P0/Medium）: 画素数上限は IDAT の
        # 展開・配列確保より前、IHDR の寸法だけで検証されなければならない。
        # ここでは IDAT に検証を通過しないゴミバイト列を置き、それでも
        # （zlib エラーではなく）画素数上限の `PngDecodeError` で弾かれる
        # ことを確認する（=上限チェックが展開より先に効いている証拠）。
        big_w, big_h = 3000, 3000  # 9,000,000 > MAX_SSIM_PIXELS (2560*1600)
        ihdr = struct.pack(">IIBBBBB", big_w, big_h, 8, 2, 0, 0, 0)
        data = (
            cs.PNG_SIGNATURE
            + _chunk(b"IHDR", ihdr)
            + _chunk(b"IDAT", b"not a valid zlib stream")
            + _chunk(b"IEND", b"")
        )
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "a.png"
            path.write_bytes(data)
            with self.assertRaises(ms.PngDecodeError) as ctx:
                ms.decode_png_gray(path)
            self.assertIn("exceeds", str(ctx.exception))

    def test_rejects_corrupt_zlib_stream_without_crashing(self) -> None:
        # RENDER-5 / Cursor Bugbot 指摘（High）: IDAT の CRC は正しいが zlib
        # ストリームとして不正な場合、`zlib.error` が未処理のまま漏れず
        # `PngDecodeError` に変換されることを確認する。
        ihdr = struct.pack(">IIBBBBB", 4, 4, 8, 2, 0, 0, 0)
        data = (
            cs.PNG_SIGNATURE
            + _chunk(b"IHDR", ihdr)
            + _chunk(b"IDAT", b"\x78\x9c" + b"\xff" * 16)
            + _chunk(b"IEND", b"")
        )
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "a.png"
            path.write_bytes(data)
            with self.assertRaises(ms.PngDecodeError):
                ms.decode_png_gray(path)

    def test_rejects_decompression_bomb(self) -> None:
        # IHDR は巨大な寸法を宣言するが IDAT はごく小さい zlib ストリーム
        # （解凍爆弾の簡易再現）。展開後サイズ上限 `cs.MAX_PNG_RAW_BYTES` の
        # 手前で打ち切られることを確認する。
        huge_w = 60000
        huge_h = 60000
        ihdr = struct.pack(">IIBBBBB", huge_w, huge_h, 8, 2, 0, 0, 0)
        payload = zlib.compress(b"\x00" * 10_000_000)
        data = cs.PNG_SIGNATURE + _chunk(b"IHDR", ihdr) + _chunk(b"IDAT", payload) + _chunk(b"IEND", b"")
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "a.png"
            path.write_bytes(data)
            with self.assertRaises(ms.PngDecodeError):
                ms.decode_png_gray(path)

    def test_refuses_symlink(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            target = Path(tmp) / "real.png"
            target.write_bytes(build_png(4, 4, rgb_pixel, color_type=2))
            link = Path(tmp) / "link.png"
            try:
                link.symlink_to(target)
            except OSError:
                self.skipTest("symlink creation is not permitted in this environment")
            with self.assertRaises(ms.PngDecodeError):
                ms.decode_png_gray(link)

    def test_oversized_file_is_rejected_without_reading(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "a.png"
            path.write_bytes(build_png(4, 4, rgb_pixel, color_type=2))
            with mock.patch.object(Path, "stat") as stat_mock:
                stat_mock.return_value = mock.Mock(st_size=cs.MAX_PNG_BYTES + 1)
                with self.assertRaises(ms.PngDecodeError):
                    ms.decode_png_gray(path)


class ComputeSsimTest(unittest.TestCase):
    """RENDER-5 / MEAS-3: `compute_ssim`（積分計算の正しさが最重要）。"""

    @staticmethod
    def _naive_ssim(w: int, h: int, a: list[int], b: list[int]) -> float:
        """素朴な 4 重ループ実装（本実装のスライディング和のオフバイワンを検出する基準）。"""
        win = 7
        n = win * win
        total = 0.0
        count = 0
        for wy in range(h - win + 1):
            for wx in range(w - win + 1):
                sa = sb = saa = sbb = sab = 0
                for dy in range(win):
                    for dx in range(win):
                        va = a[(wy + dy) * w + (wx + dx)]
                        vb = b[(wy + dy) * w + (wx + dx)]
                        sa += va
                        sb += vb
                        saa += va * va
                        sbb += vb * vb
                        sab += va * vb
                ma = sa / n
                mb = sb / n
                vara = saa / n - ma * ma
                varb = sbb / n - mb * mb
                cov = sab / n - ma * mb
                c1 = (0.01 * 255) ** 2
                c2 = (0.03 * 255) ** 2
                num = (2 * ma * mb + c1) * (2 * cov + c2)
                den = (ma * ma + mb * mb + c1) * (vara + varb + c2)
                total += num / den
                count += 1
        return total / count

    def test_matches_naive_reference_on_random_images(self) -> None:
        rng = random.Random(1234)
        w, h = 23, 17
        a = [rng.randint(0, 255) for _ in range(w * h)]
        b = [rng.randint(0, 255) for _ in range(w * h)]
        got = ms.compute_ssim(w, h, a, b)
        expected = self._naive_ssim(w, h, a, b)
        self.assertAlmostEqual(got, expected, delta=1e-9)

    def test_matches_naive_reference_on_non_square_image(self) -> None:
        rng = random.Random(99)
        w, h = 31, 8
        a = [rng.randint(0, 255) for _ in range(w * h)]
        b = [rng.randint(0, 255) for _ in range(w * h)]
        got = ms.compute_ssim(w, h, a, b)
        expected = self._naive_ssim(w, h, a, b)
        self.assertAlmostEqual(got, expected, delta=1e-9)

    def test_identical_images_is_one(self) -> None:
        rng = random.Random(7)
        w, h = 12, 9
        a = [rng.randint(0, 255) for _ in range(w * h)]
        self.assertAlmostEqual(ms.compute_ssim(w, h, a, a), 1.0, delta=1e-12)

    def test_constant_images_match_closed_form(self) -> None:
        w, h = 10, 10
        a = [50] * (w * h)
        b = [60] * (w * h)
        got = ms.compute_ssim(w, h, a, b)
        c1 = (0.01 * 255) ** 2
        expected = (2 * 50 * 60 + c1) / (50 * 50 + 60 * 60 + c1)
        self.assertAlmostEqual(got, expected, delta=1e-9)

    def test_shifted_image_is_less_than_one(self) -> None:
        rng = random.Random(3)
        w, h = 15, 15
        a = [rng.randint(0, 255) for _ in range(w * h)]
        b = [0] * (w * h)
        for y in range(h):
            for x in range(w - 1):
                b[y * w + x] = a[y * w + x + 1]
        got = ms.compute_ssim(w, h, a, b)
        self.assertLess(got, 1.0)

    def test_too_small_image_raises(self) -> None:
        with self.assertRaises(ValueError):
            ms.compute_ssim(5, 5, [0] * 25, [0] * 25)


class MeasureSsimPairTest(unittest.TestCase):
    """RENDER-5 / TASK-37.2: `_measure_ssim_pair`（寸法不一致・上限超過の fail-closed）。"""

    def test_dimension_mismatch_is_error(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            p1 = Path(tmp) / "a.png"
            p2 = Path(tmp) / "b.png"
            p1.write_bytes(build_png(10, 10, rgb_pixel, color_type=2))
            p2.write_bytes(build_png(10, 12, rgb_pixel, color_type=2))
            result = ms._measure_ssim_pair(p1, p2, {"width": 10, "height": 10})
            self.assertEqual(result["status"], "error")
            self.assertIsNone(result["value"])
            self.assertFalse(result["passed"])

    def test_pixel_limit_exceeded_is_error(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            p1 = Path(tmp) / "a.png"
            p2 = Path(tmp) / "b.png"
            p1.write_bytes(build_png(10, 10, rgb_pixel, color_type=2))
            p2.write_bytes(build_png(10, 10, rgb_pixel, color_type=2))
            big_w, big_h = 3000, 3000  # 9,000,000 > MAX_SSIM_PIXELS (2560*1600)
            with mock.patch.object(
                ms, "decode_png_gray", return_value=(big_w, big_h, [0] * (big_w * big_h))
            ):
                result = ms._measure_ssim_pair(p1, p2, {"width": big_w, "height": big_h})
            self.assertEqual(result["status"], "error")
            self.assertIn("exceeds", result["reason"])

    def test_viewport_mismatch_is_error(self) -> None:
        # RENDER-5 / Bugbot 指摘: 両 PNG の寸法が一致していても、宣言された
        # viewport と食い違う場合は撮影条件不一致として error を返す。
        with tempfile.TemporaryDirectory() as tmp:
            p1 = Path(tmp) / "a.png"
            p2 = Path(tmp) / "b.png"
            p1.write_bytes(build_png(10, 10, rgb_pixel, color_type=2))
            p2.write_bytes(build_png(10, 10, rgb_pixel, color_type=2))
            result = ms._measure_ssim_pair(p1, p2, {"width": 20, "height": 20})
            self.assertEqual(result["status"], "error")
            self.assertIsNone(result["value"])
            self.assertFalse(result["passed"])
            self.assertIn("viewport", result["reason"])

    def test_measured_pass_and_fail(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            same_a = Path(tmp) / "a.png"
            same_b = Path(tmp) / "b.png"
            same_a.write_bytes(build_png(12, 12, rgb_pixel, color_type=2))
            same_b.write_bytes(build_png(12, 12, rgb_pixel, color_type=2))
            passing = ms._measure_ssim_pair(same_a, same_b, {"width": 12, "height": 12})
            self.assertEqual(passing["status"], "measured")
            self.assertTrue(passing["passed"])

            noisy_a = Path(tmp) / "c.png"
            noisy_b = Path(tmp) / "d.png"
            rng = random.Random(5)
            noisy_a.write_bytes(build_png(12, 12, rgb_pixel, color_type=2))
            noisy_b.write_bytes(
                build_png(12, 12, lambda x, y: tuple(rng.randint(0, 255) for _ in range(3)), color_type=2)
            )
            failing = ms._measure_ssim_pair(noisy_a, noisy_b, {"width": 12, "height": 12})
            self.assertEqual(failing["status"], "measured")
            self.assertFalse(failing["passed"])


class LoadBboxesTest(unittest.TestCase):
    """RENDER-5 / TASK-37.2: `load_bboxes`（bbox 入力契約の検証。§2.3・§2.6）。"""

    def _write(self, tmp: str, payload: dict) -> Path:
        path = Path(tmp) / "chromium" / "site.bboxes.json"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(payload), encoding="utf-8")
        return path

    def test_missing_file_returns_none(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            self.assertIsNone(ms.load_bboxes(Path(tmp) / "nope.bboxes.json"))

    def test_valid_file_parses(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = self._write(
                tmp,
                {
                    "schema_version": 1,
                    "elements": [{"id": "header", "x": 0, "y": 0, "width": 1280, "height": 64}],
                },
            )
            result = ms.load_bboxes(path)
            self.assertEqual(result, {"header": {"x": 0.0, "y": 0.0, "width": 1280.0, "height": 64.0}})

    def test_rejects_nan_and_infinity(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = self._write(tmp, {})
            path.write_text(
                '{"schema_version": 1, "elements": [{"id": "a", "x": NaN, "y": 0, "width": 1, "height": 1}]}',
                encoding="utf-8",
            )
            with self.assertRaises(ms.BboxError):
                ms.load_bboxes(path)

            path.write_text(
                '{"schema_version": 1, "elements": [{"id": "a", "x": Infinity, "y": 0, "width": 1, "height": 1}]}',
                encoding="utf-8",
            )
            with self.assertRaises(ms.BboxError):
                ms.load_bboxes(path)

    def test_rejects_huge_exponent_literal(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = self._write(tmp, {})
            path.write_text(
                '{"schema_version": 1, "elements": [{"id": "a", "x": 1e999999999, "y": 0, "width": 1, "height": 1}]}',
                encoding="utf-8",
            )
            with self.assertRaises(ms.BboxError):
                ms.load_bboxes(path)

    def test_rejects_huge_integer_literal(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = self._write(tmp, {})
            huge_int = "9" * 5000
            path.write_text(
                f'{{"schema_version": 1, "elements": [{{"id": "a", "x": {huge_int}, "y": 0, '
                '"width": 1, "height": 1}]}',
                encoding="utf-8",
            )
            with self.assertRaises(ms.BboxError):
                ms.load_bboxes(path)

    def test_rejects_bool_as_number(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = self._write(
                tmp,
                {"schema_version": 1, "elements": [{"id": "a", "x": True, "y": 0, "width": 1, "height": 1}]},
            )
            with self.assertRaises(ms.BboxError):
                ms.load_bboxes(path)

    def test_rejects_negative_width(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = self._write(
                tmp,
                {"schema_version": 1, "elements": [{"id": "a", "x": 0, "y": 0, "width": -1, "height": 1}]},
            )
            with self.assertRaises(ms.BboxError):
                ms.load_bboxes(path)

    def test_rejects_duplicate_id(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = self._write(
                tmp,
                {
                    "schema_version": 1,
                    "elements": [
                        {"id": "a", "x": 0, "y": 0, "width": 1, "height": 1},
                        {"id": "a", "x": 1, "y": 1, "width": 1, "height": 1},
                    ],
                },
            )
            with self.assertRaises(ms.BboxError):
                ms.load_bboxes(path)

    def test_rejects_invalid_id_pattern(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = self._write(
                tmp,
                {"schema_version": 1, "elements": [{"id": "-bad", "x": 0, "y": 0, "width": 1, "height": 1}]},
            )
            with self.assertRaises(ms.BboxError):
                ms.load_bboxes(path)

    def test_rejects_too_many_elements(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            elements = [
                {"id": f"e{i}", "x": 0, "y": 0, "width": 1, "height": 1} for i in range(ms.MAX_BBOX_ELEMENTS + 1)
            ]
            path = self._write(tmp, {"schema_version": 1, "elements": elements})
            with self.assertRaises(ms.BboxError):
                ms.load_bboxes(path)

    def test_rejects_oversized_file(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = self._write(tmp, {"schema_version": 1, "elements": []})
            with mock.patch.object(Path, "stat") as stat_mock:
                stat_mock.return_value = mock.Mock(st_size=ms.MAX_BBOX_FILE_BYTES + 1)
                with self.assertRaises(ms.BboxError):
                    ms.load_bboxes(path)

    def test_refuses_symlink(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            target = self._write(tmp, {"schema_version": 1, "elements": []})
            link = Path(tmp) / "link.bboxes.json"
            try:
                link.symlink_to(target)
            except OSError:
                self.skipTest("symlink creation is not permitted in this environment")
            with self.assertRaises(ms.BboxError):
                ms.load_bboxes(link)

    def test_rejects_invalid_utf8_without_crashing(self) -> None:
        # Cursor Bugbot 指摘（Medium）: `UnicodeDecodeError` を `BboxError`
        # へ変換せず素通りしていないことを確認する。
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "chromium" / "site.bboxes.json"
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"\xff\xfe\x00invalid-utf8")
            with self.assertRaises(ms.BboxError):
                ms.load_bboxes(path)

    def test_rejects_overflow_integer_value(self) -> None:
        # codex レビュー指摘（P1）: `x`/`y`/`width`/`height` が JSON の `int`
        # としては読み込める桁数（`json` の桁数上限未満）でも、`float()` 変換で
        # `OverflowError` を送出しうる巨大整数（400 桁）だと未処理例外で
        # `measure_site` の部分失敗処理を経由せず落ちていた。`BboxError` へ
        # 変換されることを確認する。
        with tempfile.TemporaryDirectory() as tmp:
            path = self._write(tmp, {})
            huge_int = "9" * 400
            path.write_text(
                f'{{"schema_version": 1, "elements": [{{"id": "a", "x": {huge_int}, "y": 0, '
                '"width": 1, "height": 1}]}',
                encoding="utf-8",
            )
            with self.assertRaises(ms.BboxError):
                ms.load_bboxes(path)

    def test_rejects_deeply_nested_json_without_crashing(self) -> None:
        # codex レビュー指摘（P1）: 深くネストした bbox JSON は `json.loads` が
        # `RecursionError` を送出しうるが、`BboxError` へ変換されず未処理例外
        # として計測プロセス全体を止めていた。
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "chromium" / "site.bboxes.json"
            path.parent.mkdir(parents=True, exist_ok=True)
            depth = 200_000
            path.write_text("[" * depth + "]" * depth, encoding="utf-8")
            with self.assertRaises(ms.BboxError):
                ms.load_bboxes(path)


class BboxIsStaleTest(unittest.TestCase):
    """RENDER-5 / TASK-37.2: `_bbox_is_stale`（Codex P1 対応。再撮影で PNG だけが
    更新された場合に古い bbox を混入させない mtime ヒューリスティック）。"""

    def test_bbox_older_than_png_is_stale(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            bbox_path = Path(tmp) / "site.bboxes.json"
            png_path = Path(tmp) / "site.png"
            bbox_path.write_text("{}", encoding="utf-8")
            png_path.write_bytes(b"png")
            # bbox 側の更新時刻を PNG より明確に古くする（再撮影で PNG だけが
            # 更新された状況を模す）。
            old = bbox_path.stat().st_mtime - 3600
            os.utime(bbox_path, (old, old))
            self.assertTrue(ms._bbox_is_stale(bbox_path, png_path))

    def test_bbox_newer_than_or_equal_to_png_is_not_stale(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            png_path = Path(tmp) / "site.png"
            bbox_path = Path(tmp) / "site.bboxes.json"
            png_path.write_bytes(b"png")
            bbox_path.write_text("{}", encoding="utf-8")
            self.assertFalse(ms._bbox_is_stale(bbox_path, png_path))

    def test_stat_failure_is_not_stale(self) -> None:
        # stat 自体の失敗（レース等）は `load_bboxes` 等の別経路が扱うため、
        # ここでは stale 扱いにしない（fail-closed の二重適用を避ける）。
        with tempfile.TemporaryDirectory() as tmp:
            missing = Path(tmp) / "does-not-exist.bboxes.json"
            png_path = Path(tmp) / "site.png"
            png_path.write_bytes(b"png")
            self.assertFalse(ms._bbox_is_stale(missing, png_path))


class CompareBboxesTest(unittest.TestCase):
    """RENDER-5 / TASK-37.2: `compare_bboxes`（±5% 判定・一致率。§2.4）。"""

    VIEWPORT = {"width": 1000, "height": 1000}

    def test_exact_match_is_matched(self) -> None:
        ref = {"a": {"x": 0, "y": 0, "width": 100, "height": 100}}
        tgt = {"a": {"x": 0, "y": 0, "width": 100, "height": 100}}
        result = ms.compare_bboxes(ref, tgt, self.VIEWPORT)
        self.assertEqual(result["matched"], 1)
        self.assertEqual(result["rate"], 1.0)

    def test_exactly_5_percent_off_is_matched(self) -> None:
        ref = {"a": {"x": 0, "y": 0, "width": 100, "height": 100}}
        tgt = {"a": {"x": 50, "y": 0, "width": 100, "height": 100}}  # 5% of viewport width = 50
        result = ms.compare_bboxes(ref, tgt, self.VIEWPORT)
        self.assertEqual(result["matched"], 1)
        self.assertTrue(result["elements"][0]["within_tolerance"])

    def test_slightly_over_5_percent_x_is_unmatched(self) -> None:
        ref = {"a": {"x": 0, "y": 0, "width": 100, "height": 100}}
        tgt = {"a": {"x": 51, "y": 0, "width": 100, "height": 100}}
        result = ms.compare_bboxes(ref, tgt, self.VIEWPORT)
        self.assertEqual(result["matched"], 0)
        self.assertFalse(result["elements"][0]["within_tolerance"])

    def test_slightly_over_5_percent_y_is_unmatched(self) -> None:
        ref = {"a": {"x": 0, "y": 0, "width": 100, "height": 100}}
        tgt = {"a": {"x": 0, "y": 51, "width": 100, "height": 100}}
        result = ms.compare_bboxes(ref, tgt, self.VIEWPORT)
        self.assertEqual(result["matched"], 0)

    def test_width_only_deviation_detected_via_right_edge(self) -> None:
        # x/y は一致していても width がずれていれば右辺 (x+width) のずれとして検出される。
        ref = {"a": {"x": 0, "y": 0, "width": 100, "height": 100}}
        tgt = {"a": {"x": 0, "y": 0, "width": 200, "height": 100}}
        result = ms.compare_bboxes(ref, tgt, self.VIEWPORT)
        self.assertEqual(result["matched"], 0)
        self.assertFalse(result["elements"][0]["within_tolerance"])

    def test_match_rate_8_of_10_passes(self) -> None:
        ref = {f"e{i}": {"x": 0, "y": 0, "width": 10, "height": 10} for i in range(10)}
        tgt = {f"e{i}": {"x": 0, "y": 0, "width": 10, "height": 10} for i in range(8)}
        for i in range(8, 10):
            tgt[f"e{i}"] = {"x": 900, "y": 900, "width": 10, "height": 10}
        result = ms.compare_bboxes(ref, tgt, self.VIEWPORT)
        self.assertEqual(result["matched"], 8)
        self.assertAlmostEqual(result["rate"], 0.8)
        self.assertTrue(result["passed"])

    def test_match_rate_7_of_10_fails(self) -> None:
        ref = {f"e{i}": {"x": 0, "y": 0, "width": 10, "height": 10} for i in range(10)}
        tgt = {f"e{i}": {"x": 0, "y": 0, "width": 10, "height": 10} for i in range(7)}
        for i in range(7, 10):
            tgt[f"e{i}"] = {"x": 900, "y": 900, "width": 10, "height": 10}
        result = ms.compare_bboxes(ref, tgt, self.VIEWPORT)
        self.assertEqual(result["matched"], 7)
        self.assertFalse(result["passed"])

    def test_id_missing_in_target_is_unmatched(self) -> None:
        ref = {"a": {"x": 0, "y": 0, "width": 10, "height": 10}, "b": {"x": 0, "y": 0, "width": 10, "height": 10}}
        tgt = {"a": {"x": 0, "y": 0, "width": 10, "height": 10}}
        result = ms.compare_bboxes(ref, tgt, self.VIEWPORT)
        self.assertEqual(result["matched"], 1)
        missing = [e for e in result["elements"] if e["id"] == "b"][0]
        self.assertTrue(missing["missing_in_target"])

    def test_extra_id_in_target_is_ignored_with_warning(self) -> None:
        ref = {"a": {"x": 0, "y": 0, "width": 10, "height": 10}}
        tgt = {"a": {"x": 0, "y": 0, "width": 10, "height": 10}, "extra": {"x": 0, "y": 0, "width": 1, "height": 1}}
        result = ms.compare_bboxes(ref, tgt, self.VIEWPORT)
        self.assertEqual(result["matched"], 1)
        self.assertTrue(any("extra" in w for w in result["warnings"]))

    def test_missing_target_file_is_not_measured_when_none(self) -> None:
        result = ms.compare_bboxes(None, None, self.VIEWPORT)
        self.assertEqual(result["status"], "not_measured")
        self.assertFalse(result["passed"])

    def test_empty_reference_is_error(self) -> None:
        result = ms.compare_bboxes({}, {}, self.VIEWPORT)
        self.assertEqual(result["status"], "error")


class LoadCaptureResultTest(unittest.TestCase):
    """RENDER-5 / TASK-37.2: `load_capture_result`（capture-result.json の検証・ペア判定）。"""

    def _write_result(self, capture_dir: Path, captures: list[dict], viewport: dict | None = None) -> None:
        payload = {
            "schema_version": 1,
            "generated_at": "2026-01-01T00:00:00Z",
            "viewport": viewport or {"width": 1280, "height": 800},
            "sites": [],
            "captures": captures,
        }
        (capture_dir / "capture-result.json").write_text(json.dumps(payload), encoding="utf-8")

    def _capture_entry(self, site_id: str, engine: str, status: str, png: str | None) -> dict:
        return {"site_id": site_id, "engine": engine, "status": status, "png": png}

    def test_pairs_both_ok_sites(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            capture_dir = Path(tmp)
            (capture_dir / "chromium").mkdir()
            (capture_dir / "servo").mkdir()
            (capture_dir / "chromium" / "a.png").write_bytes(b"x")
            (capture_dir / "servo" / "a.png").write_bytes(b"x")
            self._write_result(
                capture_dir,
                [
                    self._capture_entry("a", "chromium", "ok", "chromium/a.png"),
                    self._capture_entry("a", "servo", "ok", "servo/a.png"),
                ],
            )
            viewport, pairs, skipped = ms.load_capture_result(capture_dir)
            self.assertEqual(viewport, {"width": 1280, "height": 800})
            self.assertEqual(len(pairs), 1)
            self.assertEqual(pairs[0]["site_id"], "a")
            self.assertEqual(skipped, [])

    def test_one_engine_failed_is_skipped(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            capture_dir = Path(tmp)
            self._write_result(
                capture_dir,
                [
                    self._capture_entry("a", "chromium", "ok", "chromium/a.png"),
                    self._capture_entry("a", "servo", "failed", None),
                ],
            )
            viewport, pairs, skipped = ms.load_capture_result(capture_dir)
            self.assertEqual(pairs, [])
            self.assertEqual(len(skipped), 1)
            self.assertEqual(skipped[0]["site_id"], "a")

    def test_non_string_site_id_or_engine_does_not_crash(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            capture_dir = Path(tmp)
            self._write_result(
                capture_dir,
                [
                    {"site_id": ["not", "a", "string"], "engine": "chromium", "status": "ok", "png": "x.png"},
                    {"site_id": "a", "engine": 123, "status": "ok", "png": "x.png"},
                ],
            )
            viewport, pairs, skipped = ms.load_capture_result(capture_dir)
            self.assertEqual(pairs, [])

    def test_rejects_png_with_parent_traversal(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            capture_dir = Path(tmp)
            self._write_result(
                capture_dir,
                [
                    self._capture_entry("a", "chromium", "ok", "../outside.png"),
                    self._capture_entry("a", "servo", "ok", "servo/a.png"),
                ],
            )
            viewport, pairs, skipped = ms.load_capture_result(capture_dir)
            self.assertEqual(pairs, [])
            self.assertEqual(len(skipped), 1)

    def test_rejects_absolute_png_path(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            capture_dir = Path(tmp)
            self._write_result(
                capture_dir,
                [
                    self._capture_entry("a", "chromium", "ok", "/etc/passwd"),
                    self._capture_entry("a", "servo", "ok", "servo/a.png"),
                ],
            )
            viewport, pairs, skipped = ms.load_capture_result(capture_dir)
            self.assertEqual(pairs, [])

    def test_rejects_symlinked_png_path(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            # `outside` はテスト専用の一時ディレクトリ内に置く（capture_dir の
            # 外だが tempdir の外ではない）。システム一時ディレクトリ直下の
            # 固定名を使うと、並列実行される他のテスト・他ワークツリーの
            # 同時実行と衝突しうるため避ける。
            capture_dir = Path(tmp) / "cap"
            capture_dir.mkdir()
            outside = Path(tmp) / "outside_target.png"
            outside.write_bytes(b"x")
            (capture_dir / "chromium").mkdir()
            link = capture_dir / "chromium" / "a.png"
            try:
                link.symlink_to(outside)
            except OSError:
                self.skipTest("symlink creation is not permitted in this environment")
            (capture_dir / "servo").mkdir()
            (capture_dir / "servo" / "a.png").write_bytes(b"x")
            self._write_result(
                capture_dir,
                [
                    self._capture_entry("a", "chromium", "ok", "chromium/a.png"),
                    self._capture_entry("a", "servo", "ok", "servo/a.png"),
                ],
            )
            viewport, pairs, skipped = ms.load_capture_result(capture_dir)
            self.assertEqual(pairs, [])
            self.assertEqual(len(skipped), 1)

    def test_missing_schema_version_raises(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            capture_dir = Path(tmp)
            (capture_dir / "capture-result.json").write_text(json.dumps({"viewport": {}}), encoding="utf-8")
            with self.assertRaises(ms.CaptureResultError):
                ms.load_capture_result(capture_dir)

    def test_invalid_json_raises(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            capture_dir = Path(tmp)
            (capture_dir / "capture-result.json").write_text("{not json", encoding="utf-8")
            with self.assertRaises(ms.CaptureResultError):
                ms.load_capture_result(capture_dir)

    def test_rejects_invalid_utf8_without_crashing(self) -> None:
        # Cursor Bugbot 指摘（Medium）: capture-result.json 側も同様に
        # `UnicodeDecodeError` を `CaptureResultError` へ変換する。
        with tempfile.TemporaryDirectory() as tmp:
            capture_dir = Path(tmp)
            (capture_dir / "capture-result.json").write_bytes(b"\xff\xfe\x00invalid-utf8")
            with self.assertRaises(ms.CaptureResultError):
                ms.load_capture_result(capture_dir)

    def test_rejects_deeply_nested_json_without_crashing(self) -> None:
        # codex レビュー指摘（P1）: 深くネストした capture-result.json は
        # `json.loads` が `RecursionError` を送出しうるが、`CaptureResultError`
        # へ変換されず未処理例外として計測プロセス全体を止めていた。
        with tempfile.TemporaryDirectory() as tmp:
            capture_dir = Path(tmp)
            depth = 200_000
            (capture_dir / "capture-result.json").write_text(
                "[" * depth + "]" * depth, encoding="utf-8"
            )
            with self.assertRaises(ms.CaptureResultError):
                ms.load_capture_result(capture_dir)

    def test_invalid_viewport_raises(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            capture_dir = Path(tmp)
            self._write_result(capture_dir, [], viewport={"width": 0, "height": 800})
            with self.assertRaises(ms.CaptureResultError):
                ms.load_capture_result(capture_dir)

    def test_refuses_symlinked_capture_result(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            capture_dir = Path(tmp) / "dir"
            capture_dir.mkdir()
            outside = Path(tmp) / "outside.json"
            outside.write_text("{}", encoding="utf-8")
            link = capture_dir / "capture-result.json"
            try:
                link.symlink_to(outside)
            except OSError:
                self.skipTest("symlink creation is not permitted in this environment")
            with self.assertRaises(ms.CaptureResultError):
                ms.load_capture_result(capture_dir)


class WriteResultTest(unittest.TestCase):
    """RENDER-5 / TASK-37.2: `write_result`（symlink 追随防止。#53 と同じ問題）。"""

    def test_writes_normally(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            capture_dir = Path(tmp)
            result_path = ms.write_result(capture_dir, {"schema_version": 1})
            self.assertEqual(json.loads(result_path.read_text(encoding="utf-8"))["schema_version"], 1)

    def test_refuses_to_overwrite_existing_symlink(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            capture_dir = Path(tmp) / "out"
            capture_dir.mkdir()
            outside_target = Path(tmp) / "outside.json"
            outside_target.write_text("keep me", encoding="utf-8")
            link = capture_dir / "measure-result.json"
            try:
                link.symlink_to(outside_target)
            except OSError:
                self.skipTest("symlink creation is not permitted in this environment")
            with self.assertRaises(ms.CaptureResultError):
                ms.write_result(capture_dir, {"schema_version": 1})
            self.assertEqual(outside_target.read_text(encoding="utf-8"), "keep me")


class MainIntegrationTest(unittest.TestCase):
    """RENDER-5 / TASK-37.2: `main()`（結合テスト。終了コード・部分失敗時の継続）。"""

    def _make_site_pngs(self, capture_dir: Path, site_id: str, *, identical: bool = True, seed: int = 0) -> None:
        (capture_dir / "chromium").mkdir(parents=True, exist_ok=True)
        (capture_dir / "servo").mkdir(parents=True, exist_ok=True)
        chromium_png = build_png(12, 12, rgb_pixel, color_type=2)
        (capture_dir / "chromium" / f"{site_id}.png").write_bytes(chromium_png)
        if identical:
            servo_png = chromium_png
        else:
            rng = random.Random(seed)
            servo_png = build_png(12, 12, lambda x, y: tuple(rng.randint(0, 255) for _ in range(3)), color_type=2)
        (capture_dir / "servo" / f"{site_id}.png").write_bytes(servo_png)

    def _write_bboxes(self, capture_dir: Path, engine: str, site_id: str, elements: list[dict]) -> None:
        path = capture_dir / engine / f"{site_id}.bboxes.json"
        path.write_text(json.dumps({"schema_version": 1, "elements": elements}), encoding="utf-8")

    def _write_capture_result(self, capture_dir: Path, site_ids: list[str]) -> None:
        captures = []
        for site_id in site_ids:
            captures.append(
                {"site_id": site_id, "engine": "chromium", "status": "ok", "png": f"chromium/{site_id}.png"}
            )
            captures.append({"site_id": site_id, "engine": "servo", "status": "ok", "png": f"servo/{site_id}.png"})
        payload = {
            "schema_version": 1,
            "generated_at": "2026-01-01T00:00:00Z",
            # `_make_site_pngs` が生成する PNG は 12x12（テスト高速化のため）。
            # `_measure_ssim_pair` が PNG 寸法と viewport の一致を要求する
            # ようになったため（Bugbot 指摘）、ここも実際の PNG 寸法に揃える。
            "viewport": {"width": 12, "height": 12},
            "sites": [{"id": s, "url": f"https://example.invalid/{s}", "category": "static", "catalog_id": s} for s in site_ids],
            "captures": captures,
        }
        (capture_dir / "capture-result.json").write_text(json.dumps(payload), encoding="utf-8")

    def test_all_sites_passing_exits_zero(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            capture_dir = Path(tmp)
            site_ids = [f"s{i}" for i in range(5)]
            for site_id in site_ids:
                self._make_site_pngs(capture_dir, site_id, identical=True)
                elements = [{"id": f"e{i}", "x": 0, "y": 0, "width": 10, "height": 10} for i in range(5)]
                self._write_bboxes(capture_dir, "chromium", site_id, elements)
                self._write_bboxes(capture_dir, "servo", site_id, elements)
            self._write_capture_result(capture_dir, site_ids)

            exit_code = ms.main(["--capture-dir", str(capture_dir), "--min-sites", "5"])
            self.assertEqual(exit_code, 0)
            result = json.loads((capture_dir / "measure-result.json").read_text(encoding="utf-8"))
            self.assertEqual(result["summary"]["passing_sites"], 5)
            self.assertEqual(result["summary"]["verdict"], "meets_threshold")

    def test_below_min_sites_exits_one(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            capture_dir = Path(tmp)
            site_ids = [f"s{i}" for i in range(5)]
            for idx, site_id in enumerate(site_ids):
                identical = idx < 4
                self._make_site_pngs(capture_dir, site_id, identical=identical, seed=idx)
                elements = [{"id": f"e{i}", "x": 0, "y": 0, "width": 10, "height": 10} for i in range(5)]
                self._write_bboxes(capture_dir, "chromium", site_id, elements)
                if identical:
                    self._write_bboxes(capture_dir, "servo", site_id, elements)
                else:
                    self._write_bboxes(capture_dir, "servo", site_id, [])
            self._write_capture_result(capture_dir, site_ids)

            exit_code = ms.main(["--capture-dir", str(capture_dir), "--min-sites", "5"])
            self.assertEqual(exit_code, 1)
            result = json.loads((capture_dir / "measure-result.json").read_text(encoding="utf-8"))
            self.assertEqual(result["summary"]["passing_sites"], 4)
            self.assertEqual(result["summary"]["verdict"], "below_threshold")

    def test_stale_bbox_from_previous_capture_does_not_pass(self) -> None:
        # Codex P1 対応: 撮影側は再実行時に PNG を削除するが bbox JSON は
        # 削除しないため、同じ capture-dir で再撮影すると今回の PNG と前回の
        # bbox を突き合わせてしまう可能性があった。bbox のファイル更新時刻が
        # 対応する PNG より古ければ stale として不合格にすることを確認する。
        with tempfile.TemporaryDirectory() as tmp:
            capture_dir = Path(tmp)
            site_id = "s0"
            (capture_dir / "chromium").mkdir(parents=True, exist_ok=True)
            (capture_dir / "servo").mkdir(parents=True, exist_ok=True)
            elements = [{"id": f"e{i}", "x": 0, "y": 0, "width": 10, "height": 10} for i in range(5)]
            self._write_bboxes(capture_dir, "chromium", site_id, elements)
            self._write_bboxes(capture_dir, "servo", site_id, elements)
            # bbox 側の更新時刻を明確に過去へ巻き戻し、「前回撮影時の bbox」を模す。
            old = (capture_dir / "chromium" / f"{site_id}.bboxes.json").stat().st_mtime - 3600
            os.utime(capture_dir / "chromium" / f"{site_id}.bboxes.json", (old, old))
            os.utime(capture_dir / "servo" / f"{site_id}.bboxes.json", (old, old))

            # bbox より後に（＝再撮影として）PNG を書き出す。SSIM は一致するよう
            # `identical=True` にし、境界ボックスの stale 判定だけを見る。
            self._make_site_pngs(capture_dir, site_id, identical=True)
            self._write_capture_result(capture_dir, [site_id])

            exit_code = ms.main(["--capture-dir", str(capture_dir), "--min-sites", "1"])
            self.assertEqual(exit_code, 1)
            result = json.loads((capture_dir / "measure-result.json").read_text(encoding="utf-8"))
            site_result = result["sites"][0]
            self.assertFalse(site_result["passed"])
            self.assertEqual(site_result["bbox"]["status"], "error")
            self.assertIn("stale", site_result["bbox"]["reason"])

    def test_invalid_capture_result_exits_two(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            capture_dir = Path(tmp)
            (capture_dir / "capture-result.json").write_text('{"schema_version": 2}', encoding="utf-8")
            exit_code = ms.main(["--capture-dir", str(capture_dir)])
            self.assertEqual(exit_code, 2)

    def test_missing_capture_dir_exits_two(self) -> None:
        exit_code = ms.main(["--capture-dir", "/nonexistent/does-not-exist"])
        self.assertEqual(exit_code, 2)

    def test_one_corrupt_png_does_not_stop_other_sites(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            capture_dir = Path(tmp)
            site_ids = ["good", "bad"]
            self._make_site_pngs(capture_dir, "good", identical=True)
            elements = [{"id": f"e{i}", "x": 0, "y": 0, "width": 10, "height": 10} for i in range(5)]
            self._write_bboxes(capture_dir, "chromium", "good", elements)
            self._write_bboxes(capture_dir, "servo", "good", elements)

            (capture_dir / "chromium").mkdir(parents=True, exist_ok=True)
            (capture_dir / "servo").mkdir(parents=True, exist_ok=True)
            (capture_dir / "chromium" / "bad.png").write_bytes(b"not a png")
            (capture_dir / "servo" / "bad.png").write_bytes(b"not a png")

            self._write_capture_result(capture_dir, site_ids)
            exit_code = ms.main(["--capture-dir", str(capture_dir), "--min-sites", "1"])
            self.assertEqual(exit_code, 0)
            result = json.loads((capture_dir / "measure-result.json").read_text(encoding="utf-8"))
            by_site = {s["site_id"]: s for s in result["sites"]}
            self.assertEqual(by_site["good"]["ssim"]["status"], "measured")
            self.assertEqual(by_site["bad"]["ssim"]["status"], "error")

    def test_missing_bbox_file_is_not_measured_and_fails_site(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            capture_dir = Path(tmp)
            self._make_site_pngs(capture_dir, "s0", identical=True)
            self._write_capture_result(capture_dir, ["s0"])
            exit_code = ms.main(["--capture-dir", str(capture_dir), "--min-sites", "1"])
            self.assertEqual(exit_code, 1)
            result = json.loads((capture_dir / "measure-result.json").read_text(encoding="utf-8"))
            site = result["sites"][0]
            self.assertEqual(site["bbox"]["status"], "not_measured")
            self.assertFalse(site["passed"])


if __name__ == "__main__":
    unittest.main()
