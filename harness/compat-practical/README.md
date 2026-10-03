# compat-practical（実測ハーネス）

PoC-9（実用互換性レベルの実測）の 22 サイト分のタスク定義と、到達可否の確認手順を
V8 統合後（TASK-30）の fandhe-browser 向けに移植したハーネス。
spec 対応: TASK-71.1 / MS-6 / `MEAS-4`（関連 `COMPAT-4`・`JS-2`）。

| 後続 | 内容 |
| ---- | ---- |
| #311（TASK-71.2） | 22 タスクの実行スクリプト `run_core.sh` |
| #312（TASK-71.3） | Chromium 実測との突合・`results/matrix.json` 生成 |
| #309（TASK-71） | 測定レポート |

## ファイル

| ファイル | 役割 |
| -------- | ---- |
| `tasks.json` | 22 タスク定義 |
| `lib.sh` | 共通関数 `resolve_bin`（実行対象バイナリの解決と検証） |
| `access_check.sh` | tasks.json の検証と、22 サイトへの到達可否の記録（curl） |
| `self-test.sh` | 上記のオフライン自己テスト（curl はスタブ。CI で実行） |
| `results/access_check.jsonl` | 実測結果（下記「計測結果」） |

## tasks.json

- PoC-9 の `harness/tasks.json` から 22 件をそのまま移植した。
  #312 が PoC-9 の Chromium 実測結果と `id` で突き合わせるため、`id` は変更しない
- スキーマ: JSON 配列。各要素は `{id, cat, url, selector, kind}`
  - `id`・`cat`: `^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$`（`cat` は `check-matrix.sh` と同じ規則）
  - `kind`: `text` / `texts` / `form`
  - `url`: `https://` のみ（空白・制御文字不可）
  - `selector`: 空でない文字列
- c2（Unsplash）は PoC-9 で 401 のため除外されており、追加し直していない
- JS 必須で失敗していた 3 件は b5（Trello）・d2（Saucedemo）・d3（DemoQA）。V8 統合後に解消するかが測定の焦点

## access_check.sh

```bash
bash harness/compat-practical/access_check.sh [--tasks P] [--out P] [--timeout SEC] [--bin P] [--validate-only]
```

- 依存は bash・jq・curl のみ。実ネットワークへ出るため CI では実行しない（CI は `self-test.sh` と `--validate-only`）
- `--timeout` は 1〜60 の整数（既定 8）。リダイレクトは自前で 5 回まで追う（`curl -L` は使わない）、https のみ（`--proto '=https' --proto-redir '=https'`）
- UA は偽装しない識別子 `fandhe-browser-harness/0.1 (+https://github.com/Fandhe-AI/fandhe-browser)`。anti-bot 回避はしない
- SSRF 対策（SEC 系）: 各ホップで `lib.sh` の `url_check`（https のみ・userinfo なし・ポート 443 のみ・IPv6 リテラル不可・localhost 系/内部ドメイン不可・IP リテラルは公開 IPv4 の厳密な 10 進表記のみ）と、DNS 解決後アドレスの公開判定（getent / dscacheutil があるとき。検証済み IPv4/IPv6 を `--resolve` で固定。解決できない・固定できないホストは取得前に拒否）を行い、curl は `-q --noproxy '*'`（.curlrc・プロキシ無効）で起動し、`remote_ip` も事後検証する。tasks.json は JSON 文書がちょうど 1 個であることを検証する（空ファイル・複数文書は拒否）拒否時は curl を呼ばず（事後検証を除く）`blocked` に理由を記録し `reachable:false`
- Windows の jq が出す CRLF は `lib.sh` の `jq` ラッパーが除去する
- 出力は JSONL。1 行目がメタ行、以降は 1 件 1 行

```json
{"type":"meta","measured_at":"2026-10-03T00:00:00Z","user_agent":"...","timeout_sec":8,"tasks_sha256":"...","bin":null}
{"type":"result","id":"a1","cat":"static","url":"https://...","http_status":200,"reachable":true,"curl_exit":0,"elapsed_ms":1234,"blocked":null}
```

- `elapsed_ms`: curl の `time_total`（リダイレクト追従を含む総所要時間）をミリ秒へ変換した値。取得できない場合は 0
- `blocked`: SSRF 対策で取得を拒否した理由（英語）。拒否していなければ null
- `reachable`: リダイレクトを追った最終応答が 200〜399 かつ `curl_exit == 0`。DNS 失敗・タイムアウト等は `http_status: 0`
- 終了コード: `0` 記録完了（到達不能サイトがあっても 0。到達可否は計測結果でありゲートではない）、`1` 記録の書き込み失敗、`2` 入力・使用エラー（スキーマ違反・引数不正・jq/curl 無し・バイナリ検証失敗）

## 実行対象バイナリの指定

実行対象は cli crate の `fandhe-browser`（`fandhe-browser-core` はライブラリで単体バイナリを持たない）。
`lib.sh` の `resolve_bin` が次の優先順位で解決・検証する。

1. `--bin <path>`
2. 環境変数 `FANDHE_BROWSER_BIN`
3. `<repo>/target/release/fandhe-browser`（Windows は `.exe` も探す）

存在・通常ファイル・実行可能であることだけを確認し、**バイナリは起動しない**。

## 制約（#311 の前提・未解決）

`fandhe-browser` は現時点で引数・サブコマンドを受け付けず、起動すると CDP サーバーが立つだけである。
このため本ハーネスはタスクを実行できない（実行できるように見せない。`REPAIR-3`）。
Issue #311 でタスクを実行するには、TASK-47（`CLI-1`）の CLI サブコマンドか、CDP クライアント経由の実行方式のどちらかが必要になる。

## 計測結果

`results/access_check.jsonl` は `access_check.sh` の実測（手書きしていない）。

- 実施日: 2026-10-03 / OS: Linux
- 結果: 22 件中 20 件が到達可（a6 StackOverflow・d4 W3Schools は HTTP 403）
- 到達可否はネットワーク環境と時期で変わる。再計測したら実施日と OS をここへ追記する
