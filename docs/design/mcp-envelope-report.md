# MCP エンベロープのトークン量・レイテンシ増分の測定レポート（PLUG-5）

- **対応ビヘイビア**: PLUG-5（方式 B 本体を MCP エンベロープで包んだときのトークン増分・レイテンシ増分の実測）
- **対応タスク**: TASK-97（97.2）
- **マイルストーン**: MS-9
- **参考**: 方式 B（`AISNAP-1`・`GET /ai/snapshot`）・PoC-15・[PLUG-4 レポート](./mcp-token-reduction-report.md)
- **基準コミット**: `5142a53`（本測定時点の `origin/main` HEAD）
- **関連 Issue**: #385（ハーネス）・#386（本 Issue・実測とレポート化）・#387（目標妥当性の判断）

本レポートは測定結果の記録であり、`PLUG-5` に数値目標はないため判定は含まない。目標としての妥当性の判断は #387 が行う。

## 測定方法

- 実行: `make measure-mcp-envelope MCP_ENVELOPE_ARGS="--iterations 100 --markdown"`
- 構成: loopback の本物の ai ルータ（`GET /ai/snapshot`）に、mcp バイナリ（`snapshot` ツール）を stdio で接続する
- 実装: `crates/fandhe-browser-ai/benches/mcp_envelope.rs` ほか `benches/mcp_envelope/`
- 入力: `crates/fandhe-browser-ai/benches/fixtures/` の合成 5 ページ（実サイトではない）
- トークナイザ: `cl100k_base`
- 系列: rawHtml = 生 HTML、direct = 方式 B 本体（応答本文）、mcpText = MCP の `result.content[0].text`、mcpLine = JSON-RPC 応答行全体（改行除く）
- 増分: envelope = mcpLine - direct。envelopePct = envelope / direct * 100。削減率 = `(1 - 系列 / raw) * 100`
- レイテンシ増分: 同じ反復番号どうしの MCP - 直接。MCP 経路は mcp から ai ホストへの HTTP ホップを含む。パーセンタイルは nearest-rank。反復は 1 fixture あたり 100 回（ウォームアップ別）
- 注記: JSON-RPC 応答行のバイト数は request id の桁数で変わるため、表には載せない（TSV 出力には出る）。トークン数は id が 3 桁以内なら変わらない
- 同期: トークンブロックは `render_markdown` の出力を逐語で貼ったもので、e2e テスト `plug5_e2e_mcp_snapshot_matches_direct_and_envelope_is_positive` が一致を検査する。snapshot の形が変わったらテストが落ちるので、上記コマンドで再生成して貼り直す。レイテンシは環境依存のため検査しない

## 測定環境

- OS: Linux 7.0.0-34-generic x86_64
- CPU: 13th Gen Intel(R) Core(TM) i7-13700K
- Rust: rustc 1.98.1（release ビルド）
- iterations: 100

## 結果

### トークン

### Per site (tokens)

| name | rawHtmlTokens | directTokens | mcpTextTokens | mcpLineTokens | envelopeTokens | envelopePct | directBytes | directReductionPct | mcpReductionPct |
| ---- | ------------- | ------------ | ------------- | ------------- | -------------- | ----------- | ----------- | ------------------ | --------------- |
| example-minimal | 103 | 193 | 193 | 255 | 62 | 32.1 | 646 | -87.4 | -147.6 |
| wikipedia-article | 56482 | 64318 | 64318 | 74555 | 10237 | 15.9 | 202138 | -13.9 | -32.0 |
| hn-list | 10220 | 21248 | 21248 | 24490 | 3242 | 15.3 | 66877 | -107.9 | -139.6 |
| login-form | 284 | 544 | 544 | 666 | 122 | 22.4 | 1710 | -91.5 | -134.5 |
| mdn-docs | 19005 | 31454 | 31454 | 35222 | 3768 | 12.0 | 96274 | -65.5 | -85.3 |

### Summary (tokens)

| metric | mean | min | max |
| ------ | ---- | --- | --- |
| DirectReductionPct | -73.2 | -107.9 | -13.9 |
| McpReductionPct | -107.8 | -147.6 | -32.0 |
| EnvelopeTokens | 3486.2 | 62.0 | 10237.0 |
| EnvelopePct | 19.5 | 12.0 | 32.1 |

### レイテンシ

### Latency (iterations=100)

| fixture | path | p50Ms | p95Ms | meanMs |
| ------- | ---- | ----- | ----- | ------ |
| example-minimal | direct | 0.116 | 0.135 | 0.115 |
| example-minimal | mcp | 0.250 | 0.316 | 0.252 |
| example-minimal | increment | 0.136 | 0.204 | 0.137 |
| wikipedia-article | direct | 11.678 | 11.954 | 11.850 |
| wikipedia-article | mcp | 15.455 | 15.933 | 15.464 |
| wikipedia-article | increment | 3.719 | 4.096 | 3.614 |
| hn-list | direct | 3.510 | 3.648 | 3.558 |
| hn-list | mcp | 4.872 | 5.658 | 5.040 |
| hn-list | increment | 1.355 | 2.155 | 1.482 |
| login-form | direct | 0.129 | 0.148 | 0.131 |
| login-form | mcp | 0.268 | 0.326 | 0.273 |
| login-form | increment | 0.138 | 0.191 | 0.143 |
| mdn-docs | direct | 4.803 | 4.901 | 4.812 |
| mdn-docs | mcp | 6.595 | 7.630 | 6.726 |
| mdn-docs | increment | 1.772 | 2.786 | 1.914 |
| ALL | direct | 3.510 | 11.757 | 4.093 |
| ALL | mcp | 4.872 | 15.581 | 5.551 |
| ALL | increment | 1.330 | 3.867 | 1.458 |

## 所見

- 方式 B 本体の対生 HTML 削減率は全 5 サイトで負（平均 -73.2%、範囲 -107.9% から -13.9%）。現行の snapshot が JSON ツリー形式で生 HTML より大きいため（`AISNAP-6`・TASK-19 の論点）。MCP 応答行全体ではさらに悪化し、平均 -107.8%
- エンベロープ（JSON-RPC フレーム等）のトークン増分は 62 から 10237 トークン（平均 3486.2）、方式 B 本体に対して 12.0% から 32.1%（平均 19.5%）。小さいページほど比率が大きい
- レイテンシ増分（MCP - 直接）は全体で p50 1.330 ms、p95 3.867 ms、平均 1.458 ms。小ページ（example-minimal・login-form）は約 0.14 ms、wikipedia-article は p50 3.719 ms
- PLUG-4 レポートは MCP の text コンテンツのみを数えるが、本レポートの mcpLine は JSON-RPC 応答行全体を数える。両レポートの数値は定義が異なる
- 入力は合成 fixture であり、実ページの値ではない。レイテンシは測定環境に依存する

## 引き継ぎ・スコープ外

- 目標妥当性の判断: #387（人間担当）
- in-process の `dispatch`（TCP なし）系列のレイテンシ追加（#385 からの申し送り。別 Issue 候補）
- ホストへの `/ai/navigate` 実装後の、MCP の navigate 経由での測定への切り替え（REPAIR-3）
- snapshot 形式の縮小による削減率の改善（`AISNAP-6`・TASK-19）
- 実ページ（PoC-15 相当）での再測定、Windows での測定（Profile が unix 専用）
