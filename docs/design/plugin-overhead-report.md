# MCP プラグインの性能計測レポート（PLUG-3）

- **対応ビヘイビア**: PLUG-3（MCP 参照プラグインのアイドル RSS 増分・cold start・呼び出しレイテンシ）、REPAIR-9（構造化出力）
- **対応タスク**: TASK-95（95.4）
- **マイルストーン**: MS-9
- **参考**: PoC-15・[可観測性の出力形式](./observability.md)・[MCP エンベロープ測定レポート](./mcp-envelope-report.md)
- **基準コミット**: `983bae9`（`origin/main` HEAD に実装を足した作業ツリーでの測定）
- **関連 Issue**: #375（RSS）・#376（cold start）・#377（レイテンシ）・#378（本レポート）・#379（目標達成の判定）

本レポートは測定結果の記録であり、`PLUG-3` の目標を達成したかどうかの判定は含まない。判定は #379（人間担当）が行う。

## 測定方法

- 実行: `make measure-plugin-overhead PLUGIN_OVERHEAD_ARGS="--metric all --format jsonl"`
- 構成・窓の定義・単位・PoC-15 との差: `benches/plugin_overhead/README.md`
- 出力: TASK-10 の JSON Lines 規約に沿った 1 指標 1 行（bench 専用イベント `plugin_overhead`。スキーマは README）。時間は µs、メモリは bytes の整数
- 値は環境に依存するため CI では照合しない

## 測定環境

- OS: Linux 7.0.0-34-generic x86_64
- CPU: 13th Gen Intel Core i7-13700K
- rustc: 1.99.0（b940084d7 2026-09-28）
- ビルド: mcp は release、ホストは bench 自身（in-process）

## 実測結果（逐語）

```jsonl
{"event":"plugin_overhead","metric":"idle_rss","tier":"official","samples":3,"host_only_bytes":4689920,"host_plus_plugin_bytes":8118272,"plugin_bytes":3223552,"increment_bytes":3428352,"target_increment_max_bytes":10485760,"timestamp_unix_ms":1792707102460}
{"event":"plugin_overhead","metric":"cold_start","tier":"official","trials":5,"median_us":2878,"min_us":1590,"max_us":3110,"target_median_max_us":50000,"timestamp_unix_ms":1792707102460}
{"event":"plugin_overhead","metric":"call_latency","tier":"official","calls":30,"p50_us":352,"p95_us":483,"mean_us":360,"min_us":273,"max_us":594,"target_p50_max_us":20000,"target_p95_max_us":100000,"timestamp_unix_ms":1792707102460}
```

## 読み方の注意（PoC-15 との差の要約）

- cold start はプラグイン起動から MCP ハンドシェイク完了までで、PoC-15 の「ホスト spawn 直後から `navigate` 完了」より窓が狭い
- ホストは独立プロセスではなく in-process。呼び出しレイテンシのページは `navigate` でなく直接 commit している
- いずれも cli が引数を持った後（TASK-47・`CLI-1`）に見直す（REPAIR-3）。値は PoC-15 と直接比較できない
- `target_*` は参考値で、判定は #379 が行う
