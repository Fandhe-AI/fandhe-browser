# 実用性測定レポート（JS 必須失敗 3 件の解消確認）

spec 対応: TASK-71（71.4）/ MS-6 / `COMPAT-4`・`JS-2`・`MEAS-4`（関連 `JS-4`〜`JS-6`・`REPAIR-3`・`SEC-2`）。
関連 Issue: #313・#309・#312・#758・#770。

本書は測定結果の記録である。`COMPAT-4` の達成判定は TASK-74（人間担当）が行う。
数値はすべてコミット済みの JSONL（`results/core_results.jsonl`・`results/core_results_js_off.jsonl`）から `jq` で導出した（「再現手順」参照）。

## 結論

- ES module 有効（`[js] execute_modules = true`）での再測定（#1119）は **15/22 で、b5・d2・d3 は 3 件とも未解消**。結果は下の「ES module 有効での再測定（#1119）」を参照
- ページ内 JS（V8）を有効にして 22 タスクを再実行した結果は **15/22（68.2%）で、JS 無効の同日対照（15/22）と成否が完全に一致した**
- b5（Trello）・d2（Saucedemo）・d3（DemoQA）は **3 件とも未解消**（`no_match`）
- PoC-9 の統合後推定 90.9%（20/22）に対して **-5 件（-22.7 ポイント）**。`COMPAT-4` の閾値 70%（16/22 以上）にも **1 件足りない**
- 確認できた原因: d2・d3 は `type=module` のスクリプトが `JS-4` の方針で実行対象外（d3 は唯一の script が module）。b5 は初期 HTML に `h1` が無く、Chromium では cross-origin の `type=module` 2 件が `h1` を生成する（本実装は module を実行対象外にしている）。分類は ES module（#918）で、b5 の節を参照

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
| b5 | Trello（`h1`） | 失敗 `no_match` | 失敗 `no_match` | 失敗 `no_match` | 未解消 | 取得は成功（HTTP 200）。初期 HTML に `h1` が無く、`h1` を生成する cross-origin の `type=module` 2 件が実行対象外（ES module。#918）。詳細は「b5（Trello）の原因特定（#964）」 |
| d2 | Saucedemo（`form`） | 失敗 `no_match` | 失敗 `no_match` | 失敗 `no_match` | 未解消 | 取得は成功（HTTP 200）。script は inline 1 件と `type=module` 1 件で、アプリ本体は module 側と推測（実行対象外） |
| d3 | DemoQA（`form`） | 失敗 `no_match` | 失敗 `no_match` | 失敗 `no_match` | 未解消 | 取得は成功（HTTP 200）。script は `type=module` 1 件のみで、実行対象外 |

根拠と補足:

- 取得段階の確認: 計測時に素の `curl` で 3 サイトとも HTTP 200 を確認した。`access_check.sh` は今回の環境で一部ホストが `curl_exit 7`（接続失敗）になったため、`results/access_check.jsonl` は更新せず 2026-10-07 のまま残した（環境要因で、harness の修正は本 Issue の範囲外）
- 実行上限による打ち切り: b5・d2・d3 を 1 件ずつ `--page-js v8 --log` で再実行し、固定診断行（`cdp: page scripts: status=... abort=...`）を回収した。**3 件とも診断行は出ず、上限による打ち切りはない**。ただし診断行が無いことは「実行されて打ち切りなし」と「実行対象なし」を区別しないため、実行の成否の確証にはならない。全 22 件の通し実行では 1 行だけ `status=aborted generation=22 abort=script_bytes` が出た（`generation` は 22 番目の遷移で、タスク順では e5 に当たる。本件の対象外）
- スパイク（`js_api_spike`・`--engine v8`・DOM バインディング無しの素のエンジン）: b5 は 4 件が最初の未定義 API `window` で例外、2 件が module でスキップ、1 件が成功。d2 は inline 1 件が `window` で例外、module 1 件がスキップ。d3 は module 1 件のみでスキップ。素のエンジンの値であり、本番ランタイムの shim 導入後の不足を直接示すものではない。shim 導入後の b5 は「b5（Trello）の原因特定（#964）」で実測した（不足 API ではなく module が主因）
- d2・d3 の解消には ES module の実行対応が前提になる可能性が高い。ただし未実装機能に基づく推測であり、実装後の検証が要る

## ES module 有効での再測定（#1119）

spec 対応: TASK-71 / MS-6 / `COMPAT-1`・`JS-2`・`JS-4`・`JS-5`・`JS-6`・`MEAS-4`・`REPAIR-3`（設計 `docs/design/es-module-design.md`）。
数値は `results/core_results_modules.jsonl` から `jq` で導出した。script 単位の観測は `page_script_probe --modules`（出力はコミットしていない）。

### 測定条件

| 項目 | 値 |
| ---- | -- |
| 実施日時（UTC） | 通し実行: 2026-10-19T01:02:39Z（プローブは同日の直後） |
| OS | Linux |
| ベース | main `8d44877b`（動的 import・設定 gate `[js] execute_modules` を含む）に、`run_core.sh --page-modules` とプローブ拡張（本 PR の 1 件目のコミット）を加えたもの |
| バイナリ | `target/release/fandhe-browser`（`cargo build --release -p fandhe-browser-cli`。既定 feature・V8 同梱）/ プローブは `--features js-v8` の example |
| 設定 | `[js] engine = "v8"`・`execute_page_scripts = true`・`execute_modules = true`（`run_core.sh --page-js v8 --page-modules`） |
| 実行上限 | 既定のまま（`JS-6`。変更していない） |
| 比較の注意 | 対照は 2026-10-11 の `results/core_results.jsonl`（module 無効）。実サイトは時期で変わる |

メタ行は `page_modules_executed` で module 有効での起動を記録する（意味は `page_js_executed` と同じで、ページごとの実行成否ではない）。

### 結果

22 件の成否は 2026-10-11 の module 無効の結果と **全 id で一致**した（15/22 = 68.2%。`COMPAT-4` の閾値 70% = 16/22 に 1 件足りない）。module 有効にしたことで他サイトが悪化した id はない。

失敗の種別は次の固定の分類から選ぶ。(a) 取得 / (b) 上限 / (c) 評価（例外・`missing_api`・`module_compile_failed`・`module_evaluation_failed`・`module_unsupported`）/ (d) 評価は成功したが DOM に出ない。

| id | サイト | module 無効（2026-10-11） | module 有効 | 判定 | 種別 | 根拠（プローブ） |
| -- | ------ | ------------------------ | ----------- | ---- | ---- | ---------------- |
| b5 | Trello（`h1`） | 失敗 `no_match` | 失敗 `no_match` | 未解消 | (a) 取得 | module 2 件は `src_fetch_failed`（`cross_origin`）。初期 HTML に `h1` が無く、実行後も 0 件 |
| d2 | Saucedemo（`form`） | 失敗 `no_match` | 失敗 `no_match` | 未解消 | (c) 評価 | module 1 件（同一オリジンの `src`）は取得できたが `module evaluation failed`。inline 1 件は成功。実行後も `form` は 0 件 |
| d3 | DemoQA（`form`） | 失敗 `no_match` | 失敗 `no_match` | 未解消 | (c) 評価 | script は module 1 件のみ（同一オリジンの `src`）。取得後に `module evaluation failed`。実行後も `form` は 0 件 |

- b5: 設計 §2 の見込みのうち、実際に起きたのは **(a) の `cross_origin`** である。別オリジンの module は取得前に拒否されるため、`src` 1 MiB 上限（`JS-6`）には到達していない。上限は「別オリジン module を許可した場合に初めて顕在化する潜在要因」で、現時点では判断待ち（設計 §6 の案 (b)）。本件は失敗ではなく「未解消・理由あり」として扱う。ほかに classic の別オリジン `src` 1 件が `TypeError`（`read_property`。未定義の値のプロパティ読み取り）、inline 2 件が `non_scalar_result` で例外扱いだが、いずれも module とは別の経路
- d2・d3: 取得・コンパイルまでは通り、**評価で失敗**する。エンジンの例外メッセージは固定文（`module evaluation failed`）で、例外の種類や不足 API は診断に出ない。このため **`JS-5`（DOM / Web API の shim 不足）の範囲か、module 側の未対応（import の解決・top-level await・`import.meta` 等）かは本測定では切り分けられていない**。動的 import は 3 件とも 0 件（`requested=0`）で、動的 import が原因ではない。推測を事実として書かない（`REPAIR-3`）
- 実行上限: 3 件とも `abort` は無し。通し実行の固定診断行は 1 行（`status=aborted generation=22 abort=script_bytes`）で、前回と同じく 22 番目（e5）で、本件の対象外
- 前回の推測「d2・d3 の解消には ES module の実行対応が前提になる」は、前提ではあるが十分ではなかった。module は実行対象になったが、評価失敗が次の壁である

### 次の対応候補

1. module 評価失敗の例外の分類名・不足 API 名を `ScriptOutcome` の診断へ出す（`ModuleEvaluationFailed` の理由を失わない。`REPAIR-4`・`JS-5`）。これで d2・d3 が `JS-5` の shim 不足か module 側の未対応かを切り分けられる
2. 切り分け結果が shim 不足なら `JS-5` の shim 追加（対象 API は切り分け後に決める）
3. b5 は別オリジン module の方針（設計 §6 の案 (b)）と `src` 上限の見直しが要る。いずれもオーナー判断を伴う（`SEC-2`・`JS-6`）
4. d2・d3 の解消は #919（回帰ゲート）の条件。今回は満たしていない

### 制約

- 実施は単一環境（Linux）・単一時点。URL のクエリ・ページ本文・script 本文・生の例外メッセージは記録していない
- `tasks.json`・セレクタ・UA・実行上限は変更していない。`matrix.json` はコミットしておらず、`core_results.jsonl` は上書きしていない

### 再現手順（module 有効）

```bash
cargo build --release -p fandhe-browser-cli
cargo build --release -p fandhe-browser-core --features js-v8 --example compat_page_script_probe
# 22 タスク（--log は絶対パス・コミットしない）
bash harness/compat-practical/run_core.sh --page-js v8 --page-modules --log /abs/path/run.log \
  --out harness/compat-practical/results/core_results_modules.jsonl
jq -s '[.[] | select(.type=="result" and .success==true)] | length' harness/compat-practical/results/core_results_modules.jsonl
jq -c 'select(.type=="meta") | [.page_js_executed,.page_js_engine,.page_modules_executed]' harness/compat-practical/results/core_results_modules.jsonl
# script 単位（d2・d3 は --selector form）
target/release/examples/compat_page_script_probe --target b5=https://trello.com --modules --out /abs/path/b5.jsonl
target/release/examples/compat_page_script_probe --target d2=https://www.saucedemo.com --modules --selector form --out /abs/path/d2.jsonl
target/release/examples/compat_page_script_probe --target d3=https://demoqa.com/automation-practice-form --modules --selector form --out /abs/path/d3.jsonl
```

## b5（Trello）の原因特定（#964）

spec 対応: TASK-71 / MS-6 / `MEAS-4`・`COMPAT-1`・`COMPAT-4`・`JS-4`・`JS-6`・`SEC-2`・`REPAIR-3`。関連 Issue: #918・#758・#309。

### 結論

b5 で `h1` が得られない原因は **ES module（#918）が主因**。`h1` は cross-origin の `type=module` 2 件の実行後に生成され、初期 HTML には無い。不足している Web API・anti-bot・認証・UA による出し分けは、今回の測定では確認されなかった。b5 の成否は `no_match` のまま（解消していない）。

### 測定条件

| 項目 | 値 |
| ---- | -- |
| 実施日時（UTC） | 2026-10-17T11:02Z 前後（単一時点・単一環境） |
| OS | Linux |
| 本実装 | `compat_page_script_probe`（`cargo build --release -p fandhe-browser-core --features js-v8 --example compat_page_script_probe`）。ベースは main の `315d1f17`。V8・`run_page_scripts` の既定の上限（1 script 2 秒・ページ 5 秒・`src` 1 MiB / 件・合計 4 MiB）。UA は `Fetcher` 既定 |
| Chromium | chrome-headless-shell（Playwright 1.63.0 の chromium revision 1243・browserVersion 153.0.8010.12）。UA・ヘッダは既定のまま変更していない（`SEC-2`） |
| 比較の注意 | Chromium 側の計測スクリプトはコミットしていない（ネットワークが要り、オフライン自己テストを持てないため）。手順は下記 |

### 4 点の確認

| 項目 | 本実装 | Chromium |
| ---- | ------ | -------- |
| (1) 初期 HTML の `h1` | 0 件（HTTP 200・リダイレクトなし・531,501 バイト。素の `curl` でも `<h1` は 0 件） | 初期 HTML 自体は未計測。DOMContentLoaded の時点で 1 件 |
| (2) スクリプトの結果 | 下表 | — |
| (3) `h1` が現れる時点 | 実行後も 0 件（実行後 HTML 531,273 バイト） | JS の実行後。DOMContentLoaded の時点で 1 件、load 後・5 秒後も 1 件。`type=module` の 2 件だけを遮断すると 0 件、同意管理の classic script だけを遮断しても 1 件 |
| (4) 応答の違い | 最終 URL はリクエストと同一オリジン | 同じ（HTTP 200・リダイレクトなし・同一オリジン）。出し分けの兆候は見えない |

(3) の追加取得（Chromium、読み込み後 5 秒までの全リクエスト 88 件）: script は 23 件がすべて cross-origin、fetch は cross-origin 17 件・同一オリジン 3 件、XHR は cross-origin 5 件。`h1` は DOMContentLoaded の時点で既にあり、fetch / XHR に依存するかは**未確認**。(4) は構造（script の内訳・SSR 済みの見出し構成）の比較までで、本文の逐語比較はしていない。Chromium で JS だけを無効にした計測は、`--blink-settings=scriptEnabled=false` の `--dump-dom` が空出力になり成立しなかったため、代わりに script の遮断で依存関係を確認した。

### スクリプトごとの結果（本実装）

初期 HTML の `<script>` は 7 件。収集時に実行対象になったのは 4 件（inline 3・src 1）。

| index | 種別 | 結果 | 分類 |
| ----- | ---- | ---- | ---- |
| 0 | データブロック（`type` が JS 以外） | skipped | `skipped_unsupported_type` |
| 1 | inline | exception | `non_scalar_result`（完了値がオブジェクト） |
| 2 | inline | exception | `non_scalar_result`（同上） |
| 3 | src（cross-origin・async。同意管理） | exception | `TypeError`・`read_property`（未定義の値の `domain` を読む） |
| 4 | module（cross-origin） | skipped | `skipped_module` |
| 5 | module（cross-origin） | skipped | `skipped_module` |
| 6 | inline | succeeded | — |

ページ全体: abort なし、`DOMContentLoaded`・`load` とも発火、タイマー発火 0・ページ内 fetch / XHR 0 件、console 出力なし、リスナー・タイマーの例外なし。

### 確認できた事実と推測

確認できた事実:

- 初期 HTML に `h1` は無く、本実装の実行後も無い
- 実行を打ち切る上限（abort）には当たっていない
- 本実装は module 2 件を実行しない。Chromium ではこの 2 件を遮断すると `h1` が消える
- 同意管理の script（index 3）は例外で終わるが、Chromium でこれを遮断しても `h1` は残るため、`h1` の生成には無関係

推測（未検証）:

- module を実行できるようにすれば `h1` が生成される。module の実行には cross-origin の取得が要るため、ページ内 fetch が cross-origin を reject する現状（`JS-8`）との整合を #918 の設計で決める必要がある
- index 1・2 の `non_scalar_result` は、完了値の変換の失敗であり誤報告だった（#1134 で確認・修正。下の小節）

### 追跡先

- ES module: #918 にコメントした（b5 も対象になる旨と根拠）
- `non_scalar_result`（完了値がオブジェクトの inline script が `Exception` と報告される点）は #1134 で対応した（下の小節）

### 完了値がスカラーでない inline script の誤報告（#1134）

- 原因: エンジンはスクリプトを最後まで実行してから完了値を `JsValue` へ変換する。オブジェクト・配列・関数や上限超過の文字列は変換に失敗し `EvaluationFailed` になり、ページ実行がそれをすべて `Exception` にしていた。評価そのものは完了しており、DOM・グローバルの副作用は反映されている（fixture のケース `non-scalar completion value is not an exception` で確認）
- `JS-5`（DOM バインディングの不足）とは無関係で、エンジン API（完了値を返す）とページ実行（完了値を使わない）のずれが原因
- 修正: ページの classic script の評価ソース末尾に `\n;void 0` を付け、完了値を常に `undefined` にした（`page_runner.rs` の `classic_script_source`）。本当の例外（`throw`・構文エラー・参照エラー）は従来どおり `Exception`
- 既知の制限: 接尾辞で 1 MiB の上限を超えるソースには付けない（従来の挙動）。末尾が `if (x)` などで終わる壊れた script は構文エラーにならなくなる
- b5 の再測定: 未実施（ネットワークを使う実測のため、本変更では fixture のテストで同じ形の script が `succeeded` になることだけを根拠とする）。実測では index 1・2 の outcome が `succeeded` に変わる見込みだが、未確認

### 再現手順

```bash
cargo build --release -p fandhe-browser-core --features js-v8 --example compat_page_script_probe
target/release/examples/compat_page_script_probe --target b5="$(jq -r '.[] | select(.id=="b5") | .url' harness/compat-practical/tasks.json)" --engine v8 --out /abs/path/b5.jsonl
```

Chromium 側（コミットしていない）は `mktemp -d` の中に `playwright-core@1.63.0` を `--ignore-scripts` で導入し（`harness/playwright-trace/verify-install.mjs` で integrity を検証）、キャッシュ済みの chrome-headless-shell を `executablePath` に指定して、次を取った。`page.on('request')` の種別別件数、`domcontentloaded`・`load`・5 秒後の `h1` 件数、`page.route` による script の遮断（module 2 件・同意管理 1 件を別々に）。

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
| b5（Trello） | 初期 HTML に `h1` が無く、実行後も生成されない。Chromium では `type=module` 2 件が生成する | ES module 対応（#918）の範囲。anti-bot・認証・UA による出し分けは確認されなかった（#758 の許容には当たらない） |
| d2・d3 | `type=module` のスクリプトが実行対象外 | ES module 対応の要否はオーナー判断 |

## 回帰ゲートとマトリクスの状況

- `results/matrix.json` は**コミットしていない**。scratch で `make_matrix.sh` から生成し `check-matrix.sh --threshold 70 --categories static,spa,form --all-categories` を実行した結果は次のとおりで、コミットすると CI の回帰ゲートが赤になるため見送った
  - 全体 15/22 は閾値未満（FAIL）、form（3/5）・lazy（1/2）・table（1/3）も閾値未満（FAIL）。static・spa は PASS
  - lazy は c1（anti-bot）が残る限り 1/2 = 50% で、JS の成否に関係なく `--all-categories` を通らない
- CI は `--allow-missing` のまま変更していない。`matrix.json` のコミットと `--allow-missing` の削除は #312 の残作業で、対象類型・閾値の扱いはオーナー判断（#759 は not planned で close 済み）

## 制約

- 実サイトは時期で変わる。単一環境（Linux）・単一時点の測定で、ばらつきは評価していない
- `tasks.json`・セレクタ・UA・実行上限は変更していない。成功させるための調整はしていない
- 22 タスクの通し実行では、ページごとのスクリプト実行成否は直接観測していない（固定診断行は打ち切り時のみ出る）。b5 だけは診断プローブ `page_script_probe` で観測した（前述「b5（Trello）の原因特定（#964）」）
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
