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

1. （済み: #1133 で対応。下の節を参照）module 評価失敗の例外の分類名・不足 API 名を診断へ出す（`ModuleEvaluationFailed` の理由を失わない。`REPAIR-4`・`JS-5`）。これで d2・d3 が `JS-5` の shim 不足か module 側の未対応かを切り分けられる
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

## module 評価失敗の分類（#1133）

spec 対応: TASK-71 / MS-6 / `COMPAT-1`・`JS-4`・`JS-5`・`MEAS-4`・`REPAIR-3`・`REPAIR-4`（設計 `docs/design/es-module-design.md`）。
上の「次の対応候補 1」を実装し、d2・d3 を `page_script_probe --modules` で再測定した。出力はコミットしていない。

### 測定条件

| 項目 | 値 |
| ---- | -- |
| 実施日時（UTC） | 2026-10-19T02:00Z 前後 |
| OS | Linux |
| ベース | main `ae6f2f63` に、例外の要約（`ExceptionSummary`）を診断へ出す変更（本 PR の `4baae368`）を加えたもの |
| バイナリ | `cargo build --release -p fandhe-browser-core --features js-v8 --example compat_page_script_probe` |
| 設定 | `--modules`（`[js] execute_modules = true` 相当）・V8・`--selector form`。実行上限は既定のまま |

### 結果

| id | サイト | script | outcome | `error_kind` | `message_class` | `missing_api` | 判定 |
| -- | ------ | ------ | ------- | ------------ | --------------- | ------------- | ---- |
| d2 | Saucedemo | index 1（module・同一オリジン `src`） | exception | `ReferenceError` | `not_defined` | `MutationObserver` | `JS-5` の shim 不足 |
| d3 | DemoQA | index 0（module・同一オリジン `src`） | exception | `ReferenceError` | `not_defined` | `MutationObserver` | `JS-5` の shim 不足 |

- 判定は測定前に固定した規則による: `ReferenceError` / `not_defined` で識別子が Web API 名なら `JS-5` の shim 不足。`SyntaxError`・`missing_export`・link 失敗なら module 側の未対応
- 2 件とも、評価で止まる原因は module の仕組み（import の解決・export の不一致・top-level await・`import.meta`）ではなく、**`MutationObserver` が未定義**であること。動的 import は 0 件（`requested=0`）で、`evaluation_errors` も空
- d2・d3 とも `form` は実行後も 0 件で、**解消していない**。`MutationObserver` の shim を足しても、その先で別の API が不足し得る（今回の測定は最初の例外で止まるため、後続は未確認）。推測を事実として書かない（`REPAIR-3`）
- 同じ module を持つ 2 サイトが同一 API で止まることは、フレームワーク由来の共通依存である可能性を示す。ただし script 本文は読んでおらず、原因の所在は未確認

### 次の対応

1. （済み: #1138 で対応。下の節を参照）`MutationObserver` の shim 追加（`JS-5`）を別 issue で扱う。追加後に d2・d3 を再測定し、次に不足する API を同じ手順で洗い出す。既存の #777・#778・#779 との差分整理が要る。起票は本 PR では行わない
2. #919（回帰ゲート）の条件（d2・d3 の解消）は、今回も満たしていない。#919 はオーナー判断の issue のため、本件は事実の報告に留める
3. 分類で扱えない例外（`other`・`TypeError` の `read_property`）が出たときは、分類語彙の拡張を先に検討する

### 制約

- 単一環境（Linux）・単一時点。URL のクエリ・ページ本文・script 本文・生の例外メッセージは記録していない
- link / instantiate の失敗（`missing_export` を含む）は評価失敗ではなく compile 失敗の固定文で扱われ、要約は付かない（範囲外。V8 の実エンジンでの確認済み）
- boa は例外メッセージに種類の接頭辞を付けないため、`not_defined` 以外は `error_kind` が `null`（`other`）になる。本測定は V8

### 再現手順

```bash
cargo build --release -p fandhe-browser-core --features js-v8 --example compat_page_script_probe
target/release/examples/compat_page_script_probe --target d2=https://www.saucedemo.com --modules --selector form --out /abs/path/d2.jsonl
target/release/examples/compat_page_script_probe --target d3=https://demoqa.com/automation-practice-form --modules --selector form --out /abs/path/d3.jsonl
jq -c 'select(.type=="script")' /abs/path/d2.jsonl /abs/path/d3.jsonl
```

## MutationObserver shim 追加後の再測定（#1138）

spec 対応: TASK-71・TASK-108 / MS-6 / `COMPAT-1`・`JS-5`・`JS-6`・`JS-7`・`MEAS-4`・`REPAIR-3`。
前節（#1133）の「次の対応 1」を実装し、d2・d3 を `page_script_probe --modules` で再測定した。出力はコミットしていない。

### 測定条件

| 項目 | 値 |
| ---- | -- |
| 実施日時（UTC） | 2026-10-19T03:14Z 前後 |
| OS | Linux |
| ベース | main `f1a71f9b` に、`MutationObserver` の shim を追加する変更（本 PR の `ced51f19`）を加えたもの |
| バイナリ | `cargo build --release -p fandhe-browser-core --features js-v8 --example compat_page_script_probe` |
| 設定 | `--modules`（`[js] execute_modules = true` 相当）・V8・`--selector form`。実行上限は既定のまま |

### 結果

| id | サイト | script | outcome | `error_kind` | `message_class` | `missing_api` | `form`（実行後） | 判定 |
| -- | ------ | ------ | ------- | ------------ | --------------- | ------------- | ---------------- | ---- |
| d2 | Saucedemo | index 1（module・同一オリジン `src`） | exception | `TypeError` | `other` | なし | 0 件 | 未解消（例外が前進した） |
| d3 | DemoQA | index 0（module・同一オリジン `src`） | exception | `TypeError` | `other` | なし | 0 件 | 未解消（例外が前進した） |

- 前回の `ReferenceError` / `not_defined` / `MutationObserver` は 2 件とも出なくなった。`MutationObserver` の不足は解消し、評価は次の例外まで進んだ。ただし `form` は実行後も 0 件で、**d2・d3 とも解消していない**
- 次の例外は 2 件とも同じ種類（`TypeError`・`other`）で、現在の分類語彙（`not_defined`・`not_a_function`・`not_a_constructor`・`read_property`・`missing_export`）に当てはまらず、`missing_api` は付かない。動的 import は 0 件（`requested=0`）、`evaluation_errors` は空、abort は無し
- 例外メッセージの形をローカルで確認した（記録はしていない）。`in` 演算子の右辺が `undefined` で、CSS のベンダー接頭辞付きプロパティ名（アニメーション系）を調べている。**推測（未検証）**: 要素の `style`（`CSSStyleDeclaration`）が未提供で、CSS 機能検出が `undefined` を引いている。script 本文は読んでおらず、原因の所在は未確認（`REPAIR-3`）
- 同じ例外が別サイト 2 件で出ており、フレームワーク由来の共通依存である可能性がある。これも script 本文を読んでいないため未確認

### 次の対応

1. （済み: #1141。下の節を参照）要素の `style`（最小の `CSSStyleDeclaration` 相当）の shim が要るかを調べる。`JS-5` の別 issue として扱い、追加後に d2・d3 を同じ手順で再測定して、その次の不足 API を洗い出す。起票は本 PR では行わない
2. （済み: #1140 で対応。下の節を参照）分類語彙の拡張を先に検討する: `TypeError` の `other` のうち「`in` 演算子の右辺が `undefined`」「`undefined` への代入」のように、不足 API の手掛かりになる形を `message_class` に足す（`REPAIR-4`）。今回はこの分類が無いため `missing_api` が付かなかった
3. #919（回帰ゲート）の条件（d2・d3 の解消）は、今回も満たしていない。#919 には解消していないためコメントしていない

### 制約

- 単一環境（Linux）・単一時点。URL のクエリ・ページ本文・script 本文・生の例外メッセージは記録していない
- 判定は最初の例外で止まる測定に基づく。後続の不足 API は未確認

### 再現手順

```bash
cargo build --release -p fandhe-browser-core --features js-v8 --example compat_page_script_probe
target/release/examples/compat_page_script_probe --target d2=https://www.saucedemo.com --modules --selector form --out /abs/path/d2.jsonl
target/release/examples/compat_page_script_probe --target d3=https://demoqa.com/automation-practice-form --modules --selector form --out /abs/path/d3.jsonl
jq -c 'select(.type=="script")' /abs/path/d2.jsonl /abs/path/d3.jsonl
```

## TypeError の分類語彙の拡張後の再測定（#1140）

spec 対応: TASK-71・TASK-108 / MS-6 / `COMPAT-1`・`JS-5`・`MEAS-4`・`REPAIR-3`・`REPAIR-4`。
前節（#1138）の「次の対応 2」を実装した。`message_class` に `cannot_use_in_operator`・`cannot_set_properties` を足し、検証済みのプロパティ名を `property` として出す（`missing_api` とは別。不足 API 名ではない）。d2・d3 を `page_script_probe --modules` で再測定した。出力はコミットしていない。

### 測定条件

| 項目 | 値 |
| ---- | -- |
| 実施日時（UTC） | 2026-10-19T04:37Z 前後 |
| OS | Linux |
| ベース | main `a8c0ddce` に、分類語彙を拡張する変更（本 PR）を加えたもの |
| バイナリ | `cargo build --release -p fandhe-browser-core --features js-v8 --example compat_page_script_probe` |
| 設定 | `--modules`・V8・`--selector form`。実行上限は既定のまま |

### 結果

| id | サイト | script | outcome | `error_kind` | `message_class` | `missing_api` | `property` | `form`（実行後） | 判定 |
| -- | ------ | ------ | ------- | ------------ | --------------- | ------------- | ---------- | ---------------- | ---- |
| d2 | Saucedemo | index 1（module・同一オリジン `src`） | exception | `TypeError` | `cannot_use_in_operator` | なし | `WebkitAnimation` | 0 件 | 未解消（分類のみ） |
| d3 | DemoQA | index 0（module・同一オリジン `src`） | exception | `TypeError` | `cannot_use_in_operator` | なし | `WebkitAnimation` | 0 件 | 未解消（分類のみ） |

- 確認できた事実: 2 件とも `in` 演算子の右辺が `undefined` か `null` の `TypeError` で、調べられたキーは `WebkitAnimation`。動的 import は 0 件、`evaluation_errors` は空、abort は無し
- 推測（未検証）: 要素の `style`（`CSSStyleDeclaration`）が未提供で、CSS のアニメーション機能検出が未定義値を引いている。script 本文は読んでおらず、右辺の値は診断に出さないため、右辺が `undefined` か `null` かも区別していない（`REPAIR-3`）
- 今回は診断の拡張だけで、実行の挙動は変えていない。そのため d2・d3 は解消しておらず、#919 にはコメントしていない

### 次の対応

1. 要素の `style`（最小の `CSSStyleDeclaration` 相当）の shim を別 issue で扱う。追加後に d2・d3 を同じ手順で再測定し、その次の不足 API を洗い出す。起票は本 PR では行わない
2. `Cannot convert undefined or null to object` は分類していない（`other` のまま）。よく出るようなら語彙に足す
3. #919（回帰ゲート）の条件（d2・d3 の解消）は満たしていない

### 制約

- 単一環境（Linux）・単一時点。URL のクエリ・ページ本文・script 本文・生の例外メッセージは記録していない
- boa は `in` の分類のみ取れ、キー名は取れない（メッセージに含まれない）。`cannot_set_properties` に当たる boa の文言は無く `other` のまま
- 判定は最初の例外で止まる測定に基づく。後続の不足 API は未確認

## style shim 追加後の再測定（#1141）

spec 対応: TASK-71・TASK-108 / MS-6 / `COMPAT-1`・`JS-5`・`JS-6`・`MEAS-4`・`REPAIR-3`・`SEC-2`。
前節（#1138）の「次の対応 1」を実装し、d2・d3 を `page_script_probe --modules` で再測定した。出力はコミットしていない。

### 原因の確認結果

- 前節の推測（要素の `style` が未提供で、CSS 機能検出が `undefined` を引く）は**正しかった**。`style` の shim を足すと、d2・d3 とも `TypeError`（`message_class=other`）が出なくなり、評価が次の例外まで進んだ
- 確認は shim 追加の前後の `error_kind` / `message_class` の変化による。script 本文・生の例外メッセージは読んでいない（当時は module 評価失敗の理由を stderr に出す経路が無かった。現在は `--debug-raw-exception` で stderr にだけ出せる。#1152）

### 測定条件

| 項目 | 値 |
| ---- | -- |
| 実施日時（UTC） | 2026-10-19T04:40Z 前後 |
| OS | Linux |
| ベース | main `a8c0ddce` に、要素の `style` の shim を追加する変更（本 PR の `0bea5d9c`）を加えたもの |
| バイナリ | `cargo build --release -p fandhe-browser-core --features js-v8 --example compat_page_script_probe` |
| 設定 | `--modules`（`[js] execute_modules = true` 相当）・V8・`--selector form`。実行上限は既定のまま |

### 結果

| id | サイト | script | outcome | `error_kind` | `message_class` | `missing_api` | `form`（実行後） | 判定 |
| -- | ------ | ------ | ------- | ------------ | --------------- | ------------- | ---------------- | ---- |
| d2 | Saucedemo | index 1（module・同一オリジン `src`） | exception | `ReferenceError` | `not_defined` | `URL` | 0 件 | 未解消（例外が前進した） |
| d3 | DemoQA | index 0（module・同一オリジン `src`） | exception | `Error` | `other` | なし | 0 件 | 未解消（例外が前進した） |

- 前節の `TypeError` / `other` は 2 件とも出なくなった。**d2・d3 とも `form` は 0 件で、解消していない**。abort は無し
- 次の不足 API
  - d2: `URL`（グローバルのコンストラクタ）。URL の解析（WHATWG URL）は仕様が大きく、JS だけの小さな shim に収まらない。core の URL 解析を呼ぶ bridge op の追加を伴うため、本 PR では扱わず止めた（新しい op の設計が要る）
  - d3: 種類は `Error`（`message_class=other`）で、不足 API は不明。script 本文と生メッセージを読んでいないため、原因の所在は未確認（`REPAIR-3`）。module 評価失敗の理由を安全に取り出す診断の拡張（`REPAIR-4`）が先に要る可能性がある
- #919（回帰ゲート）の条件（d2・d3 の解消）は今回も満たしていない。#919 にはコメントしていない

### 次の対応

1. `URL` / `URLSearchParams` の shim（core の URL 解析を呼ぶ op の設計を含む）。`JS-5` の別 issue として扱う。起票は本 PR では行わない（済み: #1144。後続の節を参照）
2. d3 の `Error` の原因特定。module 評価失敗の診断に、例外の分類名に加えて不足 API の手掛かりを出す拡張を検討する（`REPAIR-4`）（済み: #1145。下の節を参照）

### 制約

- 単一環境（Linux）・単一時点。URL のクエリ・ページ本文・script 本文・生の例外メッセージは記録していない
- 判定は最初の例外で止まる測定に基づく。後続の不足 API は未確認
- `style` の shim は `profiles/`（`PLUG-8`）を使わない。ブラウザ別の CSS 機能データを JS の面に反映すると、選んだプロファイルで `in` 判定が変わり識別面に近づくため（`SEC-2`）。ベンダー接頭辞付きのプロパティは定義せず、`getComputedStyle` も定義しない
- HTML パーサー・`innerHTML` で作られた要素の元の `style` 属性は読む op が無く、shim から最初に書くと `style` 属性全体を置き換える（`REPAIR-3`。解消済み: #1146。`getAttribute` op を足し、style の shim が最初のアクセスで元の属性を読んで初期化する。属性の列挙は未対応）

### 再現手順

```bash
cargo build --release -p fandhe-browser-core --features js-v8 --example compat_page_script_probe
target/release/examples/compat_page_script_probe --target d2=https://www.saucedemo.com --modules --selector form --out /abs/path/d2.jsonl
target/release/examples/compat_page_script_probe --target d3=https://demoqa.com/automation-practice-form --modules --selector form --out /abs/path/d3.jsonl
jq -c 'select(.type=="script")' /abs/path/d2.jsonl /abs/path/d3.jsonl
```

## module 評価失敗の Error の分類追加後の再測定（#1145）

spec 対応: TASK-71・TASK-108 / MS-6 / `COMPAT-1`・`JS-5`・`MEAS-4`・`REPAIR-3`・`REPAIR-4`。
前節（#1141）の「次の対応 2」を実装した。d3 の `Error`（`message_class=other`）の原因を特定するため、`message_class` に `minified_react_error` を、検証済みのエラー番号として `error_code` を足し、d2・d3 を `page_script_probe --modules` で再測定した。出力はコミットしていない。

### 形の確認

- 生メッセージは core が要約後に捨てるため、コミットしない一時パッチ（要約関数の入口で先頭 300 文字を stderr に出す）でローカルに 1 回だけ確認し、パッチは戻した。生メッセージはファイル・REPORT・テスト fixture には記録していない
- d3 の例外は **React 本番ビルド（minified）の定型エラー**（`Minified React error #<番号>; visit <案内 URL> ...` の形）だった。アプリ固有の文言ではなく、ライブラリのエラーコード形式（分類 A）。不足 API 名は含まれない
- 診断には番号だけを `error_code` として出す。案内 URL・引数は出さない

### 測定条件

| 項目 | 値 |
| ---- | -- |
| 実施日時（UTC） | 2026-10-19T07:55Z 前後 |
| OS | Linux |
| ベース | main `1073cc70` に、分類追加の変更（本 PR）を加えたもの |
| バイナリ | `cargo build --release -p fandhe-browser-core --features js-v8 --example compat_page_script_probe` |
| 設定 | `--modules`・V8・`--selector form`。実行上限は既定のまま |

### 結果

| id | サイト | script | outcome | `error_kind` | `message_class` | `missing_api` | `error_code` | `form`（実行後） | 判定 |
| -- | ------ | ------ | ------- | ------------ | --------------- | ------------- | ------------ | ---------------- | ---- |
| d2 | Saucedemo | index 1（module・同一オリジン `src`） | exception | `ReferenceError` | `not_defined` | `URL` | なし | 0 件 | 未解消（変化なし） |
| d3 | DemoQA | index 0（module・同一オリジン `src`） | exception | `Error` | `minified_react_error` | なし | `299` | 0 件 | 未解消（原因の分類を特定） |

- 確認できた事実: d3 は React のエラー番号 299 で module 評価が失敗している。動的 import は 0 件、abort は無し。d2 は #1141 と同じ（`URL` 未定義）
- 推測（未検証）: React の公開エラーコード表では 299 は「描画先（container）が DOM 要素でない」に当たると記憶している（表は未確認）。もし正しければ、`createRoot` に渡す要素の取得が `null` になっている。候補は、(a) `getElementById` 等で root 要素が取れない、(b) 評価時点で root 要素が DOM に無い（HTML は約 440 バイトと小さく、root 要素は初期 HTML にあるはず）。script 本文は読んでいない（`REPAIR-3`）
- 不足 API 名は特定できなかった。分類が示すのは React の内部検査の失敗で、どの DOM API が原因かは番号だけからは分からない。足りないのは、`createRoot` に渡された値の種類（`null` か別の値か）と、root の取得に使った API を安全に示す診断
- #919（回帰ゲート）の条件（d2・d3 の解消）は満たしていない。`form` は 2 件とも 0 件のため、#919 にはコメントしていない

### 次の対応

1. d3: root 要素の取得（`document.getElementById`・`querySelector` と、モジュール評価時点の DOM の状態）を、ローカルの最小 fixture で再現して確認する。`createRoot` の引数が `null` になる条件を特定する。起票は本 PR では行わない
2. d2: `URL` / `URLSearchParams` の shim（#1141 の次の対応 1 のまま）
3. 恒久的な診断: module 評価失敗の原因の確認で、一時パッチを要したのは #1140・#1141・#1145 の 3 回目。ローカル限定で安全に確認できる経路を検討する余地がある（起票は本 PR では行わない）（済み: #1152。`page_script_probe --debug-raw-exception` で stderr にだけ出せる）

### 制約

- 単一環境（Linux）・単一時点。URL のクエリ・ページ本文・script 本文・生の例外メッセージは記録していない
- boa は接頭辞なしの本文だけを返す。`Minified React error #N` は `Error` としてしか投げられないため `Error` と推定する。それ以外の文言の種類は `other` のまま
- 判定は最初の例外で止まる測定に基づく。後続の不足 API は未確認

### 再現手順

```bash
cargo build --release -p fandhe-browser-core --features js-v8 --example compat_page_script_probe
target/release/examples/compat_page_script_probe --target d2=https://www.saucedemo.com --modules --selector form --out /abs/path/d2.jsonl
target/release/examples/compat_page_script_probe --target d3=https://demoqa.com/automation-practice-form --modules --selector form --out /abs/path/d3.jsonl
jq -c 'select(.type=="script")' /abs/path/d2.jsonl /abs/path/d3.jsonl
```

## URL / URLSearchParams shim 追加後の再測定（#1144）

spec 対応: TASK-71・TASK-108 / MS-6 / `COMPAT-1`・`JS-5`・`JS-6`・`MEAS-4`・`REPAIR-3`・`SEC-2`。
前節（#1141）の「次の対応 1」を実装し、d2・d3 を `page_script_probe --modules` で再測定した。出力はコミットしていない。

### 実装の要点

- bridge op `urlParse(input, base)` を 1 つ追加した。`reqwest::Url`（WHATWG 準拠）に解析を任せ、構成要素 11 個を `\n` 区切りの 1 文字列で返す（失敗は null）。入力長は既存の文字列引数の上限（既定 64 KiB）で解析の前に検証する。ネットワークには出ず、診断も記録しない
- `js_shim/url.js` が `URL`（getter 11 個・`toString` / `toJSON`・`searchParams`・`URL.canParse`・`href` / `search` / `hash` の setter）と `URLSearchParams`（form-urlencoded の解析・直列化・`get` / `getAll` / `has` / `set` / `append` / `delete` / 反復 / `forEach`、`URL` との同期）を提供する

### 測定条件

| 項目 | 値 |
| ---- | -- |
| 実施日時（UTC） | 2026-10-19T08:04Z 前後 |
| OS | Linux |
| ベース | main `1073cc70` に、URL / URLSearchParams の shim と `urlParse` op を追加する変更（本 PR の `674e0abb`）を加えたもの |
| バイナリ | `cargo build --release -p fandhe-browser-core --features js-v8 --example compat_page_script_probe` |
| 設定 | `--modules`（`[js] execute_modules = true` 相当）・V8・`--selector form`。実行上限は既定のまま |

### 結果

| id | サイト | script | outcome | `error_kind` | `message_class` | `missing_api` | `form`（実行後） | 判定 |
| -- | ------ | ------ | ------- | ------------ | --------------- | ------------- | ---------------- | ---- |
| d2 | Saucedemo | index 1（module・同一オリジン `src`） | exception | `ReferenceError` | `not_defined` | `AbortController` | 0 件 | 未解消（例外が前進した） |
| d3 | DemoQA | index 0（module・同一オリジン `src`） | exception | `Error` | `other` | なし | 0 件 | 未解消（変化なし） |

- d2 は `URL` の `ReferenceError` を越え、次の不足 API `AbortController` で止まった。**d2・d3 とも `form` は 0 件で、解消していない**。abort は無し
- d3 は #1145 の取り込み前の測定のため `message_class=other` と記録した。#1145 の取り込み後は `minified_react_error`（`error_code=299`）に分類される（前節。再測定していない）。本 issue の対象外
- #919（回帰ゲート）の条件（d2・d3 の解消）は今回も満たしていない。#919 にはコメントしていない

### 次の対応

1. d2: `AbortController` / `AbortSignal`。`EventTarget`（現状は document / window の最小実装のみ）、`DOMException`（`AbortError` の `reason`。DOMException への写像は未実装）、`AbortSignal.timeout`（timers 連携）が絡み、30 分・1 ファイル程度には収まらないと判断し、本 PR では続行せず止めた。`net.js` の `fetch` への `signal` 連携も含めて別 issue（`JS-5`・`JS-8`）。起票済み: #1150（済み: #1150。下の節を参照）
2. d3 の原因特定（前節 #1145 の次の対応 1 のまま。`REPAIR-4`）（済み: #1151。下の節を参照）

### 制約

- 単一環境（Linux）・単一時点。URL のクエリ・ページ本文・script 本文・生の例外メッセージは記録していない
- 判定は最初の例外で止まる測定に基づく。後続の不足 API は未確認
- 未実装: `protocol` / `username` / `password` / `host` / `hostname` / `port` / `pathname` の setter（getter だけ。strict モードの代入は `TypeError`）、`URL.createObjectURL` / `revokeObjectURL`・`URLPattern`（定義しない）、`URLSearchParams` の `sort` / `size`、op の文字列上限（64 KiB）を超える URL（失敗として扱う）（`REPAIR-3`）

### 再現手順

```bash
cargo build --release -p fandhe-browser-core --features js-v8 --example compat_page_script_probe
target/release/examples/compat_page_script_probe --target d2=https://www.saucedemo.com --modules --selector form --out /abs/path/d2.jsonl
target/release/examples/compat_page_script_probe --target d3=https://demoqa.com/automation-practice-form --modules --selector form --out /abs/path/d3.jsonl
jq -c 'select(.type=="script")' /abs/path/d2.jsonl /abs/path/d3.jsonl
```

生の例外メッセージを確認するときは `--modules --debug-raw-exception` を付ける（#1152）。メッセージは 300 文字に切り詰められ、stderr にだけ出る。URL・トークンを含み得るため、stderr は端末で読むだけにし、ファイルへ保存・転記しない。

## Node の基本プロパティ追加後の再測定（#1151）

spec 対応: TASK-71・TASK-108 / MS-6 / `COMPAT-1`・`JS-5`・`JS-6`・`MEAS-4`・`REPAIR-3`。
前節（#1144）の「次の対応 2」にあたる d3（React #299）を調べ、`nodeType` など Node の基本プロパティを足して d2・d3 を `page_script_probe --modules` で再測定した。出力はコミットしていない。

### 原因の確認結果

- React 公開のエラーコード表（`facebook/react` の `scripts/error-codes/codes.json`）で #299 は「Target container is not a DOM element.」で、`createRoot` の container 検査（`isValidContainer`: `nodeType` が 1・9・11）にあたることを確認した。#1145 の節で「未確認」としていた表の照合はこれで済み
- 推測（`getElementById('root')` は要素を返すが `nodeType` が `undefined` のため検査に落ちる）を、React 本体を含まない最小 fixture（`tests/support/page_runner_cases.rs` の `react_container_check_cases`）で確認した。修正前は V8 で `typeof r.nodeType` が `undefined`（root は取得できている）で script が例外終了し、修正後は `number` で検査を通り `data-ok="1"` が付く（V8・boa とも）

### 実装の要点

- `dom.js`: `nodeType`（Element 1・Text 3・Document 9）、`Node` の定数 12 個（コンストラクタとプロトタイプの両方・読み取り専用）、Text / Document の `nodeName`（`#text` / `#document`）、`ownerDocument`（Document は `null`）を足した。getter はラッパー以外の受け手を `TypeError` で拒否し、ノード ID は露出しない
- `events.js`: 再測定で d3 の次の不足が `i.addEventListener`（要素レベル）と分かったため、`Node.prototype` に `addEventListener` / `removeEventListener` を足した。**保持のみで配送はしない**スタブ（`dispatchEvent` は未定義・capture は区別しない・1 要素 1024 件まで。上限超過は既存の `lifecycleListenerLimit` で通知。`JS-6`）
- スタブとして残したもの（`REPAIR-3`）: Element の `tagName` / `nodeName`（要素名を返す op が無い）、`parentNode` / `childNodes` 等の関係、`nodeValue` / `namespaceURI` / `isConnected` / `contains` / `compareDocumentPosition`、`attributes`、`Comment` / `DocumentFragment` のラッパー。いずれも未定義のまま（`dom.js` 冒頭に明記）

### 測定条件

| 項目 | 値 |
| ---- | -- |
| 実施日時（UTC） | 2026-10-19T10:58Z 前後 |
| OS | Linux |
| ベース | main `523779bd` に、本 PR の変更（Node の基本プロパティと要素レベルの `addEventListener` スタブ）を加えたもの |
| バイナリ | `cargo build --release -p fandhe-browser-core --features js-v8 --example compat_page_script_probe` |
| 設定 | `--modules`（`[js] execute_modules = true` 相当）・V8・`--selector form`。実行上限は既定のまま |

### 結果

| id | サイト | script | outcome | `error_kind` | `message_class` | `missing_api` | `error_code` | `form`（実行後） | 判定 |
| -- | ------ | ------ | ------- | ------------ | --------------- | ------------- | ------------ | ---------------- | ---- |
| d2 | Saucedemo | index 1（module・同一オリジン `src`） | exception | `ReferenceError` | `not_defined` | `AbortController` | なし | 0 件 | 未解消（変化なし） |
| d3 | DemoQA | index 0（module・同一オリジン `src`） | succeeded | なし | なし | なし | なし | 0 件 | module 評価は成功。`form` は未解消 |

- d3 は `nodeType` を足した時点で #299 を越え、次に要素レベルの `addEventListener` の `TypeError`（`not_a_function`）で止まった。これを保持のみのスタブで越えると、module 評価は例外なしで終わった
- 実行後も `form` は 0 件で、タイマー（`event_loop.fired` は 2）の callback で `timer_errors` に `other` が 1 件記録されている。描画段階（React の scheduler 以降）の不足が考えられるが、現在の診断では不足 API を特定できない（タイマー callback の例外は `message_class` を持たない）
- abort は無し。**d2・d3 とも `form` は 0 件のため #919 にはコメントしていない**

### 次の対応

1. d3: タイマー callback の例外の分類（`timer_errors` に `message_class` / `missing_api` を持たせる診断）。これで描画段階の次の不足 API を特定する。診断の拡張が先で、shim 単体では 30 分・1 ファイルに収まらないと判断し、本 PR では続行せず止めた。#1156 で追跡する（済み: #1156。下の節を参照）
2. d2: `AbortController`（#1150）（済み: #1150。下の節を参照）

### 制約

- 単一環境（Linux）・単一時点。URL のクエリ・ページ本文・script 本文・生の例外メッセージは記録していない
- 判定は最初の例外で止まる測定に基づく。タイマー callback 内の不足は未特定
- 要素レベルのイベントは保持のみで、`click` 等は発火しない（`REPAIR-3`）

### 再現手順

```bash
cargo build --release -p fandhe-browser-core --features js-v8 --example compat_page_script_probe
target/release/examples/compat_page_script_probe --target d2=https://www.saucedemo.com --modules --selector form --out /abs/path/d2.jsonl
target/release/examples/compat_page_script_probe --target d3=https://demoqa.com/automation-practice-form --modules --selector form --out /abs/path/d3.jsonl
jq -c 'select(.type=="script")' /abs/path/d2.jsonl /abs/path/d3.jsonl
```

## AbortController / AbortSignal shim 追加後の再測定（#1150）

spec 対応: TASK-71・TASK-108・TASK-111 / MS-6 / `COMPAT-1`・`JS-5`・`JS-6`・`JS-7`・`JS-8`・`MEAS-4`・`REPAIR-3`・`SEC-2`。
前節（#1151）の「次の対応 2」を実装し、d2・d3 を `page_script_probe --modules` で再測定した。出力はコミットしていない。

### 実装の要点

- `js_shim/abort.js`（timers.js の後・net.js の前に評価）が `DOMException`（`AbortError` / `TimeoutError` の code を含む最小実装）、`AbortSignal`（`aborted` / `reason` / `onabort` / `throwIfAborted` / `addEventListener` / `removeEventListener`・静的 `abort` / `timeout` / `any`）、`AbortController` を提供する。`AbortSignal.timeout` は install 時に捕捉した `setTimeout` に載り、期限は core のイベントループの仮想時計で進む
- 汎用の `EventTarget` / `Event` / `dispatchEvent` は定義しない（`typeof` は `'undefined'` のまま。`SEC-2`）。AbortSignal は `abort` イベントだけを自前のリスナー保持（1 シグナル 1024 件まで。超過は `lifecycleListenerLimit`）で配る
- `net.js` の `fetch` が `init.signal` を扱う。中止済みなら要求を出さずに reason で reject し、要求後の中止は未着手の要求を `netCancel` で取り除いて reject する。AbortSignal でない値は `TypeError` で reject する。`signal` は `netOptionsIgnored` の集計から外した
- Rust 側の変更は `JS_SHIM_SOURCES` への 1 件の追加のみ（新しい bridge op・依存・`unsafe` なし）

### 測定条件

| 項目 | 値 |
| ---- | -- |
| 実施日時（UTC） | 2026-10-19T11:57Z 前後 |
| OS | Linux |
| ベース | main `947e9cdd` に、abort.js の追加と fetch の signal 対応を加えたもの |
| バイナリ | `cargo build --release -p fandhe-browser-core --features js-v8 --example compat_page_script_probe` |
| 設定 | `--modules`（`[js] execute_modules = true` 相当）・V8・`--selector form`。実行上限は既定のまま |

### 結果

| id | サイト | script | outcome | `error_kind` | `message_class` | `missing_api` | `error_code` | `form`（実行後） | 判定 |
| -- | ------ | ------ | ------- | ------------ | --------------- | ------------- | ------------ | ---------------- | ---- |
| d2 | Saucedemo | index 1（module・同一オリジン `src`） | exception | `TypeError` | `read_property` | なし（`property=port`） | なし | 0 件 | 未解消（例外が前進した） |
| d3 | DemoQA | index 0（module・同一オリジン `src`） | succeeded | なし | なし | なし | なし | 0 件 | 変化なし（module 評価は成功。`form` は未解消） |

- d2 は `AbortController` の `ReferenceError` を越え、次に `port` プロパティの読み取り（`TypeError`・`read_property`）で止まった。未定義の値の `port` を読んでおり、`location.port` 周辺（`location` は `URL` 相当の構成要素を持つか）か URL 系の戻り値が候補だが、**生の例外メッセージは確認していない**ため推測にとどまる
- d3 は変化なし（タイマー callback の `timer_errors` に `other` が 1 件。#1156 で追跡）。abort は両方とも無し
- **d2・d3 とも `form` は 0 件のため #919 にはコメントしていない**

### 次の対応

1. d2: `port` を読む対象の特定（`--debug-raw-exception` で端末のみに出して確かめる）。`location.port` なら `window.js` の `location` の構成要素の不足の可能性がある。追跡: #1158（済み: #1158。下の節を参照）
2. d3: タイマー callback の例外の分類（#1156）（済み: #1156。下の節を参照）

### 制約

- 単一環境（Linux）・単一時点。URL のクエリ・ページ本文・script 本文・生の例外メッセージは記録していない
- 判定は最初の例外で止まる測定に基づく。後続の不足 API は未確認
- 未実装（`REPAIR-3`）: 汎用 `EventTarget` / `Event` / `dispatchEvent`、既存のブリッジエラーの DOMException への写像、XHR / `Request` の signal、着手済みの要求の取り消し（応答は届き、捨てられる。受理件数は戻らない）、`onabort` の呼び出し順の簡略化（リスナー一覧の設定位置ではなく最初に呼ぶ）、`addEventListener` の `signal` / `passive` / capture

### 再現手順

```bash
cargo build --release -p fandhe-browser-core --features js-v8 --example compat_page_script_probe
target/release/examples/compat_page_script_probe --target d2=https://www.saucedemo.com --modules --selector form --out /abs/path/d2.jsonl
target/release/examples/compat_page_script_probe --target d3=https://demoqa.com/automation-practice-form --modules --selector form --out /abs/path/d3.jsonl
jq -c 'select(.type=="script")' /abs/path/d2.jsonl /abs/path/d3.jsonl
```

## タイマー callback の例外の分類と d3 の再測定（#1156）

spec 対応: TASK-71・TASK-108 / MS-6 / `COMPAT-1`・`JS-5`・`JS-6`・`JS-7`・`MEAS-4`・`REPAIR-3`・`REPAIR-4`・`SEC-2`。
前節（#1150）の「次の対応 2」を実装し、d3 を `page_script_probe --modules --debug-raw-exception` で再測定した。出力はコミットしていない。

### 実装の要点

- `exception_summary` を `page_runner` の下から crate 直下へ移し、`dom_bridge` のタイマー callback の例外にも使う。`TimerError` に `summary`（`ExceptionSummary`: `kind`・`message_class`・`identifier`・`property`・`code`）を足した。生メッセージは従来どおり `message` に残るが、JSONL には出さない
- shim（`timers.js`・`observers.js`）が `error.name` を文字列のときだけ 64 文字までで `timerCallbackError` op（引数は source・name・message の 3 つ）へ渡す。Rust 側は固定語彙に一致する場合だけ `kind` に採用し、語彙外は本文からの推定か `other` に落とす（名前は保存しない）
- probe の `run` 行の `timer_errors` は `<source>:<kind>:<class>[:<missing_api>]` のラベルになる。`--debug-raw-exception` はタイマー callback の例外の生メッセージも stderr だけへ出す（`<id> timer_error#<n> <source>`）
- `listener_errors`・`net_callback_errors` は従来どおりハーネス側の `class_label` で分類したまま（`REPAIR-3`）

### 測定条件

| 項目 | 値 |
| ---- | -- |
| 実施日時（UTC） | 2026-10-19T17:10Z 前後 |
| OS | Linux |
| ベース | main `29b26205` に、本変更を加えたもの |
| バイナリ | `cargo build --release -p fandhe-browser-core --features js-v8 --example compat_page_script_probe` |
| 設定 | `--modules`・V8・`--selector form`・`--debug-raw-exception`。実行上限は既定のまま |

### 結果

| id | サイト | script | outcome | `timer_errors` | `form`（実行後） | 判定 |
| -- | ------ | ------ | ------- | -------------- | ---------------- | ---- |
| d3 | DemoQA | index 0（module・同一オリジン `src`） | succeeded | `timeout:TypeError:other` | 0 件 | 未解消（原因の種類が分かった） |

- 例外の種類は `TypeError`。`setTimeout` の callback で発生する。メッセージ型は現行の語彙外のため `other` で、`missing_api` は取れない
- 端末で確認した生メッセージ（エンジン固定文）は `Right-hand side of 'instanceof' is not an object`。`x instanceof Y` の `Y` が関数ではない値（未定義のグローバルコンストラクタなど）であることを示す。**どの `Y` かはメッセージに含まれず、現在の診断では特定できない**
- abort は無し。`event_loop.fired` は 2。**`form` は 0 件のため #919 にはコメントしていない**

### 次の対応

1. 分類語彙に `Right-hand side of 'instanceof' is not an object`（V8 / boa の文言差を含む）を加える。ただし `Y` の名前は得られないため、不足 API の特定には「`instanceof` の右辺の名前を出す」手段が別に要る（例: callback の `stack` から行位置を得る、または shim 側で主要なグローバルコンストラクタ〔`Element`・`HTMLElement`・`Node` 等〕の有無を測る）。別 issue で追跡する
2. `listener_errors`・`net_callback_errors` も core の要約で分類する（`REPAIR-3`。済み: #1172。下の節を参照）

### 制約

- 単一環境（Linux）・単一時点。URL のクエリ・ページ本文・script 本文は記録していない。生メッセージはエンジンの固定文 1 件だけを転記した
- 判定は最初の例外で止まる測定に基づく。後続の不足 API は未確認

### 再現手順

```bash
cargo build --release -p fandhe-browser-core --features js-v8 --example compat_page_script_probe
target/release/examples/compat_page_script_probe --target d3=https://demoqa.com/automation-practice-form --modules --selector form --debug-raw-exception --out /abs/path/d3.jsonl
jq -c 'select(.type=="run") | .bridge.timer_errors' /abs/path/d3.jsonl
```

## document.location / document.URL の追加と d2・d3 の再測定（#1158）

spec 対応: TASK-71・TASK-108 / MS-6 / `COMPAT-1`・`JS-5`・`MEAS-4`・`REPAIR-3`・`SEC-2`。
前節（#1150）の「次の対応 1」を調べ、d2 の `port` の読み取りで止まる原因を確認して `document.location`・`document.URL` を足し、d2・d3 を `page_script_probe --modules --debug-raw-exception` で再測定した。出力はコミットしていない。

### 実装の要点

- 原因（確認済み）: 修正前の端末出力は `Uncaught TypeError: Cannot read properties of undefined (reading 'port')`。ページの bundle 内で `.port` を読む箇所は Backtrace SDK の属性収集の `document.location.port` の 1 箇所で、`document.location` が未定義だった。`window.location.port` は実装済みで、`Location` の構成要素の不足は原因ではない
- `window.js` で `Document.prototype` に `location`（`window.location` と同一のオブジェクトを返す accessor）と `URL`（`location.href` と同じ値の読み取り専用 getter）を足した。`document.location` への代入は遷移せず、`window.location` への代入と同じ `assignLocation` 診断に記録する（長さのみ。区別しない簡略化）。`document.URL` は代入しても変わらない
- Rust の変更・新しい op・依存の追加は無い。V8・boa の共通ケース（`js_shim_cases.rs` の `window_cases`）で検証した

### 測定条件

| 項目 | 値 |
| ---- | -- |
| 実施日時（UTC） | 2026-10-19 |
| OS | Linux |
| ベース | main `0efc2c7d` に、本変更を加えたもの |
| バイナリ | `cargo build --release -p fandhe-browser-core --features js-v8 --example compat_page_script_probe` |
| 設定 | `--modules`・V8・`--selector form`・`--debug-raw-exception`。実行上限は既定のまま |

### 結果

| id | サイト | script | outcome | 最初の例外 | `form`（実行後） | 判定 |
| -- | ------ | ------ | ------- | ---------- | ---------------- | ---- |
| d2 | Saucedemo | index 1（module・同一オリジン `src`） | exception | `TypeError`・`read_property`・`property=getItem`（`module_evaluation`） | 0 件 | 未解消（`port` は越えた） |
| d3 | DemoQA | index 0（module・同一オリジン `src`） | succeeded | なし（`timer_errors` は `timeout:TypeError:other`） | 0 件 | 変化なし |

- d2 は `port` の TypeError を越え、次に `Cannot read properties of undefined (reading 'getItem')` で止まる。読み取り先の名前はメッセージに含まれないが、`getItem` は Web Storage の API で、`localStorage`（または `sessionStorage`）が shim に無いことが有力な候補（未確認）
- d3 は #1156 の節と同じ（`instanceof` の右辺が object ではない TypeError）
- **d2・d3 とも `form` は 0 件のため #919 にはコメントしていない**

### 次の対応

1. d2: `localStorage` / `sessionStorage` の不足の確認。保存を持ち越さない最小の in-memory 実装にするか、識別面にならない形で足せるかを `SEC-2` の観点で判断し、別 issue で追跡する。追跡: #1167（済み: #1167。下の節を参照）
2. d3: `instanceof` の右辺の特定（前節 #1156 の次の対応 1 のまま）
3. Backtrace SDK は `history.pushState` を包む処理も持つ。`history` が shim に無いため、`localStorage` の後に出る可能性がある（静的に読んだ推測で、未測定）

### 制約

- 単一環境（Linux）・単一時点。URL のクエリ・ページ本文・script 本文は記録していない。生メッセージはエンジンの固定文だけを転記した
- 判定は最初の例外で止まる測定に基づく。後続の不足 API は未確認
- 未実装（`REPAIR-3`）: `location` 代入による実際の遷移、`[LegacyUnforgeable]` との差異（`document.location` は `Document.prototype` に置いている）、navigator の追加プロパティ（フィンガープリント面を作らないため意図的に入れていない）

### 再現手順

```bash
cargo build --release -p fandhe-browser-core --features js-v8 --example compat_page_script_probe
target/release/examples/compat_page_script_probe --target d2=https://www.saucedemo.com --modules --selector form --debug-raw-exception --out /abs/path/d2.jsonl
target/release/examples/compat_page_script_probe --target d3=https://demoqa.com/automation-practice-form --modules --selector form --debug-raw-exception --out /abs/path/d3.jsonl
jq -c 'select(.type=="script")' /abs/path/d2.jsonl /abs/path/d3.jsonl
jq -c 'select(.type=="run") | .bridge.timer_errors' /abs/path/d2.jsonl /abs/path/d3.jsonl
```

## localStorage / sessionStorage の追加と d2・d3 の再測定（#1167）

spec 対応: TASK-71・TASK-108 / MS-6 / `COMPAT-1`・`JS-5`・`JS-6`・`MEAS-4`・`REPAIR-3`・`SEC-2`。
前節（#1158）の「次の対応 1」を調べ、`localStorage` / `sessionStorage` の in-memory 最小実装を足して、d2・d3 を `page_script_probe --modules --debug-raw-exception` で再測定した。出力はコミットしていない。

### 実装の要点

- 原因（確認済みの範囲）: 修正前の `getItem` の TypeError は、shim に `localStorage` / `sessionStorage` が無いことで説明できる。追加後に同じ TypeError が消えた。bundle の静的な調査はしていないため、読み取り先がどちらの名前かは区別していない（未確認）
- `storage.js`（新規）で `Storage` と、独立した 2 つのインスタンス `localStorage` / `sessionStorage` を足した。`getItem`・`setItem`・`removeItem`・`clear`・`key`・`length` を持ち、キーと値は文字列にする
- 保存は shim を install した 1 回のページ実行の中だけ（in-memory）。プロファイルへの永続化や実行間の共有はしない（`PROF` 系の境界。永続化の設計は別 issue）
- 上限（`JS-6`）: 1 ストアあたり、全キー長と全値長の合計 5 MiB（UTF-16 code unit）、件数 65536。超えたら `QuotaExceededError`（DOMException・code 22）を投げ、状態は変えない
- 識別面（`SEC-2`）: 残量・クォータを示すプロパティ、`navigator.storage`、`StorageEvent` は定義しない
- Rust の変更（`JS_SHIM_SOURCES` への追加のみ）・新しい op・依存の追加は無い。V8・boa の共通ケース（`js_shim_cases.rs` の `storage_cases`）で検証した

### 測定条件

| 項目 | 値 |
| ---- | -- |
| 実施日時（UTC） | 2026-10-19 |
| OS | Linux |
| ベース | main `d39d65be` に、本変更を加えたもの |
| バイナリ | `cargo build --release -p fandhe-browser-core --features js-v8 --example compat_page_script_probe` |
| 設定 | `--modules`・V8・`--selector form`・`--debug-raw-exception`。実行上限は既定のまま |

### 結果

| id | サイト | script | outcome | 最初の例外 | `form`（実行後） | 判定 |
| -- | ------ | ------ | ------- | ---------- | ---------------- | ---- |
| d2 | Saucedemo | index 1（module・同一オリジン `src`） | exception | `ReferenceError`・`not_defined`・`missing_api=history`（`module_evaluation`） | 0 件 | 未解消（`getItem` は越えた） |
| d3 | DemoQA | index 0（module・同一オリジン `src`） | succeeded | なし（`timer_errors` は `timeout:TypeError:other`） | 0 件 | 変化なし |

- d2 は `getItem` の TypeError を越え、次に `ReferenceError: history is not defined` で止まる。#1158 の節で静的に読んだ推測（Backtrace SDK の `history.pushState`）と一致する方向だが、読み取り元の特定まではしていない
- d3 は #1156 の節と同じ（`instanceof` の右辺が object ではない TypeError）
- **d2・d3 とも `form` は 0 件のため #919 にはコメントしていない**

### 次の対応

1. d2: `history`（`pushState` / `replaceState` 等）の不足。`location` と履歴の整合を含めて別 issue で追跡する（切り出し候補）。追跡: #1171（済み: #1171。下の節を参照）
2. d3: `instanceof` の右辺の特定（#1156 の次の対応 1 のまま。済み: #1168。下の節を参照）

### 制約

- 単一環境（Linux）・単一時点。URL のクエリ・ページ本文・script 本文は記録していない。生メッセージはエンジンの固定文だけを転記した
- 判定は最初の例外で止まる測定に基づく。後続の不足 API は未確認
- 未実装（`REPAIR-3`）: `storage` イベントと `StorageEvent`、プロパティ形式のアクセス（`localStorage.foo`）、永続化とオリジンごとの分離、opaque origin での `SecurityError`。書き込みを試せば上限が推測できる点は残る（緩和は上限を公開しないことまで）

### 再現手順

```bash
cargo build --release -p fandhe-browser-core --features js-v8 --example compat_page_script_probe
target/release/examples/compat_page_script_probe --target d2=https://www.saucedemo.com --modules --selector form --debug-raw-exception --out /abs/path/d2.jsonl
target/release/examples/compat_page_script_probe --target d3=https://demoqa.com/automation-practice-form --modules --selector form --debug-raw-exception --out /abs/path/d3.jsonl
jq -c 'select(.type=="script")' /abs/path/d2.jsonl /abs/path/d3.jsonl
jq -c 'select(.type=="run") | .bridge.timer_errors' /abs/path/d2.jsonl /abs/path/d3.jsonl
```

## instanceof 右辺の分類と不足コンストラクタの特定（#1168）

spec 対応: TASK-71・TASK-108 / MS-6 / `COMPAT-1`・`JS-5`・`JS-7`・`MEAS-4`・`REPAIR-3`・`REPAIR-4`・`SEC-2`。#1156・#1167 の節の「次の対応」（d3 の `instanceof` 右辺の特定）を扱い、d2・d3 を再測定した。出力はコミットしていない。

### 実装の要点

- 分類語彙に `instanceof_not_object`（右辺が object でない。V8 `Right-hand side of 'instanceof' is not an object`・boa `right-hand side of 'instanceof' should be an object, got ...`）と `instanceof_not_callable`（右辺が関数でない）を足した。boa は種類の接頭辞を付けないため `TypeError` と推定する。ハーネスの `classify` も同じ語彙にした
- 右辺の名前は文言に含まれないため、タイマー / microtask の callback の例外に限って `error.stack` の先頭 1024 UTF-16 単位を shim が渡し、core が `TimerError` に非公開で保持する。`PageRunOptions::with_retain_raw_exception_messages(true)`（opt-in）が無ければ page_runner が破棄する。`Debug` には長さだけを出す
- `page_script_probe` は `--debug-raw-exception` のときだけ、先頭フレーム（`url:line:col`）を stderr に出す（`<id> timer_error#<n> <source> frame`）。JSONL・`--out` には出さない。フレームの抽出は V8 の stack 形式が前提で、boa は形式を保証しない（`REPAIR-3`）
- stack の URL は自動では取得しない（`error.stack` はページが偽造できるため SSRF の入口になる）。該当列の前後を人間が `curl` で手動確認し、識別子名だけを転記した（script 本文・URL のクエリは記録していない）
- 特定した不足コンストラクタ: `HTMLIFrameElement`。react-dom の commit 段の `getActiveElementDeep` にある `element instanceof win.HTMLIFrameElement`（`document.activeElement` が無く `document.body` が渡る）で、右辺が `undefined` になっていた
- `dom.js` に `HTMLIFrameElement` を名前だけ足した（`Element.prototype` を継承し、`new` は `Illegal constructor`）。要素名を返す op が無いため、実際の iframe 要素でも `instanceof` は常に `false` になる簡略化（`REPAIR-3`）。標準のコンストラクタで、機能検出に新しい識別面は足さない（`SEC-2`）
- `HTMLElement` は足さなかった。`HTMLElement` も同時に定義すると、d3 の module 評価が `unsupported selector syntax (pseudo-class)` で失敗する（機能検出が通って別の経路に入ると推定。経路の特定は未実施）。擬似クラス selector の対応は別 issue の対象

### shim が提供する・しないコンストラクタ

| 状況 | コンストラクタ |
| ---- | -------------- |
| 提供 | `Node`・`Element`・`Text`・`Document`・`HTMLIFrameElement`（名前のみ）・`DOMException`・`AbortSignal`・`AbortController`・`MutationObserver`・`MutationRecord`・`URL`・`URLSearchParams`・`Storage` |
| 未定義 | `HTMLElement`・`Event`・`EventTarget`・`CustomEvent`・`DocumentFragment`・`Comment`・`Window` |

### 測定条件

| 項目 | 値 |
| ---- | -- |
| 実施日時（UTC） | 2026-10-19 |
| OS | Linux |
| ベース | main `0d9fd7cb` に、本変更を加えたもの |
| バイナリ | `cargo build --release -p fandhe-browser-core --features js-v8 --example compat_page_script_probe` |
| 設定 | `--modules`・V8・`--selector form`・`--debug-raw-exception`。実行上限は既定のまま |

### 結果

| id | サイト | 変更前の最初の例外 | 変更後 | `form`（実行後） | 判定 |
| -- | ------ | ------------------ | ------ | ---------------- | ---- |
| d2 | Saucedemo | `ReferenceError`・`not_defined`・`history`（module） | 同じ（`HTMLIFrameElement` は無関係） | 0 件 | 変化なし |
| d3 | DemoQA | `timeout:TypeError:instanceof_not_object`（先頭フレーム `assets/index-*.js:8:23571`） | `timer_errors` は 0 件。タイマーは 1 件発火し、module は成功 | 0 件 | タイマーの例外は解消、`form` は未解消 |

- d3 の実行後の HTML は 437 バイトのままで、React の描画は確認できていない。次の不足は未特定
- **d2・d3 とも `form` は 0 件のため #919 にはコメントしていない**

### 次の対応

1. d3: 描画されない原因の調査（`document.activeElement`、`HTMLElement` の定義で出る擬似クラス selector の対応を含む。別 issue で追跡する切り出し候補）
2. `listener_errors`・`net_callback_errors` を core の `ExceptionSummary` で分類する（#1156 の次の対応 2。本 issue では未対応。済み: #1172。下の節を参照）
3. d2: `history` の不足（#1167 の節の次の対応 1 のまま。済み: #1171。下の節を参照）

### 制約

- 単一環境（Linux）・単一時点。URL のクエリ・script 本文・stack は転記していない。生メッセージはエンジンの固定文だけを転記した
- stack の先頭フレームの抽出は実質 V8 だけが対象
- 判定は最初の例外で止まる測定に基づく。後続の不足 API は未確認

### 再現手順

```bash
cargo build --release -p fandhe-browser-core --features js-v8 --example compat_page_script_probe
target/release/examples/compat_page_script_probe --target d3=https://demoqa.com/automation-practice-form --modules --selector form --debug-raw-exception --out /abs/path/d3.jsonl
# stderr の "timer_error#N ... frame" 行の url:line:col を見て、人間が手動で該当列の前後を読む
jq -c 'select(.type=="run") | .bridge.timer_errors' /abs/path/d3.jsonl
```

## window.history の追加と d2・d3 の再測定（#1171）

spec 対応: TASK-71・TASK-108 / MS-6 / `COMPAT-1`・`JS-5`・`JS-6`・`MEAS-4`・`REPAIR-3`・`REPAIR-4`・`SEC-2`。
前節群（#1167・#1168）の「次の対応」（d2 の `history` の不足）を扱い、`window.history` の最小実装を足して、d2・d3 を `page_script_probe --modules --debug-raw-exception` で再測定した。出力はコミットしていない。

### 実装の要点

- `history.js`（新規）で `History` と `history` を足した。`length`（1 から。`pushState` で増え、上限 50）・`state`・`scrollRestoration`（`auto` / `manual` のみ受理）、`pushState`・`replaceState`・`back`・`forward`・`go` を持つ。メソッドは包み直せる（Backtrace SDK の想定パターン）
- URL の書き換えは新しい bridge op `historySetUrl`（Rust 側）で行う。現在の URL を基準に解決し、userinfo 付き・別オリジン（scheme・host・port）・`javascript:` / `file:` / `data:`・8192 バイト超・`about:blank` を拒否する。拒否は `SecurityError`（DOMException）で、状態は変えない。`__dom.op` を直接呼ばれても現在のオリジンの外へは書き換えられない。遷移も取得も行わない
- `state` は構造化複製の代わりに予算付きの再帰複製で複製し、累計 2 MiB（UTF-16）・10 万ノード・深さ 100 を確保前に検査して上限にする（`JS-6`。`undefined` は保持）。超過・循環参照・関数は `DataCloneError`。保持するのは現在の 1 件だけ
- **`back`・`forward`・`go` と `popstate` は未実装（`REPAIR-3`）を選んだ**。汎用の `EventTarget` / `dispatchEvent` が無く、URL だけを動かして `popstate` を配らないと、SPA のルーターの状態と URL が食い違うため。3 つは URL と state を変えず、`ignoreLocationChange` 診断（`HistoryBack`・`HistoryForward`・`HistoryGo`）に記録するだけ。`PopStateEvent`・`onpopstate` は定義しない
- 識別面（`SEC-2`）: `navigation`・`PopStateEvent`・`onpopstate`・上限や残量を示すプロパティは定義しない。診断に URL 本文は残さない
- 副作用: `pushState` 後は `page_net` の相対 URL の基準（`location`）が同一オリジン内の別パスに変わる（実ブラウザと同じ。オリジンは変わらず、クロスオリジン・SSRF 検査は従来どおり）
- 依存の追加・`unsafe` の追加は無い。V8・boa の共通ケース（`js_shim_cases.rs` の `history_cases`）で検証した

### 測定条件

| 項目 | 値 |
| ---- | -- |
| 実施日時（UTC） | 2026-10-19 |
| OS | Linux |
| ベース | main `fa8fc20a` に、本変更を加えたもの |
| バイナリ | `cargo build --release -p fandhe-browser-core --features js-v8 --example compat_page_script_probe` |
| 設定 | `--modules`・V8・`--selector form`・`--debug-raw-exception`。実行上限は既定のまま |

### 結果

| id | サイト | script | outcome | 最初の例外 | `form`（実行後） | 判定 |
| -- | ------ | ------ | ------- | ---------- | ---------------- | ---- |
| d2 | Saucedemo | index 1（module・同一オリジン `src`） | succeeded | なし | 0 件 | `history` は解消、`form` は未解消 |
| d3 | DemoQA | index 0（module・同一オリジン `src`） | succeeded | なし（`timer_errors` は 0 件） | 0 件 | 変化なし |

- d2 は module 評価が成功した（`ReferenceError: history is not defined` は解消）。実行は `abort=virtual_time` で終わり、タイマーが 1 件発火・1 件未発火（`pending_at_end`）、`console.error` が 1 件ある。実行後の HTML は 1344 バイトで `form` は 0 件
- d3 は #1168 の節と同じ（タイマー 1 件発火・module 成功・HTML 437 バイト）
- **d2・d3 とも `form` は 0 件のため #919 にはコメントしていない**

### 次の対応

1. d2: 描画されない原因の調査。`virtual_time` で打ち切られ未発火のタイマーが 1 件残る点と、`console.error` 1 件の内容（本測定では記録していない）が手掛かり。別 issue で追跡する（切り出し候補）
2. d3: 描画されない原因の調査（#1168 の次の対応 1 のまま。別 issue で追跡する切り出し候補）
3. `back`・`forward`・`go` による実際のページ内履歴の移動と `popstate` の配送（汎用の `EventTarget` が前提。別 issue の切り出し候補）

### 制約

- 単一環境（Linux）・単一時点。URL のクエリ・script 本文は記録していない
- 判定は最初の例外で止まる測定に基づく。d2 の `console.error` の内容と未発火タイマーの原因は未特定
- 未実装（`REPAIR-3`）: `back`・`forward`・`go` の移動、`popstate`・`hashchange` の配送、構造化複製の完全な実装（`Date`・`Map`・`Blob` は保持されない）、`about:blank` での URL 付き `pushState`、履歴の永続化

### 再現手順

```bash
cargo build --release -p fandhe-browser-core --features js-v8 --example compat_page_script_probe
target/release/examples/compat_page_script_probe --target d2=https://www.saucedemo.com --modules --selector form --debug-raw-exception --out /abs/path/d2.jsonl
target/release/examples/compat_page_script_probe --target d3=https://demoqa.com/automation-practice-form --modules --selector form --debug-raw-exception --out /abs/path/d3.jsonl
jq -c 'select(.type=="script")' /abs/path/d2.jsonl /abs/path/d3.jsonl
jq -c 'select(.type=="run")' /abs/path/d2.jsonl /abs/path/d3.jsonl
```

## listener_errors・net_callback_errors の分類と d2・d3 の再測定（#1172）

spec 対応: TASK-71・TASK-108 / MS-6 / `JS-4`・`JS-7`・`JS-8`・`MEAS-4`・`REPAIR-3`・`REPAIR-4`・`SEC-2`。
前節群（#1156・#1168）の「次の対応 2」（リスナーと fetch / XHR callback の例外の分類）を扱い、d2・d3 を `page_script_probe --modules --debug-raw-exception` で再測定した。出力はコミットしていない。

### 実装の要点

- `events.js`・`net.js` の報告関数が、`timers.js` と同じく `error.name`（文字列のときだけ・64 単位）・メッセージ・`error.stack`（文字列のときだけ・先頭 1024 単位）を渡す。bridge op `lifecycleListenerError`・`netCallbackError` は 4 引数（種別・name・message・stack）になり、旧形式の 2 引数は `ArityMismatch` で拒否する
- core の `ListenerError`・`NetCallbackError` に `summary`（`ExceptionSummary`）と非公開の `stack`（`raw_stack()` は `#[doc(hidden)]`）を足した。`error.name` は固定語彙に一致したときだけ kind に使い、保存しない。stack は `PageRunOptions::with_retain_raw_exception_messages(true)`（opt-in）が無ければ page_runner が破棄する（タイマーと同じ fail-closed）。`Debug` は長さと要約だけを出す
- probe は 3 種の JSONL ラベルを `<source>:<kind>:<class>[:<api>]` にそろえた（リスナーの source は `dom_content_loaded` / `load`、net は `fetch` / `xhr`）。`property`・`code` は要約が保持するがラベルには出さない。生メッセージと先頭フレームは `--debug-raw-exception` のときだけ stderr へ出す
- stack 中の URL は自動取得しない（ページが偽造でき SSRF の入口になるため）。依存の追加・`unsafe` の追加は無い。V8・boa の共通ケース（リスナーと XHR の `ReferenceError` の分類・既定で stack なし・opt-in で 1024 バイト以下）と単体テストで検証した

### 測定条件

| 項目 | 値 |
| ---- | -- |
| 実施日時（UTC） | 2026-10-20 |
| OS | Linux |
| ベース | main `29dae75f` に、本変更を加えたもの |
| バイナリ | `cargo build --release -p fandhe-browser-core --features js-v8 --example compat_page_script_probe` |
| 設定 | `--modules`・V8・`--selector form`・`--debug-raw-exception`。実行上限は既定のまま |

### 結果

| id | サイト | script | outcome | `listener_errors` | `net_callback_errors` | `timer_errors` | `form`（実行後） |
| -- | ------ | ------ | ------- | ----------------- | --------------------- | -------------- | ---------------- |
| d2 | Saucedemo | index 1（module・同一オリジン `src`） | succeeded | 0 件 | 0 件 | 0 件 | 0 件 |
| d3 | DemoQA | index 0（module・同一オリジン `src`） | succeeded | 0 件 | 0 件 | 0 件 | 0 件 |

- 2 サイトとも、リスナー・fetch / XHR callback の例外は 0 件だった。#1156・#1168 の節で「ここに隠れている可能性」とした仮説は、少なくとも本測定では否定された。d2・d3 の描画が進まない原因はこの 2 種の例外ではない
- d2 は `abort=virtual_time`（タイマー 1 件発火・1 件未発火）・`console.error` 1 件、d3 は abort なし（タイマー 1 件発火）・`console.error` 1 件で、#1171 の節から変化はない。ライフサイクルは両サイトとも `dom_content_loaded:dispatched`・`load:dispatched`、`net.started` は 0
- **d2・d3 とも `form` は 0 件のため #919 にはコメントしていない**

### 次の対応

1. d2・d3: `console.error` 1 件の内容の記録。現在の probe は件数しか出さず、描画が進まない手掛かりが残っている（別 issue の切り出し候補）（済み: #1173。下の節を参照）
2. d2: `virtual_time` で打ち切られる未発火タイマー 1 件の原因調査（#1171 の次の対応 1 のまま。別 issue の切り出し候補）
3. d3: 描画されない原因の調査（#1171 の次の対応 2 のまま。別 issue の切り出し候補）（一部済み: #1173。下の節を参照）

### 制約

- 単一環境（Linux）・単一時点。URL のクエリ・script 本文・stack 全文・生メッセージは転記していない
- fetch の `.then` 内の例外は Promise の reject になり `netCallbackError` に届かない（unhandled rejection の捕捉は未実装。`REPAIR-3`）。boa では fetch の続きが走らないため、両エンジン共通のテストは XHR で検証している
- `listener_errors` は `DOMContentLoaded` / `load` のリスナーだけが対象（`click` 等の汎用イベントは未実装）

### 再現手順

```bash
cargo build --release -p fandhe-browser-core --features js-v8 --example compat_page_script_probe
target/release/examples/compat_page_script_probe --target d2=https://www.saucedemo.com --modules --selector form --debug-raw-exception --out /abs/path/d2.jsonl
target/release/examples/compat_page_script_probe --target d3=https://demoqa.com/automation-practice-form --modules --selector form --debug-raw-exception --out /abs/path/d3.jsonl
jq -c 'select(.type=="run") | .bridge | {listener_errors, timer_errors, net_callback_errors}' /abs/path/d2.jsonl /abs/path/d3.jsonl
```

## document.activeElement・document.defaultView の追加と d3 の原因調査（#1173）

spec 対応: TASK-71・TASK-108 / MS-6 / `COMPAT-1`・`JS-5`・`MEAS-4`・`REPAIR-3`・`REPAIR-4`・`SEC-2`。
前節（#1172）の「次の対応 1・3」にあたる、`console.error` の本文の確認と d3 の描画されない原因の調査を行った。d2・d3 を `page_script_probe --modules --debug-raw-exception` で測定した。出力はコミットしていない。

### 実装の要点

- probe: `--debug-raw-exception` のときだけ、`console.error` / `console.warn` の本文を stderr に `[<id> console_error#<n>] <本文>` で出す。長さ上限・制御文字と bidi 制御の除去は既存の `debug_stderr_line` が行う。JSONL と `--out` には件数だけが残る
- 原因: d3 の `console.error` は `TypeError: Cannot read properties of undefined (reading 'history')` だった。ルーター系のコードが `document.defaultView.history` を読む形で、`document.defaultView` が未定義だったことによる（標準の既定値 `options.window = document.defaultView` の経路。推定であり、bundle 本体は確認していない）
- `document.defaultView`（`window.js`）: `window` と同一のオブジェクトを返す read-only の標準プロパティ。新しい識別面は足さない（`SEC-2`）
- `document.activeElement`（`dom.js`）: フォーカス管理が無いため常に `body`（無ければ `null`）を返す簡略版（`REPAIR-3`。実装済みを装わない）。`focus()` / `blur()` / `hasFocus()` は未定義のまま。react-dom の `getActiveElementDeep` が読むための先回りで、今回の測定で d3 が止まった原因そのものではない
- 新しい bridge op・依存・`unsafe` の追加は無い。V8・boa 共通ケースで検証した

### 測定条件

| 項目 | 値 |
| ---- | -- |
| 実施日時（UTC） | 2026-10-20 |
| OS | Linux |
| ベース | main `c9b29849` に、本変更を加えたもの |
| 設定 | `--modules`・V8・`--selector form`・`--debug-raw-exception`。実行上限は既定のまま |

### 結果

| id | 段階 | `console_error` の分類 | `timer_errors` | HTML バイト（実行後） | `form`（実行後） |
| -- | ---- | ---------------------- | -------------- | --------------------- | ---------------- |
| d3 | `defaultView` 追加前 | `TypeError`・undefined の `history` 読み取り | 0 件 | 437 | 0 件 |
| d3 | `defaultView` 追加後 | `TypeError`・null の `registered` 読み取り | 0 件 | 437 | 0 件 |
| d2 | `defaultView` 追加後 | 0 件 | 0 件 | 2618 | 1 件 |

- d3 は 1 段進んだが、次の `console.error`（null の `registered` 読み取り）で止まり、`form` は 0 件のまま。**d3 では #919 にコメントしていない**
- d2 は `abort=virtual_time`（タイマー 2 件発火・1 件未発火）のまま、`console.error` が 1 件から 0 件になり、`form` が 0 件から 1 件になった。**d2 は `form` が 1 件以上になったため #919 にコメントした**。h1 は 0 件
- `document.activeElement` 単体の効果は今回の測定では分離していない（`defaultView` と同時に入れた）

### 次の対応

1. d3: `console.error` の次の不足（null の `registered` 読み取り）の特定。読む対象の候補は service worker 登録など `navigator` 系だが未確認。`--debug-raw-exception` にスタックの先頭フレームがまだ出ないため、console 側にも先頭フレームを出す診断の拡張を先に検討する（別 issue の切り出し候補）
2. `HTMLElement` と擬似クラス selector: 今回は特定していない。core の selector は `:` を含む擬似クラスをすべて未対応として扱うため、`HTMLElement` は定義せず `typeof HTMLElement === "undefined"` を保った（`selector.rs` の拡張は別 issue の切り出し候補）
3. `focus()` / `blur()` と連動したフォーカス追跡（`activeElement` の簡略化の解消。`REPAIR-3`）

### 制約

- 単一環境（Linux）・単一時点。URL のクエリ・script 本文・stack 全文は転記していない。stack 中の URL は取得していない
- 原因の特定は `console.error` の固定文に基づく。bundle の該当コードは未確認

### 再現手順

```bash
cargo build --release -p fandhe-browser-core --features js-v8 --example compat_page_script_probe
target/release/examples/compat_page_script_probe --target d3=https://demoqa.com/automation-practice-form --modules --selector form --debug-raw-exception --out /abs/path/d3.jsonl 2>&1 >/dev/null | grep console_error
jq -c 'select(.type=="run") | {abort, html_bytes_after, selector_after, console_error: .bridge.console_error}' /abs/path/d3.jsonl
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
- 修正: `EvaluateOptions::with_discard_completion_value(true)` をエンジン抽象に追加し、ページ実行の評価で完了値を変換せず捨てるようにした（V8・boa・子プロセスのワイヤ〔protocol 8〕）。ソースは書き換えない。本当の例外（`throw`・構文エラー・参照エラー）は従来どおり `Exception`
- 末尾が `if (true)` などで終わる壊れた script は従来どおり構文エラーのまま（ソース不変のため。回帰テストあり）
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
