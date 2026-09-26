# compat-regression

対象サイト群の動作率マトリクス（JSON）を読み込み、全体および指定した類型別の動作率が
閾値を下回っていれば非ゼロ終了する回帰チェッカー。TASK-9.2（Issue #339）で導入。
対応するビヘイビア ID は `REPAIR-8`（Could。対象サイト群の互換性回帰チェックを含む
本格 CI）で、閾値の根拠は `COMPAT-4`（全体動作率 70% 以上）・`COMPAT-1`（静的・SPA・
フォーム等の類型別動作率 70% 以上）。ID から SSOT（`docs/spec` の `04-behavior/`）を
参照すること（[spec-reference](../../.claude/rules/spec-reference.md)）。

## 実マトリクスは未導入（重要）

本 Issue の時点で実マトリクス `harness/compat-practical/results/matrix.json` は
存在しない（生成は TASK-71.3・Issue #312 が担当。36 サイト版の更新は TASK-89・
Issue #68・人間担当）。そのため `.github/workflows/ci.yml`・`Makefile` では
`check-matrix.sh` を `--allow-missing` 付きで呼び出しており、ファイルが無い間は
`::warning::` を出して exit 0（回帰ゲートは実質的に無効）になる。

**`--allow-missing` の削除条件**: #312（TASK-71.3）が `matrix.json` をコミットしたら、
`ci.yml`・`Makefile` から `--allow-missing` を削除し、ファイル不在を fail-closed
（exit 2）に戻すこと。

## スキーマ契約

トップレベルは JSON 配列。各要素は次を満たすオブジェクト。

| フィールド | 型 | 必須 | 説明 |
| ---------- | -- | ---- | ---- |
| `id` | 非空文字列 | 必須 | サイト・ケースの識別子。配列内で重複不可 |
| `cat` | 文字列 | 必須 | `static` / `spa` / `lazy` / `form` / `table` 等の類型（PoC-9 の分類を踏襲。値自体は任意の文字列として扱う） |
| `<key>` | boolean | 必須 | 動作可否。既定のキー名は `fandhe_browser_core`（`--key` で変更可） |
| `chromium` | boolean | 任意 | 参考値（比較用の Chromium 実測）。判定には使わない |

配列の長さは 1〜10000 件。ファイルサイズは 1 MiB 以下。この契約は PoC-9
（spec `03-poc/practical-compat-level/harness/results/matrix.json`）の形式
`[{id, cat, core_proto: bool, chromium: bool}, ...]` を踏襲したもの。
**#312（TASK-71.3）はこの契約に従うこと**（エンジンキー名は `fandhe_browser_core`
を既定値として想定）。

## CLI

```bash
harness/compat-regression/check-matrix.sh \
  --matrix <path> \
  [--threshold <0-100>] \
  [--key <name>] \
  [--categories <csv>] \
  [--allow-missing]
```

- `--matrix`: マトリクス JSON のパス（必須）
- `--threshold`: 全体・各類型に共通の合格閾値（既定 70。0〜100 の整数のみ・先頭ゼロ不可（bash の 8 進数解釈を避けるため）。`passed * 100 >= threshold * total` で判定し、閾値ちょうどは合格）
- `--key`: 判定に使う boolean フィールド名（既定 `fandhe_browser_core`）
- `--categories`: 個別にも 70% 以上を要求する `cat` 値の CSV（例: `static,spa,form`）。列挙した類型のエントリが 0 件なら使用エラー（exit 2）
- `--allow-missing`: `--matrix` のファイルが存在しない場合に `::warning::` を出して exit 0 にする（実マトリクス未導入期間の暫定運用。上記「実マトリクスは未導入」参照）

## 終了コード

| exit | 意味 |
| ---- | ---- |
| 0 | 合格（全体・列挙した類型すべてが閾値以上）、または `--allow-missing` 指定時のファイル不在 |
| 1 | 不合格（全体または列挙した類型のいずれかが閾値未満） |
| 2 | 入力・使用エラー（JSON 不正・スキーマ違反・`id` 重複・空配列・サイズ超過・列挙した類型のエントリ 0 件・引数不正・ファイル不在〔`--allow-missing` 無し〕・`jq` 未導入） |

## fixture は合成データ（実測値ではない）

`fixtures/*.json` はすべてこのチェッカーの自己テスト用に作った合成データで、
実サイトの計測結果ではない。PoC-9 の `core_proto` 実測値をここへ転記していない
（実装済みの本格計測結果であるかのように見せることになり、`REPAIR-3`
「実装済みを装わない」に反するため）。ID（`t1` 等）も合成値であり、実サイトの
識別子とは無関係。

## ローカル実行

```bash
make check-compat-regression
```

内部で `self-test.sh`（fixture による自己テスト。閾値未満で確実に fail することの
証跡）→ 実マトリクスへの `check-matrix.sh --allow-missing` 呼び出し、の順に実行する
（CI の `compat-regression` ジョブと同じ手順）。
