# MCP 経由トークン削減率の測定レポート（PLUG-4）

- **対応ビヘイビア**: PLUG-4（MCP 参照プラグインの `snapshot` 出力が生 HTML 比 84.0% 以上の削減）
- **対応タスク**: TASK-96（96.3）
- **マイルストーン**: MS-9
- **参考**: PoC-15（代表 5 サイト・87.8%）・方式 B（TASK-14・`AISNAP-1`・87.0%）
- **基準コミット**: `5383534`（TASK-23.8 の再測定時点の `origin/main` HEAD。当初の測定は `1d66e34`）
- **関連 Issue**: #381（測定）・#382（しきい値判定）・#383（レポート出力）・#830（TASK-23.8 の再測定）

本レポートは測定結果の記録であり、84.0% 未達の扱いの判断は含まない。

## 測定方法

- 実行: `cargo test -p fandhe-browser-ai --test mcp_token_reduction plug4_mcp_token_reduction_report_prints_markdown -- --nocapture`
- 実装: `crates/fandhe-browser-ai/tests/mcp_token_reduction.rs`（`render_report`）
- 入力: `crates/fandhe-browser-ai/benches/fixtures/` の合成 5 ページ（実サイトではない）
- トークナイザ: `cl100k_base`
- 数える範囲: MCP の text コンテンツのみ（JSON-RPC フレームは除外。PoC-15 と同じ）
- 削減率: `(1 - snapshot / raw) * 100`。平均は算術平均
- 同期: 下記「結果」のブロックは `render_report` の出力を逐語で貼ったもので、`plug4_committed_report_contains_rendered_block` が一致を検査する。snapshot の形が変わったらテストが落ちるので、上記コマンドで再生成して貼り直す

## 結果

### Per site

| site | rawHtmlTokens | snapshotTokens | reductionPct | truncated |
| ---- | ------------- | -------------- | ------------ | --------- |
| example-minimal | 103 | 77 | 25.2 | false |
| wikipedia-article | 56482 | 33177 | 41.3 | false |
| hn-list | 10220 | 11246 | -10.0 | false |
| login-form | 284 | 174 | 38.7 | false |
| mdn-docs | 19005 | 25200 | -32.6 | false |

### Summary

| pages | meanReductionPct | minReductionPct | maxReductionPct | medianSnapshotTokens |
| ----- | ---------------- | --------------- | --------------- | -------------------- |
| 5 | 12.5 | -32.6 | 41.3 | 11246.0 |

### PLUG-4 verdict

| thresholdPct | meanPct | verdict | gapPts |
| ------------ | ------- | ------- | ------ |
| 84.0 | 12.5 | Shortfall | 71.5 |

## 所見

- 平均削減率は -75.7%（`5786055`、TASK-23.3 より前）から 12.5% に変わった。`hn-list`・`mdn-docs` は依然として負（snapshot が生 HTML より大きい。現行ホストが JSON ツリーのエンベロープを返すため）。84.0% には届いていない。達成可否の判断はしない（#387）。TASK-23 全体の対照表は [TASK-23 再測定レポート](./token-reduction-remeasurement-report.md) を参照
- 合成フィクスチャのため、PoC-15 の実ページ（87.8%）とは厳密に比較できない
- TASK-23.6 以降、`hn-list` の snapshot は truncated ではない（フル name の 120 文字打ち切りが出力から消えたため）

## 引き継ぎ

- 出力形式の縮小は `PLUG-5`・TASK-97・`AISNAP-6` 側の論点
- verdict が `Met` に変わったら、`plug4_mcp_token_reduction_current_verdict_is_pinned_shortfall` を `assert_plug4` による本ゲートに置き換え、本レポートを再生成する

## JS 実行有効時の参考測定（TASK-113・`JS-4`・`PLUG-4`）

設定 `[js] execute_page_scripts = true` のとき、`GET /ai/snapshot` の入力はページ JS の実行後 DOM になる（`AISNAP-6`・`JS-4`）。
その影響を確認した参考測定で、上の「結果」ブロックとは別物である。**テストによる同期検査の対象外**（数値の出どころはテストログ。固定値は `tests/mcp_token_reduction.rs` の `PINNED` が正本）。

- 実行: `cargo test -p fandhe-browser-ai --features js-v8 --test page_scripts_snapshot_v8`（boa は `--features js-boa --test page_scripts_snapshot_boa`。macOS の boa は fail-closed のため V8 版が 3 OS を担う）
- 経路: cdp の `navigate_gated` と同じ（実行 → 実行後 HTML を確定 → `snapshot_body`）を core の公開 API で再現。ai ⇔ cdp の依存禁止のためこの形にしている
- 分母: 取得した生 HTML のトークン数（`PLUG-4` の定義どおり）。実行後 HTML のトークン数は参考として併記
- 基準コミット: `dd92eab3`（V8・boa で数値は一致）

### JS 有効時: サイト別

| site | rawHtmlTokens | snapshotTokens | reductionPct | truncated | executedHtmlTokens | script stage |
| ---- | ------------- | -------------- | ------------ | --------- | ------------------ | ------------ |
| example-minimal | 103 | 77 | 25.2 | false | 103 | Skipped(NoScriptElement) |
| wikipedia-article | 56482 | 33177 | 41.3 | false | 56482 | Skipped(NoScriptElement) |
| hn-list | 10220 | 11246 | -10.0 | false | 10220 | Skipped(NoScriptElement) |
| login-form | 284 | 174 | 38.7 | false | 283 | Executed |
| mdn-docs | 19005 | 25200 | -32.6 | false | 19005 | Skipped(NoScriptElement) |

### JS 有効時: 集計

| pages | meanReductionPct | minReductionPct | maxReductionPct | medianSnapshotTokens | 84.0% との差（pt・表示のみ） |
| ----- | ---------------- | --------------- | --------------- | -------------------- | ---------------------------- |
| 5 | 12.5 | -32.6 | 41.3 | 11246.0 | 71.5 |

### SPA 風フィクスチャ（テスト内蔵・JS が見出し・リスト・フォームを描画）

| rawHtmlTokens | executedHtmlTokens | snapshotTokens（オフ） | snapshotTokens（オン） | reductionPct（オフ） | reductionPct（オン） |
| ------------- | ------------------ | ---------------------- | ---------------------- | -------------------- | -------------------- |
| 318 | 398 | 76 | 251 | 76.1 | 21.1 |

### 所見

- 5 サイトの snapshot は、有効時も既定オフと同一（トークン数・truncated・`login-form` を含むバイト列）。`<script` を持つのは `login-form` のみで、`<script src>` は loopback のためアクターの `Fetcher` が接続前に拒否し（`SrcFetchFailed`・`DisallowedAddress`）、JS は 1 行も実行されない。他 4 サイトは `Skipped(NoScriptElement)`。再シリアライズ由来の差も出なかった
- 平均 12.5% は既定オフと同じで、84.0% 未達のまま。原因は JS 実行ではなく、応答が JSON ツリーのエンベロープであること（上の「所見」と同じ）。しきい値は変更していない
- 描画を伴うページでは snapshot が JS 描画分だけ増える（SPA 風フィクスチャで 76 → 251 トークン）。これは取得対象が実行後 DOM になる `JS-4` の意図どおりの挙動
- 例外メッセージ・console 出力・スクリプト本文は AI 向け出力（snapshot 本文・MCP テキスト）に混入しない。静的要素の `ref` は JS 実行の有無で一致し、再読み出し・再ナビゲーションでもバイト一致する（`AISNAP-6`・`AISNAP-10`）
