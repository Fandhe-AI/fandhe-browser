# MCP 経由トークン削減率の測定レポート（PLUG-4）

- **対応ビヘイビア**: PLUG-4（MCP 参照プラグインの `snapshot` 出力が生 HTML 比 84.0% 以上の削減）
- **対応タスク**: TASK-96（96.3）
- **マイルストーン**: MS-9
- **参考**: PoC-15（代表 5 サイト・87.8%）・方式 B（TASK-14・`AISNAP-1`・87.0%）
- **基準コミット**: `1d66e34`（本測定時点の `origin/main` HEAD）
- **関連 Issue**: #381（測定）・#382（しきい値判定）・#383（本 Issue・レポート出力）

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
| example-minimal | 103 | 202 | -96.1 | false |
| wikipedia-article | 56482 | 64327 | -13.9 | false |
| hn-list | 10220 | 21257 | -108.0 | true |
| login-form | 284 | 553 | -94.7 | false |
| mdn-docs | 19005 | 31463 | -65.6 | false |

### Summary

| pages | meanReductionPct | minReductionPct | maxReductionPct | medianSnapshotTokens |
| ----- | ---------------- | --------------- | --------------- | -------------------- |
| 5 | -75.7 | -108.0 | -13.9 | 21257.0 |

### PLUG-4 verdict

| thresholdPct | meanPct | verdict | gapPts |
| ------------ | ------- | ------- | ------ |
| 84.0 | -75.7 | Shortfall | 159.7 |

## 所見

- 全 5 サイトで削減率が負（snapshot が生 HTML より大きい）。現行ホストが PoC-5 のフラット形式ではなく JSON ツリーのエンベロープを返すため
- 合成フィクスチャのため、PoC-15 の実ページ（87.8%）とは厳密に比較できない
- `hn-list` は snapshot が truncated

## 引き継ぎ

- 出力形式の縮小は `PLUG-5`・TASK-97・`AISNAP-6` 側の論点
- verdict が `Met` に変わったら、`plug4_mcp_token_reduction_current_verdict_is_pinned_shortfall` を `assert_plug4` による本ゲートに置き換え、本レポートを再生成する
