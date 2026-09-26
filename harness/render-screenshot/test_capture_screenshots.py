"""capture_screenshots.py のユニットテスト・結合テスト（RENDER-5 / TASK-37.1）。

偽エンジン（fixtures/fake_engine.py）を使い、ネットワークにも実エンジンにも
依存せずオフラインで完結する。実機での撮影可否は #55（TASK-37.h1）の範囲。
"""

from __future__ import annotations

import io
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))

import capture_screenshots as cs  # noqa: E402

FIXTURES_DIR = Path(__file__).resolve().parent / "fixtures"
FAKE_ENGINE = FIXTURES_DIR / "fake_engine.py"
SITE_CONDITIONAL_ENGINE = FIXTURES_DIR / "site_conditional_engine.py"
DEFAULT_SITES_PATH = Path(__file__).resolve().parent / "sites.json"


def fake_engine_template(mode: str = "ok") -> list[str]:
    return [sys.executable, str(FAKE_ENGINE), "--mode", mode, "--out", "{out}", "--width", "{width}", "--height", "{height}"]


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


class ReadPngSizeTest(unittest.TestCase):
    """RENDER-5 / TASK-37.1: PNG ヘッダ検証（シグネチャ・IHDR）。"""

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


class _Resp(io.BytesIO):
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
                cs.fetch_snapshot("http://example.invalid/", dest)

    def test_truncates_oversized_response(self) -> None:
        big_body = b"x" * (cs.SNAPSHOT_MAX_BYTES + 1)

        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp) / "s.html"
            with (
                mock.patch.object(cs, "_check_public_host"),
                mock.patch.object(cs, "_open_url", return_value=_Resp(big_body)),
            ):
                with self.assertRaises(cs.SnapshotError):
                    cs.fetch_snapshot("https://example.invalid/", dest)
            self.assertFalse(dest.exists())

    def test_writes_response_body_on_success(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp) / "s.html"
            with (
                mock.patch.object(cs, "_check_public_host"),
                mock.patch.object(cs, "_open_url", return_value=_Resp(b"<html></html>")),
            ):
                cs.fetch_snapshot("https://example.invalid/", dest)
            self.assertEqual(
                dest.read_bytes(),
                b'<base href="https://example.invalid/"><html></html>',
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
                    cs.fetch_snapshot("https://example.invalid/", dest)
            self.assertEqual(outside_target.read_text(encoding="utf-8"), "do not overwrite me")

    def test_injects_base_href_even_without_head_tag(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp) / "s.html"
            with (
                mock.patch.object(cs, "_check_public_host"),
                mock.patch.object(cs, "_open_url", return_value=_Resp(b"no head here")),
            ):
                cs.fetch_snapshot("https://example.invalid/x", dest)
            self.assertEqual(
                dest.read_bytes(),
                b'<base href="https://example.invalid/x">no head here',
            )

    def test_rejects_loopback_ip_literal(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp) / "s.html"
            with self.assertRaises(cs.SnapshotError):
                cs.fetch_snapshot("https://127.0.0.1/", dest)

    def test_rejects_localhost_hostname(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp) / "s.html"
            with self.assertRaises(cs.SnapshotError):
                cs.fetch_snapshot("https://localhost/", dest)

    def test_rejects_private_ip_literal(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp) / "s.html"
            with self.assertRaises(cs.SnapshotError):
                cs.fetch_snapshot("https://10.0.0.1/", dest)

    def test_rejects_link_local_metadata_ip_literal(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp) / "s.html"
            with self.assertRaises(cs.SnapshotError):
                cs.fetch_snapshot("https://169.254.169.254/", dest)

    def test_rejects_ipv6_loopback_literal(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp) / "s.html"
            with self.assertRaises(cs.SnapshotError):
                cs.fetch_snapshot("https://[::1]/", dest)

    def test_rejects_hostname_resolving_to_private_address(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp) / "s.html"
            with mock.patch.object(
                cs.socket,
                "getaddrinfo",
                return_value=[(cs.socket.AF_INET, None, None, "", ("10.1.2.3", 443))],
            ):
                with self.assertRaises(cs.SnapshotError):
                    cs.fetch_snapshot("https://internal.example.invalid/", dest)

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
                cs.fetch_snapshot("https://example.invalid/", dest)
            self.assertTrue(dest.exists())

    def test_rejects_url_with_no_hostname(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp) / "s.html"
            with self.assertRaises(cs.SnapshotError):
                cs.fetch_snapshot("https:///path", dest)

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
            },
        )
        self.assertIn("--force-device-scale-factor=1", argv)


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
