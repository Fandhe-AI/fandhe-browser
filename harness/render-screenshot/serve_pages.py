#!/usr/bin/env python3
"""保存済みページを loopback（127.0.0.1）の HTTP で静的配信する最小サーバー。

役割・責務境界:
    Issue #1281（TASK-37・`RENDER-5`）。`--network none` の計測用コンテナの中で、
    保存済みページ（`<root>/<site_id>/index.html` とそのサブリソース）を
    `http://pages.test:<port>/...` として Servo と Chromium の両方へ配る。
    用途はコンテナ内に限る（コンテナ外で動かしても害はないが、想定した運用ではない）。

呼び出し文脈:
    `container/run-in-container.sh` が撮影の前にバックグラウンドで起動し、
    `isolation_check.py` が到達性を確かめ、`capture_screenshots.py --saved-pages` が
    エンジンへ URL を渡す。

制約:
    - bind は 127.0.0.1 固定（引数で変えさせない。外部インターフェースへ公開しない）。
    - GET・HEAD 以外は 405、配信ルートの外（`..`・symlink）は 404、ディレクトリ一覧は返さない。
    - 1 ファイルの配信サイズは `MAX_FILE_BYTES` 以下に限り、超えたら 413。本文は固定長チャンクで
      ストリーム配信し、全量をメモリへ読まない。HEAD は本文を読まない（stat のみ）。
    - 同時処理数は `MAX_CONCURRENT_REQUESTS` まで。超えた要求は 503（無制限確保による DoS を防ぐ）。
    - 末尾スラッシュのないディレクトリ要求は、`index.html` を返す前に末尾スラッシュ付きの
      同一オリジン相対 URL へ 301 で転送する（相対参照のサブリソースを正しい場所へ解決させるため）。
Python 標準ライブラリのみ。
"""

from __future__ import annotations

import argparse
import functools
import http.server
import os
import sys
import threading
from pathlib import Path
from urllib.parse import urlsplit

BIND_HOST = "127.0.0.1"
MIN_PORT = 1024
MAX_PORT = 65535
# 1 ファイルあたりの配信上限（保存済みページのサブリソース想定。超過は 413）。
MAX_FILE_BYTES = 64 * 1024 * 1024
# 本文を書き出す 1 回あたりのチャンクサイズ。
CHUNK_BYTES = 64 * 1024
# 同時に処理する要求数の上限。超過は 503。
MAX_CONCURRENT_REQUESTS = 16
_REQUEST_SLOTS = threading.BoundedSemaphore(MAX_CONCURRENT_REQUESTS)


class _PagesHandler(http.server.SimpleHTTPRequestHandler):
    """配信ルート配下のファイルだけを返すハンドラ。"""

    server_version = "fandhe-serve-pages"

    def _root(self) -> Path:
        return Path(self.directory).resolve()

    def _resolve_target(self) -> Path | None:
        """要求パスを解決し、ルート配下の通常ファイルならそのパスを返す（それ以外は None）。"""
        translated = Path(self.translate_path(self.path))
        try:
            resolved = translated.resolve(strict=True)
        except (OSError, RuntimeError):
            return None
        if not resolved.is_relative_to(self._root()):
            return None
        return resolved

    def _redirect_to_slash(self) -> None:
        """末尾スラッシュなしのディレクトリ要求を、同一オリジンの相対 URL（パスのみ）へ 301 で転送する。"""
        parts = urlsplit(self.path)
        # 先頭の複数スラッシュは scheme-relative URL（外部ホスト）と解釈されうるため 1 個へ畳む。
        path = "/" + parts.path.lstrip("/") + "/"
        location = path + ("?" + parts.query if parts.query else "")
        self.send_response(301)
        self.send_header("Location", location)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def _serve(self, send_body: bool) -> None:
        if not _REQUEST_SLOTS.acquire(blocking=False):
            self.send_error(503)
            return
        try:
            self._serve_limited(send_body)
        finally:
            _REQUEST_SLOTS.release()

    def _serve_limited(self, send_body: bool) -> None:
        target = self._resolve_target()
        if target is None:
            self.send_error(404)
            return
        if target.is_dir():
            if not urlsplit(self.path).path.endswith("/"):
                self._redirect_to_slash()
                return
            # ディレクトリ一覧は返さない。index.html があるときだけそれを返す。
            index = target / "index.html"
            try:
                index = index.resolve(strict=True)
            except (OSError, RuntimeError):
                self.send_error(404)
                return
            if not index.is_relative_to(self._root()) or not index.is_file():
                self.send_error(404)
                return
            target = index
        if not target.is_file():
            self.send_error(404)
            return
        try:
            with target.open("rb") as fh:
                size = os.fstat(fh.fileno()).st_size
                if size > MAX_FILE_BYTES:
                    self.send_error(413)
                    return
                self.send_response(200)
                self.send_header("Content-Type", self.guess_type(str(target)))
                self.send_header("Content-Length", str(size))
                self.end_headers()
                if not send_body:
                    return
                remaining = size
                while remaining > 0:
                    chunk = fh.read(min(CHUNK_BYTES, remaining))
                    if not chunk:
                        break
                    self.wfile.write(chunk)
                    remaining -= len(chunk)
        except OSError:
            # ヘッダ送信前なら 404、送信後ならそのまま切断する（接続側がエラーを検知する）。
            if not self.wfile.closed:
                try:
                    self.send_error(404)
                except OSError:
                    pass

    def do_GET(self) -> None:  # noqa: N802
        self._serve(send_body=True)

    def do_HEAD(self) -> None:  # noqa: N802
        self._serve(send_body=False)

    def _method_not_allowed(self) -> None:
        self.send_response(405)
        self.send_header("Allow", "GET, HEAD")
        self.send_header("Content-Length", "0")
        self.end_headers()

    do_POST = do_PUT = do_DELETE = do_PATCH = do_OPTIONS = _method_not_allowed  # noqa: N815

    def log_message(self, format: str, *args: object) -> None:  # noqa: A002
        sys.stderr.write("serve_pages: " + (format % args) + "\n")


def make_server(root: Path, port: int) -> http.server.ThreadingHTTPServer:
    """`root` を 127.0.0.1:`port` で配る HTTP サーバーを作る（`port=0` はテスト用の空きポート）。"""
    if port != 0 and not (MIN_PORT <= port <= MAX_PORT):
        raise ValueError(f"port must be in [{MIN_PORT}, {MAX_PORT}]")
    if not root.is_dir():
        raise ValueError(f"root is not a directory: {root}")
    handler = functools.partial(_PagesHandler, directory=str(root))
    server = http.server.ThreadingHTTPServer((BIND_HOST, port), handler)
    server.daemon_threads = True
    return server


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Serve saved pages on 127.0.0.1 (isolated container use).")
    parser.add_argument("--root", type=Path, required=True, help="Directory containing <site_id>/index.html.")
    parser.add_argument("--port", type=int, default=8080, help=f"Port in [{MIN_PORT}, {MAX_PORT}] (default: 8080).")
    try:
        args = parser.parse_args(argv)
    except SystemExit as exc:
        return 2 if exc.code not in (0, None) else 0
    try:
        server = make_server(args.root, args.port)
    except (ValueError, OSError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    print(f"serving {args.root} on {BIND_HOST}:{args.port}", file=sys.stderr)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
