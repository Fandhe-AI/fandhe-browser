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
| wikipedia-article | 56482 | 32160 | 43.1 | false |
| hn-list | 10220 | 10797 | -5.6 | false |
| login-form | 284 | 174 | 38.7 | false |
| mdn-docs | 19005 | 24753 | -30.2 | false |

### Summary

| pages | meanReductionPct | minReductionPct | maxReductionPct | medianSnapshotTokens |
| ----- | ---------------- | --------------- | --------------- | -------------------- |
| 5 | 14.2 | -30.2 | 43.1 | 10797.0 |

### PLUG-4 verdict

| thresholdPct | meanPct | verdict | gapPts |
| ------------ | ------- | ------- | ------ |
| 84.0 | 14.2 | Shortfall | 69.8 |

## 所見

- 平均削減率は -75.7%（`5786055`、TASK-23.3 より前）から 14.2% に変わった。`hn-list`・`mdn-docs` は依然として負（snapshot が生 HTML より大きい。現行ホストが JSON ツリーのエンベロープを返すため）。84.0% には届いていない。達成可否の判断はしない（#387）。TASK-23 全体の対照表は [TASK-23 再測定レポート](./token-reduction-remeasurement-report.md) を参照
- 合成フィクスチャのため、PoC-15 の実ページ（87.8%）とは厳密に比較できない
- TASK-23.6 以降、`hn-list` の snapshot は truncated ではない（フル name の 120 文字打ち切りが出力から消えたため）

## 引き継ぎ

- 出力形式の縮小は `PLUG-5`・TASK-97・`AISNAP-6` 側の論点
- verdict が `Met` に変わったら、`plug4_mcp_token_reduction_current_verdict_is_pinned_shortfall` を `assert_plug4` による本ゲートに置き換え、本レポートを再生成する
