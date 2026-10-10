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
| example-minimal | 103 | 83 | 83 | 124 | 41 | 49.4 | 303 | 19.4 | -20.4 |
| wikipedia-article | 56482 | 41274 | 41274 | 47379 | 6105 | 14.8 | 131984 | 26.9 | 16.1 |
| hn-list | 10220 | 16341 | 16341 | 18663 | 2322 | 14.2 | 51743 | -59.9 | -82.6 |
| login-form | 284 | 200 | 200 | 259 | 59 | 29.5 | 679 | 29.6 | 8.8 |
| mdn-docs | 19005 | 27808 | 27808 | 30844 | 3036 | 10.9 | 85180 | -46.3 | -62.3 |

### Summary (tokens)

| metric | mean | min | max |
| ------ | ---- | --- | --- |
| DirectReductionPct | -6.1 | -59.9 | 29.6 |
| McpReductionPct | -28.1 | -82.6 | 16.1 |
| EnvelopeTokens | 2312.6 | 41.0 | 6105.0 |
| EnvelopePct | 23.8 | 10.9 | 49.4 |

### レイテンシ

### Latency (iterations=100)

| fixture | path | p50Ms | p95Ms | meanMs |
| ------- | ---- | ----- | ----- | ------ |
| example-minimal | direct | 0.057 | 0.073 | 0.061 |
| example-minimal | mcp | 0.161 | 0.237 | 0.168 |
| example-minimal | increment | 0.102 | 0.179 | 0.107 |
| wikipedia-article | direct | 10.752 | 11.139 | 10.817 |
| wikipedia-article | mcp | 13.495 | 14.871 | 13.655 |
| wikipedia-article | increment | 2.680 | 4.001 | 2.838 |
| hn-list | direct | 3.398 | 3.488 | 3.415 |
| hn-list | mcp | 4.479 | 5.073 | 4.596 |
| hn-list | increment | 1.114 | 1.632 | 1.182 |
| login-form | direct | 0.143 | 0.194 | 0.148 |
| login-form | mcp | 0.233 | 0.346 | 0.253 |
| login-form | increment | 0.108 | 0.160 | 0.104 |
| mdn-docs | direct | 4.659 | 4.810 | 4.704 |
| mdn-docs | mcp | 6.433 | 7.452 | 6.620 |
| mdn-docs | increment | 1.720 | 2.642 | 1.916 |
| ALL | direct | 3.398 | 10.828 | 3.829 |
| ALL | mcp | 4.479 | 13.760 | 5.058 |
| ALL | increment | 1.113 | 2.987 | 1.229 |

## 所見

- 方式 B 本体の対生 HTML 削減率は 5 サイト中 3 サイトで正（example-minimal 19.4%・wikipedia-article 26.9%・login-form 29.6%）、hn-list（-59.9%）・mdn-docs（-46.3%）は負（平均 -6.1%、範囲 -59.9% から 29.6%）。名前のない generic の折り畳みと空ノードの剪定（TASK-23.3）で改善したが、現行の snapshot が JSON ツリー形式のため一部ページでは生 HTML より大きい（`AISNAP-6`・TASK-19 の論点）。MCP 応答行全体では平均 -28.1%
- エンベロープ（JSON-RPC フレーム等）のトークン増分は 41 から 6105 トークン（平均 2312.6）、方式 B 本体に対して 10.9% から 49.4%（平均 23.8%）。小さいページほど比率が大きい
- レイテンシ増分（MCP - 直接）は全体で p50 1.113 ms、p95 2.987 ms、平均 1.229 ms。小ページ（example-minimal・login-form）は約 0.1 ms、wikipedia-article は p50 2.680 ms
- PLUG-4 レポートは MCP の text コンテンツのみを数えるが、本レポートの mcpLine は JSON-RPC 応答行全体を数える。両レポートの数値は定義が異なる
- 入力は合成 fixture であり、実ページの値ではない。レイテンシは測定環境に依存する

## 引き継ぎ・スコープ外

- 目標妥当性の判断: #387（人間担当）
- in-process の `dispatch`（TCP なし）系列のレイテンシ追加（#385 からの申し送り。別 Issue 候補）
- ホストへの `/ai/navigate` 実装後の、MCP の navigate 経由での測定への切り替え（REPAIR-3）
- snapshot 形式の縮小による削減率の改善（`AISNAP-6`・TASK-19）
- 実ページ（PoC-15 相当）での再測定、Windows での測定（Profile が unix 専用）
