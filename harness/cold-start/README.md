# cold-start

2 つのバイナリの cold start（起動からプロセス終了まで）を同一セッションで交互に計測し、
中央値・p95・増分を JSON で出す計測ハーネス。TASK-104（Issue #397）、ビヘイビア `PERF-7`、MS-9。
ID から SSOT（`docs/spec` の `04-behavior/`）を参照すること
（[spec-reference](../../.claude/rules/spec-reference.md)）。

**測定の実行と判定はオーナーが行う**（TASK-104 の担当は人間。対象バイナリ定義の妥当性判断を要する）。
本ディレクトリは計測スクリプトまでで、出力の `reference_threshold_met` は参考値であり合否の確定ではない。

## 判定基準（PERF-7）

- 「本体」= `core-proto` 相当（CLI として 1 回起動して処理し、**プロセス終了で完了**する一発実行）
- 同一バイナリにプラグイン登録機構（`PLUG-2` のエンドポイント群）を feature で on / off した 2 構成を比較する
- 期待: on 構成の cold start 増分が `core-proto` 基準（PoC-9 実測 2.92ms、PoC-15 再計測 1.96ms）から
  **3ms 以内**。本ハーネスでは A = off（基準）、B = on として、増分 = B の中央値 - A の中央値で見る
- PoC-15 は本体が別バイナリ `plugin-host-proto`（HTTP 待ち受けが完了条件）だったため測定対象不一致・未判定だった。
  今回は対象バイナリ・完了条件（プロセス終了）を揃えることが目的

## 要判断事項（計測の前提がリポジトリ側に未整備）

1. **プラグイン登録機構の on / off feature が存在しない**: `fandhe-browser-cli` の features は
   `js-v8`・`js-boa`・`rendering` のみ。`/ai/plugins`・`/ai/plugins/register` は
   `fandhe_browser_ai::api::router_with_state` 内で常に登録される（`PLUG-2`・TASK-92.3/92.4・#355/#356）。
   2 構成を作るには別 Issue で builder が feature を実装する必要がある（案: ai crate に
   `plugin-registry` feature を追加して該当ルートと `AiState` のレジストリ保持を cfg で切り、
   cli の同名 feature から転送する。off 時は 2 ルートが存在しない形にする）
2. **cli に「一発実行して終了する」サブコマンドがない**: 現行の `fandhe-browser` は引数を取らず、
   常にサーバーを起動して待ち受け続ける（サブコマンドは TASK-47・`CLI-1` で追加予定）。
   このままでは「プロセス終了までの時間」を測れない。測定対象コマンドの定義（例: 起動して構成を組み立て、
   ローカルフィクスチャを 1 回処理して終了する `--once` 相当。プラグイン登録機構は起動時の初期化まで含める）は
   オーナー判断。決まるまで `-- ARGS...` に何を渡すかは未決定
3. 絶対値は PoC-9 / PoC-15 の測定方法と一致しない（シェルの fork/exec・終了待ちを含む）。
   基準 2.92ms / 1.96ms との比較は、同一セッション・同一方法で測った A 側（off）を基準にした増分で行う

## 使い方

```bash
# A = プラグイン登録機構 off、B = on（2 構成は feature を切り替えて別々にビルドする。上記要判断事項 1）
harness/cold-start/measure.sh \
  --bin-a target/release-off/fandhe-browser --label-a plugin-off \
  --bin-b target/release-on/fandhe-browser --label-b plugin-on \
  -n 50 --warmup 3 --out result.json \
  -- <one-shot args>
```

- `-n` は 1〜1000（既定 50）、`--warmup` は 0〜100（既定 3。捨てる）。A/B は交互に実行して時間ドリフトを均等化する
- `--threshold-ms` の既定は 3（参考判定用）
- 対象コマンドの非 0 終了は計測失敗（終了コード 1。成功を装わない）。入力不正・バイナリ不在は終了コード 2
- 同一ビルドプロファイル（release、`opt-level`・LTO 等）・同一マシン・電源/周波数設定固定で測り、
  Linux と macOS の結果は混ぜない

## タイマー方式

| 優先順 | 方式 | 備考 |
| ------ | ---- | ---- |
| 1 | bash 5 の `EPOCHREALTIME` | fork なし。マイクロ秒精度 |
| 2 | GNU `date +%s%N` | Linux 向け。`date` の fork が前後に入る |
| 3 | `perl`（`Time::HiRes`） | macOS の bash 3.2 向け。perl が前後の時刻を取り対象を fork+exec する |

採用した方式は JSON の `timer` に入る。環境変数 `COLD_START_TIMER` で強制できる（自己テスト用）。
macOS の標準 bash（3.2）では 3 が使われる。

## 出力 JSON

`a` / `b`: `label`・`runs`・`median_ms`・`p95_ms`（nearest-rank）・`min_ms`・`max_ms`・`mean_ms`。
`comparison`: `median_increase_ms`・`p95_increase_ms`・`reference_threshold_ms`・`reference_threshold_met`（参考値）。

## 自己テスト

```bash
harness/cold-start/self-test.sh
```

`true` と 20ms スリープのスクリプトで、増分の検出、3 種のタイマー、非 0 終了の失敗扱い、入力検証を確認する。
fandhe-browser 本体は使わない。CI には組み込まない。

## レポートテンプレート

```markdown
# TASK-104 cold start 増分測定（PERF-7）

- 実施日 / 実施者:
- 環境: OS / CPU / 電源設定 / 同一セッション
- 対象コマンドの定義（要判断事項 2 の決定内容）:
- A（off）/ B（on）: コミット / ビルド / feature / バイナリサイズ
- N / warmup / タイマー方式:

| 構成 | 中央値 (ms) | p95 (ms) |
| ---- | ----------- | -------- |
| A: plugin-off | | |
| B: plugin-on | | |

- 中央値の増分: ms（基準 3ms 以内）
- core-proto 基準（PoC-9: 2.92ms / PoC-15: 1.96ms）との関係:

## 判定（オーナー）

- PERF-7 に対する判定:
```
