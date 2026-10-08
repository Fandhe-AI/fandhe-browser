# プラグインの公式サポート区分と目標分離

対応 `TASK-98`（98.3）/ `MS-9` / ビヘイビア `PLUG-6` / `PLUG-3`（[spec-reference](../../.claude/rules/spec-reference.md)）。

## 目的

PoC-15 の「要件への示唆 3」（配布ポリシーの階層化）を受け、`PLUG-3` の数値目標を公式サポート（Rust ネイティブ）のプラグインに限定し、
サードパーティ・実験的プラグインには適用しない方針を記録する。
本ドキュメントは新規の実測を行うものではなく、`TASK-98.1`（#389）・`TASK-98.2`（#390）で実装済みの判定方針と、PoC-15 の実測値を出典明記のうえ整理したものである。
判定ロジックの正本は `crates/fandhe-browser-ai/src/plugin_api.rs` である。

## 区分の定義（`PLUG-6`）

| 区分 | 型 / 区分名 | 条件 |
| ---- | ----------- | ---- |
| 公式サポート | `SupportTier::Official` / `official` | `runtime` が `native`（`OFFICIAL_RUNTIME`）かつ `language` が `rust`（`OFFICIAL_LANGUAGE`）の完全一致 |
| サードパーティ・実験的 | `SupportTier::ThirdParty` / `third-party` | 上記以外すべて |

- 片方のみ一致、未知の値、省略時の既定値 `unspecified`（`DEFAULT_RUNTIME`・`DEFAULT_LANGUAGE`）はすべてサードパーティと判定する（fail-closed。未申告を公式扱いにしない）
- 申告値は小文字 ASCII トークン（`^[a-z0-9][a-z0-9.+#_-]*$`）で、各 64 文字以内（`MAX_RUNTIME_CHARS`・`MAX_LANGUAGE_CHARS`）。構築時に検証済みのため、判定時に大文字小文字の正規化はしない
- 判定の入口は `SupportTier::from_declaration` と `PluginManifest::support_tier`

| 申告（`runtime` / `language`） | 区分 |
| ------------------------------ | ---- |
| `native` / `rust` | 公式サポート |
| `node` / `javascript` | サードパーティ・実験的 |
| `native` / `go` | サードパーティ・実験的（片方のみ一致） |
| 省略（`unspecified` / `unspecified`） | サードパーティ・実験的 |

表中の値は自由記述トークンの例であり、列挙値ではない。

## 目標適用マトリクス（`PLUG-3` / `PLUG-6`）

| 区分 | アイドル RSS 増分 | cold start（end-to-end） | 呼び出し p50 | 呼び出し p95 |
| ---- | ----------------- | ------------------------ | ------------ | ------------ |
| 公式サポート | 10MB 以内 | 50ms 以内 | 20ms 以内 | 100ms 以内 |
| サードパーティ・実験的 | 目標なし | 目標なし | 目標なし | 目標なし |

- 公式側は `PLUG3_TARGETS`（`PluginManifest::plug3_targets` が `Some`）、サードパーティは `None` で実装している
- `PLUG-6` は「別枠の目標、または目標なし」を許容しており、実装は目標なしを採った。スクリプト言語ランタイムのコストはホスト側で制御できないため、数値目標を課さない設計とした
- サードパーティ向けに別枠の目標を設ける場合は、spec 側（`PLUG-6`）の改訂が前提となる

## 参考値（PoC-15 実測）

公式サポートの目標を設定した根拠となる、同一セッションでの 2 変種比較（Rust 版 `rust-rmcp` と Node.js 版 `node-sdk`）。

| 指標 | `node-sdk`（Node.js） | `rust-rmcp`（Rust） |
| ---- | --------------------- | ------------------- |
| アイドル RSS 増分（中央値、n=3） | 80.42MB | 9.64MB |
| cold start end-to-end（中央値、n=5） | 114.52ms | 17.19ms |
| 呼び出しレイテンシ p50（`snapshot`、n=30） | 0.33ms | 0.50ms |
| 呼び出しレイテンシ p95（`snapshot`、n=30） | 0.66ms | 0.71ms |

- Node.js 版の 80.42MB・114.52ms は**下限の参考値**であり、何かが強制する目標・閾値ではない。サードパーティ・実験的プラグイン（スクリプト言語ランタイム）が現実に示しうるコストの目安として記録する
- 条件は macOS ローカル（Apple Silicon）のみの実測。採用値は 2 変種を同一セッションで再計測した値で、`node-sdk` 単独の初回値（81.84MB・113.18ms）は採用していない
- Node 版と Rust 版の差の主因は Node インタプリタの起動・モジュール解決と推定されているが、PoC 側でも推定であり、分離計測は未実施である
- 出典は PoC-15（プラグイン境界 MCP の PoC）の「別プロセス境界プラグインのオーバーヘッド実測値（2 変種比較）」「要件への示唆 3」と、`PLUG-3`・`PLUG-6` のビヘイビア定義。spec 本文中の旧称 `rust-browser` は `fandhe-browser` に読み替える

## 制約・未実施事項

- 区分は untrusted な自己申告から導く。用途は `PLUG-3` のどの計測目標を当てるかの分岐のみで、信頼判定・権限付与・登録可否・検証緩和の根拠にしない。プロセス起動やインタプリタ選択にも使わない。`native` / `rust` と偽って申告しても、得られるのはより厳しい計測目標の適用だけである
- 区分そのものは HTTP 応答（`POST /ai/plugins/register`・`GET /ai/plugins`）に含めない。応答に出るのは申告値 `runtime`・`language` のみ
- 本リポジトリでの実測は未実施。計測は `TASK-95`（`PLUG-3`）の担当で、本ドキュメントの数値は PoC-15 由来である
- PoC の実測は macOS のみ。3 OS 横断の確認は XOS 系へ申し送り済み
- アイドル RSS 増分の「10MB」は spec に単位の明記がなく、実装は MiB（10 × 1024 × 1024 バイト）で保持している。計測側（`TASK-95`）の解釈と食い違う場合は spec 側での確認が必要

## 関連

- [host-api.schema.json](host-api.schema.json): マニフェストの契約
- `crates/fandhe-browser-ai/src/plugin_api.rs`: 区分判定と目標値の正本
- Issue #389（`TASK-98.1`）・#390（`TASK-98.2`）・#391（本ドキュメント）
