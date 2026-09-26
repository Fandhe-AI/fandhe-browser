"""capture_screenshots.py のユニットテスト・結合テスト（RENDER-5 / TASK-37.1）。

偽エンジン（fixtures/fake_engine.py）を使い、ネットワークにも実エンジンにも
依存せずオフラインで完結する。実機での撮影可否は #55（TASK-37.h1）の範囲。
"""

from __future__ import annotations

import io
import json
import os
import struct
import sys
import tempfile
import threading
import time
import unittest
import urllib.request
import zlib
from pathlib import Path
from unittest import mock
from urllib.parse import urlparse

sys.path.insert(0, str(Path(__file__).resolve().parent))

import capture_screenshots as cs  # noqa: E402

FIXTURES_DIR = Path(__file__).resolve().parent / "fixtures"
FAKE_ENGINE = FIXTURES_DIR / "fake_engine.py"
SITE_CONDITIONAL_ENGINE = FIXTURES_DIR / "site_conditional_engine.py"
DEFAULT_SITES_PATH = Path(__file__).resolve().parent / "sites.json"


def fake_engine_template(mode: str = "ok") -> list[str]:
    # `--proxy {proxy}` を含めることで、`main()` が起動する実際のローカル転送
    # プロキシがあっても `capture_one` の fail-closed チェック（codex P0 再指摘。
    # `{proxy}` を使わないテンプレートは既定で撮影を拒否する）を通過させる。
    # 偽エンジン自身は値を使わず無視する。
    return [
        sys.executable,
        str(FAKE_ENGINE),
        "--mode",
        mode,
        "--out",
        "{out}",
        "--width",
        "{width}",
        "--height",
        "{height}",
        "--proxy",
        "{proxy}",
    ]


def write_sites_json(path: Path, sites: list[dict], *, schema_version: int = 1, viewport: dict | None = None) -> None:
    payload = {
        "schema_version": schema_version,
        "viewport": viewport or {"width": 1280, "height": 800},
        "sites": sites,
    }
    path.write_text(json.dumps(payload), encoding="utf-8")


def make_site(site_id: str = "site-a", url: str = "https://example.invalid/a") -> dict:
    return {"id": site_id, "url": url, "category": "static", "catalog_id": "z1"}


class LoadSitesTest(unittest.TestCase):
    """RENDER-5 / TASK-37.1: サイト一覧 JSON の検証。"""

    def test_default_sites_file_has_at_least_five_https_sites_with_unique_ids(self) -> None:
        viewport, sites = cs.load_sites(DEFAULT_SITES_PATH, min_sites=5)
        self.assertGreaterEqual(len(sites), 5)
        self.assertEqual(viewport, {"width": 1280, "height": 800})
        ids = [s.site_id for s in sites]
        self.assertEqual(len(ids), len(set(ids)))
        for site in sites:
            self.assertTrue(site.url.startswith("https://"))

    def test_rejects_path_traversal_style_id(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "sites.json"
            write_sites_json(path, [make_site(site_id="../evil")] * 1 + [make_site(f"ok-{i}") for i in range(5)])
            with self.assertRaises(cs.SiteListError):
                cs.load_sites(path, min_sites=5)

    def test_rejects_uppercase_id_to_avoid_case_insensitive_collision(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "sites.json"
            write_sites_json(path, [make_site(site_id="A/B")] + [make_site(f"ok-{i}") for i in range(5)])
            with self.assertRaises(cs.SiteListError):
                cs.load_sites(path, min_sites=5)

    def test_rejects_empty_id(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "sites.json"
            write_sites_json(path, [make_site(site_id="")] + [make_site(f"ok-{i}") for i in range(5)])
            with self.assertRaises(cs.SiteListError):
                cs.load_sites(path, min_sites=5)

    def test_rejects_file_scheme_url(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "sites.json"
            sites = [make_site(f"ok-{i}") for i in range(5)]
            sites[0]["url"] = "file:///etc/passwd"
            write_sites_json(path, sites)
            with self.assertRaises(cs.SiteListError):
                cs.load_sites(path, min_sites=5)

    def test_rejects_javascript_scheme_url(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "sites.json"
            sites = [make_site(f"ok-{i}") for i in range(5)]
            sites[0]["url"] = "javascript:alert(1)"
            write_sites_json(path, sites)
            with self.assertRaises(cs.SiteListError):
                cs.load_sites(path, min_sites=5)

    def test_rejects_too_many_sites(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "sites.json"
            sites = [make_site(f"ok-{i}") for i in range(51)]
            write_sites_json(path, sites)
            with self.assertRaises(cs.SiteListError):
                cs.load_sites(path, min_sites=5)

    def test_rejects_duplicate_ids(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "sites.json"
            sites = [make_site("dup") for _ in range(2)] + [make_site(f"ok-{i}") for i in range(4)]
            write_sites_json(path, sites)
            with self.assertRaises(cs.SiteListError):
                cs.load_sites(path, min_sites=5)

    def test_rejects_fewer_than_min_sites(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "sites.json"
            write_sites_json(path, [make_site(f"ok-{i}") for i in range(3)])
            with self.assertRaises(cs.SiteListError):
                cs.load_sites(path, min_sites=5)

    def test_rejects_file_scheme_url_even_with_allow_file_url_false(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "sites.json"
            sites = [make_site(f"ok-{i}") for i in range(5)]
            sites[0]["url"] = "file:///tmp/fixture.html"
            write_sites_json(path, sites)
            with self.assertRaises(cs.SiteListError):
                cs.load_sites(path, min_sites=5, allow_file_url=False)

    def test_accepts_file_scheme_url_when_allow_file_url_true(self) -> None:
        # codex P2: `--allow-file-url` を指定しても `load_sites` が https 限定の
        # ままだと、README が案内するテスト用フラグが常に拒否されて機能しなかった。
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "sites.json"
            sites = [make_site(f"ok-{i}") for i in range(5)]
            sites[0]["url"] = "file:///tmp/fixture.html"
            write_sites_json(path, sites)
            _viewport, loaded_sites = cs.load_sites(path, min_sites=5, allow_file_url=True)
            self.assertEqual(loaded_sites[0].url, "file:///tmp/fixture.html")

    def test_rejects_out_of_range_viewport(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "sites.json"
            write_sites_json(
                path,
                [make_site(f"ok-{i}") for i in range(5)],
                viewport={"width": 0, "height": 800},
            )
            with self.assertRaises(cs.SiteListError):
                cs.load_sites(path, min_sites=5)


class CountCommonOkSitesTest(unittest.TestCase):
    """RENDER-5 / TASK-37.1: `_count_common_ok_sites`（両エンジン共通 ok 件数。codex P1）。

    エンジンごとに独立した ok 件数ではなく、同じ site_id で全エンジンとも
    "ok" だった件数を数える必要がある（片方だけ成功したサイトは比較できない）。
    """

    @staticmethod
    def _record(site_id: str, engine: str, status: str) -> dict:
        return {"site_id": site_id, "engine": engine, "status": status}

    def test_counts_only_sites_ok_on_every_engine(self) -> None:
        # servo: site-1..5 が ok / chromium: site-2..6 が ok
        # → 両エンジンで共通して ok なのは site-2..5 の 4 件。
        captures = [self._record(f"site-{i}", "servo", "ok") for i in range(1, 6)]
        captures += [self._record(f"site-{i}", "chromium", "ok") for i in range(2, 7)]
        self.assertEqual(cs._count_common_ok_sites(captures, ["servo", "chromium"]), 4)

    def test_non_ok_status_does_not_count(self) -> None:
        captures = [
            self._record("site-1", "servo", "ok"),
            self._record("site-1", "chromium", "failed"),
        ]
        self.assertEqual(cs._count_common_ok_sites(captures, ["servo", "chromium"]), 0)

    def test_single_engine_matches_its_own_ok_count(self) -> None:
        # `--engines chromium` のような単一エンジン実行では、従来通りそのエンジン
        # の ok 件数そのものになる（後方互換）。
        captures = [self._record(f"site-{i}", "chromium", "ok") for i in range(1, 4)]
        self.assertEqual(cs._count_common_ok_sites(captures, ["chromium"]), 3)

    def test_no_captures_yields_zero(self) -> None:
        self.assertEqual(cs._count_common_ok_sites([], ["servo", "chromium"]), 0)


class ExpandTemplateTest(unittest.TestCase):
    """RENDER-5 / TASK-37.1: テンプレート展開（argv 単位の置換・shell=True 不使用）。"""

    def test_expands_placeholders_within_single_argv_element(self) -> None:
        result = cs.expand_template(
            ["engine", "--url={url}", "--out={out}"],
            {"url": "https://example.invalid/a b", "out": "/tmp/x.png"},
        )
        self.assertEqual(result, ["engine", "--url=https://example.invalid/a b", "--out=/tmp/x.png"])

    def test_url_with_whitespace_stays_one_argv_element(self) -> None:
        result = cs.expand_template(["engine", "{url}"], {"url": "https://example.invalid/a b;rm -rf"})
        self.assertEqual(len(result), 2)
        self.assertEqual(result[1], "https://example.invalid/a b;rm -rf")

    def test_unknown_placeholder_raises(self) -> None:
        with self.assertRaises(cs.TemplateError):
            cs.expand_template(["engine", "{unknown}"], {"url": "https://example.invalid"})

    def test_user_data_dir_differs_per_call(self) -> None:
        argv1 = cs.expand_template(["--dir={user_data_dir}"], {"user_data_dir": "/tmp/a"})
        argv2 = cs.expand_template(["--dir={user_data_dir}"], {"user_data_dir": "/tmp/b"})
        self.assertNotEqual(argv1, argv2)


def _build_png(
    width: int,
    height: int,
    idat_payloads: list[bytes],
    *,
    bit_depth: int = 8,
    color_type: int = 2,
    compression: int = 0,
    filter_method: int = 0,
    interlace: int = 0,
) -> bytes:
    """テスト用に IHDR・任意個の IDAT・IEND から成る PNG バイト列を組み立てる。

    `fake_engine._chunk` を再利用して各チャンクの CRC を正しく計算し、
    `read_png_size` の IDAT 展開検証（codex P1）を IHDR フィールド・IDAT の
    個数や内容を変えながら検証できるようにする。
    """
    sys.path.insert(0, str(FIXTURES_DIR))
    import fake_engine  # noqa: PLC0415

    ihdr = struct.pack(">IIBBBBB", width, height, bit_depth, color_type, compression, filter_method, interlace)
    chunks = [fake_engine._chunk(b"IHDR", ihdr)]  # noqa: SLF001
    for payload in idat_payloads:
        chunks.append(fake_engine._chunk(b"IDAT", payload))  # noqa: SLF001
    chunks.append(fake_engine._chunk(b"IEND", b""))  # noqa: SLF001
    return cs.PNG_SIGNATURE + b"".join(chunks)


def _raw_pixels(width: int, height: int, *, channels: int = 3) -> bytes:
    """`write_minimal_png` と同じ形式（filter byte 0 + 単色ピクセル行）の生データを作る。"""
    pixel_row = bytes([200] * channels * width)
    return b"".join(bytes([0]) + pixel_row for _ in range(height))


class ReadPngSizeTest(unittest.TestCase):
    """RENDER-5 / TASK-37.1: PNG ヘッダ検証（シグネチャ・IHDR・IDAT 展開）。"""

    def test_reads_width_and_height_from_minimal_png(self) -> None:
        sys.path.insert(0, str(FIXTURES_DIR))
        import fake_engine  # noqa: PLC0415

        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "x.png"
            fake_engine.write_minimal_png(path, 37, 41)
            width, height = cs.read_png_size(path)
            self.assertEqual((width, height), (37, 41))

    def test_rejects_bad_signature(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "bad.png"
            path.write_bytes(b"not a png at all, just some bytes")
            with self.assertRaises(cs.PngError):
                cs.read_png_size(path)

    def test_rejects_truncated_file(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "short.png"
            path.write_bytes(cs.PNG_SIGNATURE)
            with self.assertRaises(cs.PngError):
                cs.read_png_size(path)

    def test_rejects_file_truncated_mid_idat(self) -> None:
        # エンジンが途中終了して IEND を書き切れなかったケース（codex P1）。
        # 先頭 33 バイト（シグネチャ + IHDR）は正しいままなので、旧実装では
        # 寸法一致だけを見て "ok" と誤判定してしまっていた。
        sys.path.insert(0, str(FIXTURES_DIR))
        import fake_engine  # noqa: PLC0415

        with tempfile.TemporaryDirectory() as tmp:
            full_path = Path(tmp) / "full.png"
            fake_engine.write_minimal_png(full_path, 37, 41)
            full_bytes = full_path.read_bytes()
            path = Path(tmp) / "truncated.png"
            path.write_bytes(full_bytes[: len(full_bytes) - 20])
            with self.assertRaises(cs.PngError):
                cs.read_png_size(path)

    def test_rejects_corrupted_idat_crc(self) -> None:
        # IHDR は無傷のまま IDAT チャンクのデータだけが壊れた（画像本体が
        # 破損した）ケース。寸法検証だけでは検出できない（codex P1）。
        sys.path.insert(0, str(FIXTURES_DIR))
        import fake_engine  # noqa: PLC0415

        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "corrupt.png"
            fake_engine.write_minimal_png(path, 37, 41)
            data = bytearray(path.read_bytes())
            # IDAT チャンクのデータ先頭 1 バイトを反転させ、CRC と不整合にする。
            idat_data_start = len(cs.PNG_SIGNATURE) + 8 + 13 + 4 + 8
            data[idat_data_start] ^= 0xFF
            path.write_bytes(bytes(data))
            with self.assertRaises(cs.PngError):
                cs.read_png_size(path)

    def test_rejects_trailing_garbage_after_iend(self) -> None:
        sys.path.insert(0, str(FIXTURES_DIR))
        import fake_engine  # noqa: PLC0415

        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "trailing.png"
            fake_engine.write_minimal_png(path, 37, 41)
            with path.open("ab") as fh:
                fh.write(b"trailing-garbage")
            with self.assertRaises(cs.PngError):
                cs.read_png_size(path)

    def test_rejects_oversized_file_before_reading_full_contents(self) -> None:
        # Cursor/codex P1: `read_bytes()` で無制限にメモリへ読み込む前に、`stat`
        # の時点でサイズ上限（`MAX_PNG_BYTES`）超過を検出して拒否する。テストでは
        # 上限を小さく差し替え、実際に巨大なファイルを書かずに検証する。
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "oversized.png"
            # シグネチャだけの小さいファイルでも、サイズ上限チェックが `read_bytes()`
            # より先に効くことを確認できればよい（内容の妥当性は問わない）。
            path.write_bytes(cs.PNG_SIGNATURE + b"\x00" * 64)
            with mock.patch.object(cs, "MAX_PNG_BYTES", 8):
                with self.assertRaises(cs.PngError) as ctx:
                    cs.read_png_size(path)
            self.assertIn("byte limit", str(ctx.exception))

    def test_rejects_png_with_ihdr_and_iend_but_no_idat(self) -> None:
        # codex P1 再指摘: IHDR・IEND の CRC が正しく寸法も一致していても、IDAT
        # （画素データ）が 1 つも無いファイルを "ok" と誤判定してはならない。
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "no-idat.png"
            path.write_bytes(_build_png(37, 41, []))
            with self.assertRaises(cs.PngError) as ctx:
                cs.read_png_size(path)
            self.assertIn("IDAT", str(ctx.exception))

    def test_accepts_idat_split_across_multiple_chunks(self) -> None:
        # IDAT はエンジンによって複数チャンクに分割されうる。連結して展開できる
        # ことを確認する。
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "split-idat.png"
            raw = _raw_pixels(37, 41)
            compressed = zlib.compress(raw)
            midpoint = len(compressed) // 2
            path.write_bytes(_build_png(37, 41, [compressed[:midpoint], compressed[midpoint:]]))
            width, height = cs.read_png_size(path)
            self.assertEqual((width, height), (37, 41))

    def test_rejects_idat_that_is_not_valid_zlib_data(self) -> None:
        # IDAT チャンクの CRC 自体は正しくても（＝チャンクとしては壊れていない
        # ように見えても）、中身が zlib として展開できないケース。
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "garbage-idat.png"
            path.write_bytes(_build_png(37, 41, [b"this is not a zlib stream at all"]))
            with self.assertRaises(cs.PngError) as ctx:
                cs.read_png_size(path)
            self.assertIn("zlib", str(ctx.exception))

    def test_rejects_idat_whose_decompressed_size_mismatches_ihdr(self) -> None:
        # 展開自体はできても、IHDR の寸法から計算される期待サイズと一致しない
        # （＝画像本体が別のサイズのデータにすり替わっている）ケース。
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "size-mismatch.png"
            wrong_raw = _raw_pixels(37, 10)  # IHDR は height=41 だが中身は height=10 相当
            path.write_bytes(_build_png(37, 41, [zlib.compress(wrong_raw)]))
            with self.assertRaises(cs.PngError) as ctx:
                cs.read_png_size(path)
            self.assertIn("does not match", str(ctx.exception))

    def test_rejects_ihdr_declaring_size_over_decompression_bomb_limit(self) -> None:
        # codex P1: 展開を試みる前に、IHDR の寸法から計算した期待展開サイズが
        # `MAX_PNG_RAW_BYTES` を超えないことを確認する（解凍爆弾対策）。実際に
        # 巨大なデータを展開させずに検出できることをテストする。
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "huge-ihdr.png"
            # width/height はテスト用に大きいだけで、IDAT の中身は展開されない
            # （サイズチェックが先に発火する）ためダミーで良い。
            path.write_bytes(_build_png(60000, 60000, [b"dummy"]))
            with mock.patch.object(cs, "MAX_PNG_RAW_BYTES", 1024):
                with self.assertRaises(cs.PngError) as ctx:
                    cs.read_png_size(path)
            self.assertIn("exceeding", str(ctx.exception))

    def test_rejects_invalid_bit_depth_for_color_type(self) -> None:
        # RGB（color_type=2）は bit_depth に 8 か 16 のみを許す（PNG 仕様）。
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "bad-bit-depth.png"
            path.write_bytes(_build_png(37, 41, [b"dummy"], bit_depth=4, color_type=2))
            with self.assertRaises(cs.PngError) as ctx:
                cs.read_png_size(path)
            self.assertIn("bit depth", str(ctx.exception))

    def test_rejects_unsupported_interlace_method(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "bad-interlace.png"
            path.write_bytes(_build_png(37, 41, [b"dummy"], interlace=9))
            with self.assertRaises(cs.PngError) as ctx:
                cs.read_png_size(path)
            self.assertIn("interlace", str(ctx.exception))

    def test_accepts_adam7_interlaced_png_with_matching_idat(self) -> None:
        # インターレース（Adam7）は 7 パスの合算サイズになる。`_expected_raw_size`
        # の計算が実際の展開結果と一致することを、パスごとのダミーデータで確認する。
        width, height = 8, 8
        expected = cs._expected_raw_size(width, height, color_type=0, bit_depth=8, interlace=1)
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "interlaced.png"
            path.write_bytes(
                _build_png(
                    width,
                    height,
                    [zlib.compress(b"\x00" * expected)],
                    bit_depth=8,
                    color_type=0,
                    interlace=1,
                )
            )
            result_width, result_height = cs.read_png_size(path)
            self.assertEqual((result_width, result_height), (width, height))


class _Resp(io.BytesIO):
    """`urllib` のレスポンスオブジェクトを模したテスト用スタブ。

    `geturl()` はリダイレクトを辿った後の最終 URL を返す（`response.geturl()`
    が返す値。Cursor Medium 再指摘: `inject_base_href` に渡す URL は最初の
    リクエスト URL ではなくこれでなければならない）。省略時は最初の URL を
    そのまま最終 URL として扱う（リダイレクトなしの通常応答を模す）。
    """

    def __init__(self, data: bytes, *, url: str | None = None) -> None:
        super().__init__(data)
        self._url = url

    def geturl(self) -> str | None:
        return self._url

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False


class WriteBytesNoFollowTest(unittest.TestCase):
    """RENDER-5 / TASK-37.1: `_write_bytes_nofollow`（symlink 追随の拒否。codex P0）。"""

    def test_writes_new_file(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "out.bin"
            cs._write_bytes_nofollow(path, b"hello")
            self.assertEqual(path.read_bytes(), b"hello")

    def test_overwrites_existing_regular_file(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "out.bin"
            path.write_bytes(b"old")
            cs._write_bytes_nofollow(path, b"new")
            self.assertEqual(path.read_bytes(), b"new")

    def test_refuses_existing_symlink_via_is_symlink_check(self) -> None:
        # 実際に symlink を作らずとも、`Path.is_symlink()` が真を返す状況を
        # モックで再現すれば OS 非依存（Windows 含む）に検証できる。
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "link.bin"
            with mock.patch.object(Path, "is_symlink", return_value=True):
                with self.assertRaises(OSError):
                    cs._write_bytes_nofollow(path, b"data")
            self.assertFalse(path.exists())

    def test_refuses_real_symlink_to_outside_file(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            outside_target = Path(tmp) / "outside.txt"
            outside_target.write_text("keep me", encoding="utf-8")
            link = Path(tmp) / "link.bin"
            try:
                link.symlink_to(outside_target)
            except OSError:
                self.skipTest("symlink creation is not permitted in this environment")
            with self.assertRaises(OSError):
                cs._write_bytes_nofollow(link, b"overwrite attempt")
            self.assertEqual(outside_target.read_text(encoding="utf-8"), "keep me")


class WriteResultTest(unittest.TestCase):
    """RENDER-5 / TASK-37.1: `write_result`（`capture-result.json` の symlink 追随防止。codex P0）。"""

    def test_writes_normally(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            out_dir = Path(tmp)
            site = cs.Site(site_id="a", url="https://example.invalid", category="static", catalog_id="z1")
            result_path = cs.write_result(
                out_dir, viewport={"width": 1280, "height": 800}, sites=[site], captures=[], partial=False
            )
            payload = json.loads(result_path.read_text(encoding="utf-8"))
            self.assertEqual(payload["schema_version"], cs.RESULT_SCHEMA_VERSION)

    def test_refuses_to_overwrite_existing_symlink(self) -> None:
        # `--out-dir` を使い回す再実行で `capture-result.json` が外部ファイルへの
        # symlink に差し替えられていた場合、そのファイルを結果 JSON で上書き
        # してはならない（`fetch_snapshot` の HTML 保存と同じ問題。codex P0）。
        with tempfile.TemporaryDirectory() as tmp:
            out_dir = Path(tmp) / "out"
            out_dir.mkdir()
            outside_target = Path(tmp) / "outside.json"
            outside_target.write_text("keep me", encoding="utf-8")
            result_path = out_dir / "capture-result.json"
            try:
                result_path.symlink_to(outside_target)
            except OSError:
                self.skipTest("symlink creation is not permitted in this environment")

            site = cs.Site(site_id="a", url="https://example.invalid", category="static", catalog_id="z1")
            with self.assertRaises(cs.CaptureError):
                cs.write_result(
                    out_dir, viewport={"width": 1280, "height": 800}, sites=[site], captures=[], partial=False
                )
            self.assertEqual(outside_target.read_text(encoding="utf-8"), "keep me")


class FetchSnapshotTest(unittest.TestCase):
    """RENDER-5 / TASK-37.1: `{html_path}` 用スナップショット取得（実ネットワークには出ない）。

    実 DNS 解決に依存しないよう、公開ホストとして扱いたいケースは `_check_public_host`
    を、実際の応答は `_open_url` をそれぞれモックする。
    """

    def test_rejects_http_scheme(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp) / "s.html"
            with self.assertRaises(cs.SnapshotError):
                cs.fetch_snapshot("http://example.invalid/", dest, proxy_url="http://127.0.0.1:1")

    def test_truncates_oversized_response(self) -> None:
        big_body = b"x" * (cs.SNAPSHOT_MAX_BYTES + 1)

        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp) / "s.html"
            with (
                mock.patch.object(cs, "_check_public_host"),
                mock.patch.object(cs, "_open_url", return_value=_Resp(big_body)),
            ):
                with self.assertRaises(cs.SnapshotError):
                    cs.fetch_snapshot("https://example.invalid/", dest, proxy_url="http://127.0.0.1:1")
            self.assertFalse(dest.exists())

    def test_writes_response_body_on_success(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp) / "s.html"
            with (
                mock.patch.object(cs, "_check_public_host"),
                mock.patch.object(cs, "_open_url", return_value=_Resp(b"<html></html>")),
            ):
                cs.fetch_snapshot("https://example.invalid/", dest, proxy_url="http://127.0.0.1:1")
            self.assertEqual(
                dest.read_bytes(),
                b'<base href="https://example.invalid/"><html></html>',
            )

    def test_injects_base_href_using_final_url_after_redirect(self) -> None:
        # Cursor Medium 再指摘: リダイレクトを辿った後の本文を保存するのに、
        # `<base href>` が最初にリクエストした URL のままだと、相対 URL の
        # リソースがリダイレクト前の場所を基準に解決されてしまう。
        # `response.geturl()`（最終 URL）を使うべきである。
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp) / "s.html"
            with (
                mock.patch.object(cs, "_check_public_host"),
                mock.patch.object(
                    cs,
                    "_open_url",
                    return_value=_Resp(b"<html></html>", url="https://example.invalid/final/"),
                ),
            ):
                cs.fetch_snapshot("https://example.invalid/original", dest, proxy_url="http://127.0.0.1:1")
            self.assertEqual(
                dest.read_bytes(),
                b'<base href="https://example.invalid/final/"><html></html>',
            )

    def test_refuses_to_write_through_existing_symlink(self) -> None:
        # codex P0: `write_bytes` は既存の symlink をたどってしまう。`--out-dir`
        # を使い回す再実行で `snapshots/<site_id>.html` が外部ファイルへの
        # symlink だった場合、そのファイルを取得 HTML で上書きできてはならない。
        with tempfile.TemporaryDirectory() as tmp:
            outside_target = Path(tmp) / "outside.txt"
            outside_target.write_text("do not overwrite me", encoding="utf-8")
            dest = Path(tmp) / "s.html"
            try:
                dest.symlink_to(outside_target)
            except OSError:
                self.skipTest("symlink creation is not permitted in this environment")
            with (
                mock.patch.object(cs, "_check_public_host"),
                mock.patch.object(cs, "_open_url", return_value=_Resp(b"<html></html>")),
            ):
                with self.assertRaises(cs.SnapshotError):
                    cs.fetch_snapshot("https://example.invalid/", dest, proxy_url="http://127.0.0.1:1")
            self.assertEqual(outside_target.read_text(encoding="utf-8"), "do not overwrite me")

    def test_injects_base_href_even_without_head_tag(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp) / "s.html"
            with (
                mock.patch.object(cs, "_check_public_host"),
                mock.patch.object(cs, "_open_url", return_value=_Resp(b"no head here")),
            ):
                cs.fetch_snapshot("https://example.invalid/x", dest, proxy_url="http://127.0.0.1:1")
            self.assertEqual(
                dest.read_bytes(),
                b'<base href="https://example.invalid/x">no head here',
            )

    def test_rejects_loopback_ip_literal(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp) / "s.html"
            with self.assertRaises(cs.SnapshotError):
                cs.fetch_snapshot("https://127.0.0.1/", dest, proxy_url="http://127.0.0.1:1")

    def test_rejects_localhost_hostname(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp) / "s.html"
            with self.assertRaises(cs.SnapshotError):
                cs.fetch_snapshot("https://localhost/", dest, proxy_url="http://127.0.0.1:1")

    def test_rejects_private_ip_literal(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp) / "s.html"
            with self.assertRaises(cs.SnapshotError):
                cs.fetch_snapshot("https://10.0.0.1/", dest, proxy_url="http://127.0.0.1:1")

    def test_rejects_link_local_metadata_ip_literal(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp) / "s.html"
            with self.assertRaises(cs.SnapshotError):
                cs.fetch_snapshot("https://169.254.169.254/", dest, proxy_url="http://127.0.0.1:1")

    def test_rejects_ipv6_loopback_literal(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp) / "s.html"
            with self.assertRaises(cs.SnapshotError):
                cs.fetch_snapshot("https://[::1]/", dest, proxy_url="http://127.0.0.1:1")

    def test_rejects_hostname_resolving_to_private_address(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp) / "s.html"
            with mock.patch.object(
                cs.socket,
                "getaddrinfo",
                return_value=[(cs.socket.AF_INET, None, None, "", ("10.1.2.3", 443))],
            ):
                with self.assertRaises(cs.SnapshotError):
                    cs.fetch_snapshot("https://internal.example.invalid/", dest, proxy_url="http://127.0.0.1:1")

    def test_accepts_hostname_resolving_to_public_address(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp) / "s.html"
            with (
                mock.patch.object(
                    cs.socket,
                    "getaddrinfo",
                    return_value=[(cs.socket.AF_INET, None, None, "", ("93.184.216.34", 443))],
                ),
                mock.patch.object(cs, "_open_url", return_value=_Resp(b"<html></html>")),
            ):
                cs.fetch_snapshot("https://example.invalid/", dest, proxy_url="http://127.0.0.1:1")
            self.assertTrue(dest.exists())

    def test_rejects_dns_rebinding_between_precheck_and_proxy_connect(self) -> None:
        # codex P0 再指摘: `_check_public_host` は事前に DNS 解決結果を検証する
        # だけで、実際の接続（旧実装では `opener.open` が改めて名前解決していた）
        # との間に DNS 応答が内部アドレスへ変わっていれば直接接続できてしまう
        # （DNS リバインディング）。修正後は実際の接続を必ずローカル転送
        # プロキシへ強制するため、`fetch_snapshot` の事前検査を通過した直後に
        # 名前解決結果が内部アドレスへ変わっても、実際に接続するのはプロキシの
        # `_resolve_public_addresses` が（接続直前に再度）検証した結果であり、
        # 内部アドレスへは到達しないことを確認する。
        #
        # 実際のプロキシを起動して検証する（`_open_url` をモックすると検証したい
        # 経路自体が無くなってしまうため）。プロキシ自身への接続（127.0.0.1）は
        # 実際の `getaddrinfo` にフォールバックし、対象ホストの解決だけを
        # 1 回目は公開アドレス・2 回目は内部アドレスに差し替える。
        #
        # 「内部アドレスへ実際に到達しない」ことは、単に `SnapshotError` が
        # 送出されるかどうかでは判定しない（サンドボックス環境では 10.x への
        # 接続自体がタイムアウトで失敗するだけでも同じ例外が出てしまい、旧実装の
        # バグ（直接接続してしまう）を見逃す）。`socket.socket.connect` を
        # フックし、内部アドレスへの `connect()` 呼び出しがそもそも発生しないこと
        # を直接検証する。
        real_getaddrinfo = cs.socket.getaddrinfo
        real_connect = cs.socket.socket.connect
        target_host = "internal.example.invalid"
        rebound_address = "10.1.2.3"
        call_count = {"n": 0}
        connect_attempts: list[tuple] = []

        def fake_getaddrinfo(host, *args, **kwargs):
            if host == target_host:
                call_count["n"] += 1
                if call_count["n"] == 1:
                    # fetch_snapshot 側の事前検査には公開アドレスを返す。
                    return [(cs.socket.AF_INET, cs.socket.SOCK_STREAM, 0, "", ("93.184.216.34", 0))]
                # プロキシが CONNECT 時に検証する時点では内部アドレスへ
                # 「変わった」ことにする（DNS リバインディングの再現）。
                return [(cs.socket.AF_INET, cs.socket.SOCK_STREAM, 0, "", (rebound_address, 443))]
            return real_getaddrinfo(host, *args, **kwargs)

        def guarded_connect(self_sock, address):
            connect_attempts.append(address)
            if isinstance(address, tuple) and address and address[0] == rebound_address:
                # 内部アドレスへの接続そのものを即座に失敗させる（タイムアウト
                # 待ちにせず、テストを高速・決定的にする）。
                raise AssertionError(f"must never connect directly to rebound address: {address}")
            return real_connect(self_sock, address)

        server, thread, proxy_url = cs.start_filtering_proxy()
        try:
            with tempfile.TemporaryDirectory() as tmp:
                dest = Path(tmp) / "s.html"
                with (
                    mock.patch.object(cs.socket, "getaddrinfo", side_effect=fake_getaddrinfo),
                    mock.patch.object(cs.socket.socket, "connect", guarded_connect),
                ):
                    with self.assertRaises(cs.SnapshotError):
                        cs.fetch_snapshot(f"https://{target_host}/", dest, proxy_url=proxy_url)
                self.assertFalse(dest.exists())
        finally:
            cs.stop_filtering_proxy(server, thread)
        # 事前検査（1 回目）とプロキシ側検証（2 回目）の両方が実際に呼ばれた
        # ことを確認し、テストが意図どおりの経路を通ったことを保証する。
        self.assertEqual(call_count["n"], 2)
        # 内部アドレスへの `connect()` は一度も発生していないこと
        # （`guarded_connect` が `AssertionError` を送出せずに完走したこと）。
        self.assertTrue(
            all(addr[0] != rebound_address for addr in connect_attempts if isinstance(addr, tuple)),
        )

    def test_stock_proxy_handler_bypasses_proxy_when_no_proxy_env_matches(self) -> None:
        # 前提の確認: 素の `urllib.request.ProxyHandler` は明示的な辞書を
        # 渡していても `no_proxy`/`NO_PROXY` 環境変数を見て直接接続に
        # フォールバックする。`_ForcedProxyHandler`（次のテスト）が必要な
        # 理由を示す対照実験。
        with mock.patch.dict(os.environ, {"no_proxy": "*", "NO_PROXY": "*"}):
            handler = urllib.request.ProxyHandler({"https": "http://127.0.0.1:9"})
            req = urllib.request.Request("https://internal.example.invalid/")
            original_host = req.host
            result = handler.proxy_open(req, "http://127.0.0.1:9", "https")
        # バイパスされた場合、`proxy_open` は `None` を返し、`req.host` は
        # 元のホストのまま変わらない（プロキシへ向いていない）。
        self.assertIsNone(result)
        self.assertEqual(req.host, original_host)

    def test_forced_proxy_handler_ignores_no_proxy_env_var(self) -> None:
        # codex P0 再指摘: CI や開発機で `NO_PROXY=*`（対象ホストを含む値）が
        # 設定されているだけで、明示的に渡した `ProxyHandler` の辞書が無視され
        # 直接接続にフォールバックしてしまう（上のテストで確認した挙動）。
        # `fetch_snapshot`／`_open_url` が使う `_ForcedProxyHandler` はこの
        # 環境変数によるバイパスを無視し、必ずプロキシへ向けることを確認する。
        with mock.patch.dict(os.environ, {"no_proxy": "*", "NO_PROXY": "*"}):
            handler = cs._ForcedProxyHandler({"https": "http://127.0.0.1:9"})
            req = urllib.request.Request("https://internal.example.invalid/")
            result = handler.proxy_open(req, "http://127.0.0.1:9", "https")
        # https の場合、`set_proxy` はトンネル先ホスト（`_tunnel_host`）に
        # 元のホストを保持しつつ、実際に接続する `req.host` をプロキシへ
        # 差し替える。
        self.assertEqual(req.host, "127.0.0.1:9")
        self.assertEqual(req._tunnel_host, "internal.example.invalid")  # noqa: SLF001
        self.assertIsNone(result)

    def test_rejects_url_with_no_hostname(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp) / "s.html"
            with self.assertRaises(cs.SnapshotError):
                cs.fetch_snapshot("https:///path", dest, proxy_url="http://127.0.0.1:1")

    def test_redirect_handler_rejects_non_https_target(self) -> None:
        handler = cs._PublicOnlyRedirectHandler()
        with self.assertRaises(cs.SnapshotError):
            handler.redirect_request(
                mock.Mock(full_url="https://example.invalid/"),
                None,
                302,
                "Found",
                {},
                "http://169.254.169.254/",
            )

    def test_redirect_handler_rejects_internal_https_target(self) -> None:
        handler = cs._PublicOnlyRedirectHandler()
        with self.assertRaises(cs.SnapshotError):
            handler.redirect_request(
                mock.Mock(full_url="https://example.invalid/"),
                None,
                302,
                "Found",
                {},
                "https://127.0.0.1/",
            )


class FilteringProxyTest(unittest.TestCase):
    """RENDER-5 / TASK-37.1: ローカル転送プロキシの宛先フィルタ（codex P0 再指摘）。

    `_check_public_host` は撮影対象の最初の URL しか検証しないため、撮影
    プロセス（Chromium 等）自身が読み込むサブリソース・リダイレクト先を
    防げない。本プロキシは撮影プロセスの全通信をここで検証・中継する。
    """

    def setUp(self) -> None:
        self.server, self.thread, self.proxy_url = cs.start_filtering_proxy()
        self.addCleanup(cs.stop_filtering_proxy, self.server, self.thread)
        parsed = urlparse(self.proxy_url)
        self.proxy_host = parsed.hostname
        self.proxy_port = parsed.port

    def _open_proxy_connection(self) -> "socket.socket":
        import socket as socket_module  # noqa: PLC0415

        sock = socket_module.create_connection((self.proxy_host, self.proxy_port), timeout=5)
        sock.settimeout(5)
        return sock

    def _read_status_line(self, sock: "socket.socket") -> str:
        data = b""
        while b"\r\n" not in data and len(data) < 4096:
            chunk = sock.recv(4096)
            if not chunk:
                break
            data += chunk
        return data.split(b"\r\n", 1)[0].decode("latin-1", errors="replace")

    def test_connect_to_loopback_ip_literal_is_denied(self) -> None:
        # IP リテラルなので `_resolve_public_addresses` の実装がそのまま働き、
        # モック無しで検証できる。
        sock = self._open_proxy_connection()
        try:
            sock.sendall(b"CONNECT 127.0.0.1:443 HTTP/1.1\r\nHost: 127.0.0.1:443\r\n\r\n")
            status_line = self._read_status_line(sock)
        finally:
            sock.close()
        self.assertIn("403", status_line)

    def test_connect_to_private_ip_literal_is_denied(self) -> None:
        sock = self._open_proxy_connection()
        try:
            sock.sendall(b"CONNECT 10.0.0.1:443 HTTP/1.1\r\nHost: 10.0.0.1:443\r\n\r\n")
            status_line = self._read_status_line(sock)
        finally:
            sock.close()
        self.assertIn("403", status_line)

    def test_get_absolute_uri_to_metadata_address_is_denied(self) -> None:
        sock = self._open_proxy_connection()
        try:
            sock.sendall(b"GET http://169.254.169.254/latest/meta-data/ HTTP/1.1\r\nHost: 169.254.169.254\r\n\r\n")
            status_line = self._read_status_line(sock)
        finally:
            sock.close()
        self.assertIn("403", status_line)

    def test_connect_to_disallowed_port_is_denied_even_for_public_looking_host(self) -> None:
        # 宛先フィルタの判定より先にポート許可リスト（80/443 のみ）を見るため、
        # ホスト名解決をモックしなくても検証できる。
        sock = self._open_proxy_connection()
        try:
            sock.sendall(b"CONNECT example.invalid:8443 HTTP/1.1\r\nHost: example.invalid:8443\r\n\r\n")
            status_line = self._read_status_line(sock)
        finally:
            sock.close()
        self.assertIn("403", status_line)

    def test_connect_to_mocked_public_address_relays_bytes_both_ways(self) -> None:
        # 「公開アドレス」はモックの判定関数（`_resolve_public_addresses`）で
        # 表現し、実 DNS 解決には依存しない。CONNECT トンネル確立後、生の
        # バイト列がクライアント→origin・origin→クライアントの両方向に
        # 中継されることを、ローカルの TCP エコーサーバーで確認する。
        import socketserver as socketserver_module  # noqa: PLC0415

        class _EchoHandler(socketserver_module.BaseRequestHandler):
            def handle(self) -> None:
                data = self.request.recv(4096)
                if data:
                    self.request.sendall(data)

        with socketserver_module.TCPServer(("127.0.0.1", 0), _EchoHandler) as echo_server:
            echo_port = echo_server.server_address[1]
            echo_thread = threading.Thread(target=echo_server.handle_request, daemon=True)
            echo_thread.start()

            with mock.patch.object(
                cs, "_resolve_public_addresses", return_value=[("127.0.0.1", echo_port)]
            ):
                sock = self._open_proxy_connection()
                try:
                    sock.sendall(b"CONNECT example.test:443 HTTP/1.1\r\nHost: example.test:443\r\n\r\n")
                    status_line = self._read_status_line(sock)
                    self.assertIn("200", status_line)
                    sock.sendall(b"ping")
                    echoed = sock.recv(4096)
                finally:
                    sock.close()
            echo_thread.join(timeout=5)
        self.assertEqual(echoed, b"ping")

    def test_connect_relay_transfers_large_payload_without_truncation(self) -> None:
        # Cursor Medium: 両ソケットを non-blocking にして `sendall` すると、
        # 相手側の送信バッファが埋まった際に `BlockingIOError` が飛び、
        # `_relay` がそれを `except OSError: return` で捕まえてトンネルを
        # 途中で閉じてしまう（大きな転送が欠落する）。クライアント側の読み出しを
        # わざと遅らせ、origin からの送信で中継バッファを埋めてこの状況を
        # 再現し、最終的に全バイトが欠落なく届くことを確認する。
        payload = bytes((i % 251) for i in range(4 * 1024 * 1024))  # 4 MiB

        import socketserver as socketserver_module  # noqa: PLC0415

        class _BigSenderHandler(socketserver_module.BaseRequestHandler):
            def handle(self) -> None:
                self.request.sendall(payload)

        with socketserver_module.TCPServer(("127.0.0.1", 0), _BigSenderHandler) as origin_server:
            origin_port = origin_server.server_address[1]
            origin_thread = threading.Thread(target=origin_server.handle_request, daemon=True)
            origin_thread.start()

            with mock.patch.object(
                cs, "_resolve_public_addresses", return_value=[("127.0.0.1", origin_port)]
            ):
                sock = self._open_proxy_connection()
                try:
                    sock.sendall(b"CONNECT example.test:443 HTTP/1.1\r\nHost: example.test:443\r\n\r\n")
                    status_line = self._read_status_line(sock)
                    self.assertIn("200", status_line)
                    # 読み出しを遅らせ、origin からの送信で中継側の送信バッファを
                    # 埋める時間を与える（non-blocking sendall のバグを再現しやすくする）。
                    time.sleep(0.3)
                    sock.settimeout(10)
                    received = b""
                    while len(received) < len(payload):
                        chunk = sock.recv(262144)
                        if not chunk:
                            break
                        received += chunk
                finally:
                    sock.close()
            origin_thread.join(timeout=5)
        self.assertEqual(received, payload)

    def test_get_absolute_uri_to_mocked_public_address_returns_origin_response(self) -> None:
        # 絶対 URI 形式の GET フォワード（http:// 用）の正常系。origin をローカルの
        # `http.server` で立て、`_resolve_public_addresses` をモックしてそこへ
        # 誘導する。
        import http.server as http_server_module  # noqa: PLC0415
        import socketserver as socketserver_module  # noqa: PLC0415

        class _OriginHandler(http_server_module.BaseHTTPRequestHandler):
            def do_GET(self) -> None:  # noqa: N802
                body = b"hello from origin"
                self.send_response(200)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, format: str, *args: object) -> None:  # noqa: A002
                pass

        with socketserver_module.TCPServer(("127.0.0.1", 0), _OriginHandler) as origin_server:
            origin_port = origin_server.server_address[1]
            origin_thread = threading.Thread(target=origin_server.handle_request, daemon=True)
            origin_thread.start()

            with mock.patch.object(
                cs, "_resolve_public_addresses", return_value=[("127.0.0.1", origin_port)]
            ):
                sock = self._open_proxy_connection()
                try:
                    sock.sendall(b"GET http://example.test/ HTTP/1.1\r\nHost: example.test\r\n\r\n")
                    response = b""
                    while b"hello from origin" not in response and len(response) < 65536:
                        chunk = sock.recv(4096)
                        if not chunk:
                            break
                        response += chunk
                finally:
                    sock.close()
            origin_thread.join(timeout=5)
        self.assertIn(b"200", response.split(b"\r\n", 1)[0])
        self.assertIn(b"hello from origin", response)


class CaptureOneProxyRequirementTest(unittest.TestCase):
    """RENDER-5 / TASK-37.1: `capture_one` の fail-closed プロキシ必須化（codex P0 再指摘）。"""

    def test_skips_when_template_omits_proxy_and_unproxied_not_allowed(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            site = cs.Site(site_id="no-proxy", url="https://example.invalid", category="static", catalog_id="z1")
            record = cs.capture_one(
                site,
                "servo",
                [sys.executable, str(FAKE_ENGINE), "--out", "{out}"],
                out_dir=Path(tmp),
                snapshots_dir=Path(tmp) / "snapshots",
                chromium_bin=None,
                width=1280,
                height=800,
                settle_ms=1000,
                timeout_sec=5,
                allow_file_url=False,
                dry_run=False,
            )
            self.assertEqual(record["status"], "skipped")
            self.assertFalse((Path(tmp) / "servo" / "no-proxy.png").exists())

    def test_allow_unproxied_engine_bypasses_the_requirement(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            site = cs.Site(site_id="opt-out", url="https://example.invalid", category="static", catalog_id="z1")
            record = cs.capture_one(
                site,
                "servo",
                [sys.executable, str(FAKE_ENGINE), "--out", "{out}"],
                out_dir=Path(tmp),
                snapshots_dir=Path(tmp) / "snapshots",
                chromium_bin=None,
                width=1280,
                height=800,
                settle_ms=1000,
                timeout_sec=5,
                allow_file_url=False,
                dry_run=False,
                allow_unproxied_engine=True,
            )
            self.assertEqual(record["status"], "ok")


class ProcessTreeKillTest(unittest.TestCase):
    """RENDER-5 / TASK-37.1: タイムアウト時のプロセスツリー終了（Cursor Medium 再指摘）。

    `subprocess.run(..., timeout=)` はタイムアウト時に直接の子しか kill しない。
    Chromium のレンダラー・GPU プロセス等の子孫が残ってしまう問題の再現・修正確認。
    """

    @unittest.skipUnless(sys.platform != "win32", "os.killpg によるプロセスグループ終了は POSIX 専用")
    def test_grandchild_process_is_gone_after_timeout(self) -> None:
        import os as os_module  # noqa: PLC0415

        with tempfile.TemporaryDirectory() as tmp:
            pid_file = Path(tmp) / "grandchild.pid"
            site = cs.Site(site_id="grandchild", url="https://example.invalid", category="static", catalog_id="z1")
            record = cs.capture_one(
                site,
                "servo",
                [
                    sys.executable,
                    str(FAKE_ENGINE),
                    "--mode",
                    "spawn-grandchild",
                    "--out",
                    "{out}",
                    "--pid-file",
                    str(pid_file),
                ],
                out_dir=Path(tmp),
                snapshots_dir=Path(tmp) / "snapshots",
                chromium_bin=None,
                width=1280,
                height=800,
                settle_ms=1000,
                timeout_sec=1,
                allow_file_url=False,
                dry_run=False,
                allow_unproxied_engine=True,
            )
            self.assertEqual(record["status"], "timeout")

            # 孫プロセスが PID ファイルを書き出すまで待つ（`capture_one` が
            # 返った時点で書き出し済みのはずだが、フォールバックとして少し待つ）。
            deadline = time.monotonic() + 5
            while not pid_file.exists() and time.monotonic() < deadline:
                time.sleep(0.05)
            self.assertTrue(pid_file.exists(), "grandchild did not write its PID in time")
            grandchild_pid = int(pid_file.read_text(encoding="utf-8"))

            # SIGKILL の配送・reap は非同期なので、消えるまで少し待ってから判定する。
            deadline = time.monotonic() + 2
            gone = False
            while time.monotonic() < deadline:
                try:
                    os_module.kill(grandchild_pid, 0)
                except ProcessLookupError:
                    gone = True
                    break
                time.sleep(0.05)
            self.assertTrue(gone, f"grandchild pid {grandchild_pid} is still alive after timeout handling")


class InjectBaseHrefTest(unittest.TestCase):
    """RENDER-5 / TASK-37.1: 相対 URL 解決基準を揃える `<base>` 注入（P1）。"""

    def test_inserts_after_head_tag_case_insensitively(self) -> None:
        html_bytes = b"<HTML><HEAD lang='en'><title>t</title></HEAD></HTML>"
        result = cs.inject_base_href(html_bytes, "https://example.invalid/a")
        self.assertIn(b'<base href="https://example.invalid/a">', result)
        self.assertTrue(result.startswith(b"<HTML><HEAD lang='en'><base href="))

    def test_escapes_double_quote_in_url(self) -> None:
        result = cs.inject_base_href(b"<head></head>", 'https://example.invalid/"x')
        self.assertIn(b"&quot;", result)
        self.assertNotIn(b'href="https://example.invalid/"x"', result)

    def test_does_not_mistake_header_tag_for_head_tag(self) -> None:
        # codex/Bugbot P2: 素朴な `<head[^>]*>` は `<header>` にも一致し、`<head>`
        # を持たず `<header>` を含む HTML では本文中の誤位置へ `<base>` が
        # 挿入されていた。`<head>` が無いので先頭へ挿入されるのが正しい挙動。
        html_bytes = b"<html><body><header>nav</header><p>body</p></body></html>"
        result = cs.inject_base_href(html_bytes, "https://example.invalid/a")
        self.assertTrue(result.startswith(b'<base href="https://example.invalid/a">'))
        self.assertEqual(result, b'<base href="https://example.invalid/a">' + html_bytes)

    def test_inserts_after_head_tag_not_header_when_both_present(self) -> None:
        html_bytes = b"<html><head><title>t</title></head><body><header>nav</header></body></html>"
        result = cs.inject_base_href(html_bytes, "https://example.invalid/a")
        self.assertTrue(
            result.startswith(b'<html><head><base href="https://example.invalid/a">')
        )


class CaptureIntegrationTest(unittest.TestCase):
    """RENDER-5 / TASK-37.1: 偽エンジンによる結合テスト（両エンジン x 複数サイト）。"""

    def _write_sites(self, tmp: Path, count: int = 5) -> Path:
        path = tmp / "sites.json"
        write_sites_json(path, [make_site(f"site-{i}") for i in range(count)])
        return path

    def test_both_engines_all_ok_produces_ten_pngs_and_exit_zero(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_str:
            tmp = Path(tmp_str)
            sites_path = self._write_sites(tmp)
            out_dir = tmp / "out"
            servo_cmd = json.dumps(fake_engine_template("ok"))
            chromium_cmd = json.dumps(fake_engine_template("ok"))
            code = cs.main(
                [
                    "--sites",
                    str(sites_path),
                    "--out-dir",
                    str(out_dir),
                    "--engines",
                    "servo,chromium",
                    "--servo-cmd",
                    servo_cmd,
                    "--chromium-cmd",
                    chromium_cmd,
                    "--min-sites",
                    "5",
                ]
            )
            self.assertEqual(code, 0)
            result = json.loads((out_dir / "capture-result.json").read_text(encoding="utf-8"))
            self.assertEqual(result["schema_version"], 1)
            self.assertEqual(len(result["captures"]), 10)
            self.assertTrue(all(c["status"] == "ok" for c in result["captures"]))
            self.assertNotIn("partial", result)
            pngs = list((out_dir / "servo").glob("*.png")) + list((out_dir / "chromium").glob("*.png"))
            self.assertEqual(len(pngs), 10)
            # `main()` が実際に起動したローカル転送プロキシの URL が argv へ
            # 展開されていること（`{proxy}` がプレースホルダのまま・空のまま
            # ではないこと）を確認する。偽エンジンは値を使わず無視するだけ
            # なので、これが無いと実プロキシが配線されていなくても気付けない。
            self.assertTrue(
                any("http://127.0.0.1:" in " ".join(c["command"]) for c in result["captures"]),
            )

    def test_exit_code_uses_common_ok_sites_not_per_engine_counts(self) -> None:
        # codex P1: Servo が site-0..4（5 件）・Chromium が site-1..5（5 件）で
        # 成功する（サイト一覧は 6 件 site-0..5）。エンジンごとの独立集計では
        # どちらも 5 件で `--min-sites 5` を満たすが、両エンジンで共通して ok
        # なのは site-1..4 の 4 件のみであり、RENDER-5 の比較基準を満たさない
        # ため exit code は 1 になるべきである。
        with tempfile.TemporaryDirectory() as tmp_str:
            tmp = Path(tmp_str)
            sites_path = self._write_sites(tmp, count=6)
            out_dir = tmp / "out"
            servo_cmd = json.dumps(
                [
                    sys.executable,
                    str(SITE_CONDITIONAL_ENGINE),
                    "--out",
                    "{out}",
                    "--width",
                    "{width}",
                    "--height",
                    "{height}",
                    "--fail-sites",
                    "site-5",
                    "--proxy",
                    "{proxy}",
                ]
            )
            chromium_cmd = json.dumps(
                [
                    sys.executable,
                    str(SITE_CONDITIONAL_ENGINE),
                    "--out",
                    "{out}",
                    "--width",
                    "{width}",
                    "--height",
                    "{height}",
                    "--fail-sites",
                    "site-0",
                    "--proxy",
                    "{proxy}",
                ]
            )
            code = cs.main(
                [
                    "--sites",
                    str(sites_path),
                    "--out-dir",
                    str(out_dir),
                    "--engines",
                    "servo,chromium",
                    "--servo-cmd",
                    servo_cmd,
                    "--chromium-cmd",
                    chromium_cmd,
                    "--min-sites",
                    "5",
                ]
            )
            self.assertEqual(code, 1)
            result = json.loads((out_dir / "capture-result.json").read_text(encoding="utf-8"))
            servo_ok = {c["site_id"] for c in result["captures"] if c["engine"] == "servo" and c["status"] == "ok"}
            chromium_ok = {
                c["site_id"] for c in result["captures"] if c["engine"] == "chromium" and c["status"] == "ok"
            }
            self.assertEqual(len(servo_ok), 5)
            self.assertEqual(len(chromium_ok), 5)
            self.assertEqual(len(servo_ok & chromium_ok), 4)

    def test_exit_code_zero_when_common_ok_sites_reach_min_sites(self) -> None:
        # 上と同じ overlap パターンだが `--min-sites 4` なら共通 4 件で基準を
        # 満たし exit code 0 になることを確認する（正常系の回帰防止）。
        with tempfile.TemporaryDirectory() as tmp_str:
            tmp = Path(tmp_str)
            sites_path = self._write_sites(tmp, count=6)
            out_dir = tmp / "out"
            servo_cmd = json.dumps(
                [
                    sys.executable,
                    str(SITE_CONDITIONAL_ENGINE),
                    "--out",
                    "{out}",
                    "--width",
                    "{width}",
                    "--height",
                    "{height}",
                    "--fail-sites",
                    "site-5",
                    "--proxy",
                    "{proxy}",
                ]
            )
            chromium_cmd = json.dumps(
                [
                    sys.executable,
                    str(SITE_CONDITIONAL_ENGINE),
                    "--out",
                    "{out}",
                    "--width",
                    "{width}",
                    "--height",
                    "{height}",
                    "--fail-sites",
                    "site-0",
                    "--proxy",
                    "{proxy}",
                ]
            )
            code = cs.main(
                [
                    "--sites",
                    str(sites_path),
                    "--out-dir",
                    str(out_dir),
                    "--engines",
                    "servo,chromium",
                    "--servo-cmd",
                    servo_cmd,
                    "--chromium-cmd",
                    chromium_cmd,
                    "--min-sites",
                    "4",
                ]
            )
            self.assertEqual(code, 0)

    def test_fail_mode_marks_failed_and_exit_one(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_str:
            tmp = Path(tmp_str)
            sites_path = self._write_sites(tmp)
            out_dir = tmp / "out"
            code = cs.main(
                [
                    "--sites",
                    str(sites_path),
                    "--out-dir",
                    str(out_dir),
                    "--engines",
                    "servo",
                    "--servo-cmd",
                    json.dumps(fake_engine_template("fail")),
                    "--min-sites",
                    "5",
                ]
            )
            self.assertEqual(code, 1)
            result = json.loads((out_dir / "capture-result.json").read_text(encoding="utf-8"))
            self.assertTrue(all(c["status"] == "failed" for c in result["captures"]))
            self.assertTrue(result["partial"])

    def test_sleep_mode_times_out(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_str:
            tmp = Path(tmp_str)
            sites_path = self._write_sites(tmp, count=5)
            out_dir = tmp / "out"
            code = cs.main(
                [
                    "--sites",
                    str(sites_path),
                    "--out-dir",
                    str(out_dir),
                    "--engines",
                    "servo",
                    "--servo-cmd",
                    json.dumps(fake_engine_template("sleep")),
                    "--min-sites",
                    "5",
                    "--timeout-sec",
                    "1",
                ]
            )
            self.assertEqual(code, 1)
            result = json.loads((out_dir / "capture-result.json").read_text(encoding="utf-8"))
            self.assertTrue(all(c["status"] == "timeout" for c in result["captures"]))

    def test_garbage_png_marks_failed(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_str:
            tmp = Path(tmp_str)
            sites_path = self._write_sites(tmp, count=5)
            out_dir = tmp / "out"
            code = cs.main(
                [
                    "--sites",
                    str(sites_path),
                    "--out-dir",
                    str(out_dir),
                    "--engines",
                    "servo",
                    "--servo-cmd",
                    json.dumps(fake_engine_template("garbage")),
                    "--min-sites",
                    "5",
                ]
            )
            self.assertEqual(code, 1)
            result = json.loads((out_dir / "capture-result.json").read_text(encoding="utf-8"))
            self.assertTrue(all(c["status"] == "failed" for c in result["captures"]))

    def test_dry_run_creates_no_files_and_exits_zero(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_str:
            tmp = Path(tmp_str)
            sites_path = self._write_sites(tmp)
            out_dir = tmp / "out"
            code = cs.main(
                [
                    "--sites",
                    str(sites_path),
                    "--out-dir",
                    str(out_dir),
                    "--engines",
                    "servo,chromium",
                    "--servo-cmd",
                    json.dumps(fake_engine_template("ok")),
                    "--dry-run",
                    "--min-sites",
                    "5",
                ]
            )
            self.assertEqual(code, 0)
            self.assertFalse(out_dir.exists())

    def test_dry_run_does_not_fetch_html_snapshot(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_str:
            tmp = Path(tmp_str)
            sites_path = self._write_sites(tmp)
            out_dir = tmp / "out"
            with mock.patch.object(cs, "fetch_snapshot") as mocked_fetch:
                code = cs.main(
                    [
                        "--sites",
                        str(sites_path),
                        "--out-dir",
                        str(out_dir),
                        "--engines",
                        "servo",
                        "--servo-cmd",
                        json.dumps([sys.executable, str(FAKE_ENGINE), "--out", "{html_path}"]),
                        "--dry-run",
                        "--min-sites",
                        "5",
                    ]
                )
            self.assertEqual(code, 0)
            mocked_fetch.assert_not_called()

    def test_dry_run_without_chromium_binary_falls_back_to_placeholder(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_str:
            tmp = Path(tmp_str)
            sites_path = self._write_sites(tmp)
            out_dir = tmp / "out"
            with mock.patch.object(cs.shutil, "which", return_value=None):
                code = cs.main(
                    [
                        "--sites",
                        str(sites_path),
                        "--out-dir",
                        str(out_dir),
                        "--engines",
                        "chromium",
                        "--dry-run",
                        "--min-sites",
                        "5",
                    ]
                )
            self.assertEqual(code, 0)

    def test_missing_chromium_binary_is_fail_closed_when_not_dry_run(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_str:
            tmp = Path(tmp_str)
            sites_path = self._write_sites(tmp)
            out_dir = tmp / "out"
            with mock.patch.object(cs.shutil, "which", return_value=None):
                code = cs.main(
                    [
                        "--sites",
                        str(sites_path),
                        "--out-dir",
                        str(out_dir),
                        "--engines",
                        "chromium",
                        "--min-sites",
                        "5",
                    ]
                )
            self.assertEqual(code, 2)

    def test_single_engine_reaching_min_sites_exits_zero_with_partial_flag(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_str:
            tmp = Path(tmp_str)
            sites_path = self._write_sites(tmp, count=5)
            out_dir = tmp / "out"
            code = cs.main(
                [
                    "--sites",
                    str(sites_path),
                    "--out-dir",
                    str(out_dir),
                    "--engines",
                    "servo",
                    "--servo-cmd",
                    json.dumps(fake_engine_template("ok")),
                    "--min-sites",
                    "5",
                ]
            )
            self.assertEqual(code, 0)
            result = json.loads((out_dir / "capture-result.json").read_text(encoding="utf-8"))
            self.assertTrue(result["partial"])

    def test_duplicate_engine_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_str:
            tmp = Path(tmp_str)
            sites_path = self._write_sites(tmp)
            out_dir = tmp / "out"
            code = cs.main(
                [
                    "--sites",
                    str(sites_path),
                    "--out-dir",
                    str(out_dir),
                    "--engines",
                    "servo,servo",
                    "--servo-cmd",
                    json.dumps(fake_engine_template("ok")),
                    "--min-sites",
                    "5",
                ]
            )
            self.assertEqual(code, 2)

    def test_stale_png_from_previous_run_is_not_reported_as_ok(self) -> None:
        # --out-dir を使い回す再実行で、今回失敗したサイトの PNG が前回分の
        # 残置ファイルのまま残っていると誤って ok 判定されないことを確認する
        # （P1: 再実行時の残置ファイル誤判定対策）。
        with tempfile.TemporaryDirectory() as tmp:
            out_dir = Path(tmp) / "out"
            site = cs.Site(site_id="stale", url="https://example.invalid", category="static", catalog_id="z1")
            stale_png_dir = out_dir / "servo"
            stale_png_dir.mkdir(parents=True)
            fixtures_path = str(FIXTURES_DIR)
            sys.path.insert(0, fixtures_path)
            import fake_engine  # noqa: PLC0415

            fake_engine.write_minimal_png(stale_png_dir / "stale.png", 1280, 800)

            record = cs.capture_one(
                site,
                "servo",
                fake_engine_template("fail"),
                out_dir=out_dir,
                snapshots_dir=out_dir / "snapshots",
                chromium_bin=None,
                width=1280,
                height=800,
                settle_ms=1000,
                timeout_sec=5,
                allow_file_url=False,
                dry_run=False,
                proxy_url="http://127.0.0.1:1",
            )
            self.assertEqual(record["status"], "failed")
            self.assertIsNone(record["png"])

    def test_png_dimension_mismatch_is_reported_as_failed(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            site = cs.Site(site_id="wrong-size", url="https://example.invalid", category="static", catalog_id="z1")
            template = [
                sys.executable,
                str(FAKE_ENGINE),
                "--out",
                "{out}",
                "--width",
                "10",
                "--height",
                "10",
            ]
            record = cs.capture_one(
                site,
                "servo",
                template,
                out_dir=Path(tmp),
                snapshots_dir=Path(tmp) / "snapshots",
                chromium_bin=None,
                width=1280,
                height=800,
                settle_ms=1000,
                timeout_sec=5,
                allow_file_url=False,
                dry_run=False,
                allow_unproxied_engine=True,
            )
            self.assertEqual(record["status"], "failed")
            self.assertIsNone(record["png"])
            self.assertIn("does not match", record["stderr_tail"])

    def test_missing_engine_binary_is_reported_as_failed_not_a_crash(self) -> None:
        # テンプレートが `{url}` を直接使うため、直接ナビゲーション向けの SSRF
        # チェック（`_check_public_host`）と既定サイト許可リストの検証が挟まる。
        # 実 DNS 解決・sites.json への追加に依存しないよう両方をモック・差し替える
        # （他の FetchSnapshotTest 系テストと同じ方針）。
        with tempfile.TemporaryDirectory() as tmp:
            site = cs.Site(site_id="no-bin", url="https://example.invalid", category="static", catalog_id="z1")
            with (
                mock.patch.object(cs, "_check_public_host"),
                mock.patch.object(cs, "DIRECT_NAVIGATION_ALLOWED_URLS", frozenset({site.url})),
            ):
                record = cs.capture_one(
                    site,
                    "servo",
                    ["/nonexistent/definitely-not-a-real-binary", "{url}"],
                    out_dir=Path(tmp),
                    snapshots_dir=Path(tmp) / "snapshots",
                    chromium_bin=None,
                    width=1280,
                    height=800,
                    settle_ms=1000,
                    timeout_sec=5,
                    allow_file_url=False,
                    dry_run=False,
                    allow_unproxied_engine=True,
                )
            self.assertEqual(record["status"], "failed")
            self.assertIsNone(record["exit_code"])

    def test_user_data_dir_is_cleaned_up_when_snapshot_fetch_fails(self) -> None:
        # SnapshotError による早期 return でも一時 user-data-dir が残置されない
        # ことを確認する（Bugbot Low）。
        with tempfile.TemporaryDirectory() as tmp:
            site = cs.Site(site_id="udd-leak", url="https://example.invalid", category="static", catalog_id="z1")
            created: list[Path] = []
            real_mkdtemp = tempfile.mkdtemp

            def _tracking_mkdtemp(*args, **kwargs):
                path = Path(real_mkdtemp(*args, **kwargs, dir=tmp))
                created.append(path)
                return str(path)

            with (
                mock.patch.object(cs.tempfile, "mkdtemp", side_effect=_tracking_mkdtemp),
                mock.patch.object(cs, "fetch_snapshot", side_effect=cs.SnapshotError("boom")),
            ):
                record = cs.capture_one(
                    site,
                    "chromium",
                    ["engine", "{user_data_dir}", "{html_path}", "{out}"],
                    out_dir=Path(tmp) / "out",
                    snapshots_dir=Path(tmp) / "out" / "snapshots",
                    chromium_bin=None,
                    width=1280,
                    height=800,
                    settle_ms=1000,
                    timeout_sec=5,
                    allow_file_url=False,
                    dry_run=False,
                    allow_unproxied_engine=True,
                    proxy_url="http://127.0.0.1:1",
                )
            self.assertEqual(record["status"], "skipped")
            self.assertEqual(len(created), 1)
            self.assertFalse(created[0].exists())

    def test_unknown_engine_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_str:
            tmp = Path(tmp_str)
            sites_path = self._write_sites(tmp)
            out_dir = tmp / "out"
            code = cs.main(
                [
                    "--sites",
                    str(sites_path),
                    "--out-dir",
                    str(out_dir),
                    "--engines",
                    "gecko",
                    "--min-sites",
                    "5",
                ]
            )
            self.assertEqual(code, 2)

    def test_direct_url_navigation_to_loopback_is_rejected(self) -> None:
        # codex P0: `{html_path}` を経由しない直接ナビゲーション（既定 Chromium
        # テンプレート等、`{url}` をそのままエンジンへ渡すテンプレート）には
        # SSRF 検証（`_check_public_host`）が掛かっていなかった。`--sites` に
        # 内部アドレスの https URL を指定すると迂回できていた問題の再現・修正確認。
        with tempfile.TemporaryDirectory() as tmp:
            site = cs.Site(
                site_id="ssrf-loopback", url="https://127.0.0.1/secret", category="static", catalog_id="z1"
            )
            record = cs.capture_one(
                site,
                "chromium",
                [sys.executable, str(FAKE_ENGINE), "--out", "{out}", "--url", "{url}"],
                out_dir=Path(tmp),
                snapshots_dir=Path(tmp) / "snapshots",
                chromium_bin=None,
                width=1280,
                height=800,
                settle_ms=1000,
                timeout_sec=5,
                allow_file_url=False,
                dry_run=False,
                allow_unproxied_engine=True,
            )
            self.assertEqual(record["status"], "skipped")
            self.assertFalse((Path(tmp) / "chromium" / "ssrf-loopback.png").exists())

    def test_direct_url_navigation_to_link_local_metadata_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            site = cs.Site(
                site_id="ssrf-metadata",
                url="https://169.254.169.254/latest/meta-data/",
                category="static",
                catalog_id="z1",
            )
            record = cs.capture_one(
                site,
                "chromium",
                [sys.executable, str(FAKE_ENGINE), "--out", "{out}", "--url", "{url}"],
                out_dir=Path(tmp),
                snapshots_dir=Path(tmp) / "snapshots",
                chromium_bin=None,
                width=1280,
                height=800,
                settle_ms=1000,
                timeout_sec=5,
                allow_file_url=False,
                dry_run=False,
                allow_unproxied_engine=True,
            )
            self.assertEqual(record["status"], "skipped")

    def test_direct_url_navigation_to_public_host_still_captures(self) -> None:
        # 直接ナビゲーションの SSRF チェック・既定サイト許可リスト検証が公開ホスト
        # の正常系まで壊していないことを確認する（実 DNS 解決は `_check_public_host`
        # をモックして避け、許可リストはこのテストの URL だけに差し替える）。
        with tempfile.TemporaryDirectory() as tmp:
            site = cs.Site(
                site_id="ssrf-public-ok", url="https://example.invalid/a", category="static", catalog_id="z1"
            )
            with (
                mock.patch.object(cs, "_check_public_host"),
                mock.patch.object(cs, "DIRECT_NAVIGATION_ALLOWED_URLS", frozenset({site.url})),
            ):
                record = cs.capture_one(
                    site,
                    "chromium",
                    [sys.executable, str(FAKE_ENGINE), "--out", "{out}", "--url", "{url}"],
                    out_dir=Path(tmp),
                    snapshots_dir=Path(tmp) / "snapshots",
                    chromium_bin=None,
                    width=1280,
                    height=800,
                    settle_ms=1000,
                    timeout_sec=5,
                    allow_file_url=False,
                    dry_run=False,
                    allow_unproxied_engine=True,
                )
            self.assertEqual(record["status"], "ok")

    def test_output_path_for_valid_site_id_stays_under_out_dir(self) -> None:
        # site_id は load_sites の正規表現で `../` 等を既に拒否しているが（上記
        # test_rejects_path_traversal_style_id）、capture_one 自身もパストラバーサル
        # 対策として resolve() 後の出力先が out-dir 配下にあることを確認している。
        # 正当な site_id では例外にならないことをここで確認する。
        with tempfile.TemporaryDirectory() as tmp:
            site = cs.Site(site_id="ok-id", url="https://example.invalid", category="static", catalog_id="z1")
            record = cs.capture_one(
                site,
                "servo",
                [sys.executable, str(FAKE_ENGINE), "--out", "{out}"],
                out_dir=Path(tmp),
                snapshots_dir=Path(tmp) / "snapshots",
                chromium_bin=None,
                width=1280,
                height=800,
                settle_ms=1000,
                timeout_sec=5,
                allow_file_url=False,
                dry_run=False,
                allow_unproxied_engine=True,
            )
            self.assertEqual(record["status"], "ok")
            self.assertEqual(record["png"], "servo/ok-id.png")

    def test_snapshot_path_escaping_out_dir_via_symlinked_snapshots_dir_is_rejected(self) -> None:
        # codex P0: `snapshots_dir` 自体が out-dir 外を指す symlink だった場合、
        # `snapshot_path.resolve()` はそのリンクをたどって out-dir 外のパスに
        # 解決される。containment チェックがこれを検出して拒否することを確認する。
        with tempfile.TemporaryDirectory() as tmp:
            out_dir = Path(tmp) / "out"
            out_dir.mkdir()
            outside_dir = Path(tmp) / "outside"
            outside_dir.mkdir()
            snapshots_dir = out_dir / "snapshots"
            try:
                snapshots_dir.symlink_to(outside_dir, target_is_directory=True)
            except OSError:
                self.skipTest("symlink creation is not permitted in this environment")

            site = cs.Site(site_id="escape", url="https://example.invalid", category="static", catalog_id="z1")
            with self.assertRaises(cs.CaptureError):
                cs.capture_one(
                    site,
                    "servo",
                    ["engine", "{html_path}", "{out}"],
                    out_dir=out_dir,
                    snapshots_dir=snapshots_dir,
                    chromium_bin=None,
                    width=1280,
                    height=800,
                    settle_ms=1000,
                    timeout_sec=5,
                    allow_file_url=False,
                    dry_run=False,
                    allow_unproxied_engine=True,
                )
            self.assertEqual(list(outside_dir.iterdir()), [])


class DirectNavigationAllowlistTest(unittest.TestCase):
    """RENDER-5 / TASK-37.1: 直接ナビゲーション（`{url}`）の既定サイト許可リスト（codex P0）。

    `_check_public_host` は起動前の一時点の名前解決に基づくベストエフォートに
    過ぎず、リダイレクトまでは検証できない。`sites.json` を差し替えるだけで
    任意の https URL を直接ナビゲーションさせられないよう、既定サイトの
    URL に完全一致する場合のみ許可することを確認する。
    """

    def test_default_sites_urls_are_all_allowlisted(self) -> None:
        # sites.json とコード側の固定リストが乖離すると、リポジトリ既定の実行
        # （`--sites` 省略・既定 Chromium テンプレート）が直接ナビゲーションで
        # 全滅する。逆方向（sites.json から削除されたサイトが固定リストに残り、
        # 403・robots.txt Disallow 等で除外したはずの URL への直接ナビゲーションを
        # 許可し続ける）も検出できるよう、集合として完全一致することを確認する。
        _viewport, sites = cs.load_sites(DEFAULT_SITES_PATH, min_sites=5)
        self.assertEqual({site.url for site in sites}, cs.DIRECT_NAVIGATION_ALLOWED_URLS)

    def test_rejects_url_not_in_allowlist_even_when_public_host_check_passes(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            site = cs.Site(
                site_id="not-allowlisted", url="https://example.invalid/a", category="static", catalog_id="z1"
            )
            with (
                mock.patch.object(cs, "_check_public_host"),
                mock.patch.object(cs, "DIRECT_NAVIGATION_ALLOWED_URLS", frozenset()),
            ):
                record = cs.capture_one(
                    site,
                    "chromium",
                    [sys.executable, str(FAKE_ENGINE), "--out", "{out}", "--url", "{url}"],
                    out_dir=Path(tmp),
                    snapshots_dir=Path(tmp) / "snapshots",
                    chromium_bin=None,
                    width=1280,
                    height=800,
                    settle_ms=1000,
                    timeout_sec=5,
                    allow_file_url=False,
                    dry_run=False,
                    allow_unproxied_engine=True,
                )
            self.assertEqual(record["status"], "skipped")
            self.assertFalse((Path(tmp) / "chromium" / "not-allowlisted.png").exists())

    def test_rejects_non_allowlisted_url_even_when_template_also_uses_html_path(self) -> None:
        # codex P0 再指摘: 旧実装は `elif needs_url` で `needs_html` と排他に
        # していたため、`{html_path}` と `{url}` を両方使うテンプレートでは
        # `needs_html` 分岐に入ってしまい、直接ナビゲーションの許可リスト検査
        # （`elif needs_url`）を素通りできた。`{url}` を使う限り `{html_path}`
        # の有無にかかわらず必ず許可リスト検査を通ることを確認する。
        with tempfile.TemporaryDirectory() as tmp:
            site = cs.Site(
                site_id="both-placeholders", url="https://example.invalid/a", category="static", catalog_id="z1"
            )
            with (
                mock.patch.object(cs, "fetch_snapshot", return_value=None),
                mock.patch.object(cs, "_check_public_host"),
                mock.patch.object(cs, "DIRECT_NAVIGATION_ALLOWED_URLS", frozenset()),
            ):
                record = cs.capture_one(
                    site,
                    "chromium",
                    [
                        sys.executable,
                        str(FAKE_ENGINE),
                        "--out",
                        "{out}",
                        "--url",
                        "{url}",
                        "--html",
                        "{html_path}",
                    ],
                    out_dir=Path(tmp),
                    snapshots_dir=Path(tmp) / "snapshots",
                    chromium_bin=None,
                    width=1280,
                    height=800,
                    settle_ms=1000,
                    timeout_sec=5,
                    allow_file_url=False,
                    dry_run=False,
                    allow_unproxied_engine=True,
                    proxy_url="http://127.0.0.1:1",
                )
            self.assertEqual(record["status"], "skipped")
            self.assertFalse((Path(tmp) / "chromium" / "both-placeholders.png").exists())

    def test_captures_when_template_uses_both_placeholders_and_url_is_allowlisted(self) -> None:
        # 両方の placeholder を使うテンプレートでも、URL が許可リストに含まれ
        # ていれば正常系まで壊れていないことを確認する。
        with tempfile.TemporaryDirectory() as tmp:
            site = cs.Site(
                site_id="both-placeholders-ok", url="https://example.invalid/a", category="static", catalog_id="z1"
            )
            with (
                mock.patch.object(cs, "fetch_snapshot", return_value=None),
                mock.patch.object(cs, "_check_public_host"),
                mock.patch.object(cs, "DIRECT_NAVIGATION_ALLOWED_URLS", frozenset({site.url})),
            ):
                record = cs.capture_one(
                    site,
                    "chromium",
                    [
                        sys.executable,
                        str(FAKE_ENGINE),
                        "--out",
                        "{out}",
                        "--url",
                        "{url}",
                        "--html",
                        "{html_path}",
                    ],
                    out_dir=Path(tmp),
                    snapshots_dir=Path(tmp) / "snapshots",
                    chromium_bin=None,
                    width=1280,
                    height=800,
                    settle_ms=1000,
                    timeout_sec=5,
                    allow_file_url=False,
                    dry_run=False,
                    allow_unproxied_engine=True,
                    proxy_url="http://127.0.0.1:1",
                )
            self.assertEqual(record["status"], "ok")


class DefaultChromiumTemplateTest(unittest.TestCase):
    """RENDER-5 / TASK-37.1: 既定 Chromium テンプレートの HiDPI 対策（Cursor Bugbot Medium）。"""

    def test_forces_device_scale_factor_to_one(self) -> None:
        # HiDPI ホスト（既定の `--force-device-scale-factor` が 1 でない環境）で
        # 撮影すると PNG が DPR 倍の寸法になり、viewport との寸法一致検証
        # （capture_one）で全サイトが `failed` になっていた。既定テンプレートが
        # この固定フラグを持つことを確認する。
        self.assertIn("--force-device-scale-factor=1", cs.DEFAULT_CHROMIUM_TEMPLATE)

    def test_expanded_default_template_includes_scale_factor_flag(self) -> None:
        argv = cs.expand_template(
            cs.DEFAULT_CHROMIUM_TEMPLATE,
            {
                "chromium_bin": "chromium",
                "url": "https://example.invalid/",
                "out": "/tmp/out.png",
                "width": "1280",
                "height": "800",
                "settle_ms": "5000",
                "user_data_dir": "/tmp/udd",
                "proxy": "http://127.0.0.1:12345",
            },
        )
        self.assertIn("--force-device-scale-factor=1", argv)

    def test_proxies_all_traffic_via_proxy_server_and_bypass_list_flags(self) -> None:
        # codex P0 再指摘: 撮影プロセスの通信経路そのものをプロキシへ強制する。
        # `--proxy-bypass-list=<-loopback>` が無いと Chromium は既定で loopback
        # 宛の通信をプロキシから除外してしまう。
        self.assertIn("--proxy-server={proxy}", cs.DEFAULT_CHROMIUM_TEMPLATE)
        self.assertIn("--proxy-bypass-list=<-loopback>", cs.DEFAULT_CHROMIUM_TEMPLATE)


class ArgValidationTest(unittest.TestCase):
    """RENDER-5 / TASK-37.1: `--timeout-sec` / `--settle-ms` / `--min-sites` の値検証（P1）。"""

    def _build(self) -> None:
        return cs.build_arg_parser()

    def test_rejects_nan_timeout(self) -> None:
        parser = self._build()
        with self.assertRaises(SystemExit):
            parser.parse_args(["--out-dir", "/tmp/x", "--timeout-sec", "nan"])

    def test_rejects_infinite_timeout(self) -> None:
        parser = self._build()
        with self.assertRaises(SystemExit):
            parser.parse_args(["--out-dir", "/tmp/x", "--timeout-sec", "inf"])

    def test_rejects_zero_timeout(self) -> None:
        parser = self._build()
        with self.assertRaises(SystemExit):
            parser.parse_args(["--out-dir", "/tmp/x", "--timeout-sec", "0"])

    def test_rejects_negative_settle_ms(self) -> None:
        parser = self._build()
        with self.assertRaises(SystemExit):
            parser.parse_args(["--out-dir", "/tmp/x", "--settle-ms", "-1"])

    def test_rejects_zero_min_sites(self) -> None:
        parser = self._build()
        with self.assertRaises(SystemExit):
            parser.parse_args(["--out-dir", "/tmp/x", "--min-sites", "0"])

    def test_accepts_valid_values(self) -> None:
        parser = self._build()
        args = parser.parse_args(
            ["--out-dir", "/tmp/x", "--timeout-sec", "30", "--settle-ms", "1000", "--min-sites", "3"]
        )
        self.assertEqual(args.timeout_sec, 30.0)
        self.assertEqual(args.settle_ms, 1000)
        self.assertEqual(args.min_sites, 3)


if __name__ == "__main__":
    unittest.main()
