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
import html
import http.server
import ipaddress
import json
import math
import os
import re
import select
import signal
import shutil
import socket
import socketserver
import struct
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
import zlib
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

# --engines / --timeout-sec / --settle-ms / --min-sites の CLI 引数検証に使う上限。
# 無制限値（NaN・inf・極端に大きい値）でタイムアウト待ちや DoS を招かないための
# fail-closed な境界（security.md OWASP「不安全な設計」）。
MIN_TIMEOUT_SEC = 0.001
MAX_TIMEOUT_SEC = 3600.0
MAX_SETTLE_MS = 600_000

# スナップショット取得・アクセス確認（docs/design/site-catalog-task70.md）と同方針で、
# 偽装しない正直な UA を名乗る（SEC 系: anti-bot 回避目的のヘッダ偽装を行わない）。
SNAPSHOT_USER_AGENT = "fandhe-browser-harness/0.1 (+https://github.com/Fandhe-AI/fandhe-browser)"
SNAPSHOT_TIMEOUT_SEC = 30
SNAPSHOT_MAX_BYTES = 5 * 1024 * 1024

# 撮影済み PNG を検証のため読み込む際の上限（Cursor/codex P1）。エンジンが暴走・
# 破損して巨大ファイルを書き出した場合でも `read_png_size` が `read_bytes()` で
# 無制限にメモリへ確保しないよう、サイズ超過は `stat` の時点で `failed` として
# 弾く（security.md OWASP「不安全な設計」）。1280x800 相当の非圧縮 RGBA
# （約 4 MiB）を大きく上回る値として 64 MiB を採用する。
MAX_PNG_BYTES = 64 * 1024 * 1024

# IDAT を展開した生ピクセルデータの上限（codex P1: 解凍爆弾対策）。IHDR の
# 幅・高さ・色タイプ・ビット深度から計算される期待展開サイズがこれを超える
# 場合は展開を試みずに拒否する。1280x800 の RGBA（フィルタバイト込み）で
# 約 4.1 MiB なので、代表サイトの viewport を大きく上回る余裕を見て 256 MiB
# とする。
MAX_PNG_RAW_BYTES = 256 * 1024 * 1024

STDERR_TAIL_CHARS = 2000

# ローカル転送プロキシ（`start_filtering_proxy`）の設定。撮影プロセス（Chromium 等）
# の全通信をここで宛先フィルタする（codex P0 再指摘: 最初の URL だけを検証しても
# ページが読み込むサブリソース・リダイレクト・以後の遷移までは防げない）。
PROXY_BIND_HOST = "127.0.0.1"
# CONNECT（https）・絶対 URI の HTTP フォワードのいずれも、代表サイトの通常の
# Web 閲覧で使う 80/443 のみに限定する（fail-closed。それ以外のポートは 403）。
PROXY_ALLOWED_PORTS = frozenset({80, 443})
# 同時接続数の上限（DoS 対策）。Chromium は 1 プロキシあたり既定で最大 32 本
# 程度のソケットを張るため、余裕を見て 64 とする。
PROXY_MAX_CONNECTIONS = 64
# 接続・中継が沈黙したまま固着しないためのアイドルタイムアウト（秒）。
PROXY_IDLE_TIMEOUT_SEC = 30
# 1 接続あたりの転送量上限（往復合計。バイト）。無制限のリソース確保を防ぐ
# （security.md OWASP「不安全な設計」）。HTTP/2 は 1 本の CONNECT トンネルで
# サイト全体を多重化しうるため、単一ページ分として余裕を見て 256 MiB とする。
PROXY_MAX_BYTES_PER_CONNECTION = 256 * 1024 * 1024
# リクエストボディの上限（バイト）。
PROXY_MAX_REQUEST_BODY_BYTES = 32 * 1024 * 1024

CHROMIUM_BIN_CANDIDATES = ("chromium", "chromium-browser", "google-chrome")

DEFAULT_CHROMIUM_TEMPLATE = [
    "{chromium_bin}",
    "--headless=new",
    "--disable-gpu",
    "--hide-scrollbars",
    "--no-first-run",
    "--user-data-dir={user_data_dir}",
    "--window-size={width},{height}",
    # HiDPI ホスト（`--force-device-scale-factor` 既定 1 でない環境）で撮影すると
    # PNG が要求した viewport の DPR 倍の寸法になり、`read_png_size` 後の寸法一致
    # 検証（capture_one）で全サイトが `failed` になる（Cursor Bugbot Medium）。
    # デバイススケールを 1 に固定し、`{width}x{height}` の PNG を安定して得る。
    "--force-device-scale-factor=1",
    # `start_filtering_proxy` が起動するローカル転送プロキシへ全通信を強制する
    # （codex P0 再指摘: 最初の URL の検証だけではサブリソース・リダイレクト先を
    # 防げない）。`--proxy-bypass-list=<-loopback>` は Chromium が暗黙に持つ
    # loopback 宛のプロキシ除外を無効化し、`127.0.0.1` 等への接続もプロキシ経由に
    # する（除外したままだと内部の loopback サービスへ直接到達できてしまう）。
    "--proxy-server={proxy}",
    "--proxy-bypass-list=<-loopback>",
    "--virtual-time-budget={settle_ms}",
    "--screenshot={out}",
    "{url}",
]

# `{url}` を直接エンジンへ渡す直接ナビゲーション経路（既定 Chromium テンプレート等）で
# 許可する URL の固定リスト（codex P0）。`_check_public_host` はエンジン起動前の
# 一時点の名前解決に基づくベストエフォートに過ぎず、別プロセスのブラウザが公開
# ホストから内部アドレスへリダイレクトされた場合の遷移までは検証できない。
# `--sites` を差し替えれば任意の https URL がこの経路を通ってしまうと SSRF 防御の
# 迂回になるため、直接ナビゲーションは `sites.json`（TASK-37.1 の代表サイト一覧）
# に載る既定サイトの URL に完全一致する場合のみ許可し、それ以外は
# `{html_path}` 経由のスナップショット取得（`fetch_snapshot`。取得後にリダイレクト
# 先も検証する `_PublicOnlyRedirectHandler` を通る）に限定する。
# `sites.json` を更新した場合はこの定数も合わせて更新する必要があり、
# `test_default_sites_urls_are_all_allowlisted`（test_capture_screenshots.py）が
# 乖離を検出する。
#
# 検討したが採用しなかった多層防御: Chromium の `--host-resolver-rules` は
# ホスト名の解決先を固定できるが、任意の応答先 IP を「グローバルユニキャストの
# 範囲に強制する」機能ではなく、`file:`/カスタムスキームへの遷移も塞がない。
# 「確実に防げる」と言えない対策をコメントだけで防げるかのように書かない
# （REPAIR-3）。
DIRECT_NAVIGATION_ALLOWED_URLS: frozenset[str] = frozenset(
    {
        "https://en.wikipedia.org/wiki/Rust_(programming_language)",
        "https://doc.rust-lang.org/book/",
        "https://docs.python.org/3/",
        "https://news.ycombinator.com",
        "https://react.dev",
        "https://developer.mozilla.org/en-US/docs/Web/JavaScript",
    }
)

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


def load_sites(
    path: Path, min_sites: int, *, allow_file_url: bool = False
) -> tuple[dict[str, Any], list[Site]]:
    """サイト一覧 JSON を読み込み、検証済みの `Site` 一覧を返す。

    cdp や render 側の実撮影コマンドではなく、このスクリプト自身が入力として読む
    唯一の外部データ（サイト URL 一覧）であり、fetch 先を本リポ管理のこのファイルに
    限定することで SSRF（任意 URL への到達）を防ぐ（security.md OWASP A10）。
    `allow_file_url` は `--allow-file-url`（テスト用途）から渡され、`file:` の
    サイト URL を許可する（`capture_one` 側で `{html_path}` テンプレート・
    `fetch_snapshot` に既にある同名オプションと揃える。P2: これが無いと
    `--allow-file-url` を指定してもサイト一覧の https 限定検証で常に弾かれ、
    README が案内するテスト用フラグが機能しなかった）。
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
        if parsed.scheme == "file" and allow_file_url:
            pass
        elif parsed.scheme != "https":
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


# --- symlink を追随しないファイル書き込み ---------------------------------


def _write_bytes_nofollow(path: Path, data: bytes) -> None:
    """`path` がシンボリックリンクなら追随せず拒否し、通常ファイルとして書き込む。

    `--out-dir` を使い回す再実行で `snapshots/<site_id>.html`（や結果 JSON）が
    既に外部ファイルへの symlink になっていた場合、素朴な `Path.write_bytes` は
    リンクをたどってそのファイルを取得データで上書きしてしまう（codex P0）。
    `os.open` に `O_NOFOLLOW`（Linux/macOS）を渡すことで最終コンポーネントの
    symlink 追随を OS レベルで拒否する。Windows には `O_NOFOLLOW` が無いため、
    そちらでは事前の `Path.is_symlink()`（`lstat` 相当）で明示的に拒否する
    （チェックと open の間の TOCTOU は残るが、通常運用では出力先に symlink を
    作らないため許容する）。呼び出し元の期待する例外型（`SnapshotError` /
    `CaptureError` 等）へは呼び出し元で変換する。
    """
    if path.is_symlink():
        raise OSError(f"refusing to write through an existing symlink: {path}")
    # `O_NOFOLLOW` は Linux/macOS でのみ存在する（Windows では 0 扱い）。
    # `O_BINARY`（Windows のみ）は CRT のテキストモード（`\n` を `\r\n` へ変換する
    # 挙動）を抑止し、内部データファイルの改行は LF 固定という規約
    # （coding-rust.md）を Windows でも保つ。
    flags = os.O_WRONLY | os.O_CREAT | os.O_TRUNC | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_BINARY", 0)
    fd = os.open(path, flags, 0o600)
    with os.fdopen(fd, "wb") as fh:
        fh.write(data)


# --- `{html_path}` 用スナップショット取得 --------------------------------


def _resolve_public_addresses(hostname: str | None, port: int, *, context: str) -> list[tuple[str, int]]:
    """ホスト名を解決し、すべての解決先アドレスがグローバルユニキャストであることを確認したうえで
    検証済みの `(ip, port)` の一覧を返す。

    SSRF 対策（security.md OWASP A10・security.md「プロファイル境界」とは別軸の
    サーバー側リクエスト偽造防止）。IP リテラルはそのまま検証し、ホスト名は
    `getaddrinfo` で解決した「すべて」の候補アドレスが `is_global` であることを
    要求する（マルチホームでの部分的な内部アドレス混在を fail-closed に弾く）。
    DNS 応答は検証後に変わりうる（DNS リバインディング）ため、これは名前解決
    時点のベストエフォートであり完全な対策ではない。呼び出し元は本関数が返した
    IP へ直接接続し、ホスト名を再解決してはならない（`_ProxyRequestHandler` は
    これを守って接続する。DNS リバインディング対策として、検証と接続の間で
    別の名前解決を挟まないことが重要）。
    """
    if not hostname:
        raise SnapshotError(f"{context}: URL has no hostname")

    try:
        literal = ipaddress.ip_address(hostname)
    except ValueError:
        literal = None

    if literal is not None:
        if not literal.is_global:
            raise SnapshotError(f"{context}: refusing non-global IP literal: {hostname}")
        return [(hostname, port)]

    try:
        infos = socket.getaddrinfo(hostname, port, type=socket.SOCK_STREAM)
    except OSError as exc:
        raise SnapshotError(f"{context}: failed to resolve hostname {hostname}: {exc}") from exc

    if not infos:
        raise SnapshotError(f"{context}: hostname resolved to no addresses: {hostname}")

    addresses: list[tuple[str, int]] = []
    for info in infos:
        addr = info[4][0]
        try:
            resolved = ipaddress.ip_address(addr)
        except ValueError:
            raise SnapshotError(f"{context}: unparseable resolved address {addr!r}") from None
        if not resolved.is_global:
            raise SnapshotError(
                f"{context}: hostname {hostname} resolves to a non-global address: {addr}"
            )
        addresses.append((addr, port))
    return addresses


def _check_public_host(hostname: str | None, *, context: str) -> None:
    """ホスト名がループバック・プライベート・リンクローカル等の内部アドレスでないことを確認する。

    `_resolve_public_addresses` の判定結果（例外の送出可否）だけを使い、解決済み
    アドレスの一覧は使わない呼び出し元（`fetch_snapshot` の最初の URL・直接
    ナビゲーションの最初の URL）向けの薄いラッパー。これらは「最初の 1 リクエスト」
    しか見ておらず、撮影されたページが読み込むサブリソースやリダイレクト・
    エンジンが以後たどる遷移までは検証できない（codex P0 再指摘）。撮影プロセス
    （Chromium 等）が行う全通信の宛先を制限するのは、この関数ではなく
    `start_filtering_proxy` が起動するローカル転送プロキシの役目である。
    """
    _resolve_public_addresses(hostname, 0, context=context)


class _ForcedProxyHandler(urllib.request.ProxyHandler):
    """`no_proxy`/`NO_PROXY` 環境変数によるバイパスを無視し、http/https の
    リクエストを必ずこのプロキシへルーティングする `ProxyHandler`。

    既定の `urllib.request.ProxyHandler.proxy_open` は `urllib.request.
    proxy_bypass`（`no_proxy`/`NO_PROXY` 環境変数を参照する）が真を返すと
    素通りし、`ProxyHandler` に明示的な辞書を渡していても直接接続してしまう。
    CI や開発機で `NO_PROXY=*`（あるいは対象ホストを含む値）が設定されている
    だけで `fetch_snapshot` の「撮影プロセス（この場合はスナップショット取得
    自体）の通信を必ずローカル転送プロキシへ強制する」という意図を無効化
    しうる（codex P0 再指摘の実質的な迂回経路）。本ハーネスが渡す
    `proxy_url` は常に認証なしの `http://127.0.0.1:<port>` である前提で、
    `no_proxy` チェックを行わない最小限の実装に置き換える。
    """

    def proxy_open(self, req, proxy, type):  # noqa: A002, ANN001, ANN201
        req.set_proxy(urlparse(proxy).netloc, "http")
        return None


class _PublicOnlyRedirectHandler(urllib.request.HTTPRedirectHandler):
    """リダイレクト先にも SSRF チェック（scheme・内部アドレス）を適用する `HTTPRedirectHandler`。

    既定の `urlopen` はリダイレクト先を無条件に辿るため、https の公開サイトから
    `http://169.254.169.254/` 等の内部アドレスへ転送するレスポンスを SSRF の
    踏み台にされうる。`redirect_request` で毎回検証し、拒否時は `SnapshotError`
    を送出してリダイレクトを止める（例外は `OSError` を継承しないため、
    呼び出し元の `except (URLError, OSError)` を素通りして呼び出し元へ伝わる）。
    """

    def redirect_request(self, req, fp, code, msg, headers, newurl):  # noqa: N802, ANN001, ANN201
        parsed = urlparse(newurl)
        if parsed.scheme != "https":
            raise SnapshotError(f"redirect to non-https URL is not allowed: {newurl}")
        _check_public_host(parsed.hostname, context=f"redirect target {newurl}")
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def _open_url(request: urllib.request.Request, timeout_sec: float, *, proxy_url: str):  # noqa: ANN201
    """SSRF チェック付きリダイレクトハンドラを組み込んだ opener で `request` を開く。

    `proxy_url`（`start_filtering_proxy` が起動したローカル転送プロキシの URL）を
    `_ForcedProxyHandler`（`no_proxy` 環境変数を無視する `ProxyHandler`）で
    http/https の両方に強制する（codex P0 再指摘）。`_check_public_host` は
    DNS 解決結果を検証するだけで、実際の接続は `opener.open` が改めて名前
    解決して行っていたため、検証後に DNS 応答が内部アドレスへ変わる（DNS
    リバインディング）と `{html_path}` 経由で内部サービスへ到達できてしまって
    いた。プロキシ経由にすることで、実際の接続先 IP の検証と接続を同じ場所
    （プロキシの `_resolve_public_addresses` → 検証済み IP への直接接続）に
    一本化する。https は `CONNECT` になるため、TLS の SNI・`Host` ヘッダは
    元のホスト名のまま維持され、プロキシは宛先ホスト名の妥当性のみを判定して
    接続先 IP を差し替える。リダイレクト先も `CONNECT`/フォワードのたびに
    プロキシが検証するため、各ホップで守られる。`_PublicOnlyRedirectHandler`
    による事前検査はここでは冗長になるが、多層防御として残す。

    テストからは本関数をモックすることで、実ネットワークにも `getaddrinfo` にも
    依存せず `fetch_snapshot` の呼び出し経路を検証できる。
    """
    proxy_handler = _ForcedProxyHandler({"http": proxy_url, "https": proxy_url})
    opener = urllib.request.build_opener(proxy_handler, _PublicOnlyRedirectHandler)
    return opener.open(request, timeout=timeout_sec)  # noqa: S310


def fetch_snapshot(
    url: str,
    dest_path: Path,
    *,
    proxy_url: str,
    allow_file_url: bool = False,
    timeout_sec: float = SNAPSHOT_TIMEOUT_SEC,
    max_bytes: int = SNAPSHOT_MAX_BYTES,
) -> None:
    """`{html_path}` を使うエンジンテンプレート向けに、対象 URL の HTML を 1 回取得して保存する。

    PoC-6 の `servo-embed` は HTML ファイルしか受け付けないため、URL を直接渡せる
    Chromium と条件を揃える目的でこの前処理を行う。SSRF を避けるため既定では https
    かつループバック・プライベート・リンクローカル等の内部アドレスではないホストのみを
    許可する（`_check_public_host`。事前検査で、内部アドレス宛の無駄なプロキシ
    往復を避けつつ分かりやすい理由を返す）。実際の取得は `proxy_url`
    （`start_filtering_proxy` が起動したローカル転送プロキシ。必須引数）を必ず
    経由し、直接 `socket` へ接続する経路は残さない（codex P0 再指摘。DNS
    リバインディング対策の詳細は `_open_url` 参照）。`file:` はテスト用途で
    `allow_file_url=True` を明示したときだけ許可し、`file:` はプロキシの対象外
    （`ProxyHandler` は http/https のみを差し替える）のため通常どおり
    `FileHandler` で読む。

    保存した HTML はファイル URL（`file://` 経由の相対パス基準）になるため、
    Chromium が直接開く元の https URL とは相対リンク・相対リソースの解決基準が
    異なる。この差を縮めるため保存後の HTML へ `<base href="...">` を注入する
    （`inject_base_href`）。`url` がリダイレクトされた場合、埋め込むのは
    `response.geturl()` が返す最終 URL であって最初の `url` ではない
    （Cursor Medium 再指摘: 最終的な本文を保存するのに base href が
    リダイレクト前の URL のままだと、相対 URL のリソースが誤った場所を
    基準に解決される）。JS が動的に発行するリクエストの起点までは揃わない
    ため完全な条件一致ではない（実装済みを装わない。REPAIR-3）。
    """
    parsed = urlparse(url)
    if parsed.scheme == "https":
        _check_public_host(parsed.hostname, context=f"snapshot URL {url}")
    elif parsed.scheme == "file" and allow_file_url:
        pass
    else:
        raise SnapshotError(f"unsupported scheme for snapshot fetch: {url}")

    request = urllib.request.Request(url, headers={"User-Agent": SNAPSHOT_USER_AGENT})
    try:
        with _open_url(request, timeout_sec, proxy_url=proxy_url) as response:
            # リダイレクトを辿った後の最終 URL（Cursor Medium 再指摘）。
            # `inject_base_href` に常に最初の `url` を渡すと、リダイレクト先の
            # ページなのに相対 URL がリダイレクト前の場所を基準に解決されてしまう。
            final_url = response.geturl() or url
            data = response.read(max_bytes + 1)
    except (urllib.error.URLError, OSError) as exc:
        raise SnapshotError(f"failed to fetch snapshot for {url}: {exc}") from exc

    if len(data) > max_bytes:
        raise SnapshotError(f"snapshot for {url} exceeds the {max_bytes} byte limit")

    try:
        _write_bytes_nofollow(dest_path, inject_base_href(data, final_url))
    except OSError as exc:
        raise SnapshotError(f"failed to write snapshot file: {dest_path}: {exc}") from exc


# `(?![a-zA-Z0-9_-])` は `<head>` 直後がタグ名を構成する文字でないことを要求し、
# `<header>` のような別要素まで拾わないようにする（codex/Bugbot P2: 素朴な
# `<head[^>]*>` は `<header>` にも一致し、`<head>` を持たず `<header>` を含む
# HTML で `<base>` の挿入位置がずれていた）。
HEAD_TAG_RE = re.compile(rb"<head(?![a-zA-Z0-9_-])[^>]*>", re.IGNORECASE)


def inject_base_href(html_bytes: bytes, url: str) -> bytes:
    """保存した HTML の先頭（`<head>` 直後、無ければ先頭）に `<base href="{url}">` を挿む。

    `fetch_snapshot` が保存するのはローカルファイルであり、そのまま撮影すると
    相対 URL（css・js・img 等）が `file://` の保存先基準で解決されてしまい、
    元の https URL を直接開く Chromium と撮影条件が食い違う（RENDER-5 の
    SSIM 比較が本来の描画差分ではなく取得経路の差分を拾ってしまう）。`<base>`
    要素で解決基準を元 URL に戻すことでこの差を縮める。`url` は `sites.json`
    の検証済みエントリだが、属性値へ埋め込む前に `html.escape` で `"` 等を
    エスケープし、万一の HTML 属性インジェクションを防ぐ（OWASP A03）。
    """
    base_tag = f'<base href="{html.escape(url, quote=True)}">'.encode("ascii", errors="xmlcharrefreplace")
    match = HEAD_TAG_RE.search(html_bytes)
    if match is None:
        return base_tag + html_bytes
    insert_at = match.end()
    return html_bytes[:insert_at] + base_tag + html_bytes[insert_at:]


# --- 撮影プロセス向けローカル転送プロキシ ---------------------------------
#
# codex P0 再指摘: `_check_public_host` は撮影対象の「最初の URL」しか検証
# しない。`{html_path}` 経由で保存したスナップショットが読み込む外部の
# script/img/css・`{url}` 直接ナビゲーションのリダイレクト先には、起動後の
# 撮影プロセス（Chromium 等）自身がそのままアクセスできてしまう。固定 URL
# リストもページの内容や遷移先までは制限しない。
#
# そこで「最初の URL の検証を積み増す」のではなく、撮影プロセスの通信経路
# そのものをこのローカル転送プロキシへ強制する（Chromium は
# `--proxy-server={proxy}` + `--proxy-bypass-list=<-loopback>`）。宛先ホストを
# 都度 `_resolve_public_addresses` で検証し、検証で得た IP （ホスト名の再解決
# はしない。DNS リバインディング対策）へ直接接続する。非公開アドレスは
# CONNECT・HTTP フォワードのいずれも 403 で拒否する。
#
# 正直に書いておくべき限界（README にも記載）:
#   - UDP（WebRTC/STUN・QUIC 等）はこのプロキシを経由しないため制限できない
#   - `file:` 等プロキシの対象外のスキームは制限できない
#   - エンジンがプロキシ設定を無視・迂回する実装だった場合は防げない
#     （`capture_one` は `{proxy}` を使わないテンプレートでの実行を既定で
#     拒否する fail-closed 側の対策を別途持つ）
#   - プロキシは認証なしで loopback にバインドするため、実行中は同じホスト上の
#     他プロセスからも到達できる（開発機でのローカル実行を前提とし、認証・
#     TLS 終端までは実装しない。将来必要になれば追加のユーザー承認事項とする）
#   - 宛先の到達可否のみをフィルタし、TLS で暗号化された CONNECT トンネルの
#     中身（実際にどの URL が取得されたか等）までは検査しない


def _split_host_port(hostport: str, default_port: int) -> tuple[str, int]:
    """`host:port` / `[ipv6]:port` / `host`（ポート省略）を分解する。"""
    if hostport.startswith("["):
        end = hostport.find("]")
        if end == -1:
            raise SnapshotError(f"malformed IPv6 host: {hostport!r}")
        host = hostport[1:end]
        rest = hostport[end + 1 :]
        if rest.startswith(":"):
            try:
                return host, int(rest[1:])
            except ValueError as exc:
                raise SnapshotError(f"malformed port in {hostport!r}") from exc
        return host, default_port
    if ":" in hostport:
        host, _, port_str = hostport.rpartition(":")
        try:
            return host, int(port_str)
        except ValueError as exc:
            raise SnapshotError(f"malformed port in {hostport!r}") from exc
    return hostport, default_port


def _connect_to_first_verified_address(addresses: list[tuple[str, int]], timeout: float) -> socket.socket:
    """検証済みアドレスへ順に接続を試み、最初に成功したソケットを返す。

    ホスト名は使わず `_resolve_public_addresses` が返した IP へ直接接続する
    （DNS リバインディング対策。ここで再度ホスト名を解決しない）。
    """
    last_exc: OSError | None = None
    for addr, port in addresses:
        try:
            return socket.create_connection((addr, port), timeout=timeout)
        except OSError as exc:
            last_exc = exc
    raise SnapshotError(f"could not connect to any resolved address: {last_exc}")


class _ProxyRequestHandler(http.server.BaseHTTPRequestHandler):
    """宛先を検証してから中継する、撮影プロセス専用のローカル転送プロキシハンドラ。

    `CONNECT`（https 用トンネル）と、絶対 URI 形式の `GET`/`HEAD`/`POST`
    （http 用フォワード）のみに対応する。origin-form のリクエストや他メソッドは
    `BaseHTTPRequestHandler` の既定動作で 501 になる。1 接続につき 1 リクエスト
    のみを扱い、`Connection: close` を強制する（keep-alive の複雑さを避ける）。
    """

    protocol_version = "HTTP/1.0"
    timeout = PROXY_IDLE_TIMEOUT_SEC
    server_version = "fandhe-filtering-proxy/0.1"
    # `BaseHTTPRequestHandler`（`socketserver.StreamRequestHandler`）は既定で
    # `rfile` をバッファリングする。CONNECT のヘッダ直後にクライアントが
    # 追加データ（TLS ClientHello 等）を続けて送ると、そのバイトはこの
    # バッファに残ったまま `_relay` の生ソケット read には現れず、トンネルの
    # 先頭が欠落しうる。`rbufsize = 0` でバッファリングを無効化する。
    rbufsize = 0

    def log_message(self, format: str, *args: object) -> None:  # noqa: A002
        # 標準エラーへ大量にログを出さない（呼び出し元の撮影ログと混在させない）。
        pass

    def _deny_if_not_public(self, host: str | None, port: int) -> list[tuple[str, int]] | None:
        if port not in PROXY_ALLOWED_PORTS:
            self.send_error(403, f"port {port} is not allowed by the filtering proxy")
            return None
        try:
            return _resolve_public_addresses(host, port, context=f"proxy target {host}:{port}")
        except SnapshotError as exc:
            self.send_error(403, str(exc))
            return None

    def do_CONNECT(self) -> None:  # noqa: N802
        try:
            host, port = _split_host_port(self.path, 443)
        except SnapshotError as exc:
            self.send_error(400, str(exc))
            return
        addresses = self._deny_if_not_public(host, port)
        if addresses is None:
            return
        try:
            upstream = _connect_to_first_verified_address(addresses, PROXY_IDLE_TIMEOUT_SEC)
        except SnapshotError as exc:
            self.send_error(502, str(exc))
            return
        try:
            self.send_response(200, "Connection Established")
            self.end_headers()
            self._relay(self.connection, upstream)
        finally:
            upstream.close()

    def _relay(self, client_sock: socket.socket, upstream_sock: socket.socket) -> None:
        """CONNECT トンネル確立後、クライアント・upstream 間を双方向に中継する。

        両ソケットはブロッキングのままにし、読み取り可能かどうかの待ち合わせ
        だけ `select` に任せる（Cursor Medium 再指摘: 両ソケットを
        non-blocking にして `sendall` していると、相手の送信バッファが
        埋まった時点で `BlockingIOError`（`OSError` のサブクラス）が飛び、
        `except OSError: return` がトンネルを即座に閉じてしまう。大きな
        転送の途中で欠落する）。ブロッキングソケットの `sendall` は送信し
        きるまで内部でブロックし続けるため、この問題が起きない。書き込み側にも
        `PROXY_IDLE_TIMEOUT_SEC` のタイムアウトを設定し、相手が全く読み出さない
        まま固着した接続だけを打ち切る。
        """
        client_sock.settimeout(PROXY_IDLE_TIMEOUT_SEC)
        upstream_sock.settimeout(PROXY_IDLE_TIMEOUT_SEC)
        total = 0
        sockets = [client_sock, upstream_sock]
        while True:
            try:
                readable, _, _ = select.select(sockets, [], [], PROXY_IDLE_TIMEOUT_SEC)
            except OSError:
                return
            if not readable:
                return  # アイドルタイムアウト
            for sock in readable:
                other = upstream_sock if sock is client_sock else client_sock
                try:
                    data = sock.recv(65536)
                except OSError:
                    return
                if not data:
                    return
                total += len(data)
                if total > PROXY_MAX_BYTES_PER_CONNECTION:
                    return
                try:
                    other.sendall(data)
                except OSError:
                    return

    def _forward(self, method: str) -> None:
        parsed = urlparse(self.path)
        if parsed.scheme != "http" or not parsed.hostname:
            self.send_error(400, "proxy requires an absolute-form http URL in the request line")
            return
        host = parsed.hostname
        try:
            # `parsed.port` はポート部が範囲外・非数値だと `ValueError` を送出する
            # （例: 撮影対象ページが `<img src="http://x:99999/">` を持つ場合）。
            # 未捕捉のまま伝播させるとハンドラスレッドが異常終了し、既定の
            # `handle_error` が撮影ログへトレースバックを撒き散らす。
            port = parsed.port or 80
        except ValueError:
            self.send_error(400, "invalid port in request-target")
            return

        if any(key.lower() == "transfer-encoding" for key in self.headers.keys()):
            self.send_error(400, "chunked request bodies are not supported by this proxy")
            return
        try:
            content_length = int(self.headers.get("Content-Length") or "0")
        except ValueError:
            self.send_error(400, "invalid Content-Length")
            return
        if content_length < 0 or content_length > PROXY_MAX_REQUEST_BODY_BYTES:
            self.send_error(413, "request body too large")
            return

        addresses = self._deny_if_not_public(host, port)
        if addresses is None:
            return

        body = self.rfile.read(content_length) if content_length else b""

        target_path = parsed.path or "/"
        if parsed.query:
            target_path += "?" + parsed.query

        outgoing_headers = []
        for key in self.headers.keys():
            lower = key.lower()
            if lower in {
                "proxy-connection",
                "connection",
                "keep-alive",
                "proxy-authorization",
                "proxy-authenticate",
                "transfer-encoding",
            }:
                continue
            outgoing_headers.append(f"{key}: {self.headers[key]}")
        outgoing_headers.append("Connection: close")
        request_bytes = (
            f"{method} {target_path} HTTP/1.1\r\n" + "\r\n".join(outgoing_headers) + "\r\n\r\n"
        ).encode("latin-1", errors="replace") + body

        try:
            upstream = _connect_to_first_verified_address(addresses, PROXY_IDLE_TIMEOUT_SEC)
        except SnapshotError as exc:
            self.send_error(502, str(exc))
            return
        try:
            upstream.settimeout(PROXY_IDLE_TIMEOUT_SEC)
            upstream.sendall(request_bytes)
            total = 0
            while True:
                chunk = upstream.recv(65536)
                if not chunk:
                    break
                total += len(chunk)
                if total > PROXY_MAX_BYTES_PER_CONNECTION:
                    break
                self.connection.sendall(chunk)
        except OSError:
            pass
        finally:
            upstream.close()
        self.close_connection = True

    def do_GET(self) -> None:  # noqa: N802
        self._forward("GET")

    def do_HEAD(self) -> None:  # noqa: N802
        self._forward("HEAD")

    def do_POST(self) -> None:  # noqa: N802
        self._forward("POST")

    def handle_one_request(self) -> None:
        # 1 接続 1 リクエストに固定する（keep-alive を実装しない簡略化）。
        super().handle_one_request()
        self.close_connection = True


class _FilteringProxyServer(socketserver.ThreadingMixIn, http.server.HTTPServer):
    """接続ごとにスレッドを割り当てるプロキシサーバー。同時接続数を上限で制御する。

    `process_request`（accept ループ側で同期的に呼ばれる）で
    `threading.BoundedSemaphore` を取得してから `ThreadingMixIn` の実装へ委譲し、
    新規スレッドを立てる。上限に達している間は accept ループ自体をブロックさせ、
    無制限にスレッドを増やさない（DoS 対策）。取得したセマフォは、実際の処理を
    行うスレッド（`process_request_thread`）の終了時に解放する。

    注意（`stop_filtering_proxy` から `shutdown()` を呼ぶ際）: 全 64 スロットが
    使用中のときに `shutdown()` を呼ぶと、accept ループは `process_request` の
    `semaphore.acquire()` でブロックしたままになり、既存の接続が終わる
    （またはアイドルタイムアウトで打ち切られる。最悪 `PROXY_IDLE_TIMEOUT_SEC`
    秒）まで停止が完了しない。撮影プロセス自体は `finally`（`capture_one`）で
    既に終了しているのが通常のため実運用上は問題にならないが、次に読む人が
    再発見しなくて済むよう明記しておく。
    """

    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, *args: Any, **kwargs: Any) -> None:
        super().__init__(*args, **kwargs)
        self._connection_semaphore = threading.BoundedSemaphore(PROXY_MAX_CONNECTIONS)

    def process_request(self, request: socket.socket, client_address: Any) -> None:
        self._connection_semaphore.acquire()
        super().process_request(request, client_address)

    def process_request_thread(self, request: socket.socket, client_address: Any) -> None:
        try:
            self.finish_request(request, client_address)
        except Exception:  # noqa: BLE001 - 1 接続の異常でサーバー全体を落とさない
            self.handle_error(request, client_address)
        finally:
            self.shutdown_request(request)
            self._connection_semaphore.release()

    def handle_error(self, request: socket.socket, client_address: Any) -> None:
        # 既定実装はフルトレースバックを標準エラーへ出力する。撮影対象の
        # ページ（信頼できない外部入力）が誘発しうるエラーで撮影ログを
        # 埋め尽くさないよう、要点だけの 1 行に絞る。
        exc = sys.exc_info()[1]
        print(f"filtering proxy: error handling request from {client_address}: {exc}", file=sys.stderr)


def start_filtering_proxy() -> tuple[_FilteringProxyServer, threading.Thread, str]:
    """撮影プロセス専用のローカル転送プロキシを起動し、`(server, thread, proxy_url)` を返す。

    `main` がキャプチャループの前に呼び出し、`finally` で必ず `stop_filtering_proxy`
    を呼んで停止する。`127.0.0.1` の空きポート（`0` 指定）にバインドするため、
    複数プロセスの同時実行でも衝突しない。
    """
    server = _FilteringProxyServer((PROXY_BIND_HOST, 0), _ProxyRequestHandler)
    host, port = server.server_address[:2]
    thread = threading.Thread(target=server.serve_forever, name="filtering-proxy", daemon=True)
    thread.start()
    return server, thread, f"http://{host}:{port}"


def stop_filtering_proxy(server: _FilteringProxyServer, thread: threading.Thread) -> None:
    """`start_filtering_proxy` が起動したプロキシを止める。"""
    server.shutdown()
    server.server_close()
    thread.join(timeout=PROXY_IDLE_TIMEOUT_SEC)


# --- PNG ヘッダ検証 ------------------------------------------------------

PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"


# 有効な (color_type, 許可される bit_depth 集合, 1 ピクセルあたりのチャンネル数)。
# PNG 仕様（RFC 2083 相当）が定める組み合わせのみを許可する。
_PNG_COLOR_TYPE_INFO: dict[int, tuple[set[int], int]] = {
    0: ({1, 2, 4, 8, 16}, 1),  # グレースケール
    2: ({8, 16}, 3),  # RGB
    3: ({1, 2, 4, 8}, 1),  # パレット
    4: ({8, 16}, 2),  # グレースケール + アルファ
    6: ({8, 16}, 4),  # RGBA
}


def _expected_raw_size(width: int, height: int, color_type: int, bit_depth: int, interlace: int) -> int:
    """IHDR の値から、フィルタバイトを含む展開後の生データサイズ（バイト）を計算する。

    `read_png_size` が IDAT を展開した結果の妥当性（解凍できるだけでなく画像
    サイズと矛盾しないこと）を検証するために使う（codex P1）。インターレース
    （Adam7）は 7 パスそれぞれが独立した行を持つため、パスごとに計算して
    合算する。
    """
    info = _PNG_COLOR_TYPE_INFO.get(color_type)
    if info is None:
        raise PngError(f"unsupported PNG color type: {color_type}")
    allowed_depths, channels = info
    if bit_depth not in allowed_depths:
        raise PngError(f"invalid bit depth {bit_depth} for color type {color_type}")

    def row_bytes(w: int) -> int:
        return (w * channels * bit_depth + 7) // 8

    if interlace == 0:
        return height * (1 + row_bytes(width))
    if interlace != 1:
        raise PngError(f"unsupported PNG interlace method: {interlace}")

    # Adam7: (x_start, y_start, x_step, y_step) の 7 パス。
    total = 0
    for x_start, y_start, x_step, y_step in (
        (0, 0, 8, 8),
        (4, 0, 8, 8),
        (0, 4, 4, 8),
        (2, 0, 4, 4),
        (0, 2, 2, 4),
        (1, 0, 2, 2),
        (0, 1, 1, 2),
    ):
        pass_w = max(0, (width - x_start + x_step - 1) // x_step)
        pass_h = max(0, (height - y_start + y_step - 1) // y_step)
        if pass_w == 0 or pass_h == 0:
            continue
        total += pass_h * (1 + row_bytes(pass_w))
    return total


def read_png_size(path: Path) -> tuple[int, int]:
    """PNG のシグネチャ・チャンク構造・IDAT の展開可否を検証し、IHDR の (width, height) を返す。

    撮影プロセスがクラッシュ・途中終了して壊れた PNG を残した場合でも、寸法だけが
    偶然一致すれば `capture_one` が "ok" と誤判定してしまう（codex P1）。そこで
    先頭 33 バイトの IHDR だけでなく、シグネチャ直後から全チャンクを順に走査し、
    各チャンクの CRC32（`zlib.crc32`）を検証しつつ末尾が `IEND` チャンクで
    ちょうど終わっている（トレイリングの欠落・余剰データが無い）ことまで確認する。

    さらに、IHDR と IEND だけが揃っていて IDAT が無い（＝画素データが存在しない）
    ファイルや、IDAT はあっても zlib として展開できない・展開結果が IHDR の
    寸法と矛盾するファイルを "ok" と誤判定しないよう、IDAT チャンクを連結して
    `zlib.decompressobj` で逐次展開し、Adler-32 検証（`decompressobj` が内部で
    行う）を含めて完走することと、展開後のバイト数が IHDR から計算した期待値と
    一致することまで確認する（codex P1 再指摘）。画素の内容比較（SSIM 等）は
    引き続き #54 の measure_ssim.py が担う（REPAIR-3）。

    展開後サイズは IHDR の値から `MAX_PNG_RAW_BYTES` 超過を検知した時点で
    打ち切り、細工した IHDR（巨大な width/height）による解凍爆弾を防ぐ。

    エンジンが暴走・破損して巨大な PNG を書き出した場合に `read_bytes()` で
    無制限にメモリへ確保しないよう、内容を読む前に `stat` でサイズを確認し
    `MAX_PNG_BYTES` を超えていれば `failed` 相当の `PngError` として拒否する
    （Cursor/codex P1）。
    """
    try:
        file_size = path.stat().st_size
    except OSError as exc:
        raise PngError(f"failed to stat PNG file: {path}: {exc}") from exc
    if file_size > MAX_PNG_BYTES:
        raise PngError(f"PNG file exceeds the {MAX_PNG_BYTES} byte limit: {path}")

    try:
        data = path.read_bytes()
    except OSError as exc:
        raise PngError(f"failed to read PNG file: {path}: {exc}") from exc

    if len(data) < len(PNG_SIGNATURE) or data[: len(PNG_SIGNATURE)] != PNG_SIGNATURE:
        raise PngError(f"invalid PNG signature: {path}")

    width: int | None = None
    height: int | None = None
    color_type: int | None = None
    bit_depth: int | None = None
    interlace: int | None = None
    idat_chunks: list[bytes] = []
    offset = len(PNG_SIGNATURE)
    seen_iend = False
    seen_non_idat_after_idat = False
    while offset < len(data):
        if offset + 8 > len(data):
            raise PngError(f"truncated PNG chunk header: {path}")
        (chunk_length,) = struct.unpack(">I", data[offset : offset + 4])
        chunk_type = data[offset + 4 : offset + 8]
        data_start = offset + 8
        data_end = data_start + chunk_length
        crc_end = data_end + 4
        if crc_end > len(data):
            raise PngError(f"truncated PNG chunk {chunk_type!r}: {path}")
        chunk_data = data[data_start:data_end]
        (stored_crc,) = struct.unpack(">I", data[data_end:crc_end])
        computed_crc = zlib.crc32(chunk_type + chunk_data) & 0xFFFFFFFF
        if stored_crc != computed_crc:
            raise PngError(f"CRC mismatch in {chunk_type!r} chunk (corrupted PNG): {path}")

        if chunk_type == b"IHDR":
            if width is not None:
                raise PngError(f"duplicate IHDR chunk: {path}")
            if chunk_length < 13:
                raise PngError(f"IHDR chunk too short: {path}")
            (width,) = struct.unpack(">I", chunk_data[0:4])
            (height,) = struct.unpack(">I", chunk_data[4:8])
            bit_depth = chunk_data[8]
            color_type = chunk_data[9]
            compression_method = chunk_data[10]
            filter_method = chunk_data[11]
            interlace = chunk_data[12]
            if compression_method != 0:
                raise PngError(f"unsupported PNG compression method: {compression_method}")
            if filter_method != 0:
                raise PngError(f"unsupported PNG filter method: {filter_method}")
        elif chunk_type == b"IDAT":
            if width is None:
                raise PngError(f"IDAT chunk before IHDR: {path}")
            if seen_non_idat_after_idat:
                raise PngError(f"non-contiguous IDAT chunks: {path}")
            idat_chunks.append(chunk_data)
        elif chunk_type == b"IEND":
            seen_iend = True
            offset = crc_end
            break
        else:
            if idat_chunks:
                seen_non_idat_after_idat = True

        offset = crc_end

    if width is None or height is None or bit_depth is None or color_type is None or interlace is None:
        raise PngError(f"missing IHDR chunk: {path}")
    if not seen_iend:
        raise PngError(f"missing IEND chunk (truncated PNG): {path}")
    if offset != len(data):
        raise PngError(f"trailing data after IEND chunk: {path}")
    if width == 0 or height == 0:
        raise PngError(f"PNG has zero width or height: {path}")
    if not idat_chunks:
        raise PngError(f"missing IDAT chunk (no pixel data): {path}")

    expected_raw_size = _expected_raw_size(width, height, color_type, bit_depth, interlace)
    if expected_raw_size > MAX_PNG_RAW_BYTES:
        raise PngError(
            f"IHDR declares a decompressed size ({expected_raw_size} bytes) exceeding the "
            f"{MAX_PNG_RAW_BYTES} byte limit: {path}"
        )

    decompressor = zlib.decompressobj()
    decoded_size = 0
    try:
        for chunk_data in idat_chunks:
            remaining = chunk_data
            while remaining:
                # 64 KiB ずつ展開し、期待サイズを超えた時点で即座に打ち切る
                # （解凍爆弾対策。IHDR 由来の期待値を信用しすぎないための二重の
                # 歯止め）。
                decoded = decompressor.decompress(remaining, 65536)
                decoded_size += len(decoded)
                if decoded_size > expected_raw_size:
                    raise PngError(f"IDAT decompresses larger than the IHDR-derived size: {path}")
                remaining = decompressor.unconsumed_tail
        # 全 IDAT を入力し終えた後も、最終ブロック確定のため追加の出力が
        # バッファに残っていることがある。`decompressobj.flush()` は残りを
        # 無制限に返す（`max_length` を取らない）ため使わず、代わりに
        # `max_length` 付きの `decompress(b"", ...)` を `eof` になるか進捗が
        # 止まるまで繰り返す（Cursor Bugbot 再指摘: `flush()` だけが解凍爆弾
        # 対策の外側に残っていた）。`allowance` は残り許容量ちょうど + 1 とし、
        # 1 バイトでも超過すれば検出できるようにする。
        while not decompressor.eof:
            allowance = expected_raw_size - decoded_size + 1
            if allowance <= 0:
                raise PngError(f"IDAT decompresses larger than the IHDR-derived size: {path}")
            decoded = decompressor.decompress(b"", allowance)
            if not decoded:
                # `eof` に達しないまま出力が止まった場合、ストリームが
                # 途中で切れている（続きの入力バイトが必要だが IDAT は
                # 使い切った）。無限ループにせずここで打ち切る。
                break
            decoded_size += len(decoded)
            if decoded_size > expected_raw_size:
                raise PngError(f"IDAT decompresses larger than the IHDR-derived size: {path}")
    except zlib.error as exc:
        raise PngError(f"IDAT is not valid zlib data (corrupted PNG): {path}: {exc}") from exc

    if not decompressor.eof or decompressor.unused_data:
        raise PngError(f"IDAT stream does not end cleanly (corrupted PNG): {path}")
    if decoded_size != expected_raw_size:
        raise PngError(
            f"decompressed IDAT size {decoded_size} does not match the IHDR-derived size "
            f"{expected_raw_size}: {path}"
        )

    return width, height


# --- 1 回の撮影 ----------------------------------------------------------


def _read_tail(path: Path, max_chars: int) -> str:
    """`path` の末尾（デコード後 `max_chars` 文字相当）を読む。stderr 保存ファイル向け。"""
    try:
        size = path.stat().st_size
    except OSError:
        return ""
    # UTF-8 は 1 文字最大 4 バイトなので、文字数の 4 倍バイトだけ末尾から読めば
    # 少なくとも `max_chars` 文字分は確保できる（全体を読まないためメモリは有界）。
    read_bytes = max_chars * 4
    try:
        with path.open("rb") as fh:
            if size > read_bytes:
                fh.seek(size - read_bytes)
            data = fh.read()
    except OSError:
        return ""
    return data.decode("utf-8", errors="replace")[-max_chars:]


def _process_group_popen_kwargs() -> dict[str, Any]:
    """撮影エンジンの子孫プロセスをまとめて終了できるよう、`Popen` にプロセスグループ／
    ジョブを与える引数を返す（Cursor Medium: `subprocess.run(..., timeout=)` は
    タイムアウト時に直接の子しか kill せず、Chromium のレンダラー・GPU プロセス
    等の子孫が残ってしまう）。

    POSIX では `start_new_session=True` で新しいセッション（プロセスグループ
    リーダー）にし、後で `os.killpg` によりグループ全体へシグナルを送れるように
    する。Windows には `killpg` 相当が無いため `CREATE_NEW_PROCESS_GROUP` を渡し、
    `_kill_process_tree` 側で `taskkill /T /F` を使ってプロセスツリーごと終了する。
    """
    if os.name == "posix":
        return {"start_new_session": True}
    return {"creationflags": subprocess.CREATE_NEW_PROCESS_GROUP}  # type: ignore[attr-defined]


def _kill_process_tree(proc: subprocess.Popen) -> None:
    """タイムアウトした撮影プロセスを、子孫を含めて終了させる（Cursor Medium）。

    直接の子だけを kill すると、Chromium のレンダラー・GPU プロセス等の子孫が
    残り、以後の撮影でプロキシの接続枠・CPU を取り合ったり、`capture_one` の
    `finally` で行う `user_data_dir` の削除（子孫がまだファイルを開いている
    場合、特に Windows でロック競合）と衝突しうる。本関数はベストエフォートで
    プロセスツリー全体の終了を試み、例外を上位へ伝播させない
    （タイムアウト処理自体を失敗させたくないため）。
    """
    try:
        if os.name == "posix":
            # `_process_group_popen_kwargs` で `start_new_session=True` を
            # 渡しているため、プロセスグループ ID は直接の子の pid と一致する。
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass  # 既に終了している
        else:
            # Windows: `CREATE_NEW_PROCESS_GROUP` だけでは子孫は終了しないため、
            # `taskkill /T`（プロセスツリー）/ `/F`（強制）を使う。
            subprocess.run(  # noqa: S603, S607
                ["taskkill", "/T", "/F", "/PID", str(proc.pid)],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=10,
                check=False,
            )
    except OSError:
        pass  # taskkill 不在等。ベストエフォート

    # シグナル送信・taskkill だけでは直接の子がゾンビのまま残るため、必ず reap する。
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        pass


def _raise_keyboard_interrupt_on_sigterm(signum: int, frame: object) -> None:  # noqa: ARG001
    """SIGTERM を `KeyboardInterrupt` に変換する signal handler（Cursor Medium 再指摘）。

    既定では SIGTERM はハンドラなしで即座にプロセスを終了させ、Python の
    `except`/`finally` を経由しないため、撮影中の子プロセス（と、新しい
    セッション／プロセスグループで起動しているためシグナルが自動では届かない
    その子孫）・`user_data_dir`・ローカル転送プロキシの後片付けが一切行われ
    ない。`KeyboardInterrupt` を送出することで、Ctrl-C（SIGINT）と同じ後片付け
    経路（`capture_one` の `except BaseException` → `_kill_process_tree`、
    `main` の `finally` → `stop_filtering_proxy`）に合流させる。signal handler
    は Python ではメインスレッドでしか登録できないため、`main` はメイン
    スレッドで実行されている場合のみ登録する。
    """
    raise KeyboardInterrupt("received SIGTERM")


def _count_common_ok_sites(captures: list[dict[str, Any]], engines: list[str]) -> int:
    """指定した全エンジンで "ok" だった site_id の共通集合の件数を返す。

    RENDER-5 が求めるのは「同じサイトを両エンジンで撮って比較する」ことなので、
    エンジンごとの ok 件数を独立に数えて `--min-sites` と比べるだけでは判定を
    誤る（codex P1）。例えば Servo がサイト 1〜5・Chromium がサイト 2〜6 で
    成功した場合、独立集計ではどちらも 5 件で `--min-sites 5` を満たすが、
    実際に両エンジンで比較できる（同じ site_id で両方 "ok" の）サイトは
    サイト 2〜5 の 4 件のみである。`engines` が 1 件のみの場合は、その 1
    エンジンの ok 件数そのものになる（`--engines chromium` 等の単一エンジン
    実行との後方互換）。
    """
    ok_site_ids_by_engine: dict[str, set[str]] = {engine: set() for engine in engines}
    for record in captures:
        if record["status"] == "ok":
            ok_site_ids_by_engine[record["engine"]].add(record["site_id"])
    return len(set.intersection(*ok_site_ids_by_engine.values()))


def _skip_result(site: Site, engine: str, argv: list[str], detail: str) -> dict[str, Any]:
    return {
        "site_id": site.site_id,
        "engine": engine,
        "status": "skipped",
        "png": None,
        "width": None,
        "height": None,
        "duration_ms": 0,
        "exit_code": None,
        "stderr_tail": detail[-STDERR_TAIL_CHARS:],
        "command": argv,
        "input": None,
    }


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
    proxy_url: str | None = None,
    allow_unproxied_engine: bool = False,
) -> dict[str, Any]:
    """1 サイト × 1 エンジンの撮影を実行し、結果レコード（dict）を返す。

    `dry_run=True` の場合は実際のプロセス実行・ファイル書き込み（スナップショット取得を
    含む）を一切行わず、展開済みコマンドだけを含む結果を返す（out-dir 配下には何も作らない）。

    `proxy_url` は `start_filtering_proxy` が起動したローカル転送プロキシの
    URL（`http://127.0.0.1:<port>`）。テンプレートが `{proxy}` を使う場合のみ
    `values["proxy"]` へ渡す。`allow_unproxied_engine=True` を明示しない限り、
    実行時（非 dry-run）に `{proxy}` を使わないテンプレートでの撮影は拒否する
    （codex P0 再指摘: 最初の URL だけの検証では、撮影プロセス自身が読み込む
    サブリソースやリダイレクト先を制限できないため、通信経路そのものを
    プロキシへ強制することを既定にする）。
    """
    out_path = out_dir / engine / f"{site.site_id}.png"
    # `resolve()` した出力先が out-dir 配下にあることを確認する（パストラバーサル対策）。
    resolved_out = (out_dir / engine / f"{site.site_id}.png").resolve()
    if not resolved_out.is_relative_to(out_dir.resolve()):
        raise CaptureError(f"refusing to write outside out-dir: {resolved_out}")

    needs_html = template_uses(template, "html_path")
    needs_url = template_uses(template, "url")
    needs_user_data_dir = template_uses(template, "user_data_dir")
    needs_chromium_bin = template_uses(template, "chromium_bin")
    needs_proxy = template_uses(template, "proxy")

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

    if needs_proxy:
        if dry_run:
            values["proxy"] = proxy_url if proxy_url is not None else "<proxy>"
        else:
            if proxy_url is None:
                raise CaptureError("template uses {proxy} but capture_one was not given a proxy_url")
            values["proxy"] = proxy_url

    if dry_run:
        if needs_user_data_dir:
            values["user_data_dir"] = "<user-data-dir>"
        argv = expand_template(template, values)
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
            "input": None,
        }

    out_path.parent.mkdir(parents=True, exist_ok=True)
    # 再実行で --out-dir を使い回した際、今回のプロセスが PNG を出力しなくても
    # 前回分が残っていて「成功」と誤判定されないよう、実行前に必ず消しておく
    # （P1: 再実行時の残置ファイル誤判定対策）。
    out_path.unlink(missing_ok=True)

    input_kind = "snapshot" if needs_html else "url"
    user_data_dir: str | None = None
    try:
        if needs_user_data_dir:
            user_data_dir = tempfile.mkdtemp(prefix="fandhe-chromium-udd-")
            values["user_data_dir"] = user_data_dir

        argv = expand_template(template, values)

        if not needs_proxy and not allow_unproxied_engine:
            # fail-closed: `{proxy}` を使わないテンプレートは、撮影プロセスの
            # 通信経路を一切フィルタしないまま実プロセスを起動することになる
            # （codex P0 再指摘）。`--allow-unproxied-engine` はネットワークを
            # 完全に遮断した環境やテスト専用の意図的なオプトインに限る。
            return _skip_result(
                site,
                engine,
                argv,
                "refusing to launch an engine without routing its traffic through the "
                "filtering proxy; add {proxy} to the command template or pass "
                "--allow-unproxied-engine for network-isolated test environments only",
            )

        if needs_html:
            # `snapshots/<site_id>.html` の保存先が out-dir 配下にあることを
            # `resolved_out`（PNG 出力先）と同じ方針で確認する（codex P0）。
            # `resolve()` は途中・末尾の symlink をすべて解決するため、
            # `snapshots_dir` 自体や `snapshot_path` が out-dir 外を指す symlink
            # の場合はここで検出できる。書き込み時点の TOCTOU は
            # `_write_bytes_nofollow` の `O_NOFOLLOW` / `is_symlink` チェックで
            # 別途防ぐ。
            resolved_snapshot = snapshot_path.resolve()
            if not resolved_snapshot.is_relative_to(out_dir.resolve()):
                raise CaptureError(f"refusing to write outside out-dir: {resolved_snapshot}")
            snapshot_path.parent.mkdir(parents=True, exist_ok=True)
            if proxy_url is None:
                # `fetch_snapshot` は必ずローカル転送プロキシを経由させる
                # （codex P0 再指摘）。エンジン自身のテンプレートが `{proxy}` を
                # 使うかどうか（`--allow-unproxied-engine`）とは独立に、この
                # スナップショット取得だけは常にプロキシが必要（`main` は
                # dry-run 以外で必ずプロキシを起動するため、実運用でここへは
                # 来ない。直接 `capture_one` を呼ぶテスト向けの防御）。
                raise CaptureError("fetch_snapshot requires a running filtering proxy (proxy_url)")
            try:
                fetch_snapshot(site.url, snapshot_path, allow_file_url=allow_file_url, proxy_url=proxy_url)
            except SnapshotError as exc:
                return _skip_result(site, engine, argv, str(exc))

        if needs_url:
            # Chromium 既定テンプレート等、`{url}` をエンジンへ渡す（直接
            # ナビゲーション）テンプレート向けの SSRF 対策（codex P0:
            # `fetch_snapshot` 経路にしか掛かっておらず `--sites` に内部アドレスの
            # https URL を指定すると迂回できた）。旧実装は `elif needs_url` で
            # `needs_html` と排他にしていたため、`{html_path}` と `{url}` を両方
            # 使うテンプレート（撮影対象は `{html_path}` のスナップショットでも、
            # 参照元 URL として `{url}` も渡すもの等）ではこの検証を素通りできた
            # （codex P0 再指摘）。`{url}` が使われる限り `needs_html` の有無に
            # かかわらず必ずここを通す。
            #
            # `fetch_snapshot` と同じ `_check_public_host` で内部アドレスを拒否する
            # が、これはエンジン起動前の一時点の名前解決に基づくベストエフォート
            # に過ぎず、別プロセスのブラウザ自身がその後たどるリダイレクト先までは
            # 検証できない（`fetch_snapshot` の `_PublicOnlyRedirectHandler` と異なり
            # 介入できない）。そのため `{url}` を渡すテンプレートは
            # `DIRECT_NAVIGATION_ALLOWED_URLS`（sites.json の既定サイトのみを
            # 複製した固定リスト）に完全一致する URL に限定し、それ以外は
            # `{html_path}` のみを使うテンプレートに回す（REPAIR-3）。
            parsed_url = urlparse(site.url)
            try:
                if parsed_url.scheme == "https":
                    _check_public_host(parsed_url.hostname, context=f"direct navigation URL {site.url}")
                    if site.url not in DIRECT_NAVIGATION_ALLOWED_URLS:
                        raise SnapshotError(
                            "direct navigation ({url} template) is limited to the fixed "
                            "default sites in DIRECT_NAVIGATION_ALLOWED_URLS; use a "
                            f"{{html_path}}-only template to capture other URLs: {site.url}"
                        )
                elif not (parsed_url.scheme == "file" and allow_file_url):
                    raise SnapshotError(
                        f"unsupported scheme for direct navigation: {site.url}"
                    )
            except SnapshotError as exc:
                return _skip_result(site, engine, argv, str(exc))

        start = time.monotonic()
        stderr_path: Path | None = None
        try:
            with tempfile.NamedTemporaryFile(
                prefix="fandhe-capture-stderr-", delete=False
            ) as stderr_file:
                stderr_path = Path(stderr_file.name)
                # `subprocess.run(..., timeout=...)` はタイムアウト時に直接の
                # 子プロセスしか kill しない（Cursor Medium 再指摘）。Chromium は
                # レンダラー・GPU プロセス等の子孫を持つため、直接の子だけを
                # 殺しても子孫が残り、プロキシの接続枠・CPU を奪い合い、後続の
                # `user_data_dir` 削除（Windows ではファイルロック）とも競合しうる。
                # `Popen` + 明示的な `wait`/プロセスツリー kill に置き換える。
                proc = subprocess.Popen(  # noqa: S603
                    argv,
                    stdout=subprocess.DEVNULL,
                    stderr=stderr_file,
                    shell=False,
                    **_process_group_popen_kwargs(),
                )
                try:
                    exit_code: int | None = proc.wait(timeout=timeout_sec)
                    timed_out = False
                except subprocess.TimeoutExpired:
                    _kill_process_tree(proc)
                    exit_code = None
                    timed_out = True
                except BaseException:
                    # `KeyboardInterrupt`（Ctrl-C）・`SystemExit` 等、タイムアウト
                    # 以外の理由で `wait` が中断された場合も、プロセスツリーを
                    # 終了してから再送出する（Cursor Medium 再指摘）。エンジンは
                    # `_process_group_popen_kwargs` で新しいセッション／
                    # プロセスグループとして起動しているため、端末の割り込み
                    # （SIGINT）はこのプロセスグループには届かない。ここで
                    # 後片付けせずに例外を伝播させると、エンジンの子孫プロセスが
                    # 残ったままローカル転送プロキシが停止し、`user_data_dir` の
                    # 削除（このあとの `finally`）だけが先に走ってしまう。
                    _kill_process_tree(proc)
                    raise
            duration_ms = int((time.monotonic() - start) * 1000)
            stderr_tail = _read_tail(stderr_path, STDERR_TAIL_CHARS)
            engine_missing = False
        except OSError as exc:
            # エンジンバイナリ不在（FileNotFoundError）・実行権限なし等。プロセスを
            # 起動できなかった場合も他サイト・他エンジンの撮影を継続できるよう、
            # ここで捕捉して「failed」として結果 JSON に記録する（クラッシュさせない。
            # Bugbot Medium: FileNotFoundError 未捕捉で main が異常終了する問題への対応）。
            duration_ms = int((time.monotonic() - start) * 1000)
            exit_code = None
            stderr_tail = str(exc)[-STDERR_TAIL_CHARS:]
            timed_out = False
            engine_missing = True
        finally:
            if stderr_path is not None:
                stderr_path.unlink(missing_ok=True)

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
                "input": input_kind,
            }

        if engine_missing:
            return {
                "site_id": site.site_id,
                "engine": engine,
                "status": "failed",
                "png": None,
                "width": None,
                "height": None,
                "duration_ms": duration_ms,
                "exit_code": None,
                "stderr_tail": stderr_tail,
                "command": argv,
                "input": input_kind,
            }

        status = "failed"
        png_width: int | None = None
        png_height: int | None = None
        png_rel: str | None = None
        if exit_code == 0 and out_path.exists():
            try:
                png_width, png_height = read_png_size(out_path)
                if (png_width, png_height) == (width, height):
                    status = "ok"
                    png_rel = out_path.relative_to(out_dir).as_posix()
                else:
                    stderr_tail = (
                        stderr_tail
                        + f"\nPNG size {png_width}x{png_height} does not match "
                        f"requested viewport {width}x{height}"
                    )[-STDERR_TAIL_CHARS:]
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
            "input": input_kind,
        }
    finally:
        if user_data_dir is not None:
            shutil.rmtree(user_data_dir, ignore_errors=True)


# --- 結果 JSON の書き出し -------------------------------------------------


def write_result(
    out_dir: Path,
    *,
    viewport: dict[str, int],
    sites: list[Site],
    captures: list[dict[str, Any]],
    partial: bool,
) -> Path:
    """撮影結果を `<out-dir>/capture-result.json` へ書き出す（#54 の入力契約）。

    `--out-dir` を使い回す再実行で `capture-result.json` が外部ファイルへの
    symlink になっていた場合の上書きを防ぐため、`_write_bytes_nofollow` で
    追随せずに書き込む（codex P0。`fetch_snapshot` の HTML 保存と同じ問題）。
    """
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
    try:
        _write_bytes_nofollow(result_path, text.encode("utf-8"))
    except OSError as exc:
        raise CaptureError(f"failed to write result file: {result_path}: {exc}") from exc
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


def _timeout_sec_type(raw: str) -> float:
    """`--timeout-sec` の値検証（0 以下・NaN・inf・上限超過を拒否。P1）。"""
    try:
        value = float(raw)
    except ValueError as exc:
        raise argparse.ArgumentTypeError(f"invalid float value: {raw!r}") from exc
    if not math.isfinite(value) or not (MIN_TIMEOUT_SEC <= value <= MAX_TIMEOUT_SEC):
        raise argparse.ArgumentTypeError(
            f"--timeout-sec must be a finite number in [{MIN_TIMEOUT_SEC}, {MAX_TIMEOUT_SEC}], got {raw!r}"
        )
    return value


def _settle_ms_type(raw: str) -> int:
    """`--settle-ms` の値検証（負数・上限超過を拒否。P1）。"""
    try:
        value = int(raw)
    except ValueError as exc:
        raise argparse.ArgumentTypeError(f"invalid int value: {raw!r}") from exc
    if not (0 <= value <= MAX_SETTLE_MS):
        raise argparse.ArgumentTypeError(
            f"--settle-ms must be an integer in [0, {MAX_SETTLE_MS}], got {raw!r}"
        )
    return value


def _min_sites_type(raw: str) -> int:
    """`--min-sites` の値検証（0 以下・`MAX_SITES` 超過を拒否）。"""
    try:
        value = int(raw)
    except ValueError as exc:
        raise argparse.ArgumentTypeError(f"invalid int value: {raw!r}") from exc
    if not (1 <= value <= MAX_SITES):
        raise argparse.ArgumentTypeError(
            f"--min-sites must be an integer in [1, {MAX_SITES}], got {raw!r}"
        )
    return value


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
        "--timeout-sec",
        type=_timeout_sec_type,
        default=90,
        help="Per-capture timeout in seconds (default: 90).",
    )
    parser.add_argument(
        "--settle-ms",
        type=_settle_ms_type,
        default=5000,
        help="Render settle time in milliseconds (default: 5000).",
    )
    parser.add_argument(
        "--min-sites",
        type=_min_sites_type,
        default=5,
        help="Minimum number of sites required (default: 5).",
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
    parser.add_argument(
        "--allow-unproxied-engine",
        action="store_true",
        help=(
            "Allow launching an engine whose command template does not use {proxy} "
            "(the local filtering proxy). Only for network-isolated test environments; "
            "without a proxy, the engine's own subresource/redirect traffic is not "
            "filtered for SSRF."
        ),
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_arg_parser()
    args = parser.parse_args(argv)

    try:
        engines = [e.strip() for e in args.engines.split(",") if e.strip()]
        if not engines:
            raise CaptureError("--engines must name at least one engine")
        if len(engines) != len(set(engines)):
            # 重複指定（例: `servo,servo`）を許すと ok_counts の集計対象と
            # captures の件数が食い違い、`partial` 判定が実態と合わなくなるため
            # fail-closed に拒否する（P2）。
            raise CaptureError(f"--engines must not contain duplicates: {args.engines}")

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

        viewport, sites = load_sites(args.sites, args.min_sites, allow_file_url=args.allow_file_url)
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

    # `{proxy}` を使うテンプレート（既定 Chromium テンプレートを含む）向けに、
    # 撮影プロセスの全通信を宛先フィルタするローカル転送プロキシを起動する
    # （codex P0 再指摘）。`--dry-run` はプロセスを実行しないため起動しない。
    proxy_server: _FilteringProxyServer | None = None
    proxy_thread: threading.Thread | None = None
    proxy_url: str | None = None
    if not args.dry_run:
        proxy_server, proxy_thread, proxy_url = start_filtering_proxy()

    # SIGTERM を Ctrl-C（SIGINT/KeyboardInterrupt）と同じ後片付け経路に合流させる
    # （Cursor Medium 再指摘）。signal handler はメインスレッドでしか登録できない
    # ため、`main` がメインスレッド以外（将来 GUI やテストランナーに組み込まれる
    # 場合等）から呼ばれても失敗させない。
    previous_sigterm_handler: Any = None
    if threading.current_thread() is threading.main_thread():
        try:
            previous_sigterm_handler = signal.signal(signal.SIGTERM, _raise_keyboard_interrupt_on_sigterm)
        except (ValueError, OSError):
            previous_sigterm_handler = None

    captures: list[dict[str, Any]] = []
    try:
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
                        proxy_url=proxy_url,
                        allow_unproxied_engine=args.allow_unproxied_engine,
                    )
                    captures.append(record)
        except CaptureError as exc:
            print(f"error: {exc}", file=sys.stderr)
            return 2
    finally:
        if previous_sigterm_handler is not None:
            try:
                signal.signal(signal.SIGTERM, previous_sigterm_handler)
            except (ValueError, OSError):
                pass
        if proxy_server is not None and proxy_thread is not None:
            stop_filtering_proxy(proxy_server, proxy_thread)

    if args.dry_run:
        for record in captures:
            print(f"[{record['engine']}] {record['site_id']}: {' '.join(record['command'])}")
        print("dry-run: no files were written")
        return 0

    try:
        write_result(out_dir, viewport=viewport, sites=sites, captures=captures, partial=len(engines) < 2)
    except CaptureError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2

    # 受入基準は「代表サイト分の PNG を両エンジンで出力できること」であり、個々の
    # サイトの失敗を即座に fail-closed とはしない。ただし RENDER-5 が求めるのは
    # 「同じサイトを両エンジンで撮って比較する」ことなので、エンジンごとの ok
    # 件数を独立に --min-sites と比べるだけでは、例えば Servo がサイト 1〜5・
    # Chromium がサイト 2〜6 で成功したケース（両エンジンで共通して比較できる
    # のはサイト 2〜5 の 4 件のみ）でも `--min-sites 5` を満たしたと誤判定して
    # しまう（codex P1）。指定した全エンジンで "ok" だった site_id の共通集合の
    # 件数を数え、それが --min-sites 以上であることを要求する（`--engines
    # chromium` のように 1 エンジンだけを指定した実行では、共通集合はそのまま
    # そのエンジンの ok 件数になるため従来の判定と一致する。その場合は結果 JSON
    # に "partial": true を付け、受入基準そのものは両エンジンでの実行が必要で
    # ある旨を README に明記する）。
    ok_counts = {engine: 0 for engine in engines}
    for record in captures:
        if record["status"] == "ok":
            ok_counts[record["engine"]] += 1

    common_ok_count = _count_common_ok_sites(captures, engines)
    print(
        f"common ok sites across {', '.join(engines)}: {common_ok_count} "
        f"(per-engine ok counts: {ok_counts})",
        file=sys.stderr,
    )
    return 0 if common_ok_count >= args.min_sites else 1


if __name__ == "__main__":
    sys.exit(main())
