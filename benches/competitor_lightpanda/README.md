# competitor_lightpanda の継続実行結果記録

`competitor_lightpanda` ベンチ（`PERF-1`・`PERF-3`・`PERF-6`・`AISNAP-1`。
`../competitor_lightpanda.rs`。TASK-84（84.1）・Issue #211）の結果を時系列で
記録するパイプライン。TASK-84（84.2）・Issue #212 で追加した。対応ビヘイビア
ID から SSOT（`docs/spec` の `04-behavior/`）を参照すること
（[spec-reference](../../.claude/rules/spec-reference.md)）。

## 現状の限界（重要。REPAIR-3「実装済みを装わない」）

- `fandhe-browser-cli`（TASK-41.5・Issue #174）が未実装で、CI には Lightpanda
  のバイナリも用意していない。そのため **CI 実行では `lightpanda`・
  `fandhe-browser` の両方が `skipped` になり、実際の数値は記録されない**。
  このパイプラインが渡すのは「記録の仕組みと記録フォーマット」であり、
  実測値を記録できるのは対象バイナリ（`LIGHTPANDA_BIN`・
  `FANDHE_BROWSER_BIN`）を用意したローカル・計測マシンで実行したときに限る
- `PERF-6` が求める Chromium 側の基準値は**プロセスツリー全体**のアイドル
  RSS だが、本ベンチの `idleRssKb` は起動した**直接の子プロセス 1 つ分**の
  RSS しか計測しない（`../competitor_lightpanda.rs` モジュールドキュメント
  「アイドル RSS」参照）。そのため `CHROMIUM_IDLE_RSS_KB` には、別途
  計測したプロセスツリー全体の実測値を渡す運用にする（既定値は埋め込まない）

## 運用（ローカルでの実測・記録）

```bash
export LIGHTPANDA_BIN=/path/to/lightpanda      # 任意。未設定なら lightpanda 側は skipped
export FANDHE_BROWSER_BIN=/path/to/fandhe-browser  # 任意。未設定なら fandhe-browser 側は skipped
export CHROMIUM_IDLE_RSS_KB=81306              # 任意。PERF-6 目標比較の基準値（実測値。KB）
make bench-record
```

`make bench-record` は `record.sh` を呼び、`cargo bench` を実行してから結果を
`results/history.jsonl` へ 1 行追記する。追記された行を確認し、意図した実測
であれば通常のコミットに含める（TASK-26.2 の互換性レポートと同じ「実測結果は
都度コミットする」運用）。

`record.sh` の直接呼び出し（詳細な制御が要る場合）:

```bash
bash record.sh --history results/history.jsonl [--input <file>] [--bench-exit-code <n>] [--source local|ci]
```

- `--input`: 既に得た結果 JSON ファイルを読み込む（`cargo bench` を実行しない）
- `--source`: `local` または `ci`（既定 `local`）。記録行の `source` に入る

## 記録先

- `results/history.jsonl`（コミットする。1 行 = 1 回の実行。追記専用。手で
  編集しない）
- CI アーティファクト（`.github/workflows/bench-competitor.yml`。
  `workflow_dispatch` と毎週の定期実行。3 OS matrix。90 日保持）。CI では
  上記の限界により対象バイナリがなく `skipped` の行が積み上がるだけだが、
  パイプライン自体が壊れていないことの継続的な証跡になる

## スキーマ（JSONL・`schemaVersion` 1）

1 行 1 JSON オブジェクト。

```json
{
  "schemaVersion": 1,
  "recordedAt": "2026-09-27T00:00:00Z",
  "gitCommit": "<40桁 hex または \"unknown\">",
  "os": "linux | darwin | windows | <uname -s の小文字>",
  "arch": "x86_64 | aarch64 | <uname -m の小文字>",
  "source": "local | ci",
  "runId": "<GITHUB_RUN_ID または null>",
  "benchExitCode": 0,
  "result": { "<target>": { "...": "competitor_lightpanda.rs の結果 JSON そのもの（perf6 含む）" } }
}
```

`result` は `competitor_lightpanda` ベンチの stdout 出力（`lightpanda`・
`fandhe-browser` をキーとするオブジェクト）をそのまま埋め込む。各対象の
フィールドは `../competitor_lightpanda.rs` モジュールドキュメント参照。

### `perf6` フィールド（`PERF-6` 目標比較。TASK-84.2）

各対象の `idleRssKb` と `CHROMIUM_IDLE_RSS_KB` から、`PERF-6`
（Chromium 比 85% 以上のアイドル RSS 削減。判断記録:
`../../docs/design/perf-6-decision.md`。参考実測値は PoC-9 の 92.3%）目標との
比較を表す。

| `status` | 意味 |
| -------- | ---- |
| `measured` | `baselineKb`・`reductionPct`・`targetPct`（85）・`verdict`（`met`/`below_target`）を含む |
| `skipped` | `CHROMIUM_IDLE_RSS_KB` 未設定、または対象の `idleRssKb` 自体が `skipped`（対象バイナリ未設定） |
| `unsupported` | `idleRssKb` が `unsupported`（Windows） |
| `error` | `idleRssKb` が `error`（対象の起動失敗等） |

`verdict: "below_target"` は「計測はできたが目標未達だった」ことを表す計測
結果の一種であり、計測失敗（`bench_exit_code` が非ゼロになる条件）ではない
（`../competitor_lightpanda.rs` モジュールドキュメントの終了コード契約参照）。

## record.sh の終了コード

| exit | 意味 |
| ---- | ---- |
| 0 | 追記に成功し、ベンチ自体も成功（またはベンチ結果を伴わずに正常終了）した |
| ベンチの終了コードそのまま（例: 1） | 追記には成功したが、ベンチ側が `Outcome::Error` を含んでいた（`../competitor_lightpanda.rs` の終了コード契約） |
| 2 | 入力・使用エラー（不正な引数・`--history` の拡張子違い/シンボリックリンク/親ディレクトリ不在・結果 JSON がオブジェクトでない・1 MiB 超過・`jq` 未導入等）。この場合は履歴へ追記しない |

## 自己テスト

```bash
make check-bench-record
```

内部で `self-test.sh` を実行する。`record-fixtures/*.json` は自己テスト用の
合成データであり、実際の計測結果ではない（`harness/compat-regression/
fixtures/` と同じ方針。README.md「fixture は合成データ」参照）。
