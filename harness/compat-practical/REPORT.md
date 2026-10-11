# 実用性測定レポート（JS 必須失敗 3 件の解消確認）

spec 対応: TASK-71（71.4）/ MS-6 / `COMPAT-4`・`JS-2`・`MEAS-4`（関連 `JS-4`〜`JS-6`・`REPAIR-3`・`SEC-2`）。
関連 Issue: #313・#309・#312・#758・#770。

本書は測定結果の記録である。`COMPAT-4` の達成判定は TASK-74（人間担当）が行う。
数値はすべてコミット済みの JSONL（`results/core_results.jsonl`・`results/core_results_js_off.jsonl`）から `jq` で導出した（「再現手順」参照）。

## 結論

- ページ内 JS（V8）を有効にして 22 タスクを再実行した結果は **15/22（68.2%）で、JS 無効の同日対照（15/22）と成否が完全に一致した**
- b5（Trello）・d2（Saucedemo）・d3（DemoQA）は **3 件とも未解消**（`no_match`）
- PoC-9 の統合後推定 90.9%（20/22）に対して **-5 件（-22.7 ポイント）**。`COMPAT-4` の閾値 70%（16/22 以上）にも **1 件足りない**
- 確認できた原因: d2・d3 は `type=module` のスクリプトが `JS-4` の方針で実行対象外（d3 は唯一の script が module）。b5 は 7 件中 2 件が module で実行対象外で、残りの実行後も `h1` が生成されない。b5 の詳細は未特定（後述）

## 測定条件

| 項目 | 値 |
| ---- | -- |
| 実施日時（UTC） | JS 有効: 2026-10-11T21:59:04Z / JS 無効（対照）: 2026-10-11T21:58:37Z |
| OS | Linux |
| バイナリ | `target/release/fandhe-browser`（`cargo build --release -p fandhe-browser-cli`。既定 feature・V8 同梱）。ソースは main（`e98a3cf4`）に `run_core.sh` の拡張（本 PR の 1 件目のコミット）を加えたもの |
| ページ JS | JS 有効: `--page-js v8`（`[js] engine = "v8"`・`execute_page_scripts = true`）/ 対照: 既定（オフ） |
| 実行上限（固定） | 1 script 2 秒・ページ 5 秒・`src` 1 MiB / 件・合計 4 MiB（`JS-6`。設定から変えられず、本測定でも変更していない） |
| タイムアウト | 1 タスク 45 秒・全体 1200 秒（既定） |
| 判定 | `text` は一致要素の部分木の text が非空、`texts` は先頭一致 1 件の text が非空、`form` は `name` 属性付きフィールドが 1 件以上 |
| Chromium 参照 | `reference/chromium_results.json`（PoC-9 の実測。20/22。失敗は e1・e5） |
| 比較の注意 | 実サイトは時期で変わる。Chromium 参照は別時点の実行で、条件は完全には揃わない |

メタ行は `page_js_executed` / `page_js_engine` で有効化の有無を記録する。意味は「実行を有効にして起動した」であり、ページごとの実行成否ではない。

## b5・d2・d3 の解消状況

解消の定義は、JS 有効の計測で `success:true` になること。

| id | サイト | JS 無効（2026-10-07） | JS 無効（同日対照） | JS 有効 | 判定 | 原因 |
| -- | ------ | -------------------- | ------------------- | ------- | ---- | ---- |
| b5 | Trello（`h1`） | 失敗 `no_match` | 失敗 `no_match` | 失敗 `no_match` | 未解消 | 取得は成功（HTTP 200）。script 7 件のうち 2 件は `type=module` で実行対象外。残りは未特定（下記補足） |
| d2 | Saucedemo（`form`） | 失敗 `no_match` | 失敗 `no_match` | 失敗 `no_match` | 未解消 | 取得は成功（HTTP 200）。script は inline 1 件と `type=module` 1 件で、アプリ本体は module 側と推測（実行対象外） |
| d3 | DemoQA（`form`） | 失敗 `no_match` | 失敗 `no_match` | 失敗 `no_match` | 未解消 | 取得は成功（HTTP 200）。script は `type=module` 1 件のみで、実行対象外 |

根拠と補足:

- 取得段階の確認: 計測時に素の `curl` で 3 サイトとも HTTP 200 を確認した。`access_check.sh` は今回の環境で一部ホストが `curl_exit 7`（接続失敗）になったため、`results/access_check.jsonl` は更新せず 2026-10-07 のまま残した（環境要因で、harness の修正は本 Issue の範囲外）
- 実行上限による打ち切り: b5・d2・d3 を 1 件ずつ `--page-js v8 --log` で再実行し、固定診断行（`cdp: page scripts: status=... abort=...`）を回収した。**3 件とも診断行は出ず、上限による打ち切りはない**。ただし診断行が無いことは「実行されて打ち切りなし」と「実行対象なし」を区別しないため、実行の成否の確証にはならない。全 22 件の通し実行では 1 行だけ `status=aborted generation=22 abort=script_bytes` が出た（`generation` は 22 番目の遷移で、タスク順では e5 に当たる。本件の対象外）
- スパイク（`js_api_spike`・`--engine v8`・DOM バインディング無しの素のエンジン）: b5 は 4 件が最初の未定義 API `window` で例外、2 件が module でスキップ、1 件が成功。d2 は inline 1 件が `window` で例外、module 1 件がスキップ。d3 は module 1 件のみでスキップ。素のエンジンの値であり、本番ランタイムの shim 導入後の不足を直接示すものではない。shim 導入後に b5 で何が不足するかは**未特定**（`REPAIR-3`）
- d2・d3 の解消には ES module の実行対応が前提になる可能性が高い。ただし未実装機能に基づく推測であり、実装後の検証が要る

## 全体動作率

| 基準 | 成功数 | 動作率 |
| ---- | ------ | ------ |
| JS 有効（本測定） | 15/22 | 68.2% |
| JS 無効（同日対照） | 15/22 | 68.2% |
| JS 無効（2026-10-07 の既存実測） | 15/22 | 68.2% |
| `COMPAT-4` の閾値 70% | 16/22 以上 | 72.7%（16 件で到達） |
| PoC-9 の統合後推定 | 20/22 | 90.9% |
| 参考: Chromium 生データ（c1 成功・e1 失敗） | 20/22 | 90.9% |
| 参考: Chromium（c1 を実質失敗とみなす） | 19/22 | 86.4% |

- 閾値 70% に対し 15/22 は**未達**（あと 1 件）
- 統合後推定 90.9%（20/22）との差は **-5 件（-22.7 ポイント）**。差を作っている id は a6・b5・c1・d2・d3（Chromium 参照では成功、本実装では失敗）。e1・e5 は両者とも失敗
- JS 有効と無効で成否が変わった id は**ない**。JS 有効化による動作率の改善は、今回の実測では確認できなかった

## サイト別

`reason` は JS 有効の値。ページ由来のテキストは転記しない。

| id | cat | JS 無効（対照） | JS 有効 | Chromium（生データ） | reason |
| -- | --- | -------------- | ------- | ------------------- | ------ |
| a1 | static | 成功 | 成功 | 成功 | - |
| a2 | static | 成功 | 成功 | 成功 | - |
| a3 | static | 成功 | 成功 | 成功 | - |
| a4 | static | 成功 | 成功 | 成功 | - |
| a5 | static | 成功 | 成功 | 成功 | - |
| a6 | static | 失敗 | 失敗 | 成功 | no_match |
| a7 | static | 成功 | 成功 | 成功 | - |
| b1 | spa | 成功 | 成功 | 成功 | - |
| b2 | spa | 成功 | 成功 | 成功 | - |
| b3 | spa | 成功 | 成功 | 成功 | - |
| b4 | spa | 成功 | 成功 | 成功 | - |
| b5 | spa | 失敗 | 失敗 | 成功 | no_match |
| c1 | lazy | 失敗 | 失敗 | 成功 | no_match |
| c6 | lazy | 成功 | 成功 | 成功 | - |
| d1 | form | 成功 | 成功 | 成功 | - |
| d2 | form | 失敗 | 失敗 | 成功 | no_match |
| d3 | form | 失敗 | 失敗 | 成功 | no_match |
| d4 | form | 成功 | 成功 | 成功 | - |
| d6 | form | 成功 | 成功 | 成功 | - |
| e1 | table | 失敗 | 失敗 | 失敗 | no_match |
| e2 | table | 成功 | 成功 | 成功 | - |
| e5 | table | 失敗 | 失敗 | 失敗 | no_match |

## 類型別（JS 有効）

| 類型 | 成功 / 件数 | Chromium |
| ---- | ----------- | -------- |
| static（a） | 6/7 | 7/7 |
| spa（b） | 4/5 | 5/5 |
| lazy（c） | 1/2 | 2/2 |
| form（d） | 3/5 | 5/5 |
| table（e） | 1/3 | 1/3 |

類型別の達成判定は TASK-72（#314〜#316）の範囲で、本書は判定しない。

## 残存失敗の扱い

| id | 状況 | 扱い |
| -- | ---- | ---- |
| c1（Reddit） | anti-bot 対象。`no_match`。PoC-9 の Chromium 参照は成功だが実質は失敗とみなされている | 回避しない（`SEC-2`）。失敗のまま記録する |
| a6（Stack Overflow） | 2026-10-07 の `access_check.jsonl` で HTTP 403（取得段階で止まる） | UA を偽装せず回避しない（`SEC-2`）。今回も `no_match` |
| e5（Grafana） | `table` セレクタが一致しない | 非 DOM 構造の可能性は `COMPAT-2`・TASK-73 の判断待ち。原因は未特定 |
| e1（GitHub Issues） | `a[id^="issue_"]` が一致しない。`^=` は TASK-71.6（#760）で core のセレクタに入ったが、Chromium 生データも失敗 | 対象 DOM が取得結果に含まれるかの切り分けは未実施（未特定） |
| b5（Trello） | 取得は成功し、実行後も `h1` が生成されない | オーナー判断（#758・2026-10-08）で anti-bot・認証による未解消は許容されているが、今回の原因がそれと確定したわけではない（未特定） |
| d2・d3 | `type=module` のスクリプトが実行対象外 | ES module 対応の要否はオーナー判断 |

## 回帰ゲートとマトリクスの状況

- `results/matrix.json` は**コミットしていない**。scratch で `make_matrix.sh` から生成し `check-matrix.sh --threshold 70 --categories static,spa,form --all-categories` を実行した結果は次のとおりで、コミットすると CI の回帰ゲートが赤になるため見送った
  - 全体 15/22 は閾値未満（FAIL）、form（3/5）・lazy（1/2）・table（1/3）も閾値未満（FAIL）。static・spa は PASS
  - lazy は c1（anti-bot）が残る限り 1/2 = 50% で、JS の成否に関係なく `--all-categories` を通らない
- CI は `--allow-missing` のまま変更していない。`matrix.json` のコミットと `--allow-missing` の削除は #312 の残作業で、対象類型・閾値の扱いはオーナー判断（#759 は not planned で close 済み）

## 制約

- 実サイトは時期で変わる。単一環境（Linux）・単一時点の測定で、ばらつきは評価していない
- `tasks.json`・セレクタ・UA・実行上限は変更していない。成功させるための調整はしていない
- ページごとのスクリプト実行成否は直接観測していない（固定診断行は打ち切り時のみ出る）
- macOS・Windows は測定していない（Windows は起動経路が未対応）

## 再現手順

```bash
cargo build --release -p fandhe-browser-cli
# JS 無効の対照
bash harness/compat-practical/run_core.sh --out harness/compat-practical/results/core_results_js_off.jsonl
# JS 有効（既定の出力先は results/core_results.jsonl。--log は絶対パス・コミットしない）
bash harness/compat-practical/run_core.sh --page-js v8 --log /abs/path/run.log
# 集計
jq -s '[.[] | select(.type=="result" and .success==true)] | length' harness/compat-practical/results/core_results.jsonl
jq -c 'select(.type=="meta") | [.page_js_executed,.page_js_engine]' harness/compat-practical/results/core_results.jsonl
# マトリクス（コミットしない）
bash harness/compat-practical/make_matrix.sh --out /abs/path/matrix.json
bash harness/compat-regression/check-matrix.sh --matrix /abs/path/matrix.json --threshold 70 --categories static,spa,form --all-categories
```
