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


class _Resp(io.BytesIO):
    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False


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
        with tempfile.TemporaryDirectory() as tmp:
            site = cs.Site(site_id="no-bin", url="https://example.invalid", category="static", catalog_id="z1")
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
