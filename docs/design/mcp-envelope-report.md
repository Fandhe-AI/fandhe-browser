# MCP エンベロープのトークン量・レイテンシ増分の測定レポート（PLUG-5）

- **対応ビヘイビア**: PLUG-5（方式 B 本体を MCP エンベロープで包んだときのトークン増分・レイテンシ増分の実測）
- **対応タスク**: TASK-97（97.2）
- **マイルストーン**: MS-9
- **参考**: 方式 B（`AISNAP-1`・`GET /ai/snapshot`）・PoC-15・[PLUG-4 レポート](./mcp-token-reduction-report.md)
- **基準コミット**: `5383534`（TASK-23.8 の再測定時点の `origin/main` HEAD。当初の測定は `5142a53`）
- **関連 Issue**: #385（ハーネス）・#386（実測とレポート化）・#830（TASK-23.8 の再測定）・#387（目標妥当性の判断）

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
| example-minimal | 103 | 68 | 68 | 109 | 41 | 60.3 | 276 | 34.0 | -5.8 |
| wikipedia-article | 56482 | 33168 | 33168 | 39046 | 5878 | 17.7 | 115790 | 41.3 | 30.9 |
| hn-list | 10220 | 11307 | 11307 | 13482 | 2175 | 19.2 | 37938 | -10.6 | -31.9 |
| login-form | 284 | 165 | 165 | 224 | 59 | 35.8 | 620 | 41.9 | 21.1 |
| mdn-docs | 19005 | 25191 | 25191 | 28078 | 2887 | 11.5 | 80346 | -32.5 | -47.7 |

### Summary (tokens)

| metric | mean | min | max |
| ------ | ---- | --- | --- |
| DirectReductionPct | 14.8 | -32.5 | 41.9 |
| McpReductionPct | -6.7 | -47.7 | 30.9 |
| EnvelopeTokens | 2208.0 | 41.0 | 5878.0 |
| EnvelopePct | 28.9 | 11.5 | 60.3 |

### レイテンシ

### Latency (iterations=100)

| fixture | path | p50Ms | p95Ms | meanMs |
| ------- | ---- | ----- | ----- | ------ |
| example-minimal | direct | 0.093 | 0.110 | 0.084 |
| example-minimal | mcp | 0.206 | 0.277 | 0.203 |
| example-minimal | increment | 0.118 | 0.183 | 0.120 |
| wikipedia-article | direct | 12.372 | 12.872 | 12.450 |
| wikipedia-article | mcp | 14.783 | 15.674 | 14.892 |
| wikipedia-article | increment | 2.411 | 3.121 | 2.442 |
| hn-list | direct | 3.971 | 4.065 | 3.983 |
| hn-list | mcp | 4.963 | 5.307 | 5.000 |
| hn-list | increment | 1.004 | 1.342 | 1.018 |
| login-form | direct | 0.122 | 0.137 | 0.124 |
| login-form | mcp | 0.241 | 0.280 | 0.241 |
| login-form | increment | 0.117 | 0.153 | 0.116 |
| mdn-docs | direct | 5.199 | 5.387 | 5.260 |
| mdn-docs | mcp | 6.796 | 7.695 | 6.924 |
| mdn-docs | increment | 1.564 | 2.399 | 1.664 |
| ALL | direct | 3.971 | 12.481 | 4.380 |
| ALL | mcp | 4.963 | 15.110 | 5.452 |
| ALL | increment | 0.997 | 2.687 | 1.072 |

## 所見

- 方式 B 本体の対生 HTML 削減率は 5 サイト中 3 サイトで正（example-minimal 34.0%・wikipedia-article 41.3%・login-form 41.9%）、hn-list（-10.6%）・mdn-docs（-32.5%）は負（平均 14.8%、範囲 -32.5% から 41.9%）。MCP 応答行全体では平均 -6.7%。snapshot が JSON ツリー形式のため一部ページで生 HTML より大きい（`AISNAP-6`・TASK-19 の論点）
- エンベロープ（JSON-RPC フレーム等）のトークン増分は 41 から 5878 トークン（平均 2208.0）、方式 B 本体に対して 11.5% から 60.3%（平均 28.9%）。小さいページほど比率が大きい
- レイテンシ増分（MCP - 直接）は全体で p50 0.997 ms、p95 2.687 ms、平均 1.072 ms。小ページ（example-minimal・login-form）は約 0.1 ms、wikipedia-article は p50 2.411 ms
- PLUG-4 レポートは MCP の text コンテンツのみを数えるが、本レポートの mcpLine は JSON-RPC 応答行全体を数える。両レポートの数値は定義が異なる
- 入力は合成 fixture であり、実ページの値ではない。レイテンシは測定環境に依存する

### TASK-23 改善前後（#830）

| 指標 | 改善前（`82ab0dd` のレポート値） | 改善後（`5383534`） |
| ---- | -------------------------------- | ------------------- |
| DirectReductionPct の平均 | -73.2% | 16.6% |
| McpReductionPct の平均 | -107.8% | -5.2% |
| EnvelopeTokens の平均 | 3486.2 | 2266.2 |
| EnvelopePct の平均（最小〜最大） | 19.5%（12.0〜32.1） | 29.5%（12.3〜60.3） |
| レイテンシ増分 全体 p50 / p95 | 1.330 / 3.867 ms | 0.940 / 2.571 ms |

改善前のレイテンシは別コミット・別の実行での計測のため、参考比較にとどまる。達成可否の判断はしない（#387）。全体の対照表は [TASK-23 再測定レポート](./token-reduction-remeasurement-report.md) を参照。

## 引き継ぎ・スコープ外

- 目標妥当性の判断: #387（人間担当）
- in-process の `dispatch`（TCP なし）系列のレイテンシ追加（#385 からの申し送り。別 Issue 候補）
- ホストへの `/ai/navigate` 実装後の、MCP の navigate 経由での測定への切り替え（REPAIR-3）
- snapshot 形式の縮小による削減率の改善（`AISNAP-6`・TASK-19）
- 実ページ（PoC-15 相当）での再測定、Windows での測定（Profile が unix 専用）
