#!/usr/bin/env python3
"""代表サイト群を Servo / Chromium の両エンジンで撮影し、PNG と結果 JSON を揃えるスクリプト。

役割・責務境界:
    TASK-37.1（ビヘイビア RENDER-5・関連 MEAS-3・MS-1）が受け持つのは
    「両エンジンで同じ条件の PNG を揃える取得スクリプト」のみ。
    RENDER-5 が求める SSIM・境界ボックス比較の算出は本モジュールの範囲外で、
    #54（TASK-37.2）の measure_ssim.py が本モジュールの出力する結果 JSON
    （`schema_version: 1`）を入力として読む契約になる。

呼び出し文脈:
    Servo 側の撮影コマンドは `--servo-cmd` で外から差し込む形にしてある。
    TASK-36（#50。Linux 実機での Servo ヘッドレスキャプチャ検証、人間が担当）は
    本 Issue の時点でまだ完了していないため、PoC-6 の `servo-embed` バイナリや
    servoshell 等の確定コマンドが決まっていなくても本スクリプトは完成させられる。
    実機で両エンジンの PNG が実際に撮れることの確認は #55（TASK-37.h1、人間が担当）
    の範囲であり、本モジュール自身はオフラインの偽エンジン（fixtures/fake_engine.py）
    を使った結合テストでしか検証していない（実機で撮れたとは主張しない。REPAIR-3）。

    `crates/fandhe-browser-render`（TASK-38 の本実装）や将来のスクリーンショット API
    には依存しない。本モジュールは Python 標準ライブラリのみで完結する
    （依存最小方針・Cargo.toml には触れない）。
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any
from urllib.parse import urlparse

# --- 定数 -------------------------------------------------------------

# site id はファイル名の一部として使うため、パストラバーサル対策と OS 間の
# 大文字小文字非区別ファイルシステムでの衝突防止を兼ねて小文字英数字・-・_ のみに限る。
SITE_ID_RE = re.compile(r"^[a-z0-9][a-z0-9_-]{0,63}$")
PLACEHOLDER_RE = re.compile(r"\{([a-zA-Z_][a-zA-Z0-9_]*)\}")

MIN_VIEWPORT = 1
MAX_VIEWPORT = 10000
MAX_SITES = 50

# スナップショット取得・アクセス確認（docs/design/site-catalog-task70.md）と同方針で、
# 偽装しない正直な UA を名乗る（SEC 系: anti-bot 回避目的のヘッダ偽装を行わない）。
SNAPSHOT_USER_AGENT = "fandhe-browser-harness/0.1 (+https://github.com/Fandhe-AI/fandhe-browser)"
SNAPSHOT_TIMEOUT_SEC = 30
SNAPSHOT_MAX_BYTES = 5 * 1024 * 1024

STDERR_TAIL_CHARS = 2000

CHROMIUM_BIN_CANDIDATES = ("chromium", "chromium-browser", "google-chrome")

DEFAULT_CHROMIUM_TEMPLATE = [
    "{chromium_bin}",
    "--headless=new",
    "--disable-gpu",
    "--hide-scrollbars",
    "--no-first-run",
    "--user-data-dir={user_data_dir}",
    "--window-size={width},{height}",
    "--virtual-time-budget={settle_ms}",
    "--screenshot={out}",
    "{url}",
]

RESULT_SCHEMA_VERSION = 1


class CaptureError(Exception):
    """このモジュール内の検証・処理エラーを表す基底例外（fail-closed で呼び出し元へ伝える）。"""


class SiteListError(CaptureError):
    """サイト一覧 JSON の検証エラー。"""


class TemplateError(CaptureError):
    """エンジンコマンドテンプレートの検証エラー（未知の placeholder 等）。"""


class SnapshotError(CaptureError):
    """`{html_path}` 用スナップショット取得のエラー（scheme 不許可・サイズ超過等）。"""


class PngError(CaptureError):
    """PNG ヘッダ検証のエラー。"""


@dataclass(frozen=True)
class Site:
    """サイト一覧 1 件分の検証済みレコード。"""

    site_id: str
    url: str
    category: str
    catalog_id: str


# --- サイト一覧の読み込み・検証 -----------------------------------------


def load_sites(path: Path, min_sites: int) -> tuple[dict[str, Any], list[Site]]:
    """サイト一覧 JSON を読み込み、検証済みの `Site` 一覧を返す。

    cdp や render 側の実撮影コマンドではなく、このスクリプト自身が入力として読む
    唯一の外部データ（サイト URL 一覧）であり、fetch 先を本リポ管理のこのファイルに
    限定することで SSRF（任意 URL への到達）を防ぐ（security.md OWASP A10）。
    """
    try:
        raw_text = path.read_text(encoding="utf-8")
    except OSError as exc:
        raise SiteListError(f"failed to read sites file: {path}: {exc}") from exc

    try:
        data = json.loads(raw_text)
    except json.JSONDecodeError as exc:
        raise SiteListError(f"invalid JSON in sites file: {path}: {exc}") from exc

    if not isinstance(data, dict):
        raise SiteListError("sites file must be a JSON object")

    if data.get("schema_version") != 1:
        raise SiteListError("sites file schema_version must be 1")

    viewport = data.get("viewport")
    if not isinstance(viewport, dict):
        raise SiteListError("sites file viewport must be an object")
    width = viewport.get("width")
    height = viewport.get("height")
    if not isinstance(width, int) or not (MIN_VIEWPORT <= width <= MAX_VIEWPORT):
        raise SiteListError(f"viewport width out of range [{MIN_VIEWPORT}, {MAX_VIEWPORT}]")
    if not isinstance(height, int) or not (MIN_VIEWPORT <= height <= MAX_VIEWPORT):
        raise SiteListError(f"viewport height out of range [{MIN_VIEWPORT}, {MAX_VIEWPORT}]")

    raw_sites = data.get("sites")
    if not isinstance(raw_sites, list) or not raw_sites:
        raise SiteListError("sites file must contain a non-empty 'sites' array")
    if len(raw_sites) > MAX_SITES:
        raise SiteListError(f"sites file exceeds the maximum of {MAX_SITES} entries")
    if len(raw_sites) < min_sites:
        raise SiteListError(
            f"sites file has {len(raw_sites)} entries, fewer than --min-sites {min_sites}"
        )

    seen_ids: set[str] = set()
    sites: list[Site] = []
    for index, entry in enumerate(raw_sites):
        if not isinstance(entry, dict):
            raise SiteListError(f"sites[{index}] must be an object")
        site_id = entry.get("id")
        url = entry.get("url")
        category = entry.get("category")
        catalog_id = entry.get("catalog_id")
        if not isinstance(site_id, str) or not SITE_ID_RE.match(site_id):
            raise SiteListError(f"sites[{index}].id is missing or invalid: {site_id!r}")
        if site_id.lower() in seen_ids:
            raise SiteListError(f"duplicate site id (case-insensitive): {site_id}")
        seen_ids.add(site_id.lower())
        if not isinstance(url, str) or not url:
            raise SiteListError(f"sites[{index}].url is missing or empty")
        parsed = urlparse(url)
        if parsed.scheme != "https":
            raise SiteListError(f"sites[{index}].url must use https: {url}")
        if not isinstance(category, str) or not category:
            raise SiteListError(f"sites[{index}].category is missing or empty")
        if not isinstance(catalog_id, str) or not catalog_id:
            raise SiteListError(f"sites[{index}].catalog_id is missing or empty")
        sites.append(Site(site_id=site_id, url=url, category=category, catalog_id=catalog_id))

    return {"width": width, "height": height}, sites


# --- テンプレート展開 ---------------------------------------------------


def template_uses(template: list[str], name: str) -> bool:
    """テンプレート中に `{name}` という placeholder が現れるかを判定する。"""
    return any(f"{{{name}}}" in arg for arg in template)


def expand_template(template: list[str], values: dict[str, str]) -> list[str]:
    """テンプレートの各 argv 要素に含まれる placeholder を `values` で置換する。

    `subprocess.run` へは常にこの戻り値（argv のリスト）をそのまま渡し、`shell=True`
    は使わない。URL やパスにシェルのメタ文字が含まれていても、置換は argv 要素の中で
    閉じるためコマンドとして解釈されない（OWASP A03 インジェクション対策）。
    未知の placeholder（`values` に無い名前）はここで検出し、実行前に止める。
    """
    expanded: list[str] = []
    for arg in template:
        for name in PLACEHOLDER_RE.findall(arg):
            if name not in values:
                raise TemplateError(f"unknown placeholder {{{name}}} in argument: {arg}")
        new_arg = arg
        for name, value in values.items():
            new_arg = new_arg.replace(f"{{{name}}}", value)
        expanded.append(new_arg)
    return expanded


# --- Chromium 実行ファイルの解決 ----------------------------------------


def resolve_chromium_bin(explicit: str | None) -> str | None:
    """`--chromium-bin` 指定、または既定の候補名から Chromium 実行ファイルを解決する。"""
    if explicit:
        return explicit
    for name in CHROMIUM_BIN_CANDIDATES:
        found = shutil.which(name)
        if found:
            return found
    return None


# --- `{html_path}` 用スナップショット取得 --------------------------------


def fetch_snapshot(
    url: str,
    dest_path: Path,
    *,
    allow_file_url: bool = False,
    timeout_sec: float = SNAPSHOT_TIMEOUT_SEC,
    max_bytes: int = SNAPSHOT_MAX_BYTES,
) -> None:
    """`{html_path}` を使うエンジンテンプレート向けに、対象 URL の HTML を 1 回取得して保存する。

    PoC-6 の `servo-embed` は HTML ファイルしか受け付けないため、URL を直接渡せる
    Chromium と条件を揃える目的でこの前処理を行う。SSRF を避けるため既定では https
    のみを許可し、`file:` はテスト用途で `allow_file_url=True` を明示したときだけ許可する。
    """
    parsed = urlparse(url)
    if parsed.scheme == "https":
        pass
    elif parsed.scheme == "file" and allow_file_url:
        pass
    else:
        raise SnapshotError(f"unsupported scheme for snapshot fetch: {url}")

    request = urllib.request.Request(url, headers={"User-Agent": SNAPSHOT_USER_AGENT})
    try:
        with urllib.request.urlopen(request, timeout=timeout_sec) as response:  # noqa: S310
            data = response.read(max_bytes + 1)
    except (urllib.error.URLError, OSError) as exc:
        raise SnapshotError(f"failed to fetch snapshot for {url}: {exc}") from exc

    if len(data) > max_bytes:
        raise SnapshotError(f"snapshot for {url} exceeds the {max_bytes} byte limit")

    dest_path.write_bytes(data)


# --- PNG ヘッダ検証 ------------------------------------------------------

PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"


def read_png_size(path: Path) -> tuple[int, int]:
    """PNG のシグネチャと IHDR チャンクだけを読み、(width, height) を返す。

    撮影プロセスの標準的な出力を軽く検証する目的であり、先頭 33 バイトだけを
    読む（画像全体のデコードは行わない。全体の内容比較は #54 の measure_ssim.py が担う）。
    """
    try:
        with path.open("rb") as fh:
            header = fh.read(33)
    except OSError as exc:
        raise PngError(f"failed to read PNG file: {path}: {exc}") from exc

    if len(header) < 24:
        raise PngError(f"file too short to be a valid PNG: {path}")
    if header[0:8] != PNG_SIGNATURE:
        raise PngError(f"invalid PNG signature: {path}")
    chunk_type = header[12:16]
    if chunk_type != b"IHDR":
        raise PngError(f"missing IHDR chunk: {path}")
    (chunk_length,) = struct.unpack(">I", header[8:12])
    if chunk_length < 13:
        raise PngError(f"IHDR chunk too short: {path}")
    (width,) = struct.unpack(">I", header[16:20])
    (height,) = struct.unpack(">I", header[20:24])
    if width == 0 or height == 0:
        raise PngError(f"PNG has zero width or height: {path}")
    return width, height


# --- 1 回の撮影 ----------------------------------------------------------


def capture_one(
    site: Site,
    engine: str,
    template: list[str],
    *,
    out_dir: Path,
    snapshots_dir: Path,
    chromium_bin: str | None,
    width: int,
    height: int,
    settle_ms: int,
    timeout_sec: float,
    allow_file_url: bool,
    dry_run: bool,
) -> dict[str, Any]:
    """1 サイト × 1 エンジンの撮影を実行し、結果レコード（dict）を返す。

    `dry_run=True` の場合は実際のプロセス実行・ファイル書き込み（スナップショット取得を
    含む）を一切行わず、展開済みコマンドだけを含む結果を返す（out-dir 配下には何も作らない）。
    """
    out_path = out_dir / engine / f"{site.site_id}.png"
    # `resolve()` した出力先が out-dir 配下にあることを確認する（パストラバーサル対策）。
    resolved_out = (out_dir / engine / f"{site.site_id}.png").resolve()
    if not resolved_out.is_relative_to(out_dir.resolve()):
        raise CaptureError(f"refusing to write outside out-dir: {resolved_out}")

    needs_html = template_uses(template, "html_path")
    needs_user_data_dir = template_uses(template, "user_data_dir")
    needs_chromium_bin = template_uses(template, "chromium_bin")

    values: dict[str, str] = {
        "url": site.url,
        "out": str(out_path),
        "width": str(width),
        "height": str(height),
        "settle_ms": str(settle_ms),
    }

    snapshot_path = snapshots_dir / f"{site.site_id}.html"
    if needs_html:
        values["html_path"] = str(snapshot_path)

    if needs_chromium_bin:
        if chromium_bin is None:
            if dry_run:
                chromium_bin = "chromium"  # 表示専用のプレースホルダ（実行はしない）
            else:
                raise CaptureError("chromium binary could not be resolved")
        values["chromium_bin"] = chromium_bin

    user_data_dir: str | None = None
    if needs_user_data_dir:
        if dry_run:
            values["user_data_dir"] = "<user-data-dir>"
        else:
            user_data_dir = tempfile.mkdtemp(prefix="fandhe-chromium-udd-")
            values["user_data_dir"] = user_data_dir

    argv = expand_template(template, values)

    if dry_run:
        return {
            "site_id": site.site_id,
            "engine": engine,
            "status": "skipped",
            "png": None,
            "width": None,
            "height": None,
            "duration_ms": 0,
            "exit_code": None,
            "stderr_tail": "",
            "command": argv,
        }

    out_path.parent.mkdir(parents=True, exist_ok=True)

    if needs_html:
        snapshot_path.parent.mkdir(parents=True, exist_ok=True)
        try:
            fetch_snapshot(site.url, snapshot_path, allow_file_url=allow_file_url)
        except SnapshotError as exc:
            return {
                "site_id": site.site_id,
                "engine": engine,
                "status": "skipped",
                "png": None,
                "width": None,
                "height": None,
                "duration_ms": 0,
                "exit_code": None,
                "stderr_tail": str(exc)[-STDERR_TAIL_CHARS:],
                "command": argv,
            }

    start = time.monotonic()
    try:
        proc = subprocess.run(  # noqa: S603
            argv,
            timeout=timeout_sec,
            capture_output=True,
            shell=False,
            check=False,
        )
        duration_ms = int((time.monotonic() - start) * 1000)
        exit_code: int | None = proc.returncode
        stderr_tail = proc.stderr.decode("utf-8", errors="replace")[-STDERR_TAIL_CHARS:]
        timed_out = False
    except subprocess.TimeoutExpired as exc:
        duration_ms = int((time.monotonic() - start) * 1000)
        exit_code = None
        stderr_bytes = exc.stderr if isinstance(exc.stderr, (bytes, bytearray)) else b""
        stderr_tail = stderr_bytes.decode("utf-8", errors="replace")[-STDERR_TAIL_CHARS:]
        timed_out = True
    finally:
        if user_data_dir is not None:
            shutil.rmtree(user_data_dir, ignore_errors=True)

    if timed_out:
        return {
            "site_id": site.site_id,
            "engine": engine,
            "status": "timeout",
            "png": None,
            "width": None,
            "height": None,
            "duration_ms": duration_ms,
            "exit_code": None,
            "stderr_tail": stderr_tail,
            "command": argv,
        }

    status = "failed"
    png_width: int | None = None
    png_height: int | None = None
    png_rel: str | None = None
    if exit_code == 0 and out_path.exists():
        try:
            png_width, png_height = read_png_size(out_path)
            status = "ok"
            png_rel = out_path.relative_to(out_dir).as_posix()
        except PngError as exc:
            stderr_tail = (stderr_tail + f"\n{exc}")[-STDERR_TAIL_CHARS:]

    return {
        "site_id": site.site_id,
        "engine": engine,
        "status": status,
        "png": png_rel,
        "width": png_width,
        "height": png_height,
        "duration_ms": duration_ms,
        "exit_code": exit_code,
        "stderr_tail": stderr_tail,
        "command": argv,
    }


# --- 結果 JSON の書き出し -------------------------------------------------


def write_result(
    out_dir: Path,
    *,
    viewport: dict[str, int],
    sites: list[Site],
    captures: list[dict[str, Any]],
    partial: bool,
) -> Path:
    """撮影結果を `<out-dir>/capture-result.json` へ書き出す（#54 の入力契約）。"""
    result_path = out_dir / "capture-result.json"
    payload: dict[str, Any] = {
        "schema_version": RESULT_SCHEMA_VERSION,
        "generated_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "viewport": viewport,
        "sites": [
            {
                "id": site.site_id,
                "url": site.url,
                "category": site.category,
                "catalog_id": site.catalog_id,
            }
            for site in sites
        ],
        "captures": captures,
    }
    if partial:
        payload["partial"] = True
    text = json.dumps(payload, ensure_ascii=False, indent=2) + "\n"
    result_path.write_text(text, encoding="utf-8", newline="\n")
    return result_path


# --- CLI -----------------------------------------------------------------


def _parse_command_template(raw: str, *, arg_name: str) -> list[str]:
    try:
        parsed = json.loads(raw)
    except json.JSONDecodeError as exc:
        raise CaptureError(f"{arg_name} must be a JSON array of strings: {exc}") from exc
    if not isinstance(parsed, list) or not parsed or not all(isinstance(x, str) for x in parsed):
        raise CaptureError(f"{arg_name} must be a non-empty JSON array of strings")
    return parsed


def build_arg_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=(
            "Capture PNG screenshots of representative sites with Servo and Chromium "
            "under matching conditions (RENDER-5 / TASK-37.1)."
        )
    )
    parser.add_argument(
        "--sites",
        type=Path,
        default=Path(__file__).resolve().parent / "sites.json",
        help="Path to the sites list JSON (default: sites.json next to this script).",
    )
    parser.add_argument("--out-dir", type=Path, required=True, help="Output directory.")
    parser.add_argument(
        "--engines",
        default="servo,chromium",
        help="Comma-separated list of engines to capture (default: servo,chromium).",
    )
    parser.add_argument(
        "--servo-cmd",
        default=None,
        help="Servo command template as a JSON array string, e.g. '[\"servoshell\",\"{url}\"]'.",
    )
    parser.add_argument(
        "--chromium-cmd",
        default=None,
        help="Chromium command template as a JSON array string (default: built-in template).",
    )
    parser.add_argument(
        "--chromium-bin",
        default=None,
        help="Chromium executable path (default: auto-detect chromium/chromium-browser/google-chrome).",
    )
    parser.add_argument(
        "--timeout-sec", type=float, default=90, help="Per-capture timeout in seconds (default: 90)."
    )
    parser.add_argument(
        "--settle-ms", type=int, default=5000, help="Render settle time in milliseconds (default: 5000)."
    )
    parser.add_argument(
        "--min-sites", type=int, default=5, help="Minimum number of sites required (default: 5)."
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print expanded commands without executing them or writing any files.",
    )
    parser.add_argument(
        "--allow-file-url",
        action="store_true",
        help="Allow file: URLs when fetching {html_path} snapshots (test fixtures only).",
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_arg_parser()
    args = parser.parse_args(argv)

    try:
        engines = [e.strip() for e in args.engines.split(",") if e.strip()]
        if not engines:
            raise CaptureError("--engines must name at least one engine")

        templates: dict[str, list[str]] = {}
        if "servo" in engines:
            if not args.servo_cmd:
                raise CaptureError("--servo-cmd is required to capture the servo engine")
            templates["servo"] = _parse_command_template(args.servo_cmd, arg_name="--servo-cmd")
        if "chromium" in engines:
            if args.chromium_cmd:
                templates["chromium"] = _parse_command_template(
                    args.chromium_cmd, arg_name="--chromium-cmd"
                )
            else:
                templates["chromium"] = list(DEFAULT_CHROMIUM_TEMPLATE)

        unknown_engines = set(engines) - {"servo", "chromium"}
        if unknown_engines:
            raise CaptureError(f"unknown engine(s): {', '.join(sorted(unknown_engines))}")

        viewport, sites = load_sites(args.sites, args.min_sites)
    except CaptureError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2

    chromium_bin: str | None = None
    if "chromium" in engines and template_uses(templates["chromium"], "chromium_bin"):
        chromium_bin = resolve_chromium_bin(args.chromium_bin)
        if chromium_bin is None and not args.dry_run:
            print(
                "error: chromium binary not found (tried chromium, chromium-browser, google-chrome; "
                "use --chromium-bin to specify one explicitly)",
                file=sys.stderr,
            )
            return 2

    out_dir: Path = args.out_dir
    if not args.dry_run:
        out_dir.mkdir(parents=True, exist_ok=True)
    snapshots_dir = out_dir / "snapshots"

    captures: list[dict[str, Any]] = []
    try:
        for engine in engines:
            template = templates[engine]
            for site in sites:
                record = capture_one(
                    site,
                    engine,
                    template,
                    out_dir=out_dir,
                    snapshots_dir=snapshots_dir,
                    chromium_bin=chromium_bin if engine == "chromium" else None,
                    width=viewport["width"],
                    height=viewport["height"],
                    settle_ms=args.settle_ms,
                    timeout_sec=args.timeout_sec,
                    allow_file_url=args.allow_file_url,
                    dry_run=args.dry_run,
                )
                captures.append(record)
    except CaptureError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2

    if args.dry_run:
        for record in captures:
            print(f"[{record['engine']}] {record['site_id']}: {' '.join(record['command'])}")
        print("dry-run: no files were written")
        return 0

    write_result(out_dir, viewport=viewport, sites=sites, captures=captures, partial=len(engines) < 2)

    # 受入基準は「代表サイト分の PNG を両エンジンで出力できること」であり、個々の
    # サイトの失敗を即座に fail-closed とはしない。指定した各エンジンについて
    # ok なサイト数が --min-sites 以上であれば成功とみなす（`--engines chromium` の
    # ように 1 エンジンだけを指定した実行でも、その 1 エンジンが基準を満たせば
    # exit 0 になる。その場合は結果 JSON に "partial": true を付け、受入基準
    # そのものは両エンジンでの実行が必要である旨を README に明記する）。
    ok_counts = {engine: 0 for engine in engines}
    for record in captures:
        if record["status"] == "ok":
            ok_counts[record["engine"]] += 1

    all_engines_reached_min = all(count >= args.min_sites for count in ok_counts.values())
    return 0 if all_engines_reached_min else 1


if __name__ == "__main__":
    sys.exit(main())
