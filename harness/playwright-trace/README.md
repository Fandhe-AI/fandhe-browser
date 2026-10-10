# playwright-trace

Playwright の `chromium.connectOverCDP()` → `newContext()` → `newPage()` を実行したときに送受信される
CDP メッセージ列を、再現可能な JSONL として保存するハーネス（TASK-43.1・Issue #246、
ビヘイビア `CDP-2`・MS-4）。原因調査（TASK-43.2・#247）と追加実装（TASK-43.3・#249）の入力になる。
本ハーネスは**収集のみ**で、CDP ハンドラや no-op は追加しない（`SEC-2`・`REPAIR-3`）。

## 構成

| ファイル | 役割 |
| -------- | ---- |
| `verify-install.mjs` | 一時導入した playwright-core が単独・指定版・指定 integrity であることの検証 |
| `run.sh` | `trace_server` の起動・playwright-core の一時導入・`trace.mjs` 実行・後始末 |
| `trace.mjs` | 実 Playwright を段階実行し、`DEBUG=pw:protocol` の出力を横取りして JSONL 化 |
| `lib.mjs` | 引数検証・ログ解析・正規化・スキーマ検証（純粋関数） |
| `self-test.sh` / `self-test.mjs` | オフライン自己テスト（Playwright・ネットワーク不要） |
| `results/newpage-trace.jsonl` | 収集・正規化済みトレース（コミット対象） |
| `crates/fandhe-browser-cdp/examples/trace_server.rs` | 一時プロファイル・空きポート（127.0.0.1 限定）の最小 CDP サーバー |
| `crates/fandhe-browser-cdp/tests/playwright_trace.rs` | トレースを実サーバーへ再生する結合テスト（`cargo test` で常時実行） |

## 使い方

```bash
make trace-playwright        # 再取得（要 node / npm / ネットワーク。CI 対象外）
make check-playwright-trace  # オフライン自己テスト（要 node）
```

- playwright-core は Makefile の `PLAYWRIGHT_VERSION`（exact 固定）を `mktemp -d` へ一時導入する。
  `package.json`・lockfile・`node_modules` はコミットしない。代わりに導入後に `verify-install.mjs` が
  `node_modules/.package-lock.json` を検証し、導入物が playwright-core 単独（推移的依存なし）で、
  版と npm 整合性ハッシュ（Makefile の `PLAYWRIGHT_INTEGRITY`）が一致しなければ失敗する。
  版を上げるときは integrity も更新するブラウザ本体は取得しない
  （`PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1`・`--ignore-scripts`）
- 接続先は loopback の IP リテラル（`127.0.0.1` / `[::1]`）のみ許可し、`goto()` など外部アクセスは行わない
- 取得方法は playwright-core が `logger` オプションを公開していないため `DEBUG=pw:protocol` の
  stderr 出力を解析する方式を採った

## 制限事項

- Windows は未対応。`trace_server` が使う `fandhe-browser-profile` の `Profile::open` が、Windows の
  ACL 隔離（`XOS-7`）未実装のため `Unsupported` を返す（`crates/fandhe-browser-profile/src/store.rs` に
  記載の既存制約）。成功を装わず明示的に失敗する。追跡は #295（TASK-63）

## JSONL スキーマ（schema 1）

1 行 1 レコード・LF 固定。共通で `seq`（0 始まりの連番）と `kind` を持つ。

| kind | 項目 | 内容 |
| ---- | ---- | ---- |
| `meta` | `schema`・`purpose`・`playwright`・`node` | 先頭 1 行のみ |
| `http` | `method`・`path`・`status`（失敗時 `error`） | HTTP discovery の結果。Playwright は `/json/version/`（末尾スラッシュ付き）を要求するため両方を調べる |
| `cdp` | `dir`（`send` / `recv`）・`message` | CDP メッセージ本体（`id`・`method`・`params` / `result` / `error` を含む元 JSON）。送受信順 |
| `stage` | `name`・`ok`（失敗時 `error`） | 段階の成否。失敗した段階で止め、`newPage` 到達後は先へ進まない |

ポートは `<PORT>` へ、ANSI エスケープは除去して正規化し、2 回実行して同一内容になることを確認している。

## 再生テストの更新手順

1. `make trace-playwright` でトレースを再取得する
2. `cargo test -p fandhe-browser-cdp --test playwright_trace` が失敗したら、応答が変わった箇所を確認し
   `tests/playwright_trace.rs` の期待値（`EXPECTED_*`・`stages`・件数）を更新する

## 現時点の到達点（事実のみ。原因分析は #247・`docs/design/playwright-compat.md`）

- TASK-43.3a（#804）後の実測。`GET /json/version` と `GET /json/version/` はともに 200 で、
  `Browser.getVersion` は実値（`fandhe-browser/<版>`）で成功する
- `connectOverCDP(http://…)`・`connectOverCDP(ws://…)` はともに、次に送る `Target.setAutoAttach` と
  `Browser.setDownloadBehavior` が `-32601`（未実装。`CDP-6`）となり失敗する。
  `newContext()`・`newPage()` には到達しない
