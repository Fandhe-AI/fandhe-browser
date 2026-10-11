# エージェント評価ハーネス: 代表タスク 25 件と golden answer

TASK-21.1（MS-2・`AISNAP-8`・Issue #119）。簡約表現と生 HTML をエージェントに渡したときの成功率を比較するための、代表タスクと正解の定義。
親は #118。TASK-21.2（#120）が簡約表現の生成ハーネス、TASK-21.3（#121）が採点・成功率算出。

## ファイル

| ファイル | 役割 |
| -------- | ---- |
| `tasks.rs` | 正本。タスク 25 件・golden の定数と JSON 直列化（依存なし）。#120・#121 は `#[path]` で取り込む |
| `tasks_tests.rs` | `[[test]] agent_eval_tasks`。件数・配分・ロケータ解決・JSON 一致を固定する |
| `tasks.json` | エージェントへ渡す側（id・category・page・prompt）。`tasks.rs` から生成 |
| `golden-answers.json` | 採点側。**エージェントへ渡さない**。`tasks.rs` から生成 |
| `generate_reduced.rs` | 簡約表現の生成とロケータの ref 解決（TASK-21.2・#120）。#121 は `#[path]` で取り込む |
| `generate_reduced_tests.rs` | `[[test]] agent_eval_generate_reduced`。25 タスク全件の生成・生成物一致を固定する |
| `score.rs` | 回答の採点・種別別集計・70% 判定（TASK-21.3・#121。純ロジック） |
| `score_tests.rs` | `[[test]] agent_eval_score`。合成回答で採点器を検証する（実回答は含まない） |
| `compare.rs` | 比較実行の支援（TASK-22.2・#125）。割付表・指示テンプレート・実行パケット生成・回答の回収と完全性検査。LLM は呼ばない |
| `compare_tests.rs` | `[[test]] agent_eval_compare`。合成回答だけで割付・テンプレート・回収を検証する |
| `reduced/<page>.txt` | エージェントへ渡す簡約表現（`tasks.rs` が指す 13 ページ分）。生成物。データ葉（価格・引用・地の文クラス）の本文と `option` のラベル・value は snapshot が持たないため、生成器が DOM から補う（回答可能性の確保） |
| `golden-refs.json` | 採点側。**エージェントへ渡さない**。golden の各ロケータを解決した ref（解決不能は空配列）。生成物 |

## 種別と件数

| category | 内容 | 件数 | golden の形 |
| -------- | ---- | ---- | ----------- |
| `click` | クリック対象特定 | 7 | `ref`（正解要素の候補） |
| `extract` | 値抽出 | 6 | `value`（期待値と値を持つ要素） |
| `form` | フォーム入力手順 | 6 | `steps`（fill / select / click の操作列） |
| `nav` | ナビゲーション判断 | 6 | `ref` |

## golden のスキーマ

- 要素の指定はロケータ `{selector, index}`。`fandhe_browser_core::query::query_selector_all_str` の結果の文書順 `index` 番目。
- `ref`: `any_of` のいずれかに解決された要素を指せば正解（同じ遷移先の要素が複数ある場合に備える）。
- `value`: `value` が期待値。`source` の要素の text（空白を正規化）、`attr` があればその属性値と一致する。
- `steps`: 操作列。`fill`・`select` は `value` を持ち、`click` は持たない。

## golden が ref ではなくロケータである理由

PoC の golden は連番 ref（`e1`〜）だが、本リポの ref は `AISNAP-10` のダイジェスト形式で移植できない。
このためロケータで正解を表し、ref への解決は #120（`golden-refs.json`）、照合は #121 が担う。

## 簡約表現の形式と ref 解決（TASK-21.2）

- 形式は `benches/token_reduction/snapshot_text.rs` の暫定行形式。`AISNAP-8` が前提とする `GET /ai/snapshot` の確定応答ではない（`REPAIR-3`）。TASK-19・`AISNAP-6` の確定後に `snapshot_text` 側を差し替える。
- 生成物は行末の空白だけを落としている（`header:` の後ろの空白等。`.editorconfig` の `trim_trailing_whitespace` 検査に通すため。内容は変えない）。
- ref 解決は DOM からの再計算（`retention_check::target_refs`）。`build_snapshot` が省略する要素（hidden・深さ超過）が先行すると不一致側へ倒れ、`refs` は空配列になる。圧縮行の操作要素は snapshot 上の同一ダイジェスト件数と DOM 上の件数が食い違う（行・表の上限で省略された）場合に候補を出さない（fail-closed）。解決不能はテストの失敗にせず事実として記録する。
- 現時点で `refs` が空のロケータ: extract-01（`table#table1 tbody td` index 14）のみ。簡約表現上で ref を持たない正解要素であり、読み取れるかどうかは #121 の測定結果で判定する。
- 上限で打ち切られる（`… truncated`）ページは `hn-list`・`large-table`。

## PoC との差分

- fixture は `benches/fixtures/` の自作合成ページで、PoC の実サイトと内容が異なる。タスク文と正解値は fixture の実内容に合わせて定義し直した。種別 4 つと 7/6/6/6 の配分は PoC に合わせている。
- 資格情報は fixture が案内するダミー値（`dummy-user` / `dummy-pass`）を使う。
- form-04 は「2 つとも checked の状態にする」という終状態で定義した。2 つ目は初期状態で checked のため、両方をクリックすると誤答になる。
- nav-01・nav-02・nav-04・nav-06 は fixture に PoC と同じ対象がないため、同じ性質の別対象（言語リンク・サイドバーのリファレンス・レイアウト table 内の副次リンク・フッターリンク）へ差し替えた。
- 簡約表現から読み取れるかどうかは golden に持ち込まない。それは #120・#121 の測定結果で判定する（`REPAIR-3`）。このため PoC の実測値（22/25）との比較は参考扱い。

## 再生成と検証

```bash
# tasks.rs を変更したら生成物を書き直す
AGENT_EVAL_WRITE=1 cargo test -p fandhe-browser-ai --test agent_eval_tasks

# 通常実行（生成物が正本と一致することを検証する）
cargo test -p fandhe-browser-ai --test agent_eval_tasks

# 簡約表現（reduced/*.txt）と golden-refs.json を書き直す・検証する
AGENT_EVAL_WRITE=1 cargo test -p fandhe-browser-ai --test agent_eval_generate_reduced
cargo test -p fandhe-browser-ai --test agent_eval_generate_reduced
```

## 採点（TASK-21.3）

`score.rs` は別途取得したエージェント回答を golden と照合する。実エージェントの回答と測定結果（`results.json`）は本リポジトリに含めない。「全体 70% 以上」の実測は親 #118 の工程で、採点器のテストは合成回答で検算するだけである（`REPAIR-3`）。

回答ファイルはタスク id をキーにした配列。回答不能はエントリ欠落または該当フィールドが `null`。未知フィールドは無視する。

```json
[
  {"id": "click-01", "ref": "<ref>"},
  {"id": "extract-02", "value": "<value>"},
  {"id": "form-01", "steps": [
    {"action": "fill", "ref": "<ref>", "value": "<value>"},
    {"action": "click", "ref": "<ref>"}
  ]},
  {"id": "nav-03", "ref": null}
]
```

| golden | 合格条件 |
| ------ | -------- |
| `ref`（click・nav） | 回答 ref が、全ロケータの解決済み ref のいずれかに一致（`golden-refs.json`） |
| `value`（extract） | 空白を正規化した完全一致（大文字小文字は区別）。ref では比較しないため extract-01 も値が合えば合格 |
| `steps`（form） | 手順数・各手順の action・ref・value が順序どおりに一致 |

- 不合格の分類: `unanswered` / `invalid_shape` / `mismatch` / `golden_unresolved`（golden の ref が未解決。fail-closed）。
- 成功率は 4 種別（click・extract・form・nav）と全体。判定は `pass * 100 >= 70 * total`（丸め前の整数比較）。
- 回答 JSON は外部入力として上限検証する（1 MiB・64 件・32 手順・文字列 4096 バイト）。重複 id・未知 id はエラー。

```bash
# 採点ロジックの検証（合成回答のみ）
cargo test -p fandhe-browser-ai --test agent_eval_score

# 実回答を採点して結果を stdout へ出す（results.json は AGENT_EVAL_WRITE=1 併用時のみ書く）
AGENT_EVAL_ANSWERS=/abs/path/answers.json cargo test -p fandhe-browser-ai --test agent_eval_score -- --nocapture aisnap8_score_real_answers_if_requested
```

## 生 DOM 入力（TASK-22.1）

簡約方式との比較相手（`AISNAP-9`・Issue #124・`MS-2`）として、Snapshot を通さない「生 DOM 直渡し」の入力を `generate_reduced.rs` で生成する。

| API | 役割 |
| --- | ---- |
| `generate_raw_dom_page` / `generate_raw_dom_all` | 1 ページ / 全 13 ページ（`task_pages()` の順）の生 DOM を返す |
| `task_inputs(fixtures_dir, InputMode)` | 25 タスクそれぞれへ `Reduced` または `RawDom` の入力を組み立てる（`TASKS` と同順） |
| `write_raw_dom_inputs(fixtures_dir, out_dir)` | 呼び出し側が指定したディレクトリ直下へ `<page>.html` を書き出す |

- 生 DOM は `AISNAP-15` の分母 `serialize_raw_dom`（`benches/token_reduction/raw_dom.rs`）と同一定義。`script`・`style`・`noscript`・`svg`・`link`・`meta` を除去した `body` の outerHTML 相当で、成形・要約はしない。
- 暫定（REPAIR-3）: jsdom の outerHTML とは完全一致せず、`<template>` の中身は含まない。
- 回答形式の提示文は `task_inputs` 自体は持たない。`compare.rs`（#125・TASK-22.2）の指示テンプレートが付ける。golden 情報は入力に含めない。
- 検証 `aisnap9_golden_locators_survive_raw_dom_roundtrip`: 生 DOM を再パースしても、全 golden ロケータ `{selector, index}` が原本と同じ要素（local name・属性・正規化テキスト）を指す。生 DOM 方式の回答を golden で採点できる前提の確認。
- 生成物はコミットしない。置き場所と `{selector, index}` 回答の採点は次節（#805・TASK-22.1b）で確定した。

## 生 DOM 方式の回答と採点（TASK-22.1b）

`AISNAP-9`・Issue #805・`MS-2`。生 DOM 方式の回答は `{selector, index}` ロケータで返させ、部分点なしの 0/1 で採点する（#123 のオーナー判断）。簡約方式の採点経路（`judge`・`score`）は変えない。

```json
[
  {"id": "click-01", "selector": "form#login button[type=submit]", "index": 0},
  {"id": "extract-02", "value": "<value>"},
  {"id": "form-01", "steps": [
    {"action": "fill", "selector": "input#username", "index": 0, "value": "<value>"},
    {"action": "click", "selector": "form#login button[type=submit]", "index": 0}
  ]}
]
```

- 照合は「生 DOM（`serialize_raw_dom` の出力）を再パースした Document」上で、回答と golden の両ロケータを解決し、**要素の同一性**で比べる（セレクタ文字列の一致ではない。`#login` と `button[type=submit]` が同じ要素なら合格）。`raw_dom_documents` が Document を返し、`score::resolve_raw_dom` が要素キーへ解決して既存の `score` へ渡す。
- extract の value と form の action・value・順序・手順数は簡約方式と同じ基準。
- 分類: `selector` / `index` / `value` / `steps` がすべて欠落または null は `unanswered`。片方だけ・負数・小数・文字列の index・`ref` の混入・排他フィールドの同居は `invalid_shape`。セレクタの構文エラー・未対応構文も `invalid_shape`。index が一致件数以上は `mismatch`。
- 暫定（REPAIR-3）: core のセレクタは一部の構文にしか対応しない（`*`・疑似クラス・`+` / `~`・`$=` `*=` `~=` `|=`・エスケープは未対応）。生 DOM 方式に不利に働きうるため、指示テンプレート（#125）で使用可能な構文を明示する。差の計算と −5pt 判定は `compare.rs`（#126・TASK-22.3。「比較レポート」節）が担う。
- 集計は 2 種類を出す。`ScoreReport` の `overall` / `by_category` は `golden_unresolved` を含み、`excluding_golden_unresolved`（`results.json` の同名キー）はそれを分母から除く。`excluded` に除外 id を列挙し、`summarize` へ任意の除外集合を渡せる（#126 が 2 方式の和集合で分母を揃える入口）。

| 成果物 | 置き場所 | コミット |
| ------ | -------- | -------- |
| 生 DOM 入力（13 ページ） | 実験時に `write_raw_dom_inputs` でリポ外の一時ディレクトリへ生成 | しない（fixture と `serialize_raw_dom` から決まる生成物） |
| 回答 JSON（方式別） | 実施者のローカル（リポ外）。必要なら #122 または結果レポートの PR に添付・リンク | しない（実回答はリポジトリに含めない） |
| 採点結果 | stdout。必要ならリポ外へ保存し、集計値を結果レポートへ転記 | しない |
| 割付表 | 実験中はリポ外で回答者に渡さない。実施後に結果レポートの付録へ転記 | レポートの一部として載せる（生成コードは #125） |
| 結果レポート | `docs/design/aisnap9-comparison-report.md`（生成器は #126・TASK-22.3。実測後に転記して作成） | する |

```bash
# 実回答（生 DOM 方式）を採点して結果を stdout へ出す（リポ内へは書かない）
AGENT_EVAL_RAW_ANSWERS=/abs/path/raw-answers.json cargo test -p fandhe-browser-ai --test agent_eval_score -- --nocapture aisnap9_score_raw_answers_if_requested
```

## 比較実行（TASK-22.2）

`AISNAP-9`・Issue #125・`MS-2`。簡約方式と生 DOM 方式を同じ 25 タスクで解かせるための**実行支援と回収検査**で、LLM は呼ばない（API 経由の自動実行は採らない。#123 のオーナー判断）。回答者は会話を共有しない独立したサブエージェントによる手動実行（1 実行 = 1 タスク × 1 方式、計 50 実行）。2 方式の差・−5pt 判定は「比較レポート（TASK-22.3）」節を参照。

- 割付表: `allocation`。id 昇順の通し位置が偶数なら簡約先・奇数なら生 DOM 先（種別ごとの交互割当と同値。全体 13 対 12・種別内の偏り最大 1）。実行番号は 1〜50。
- 指示テンプレート: `instruction`。生 DOM 方式では使用可能な CSS セレクタ構文を明示する（core のサブセット。テンプレートの例は `aisnap9_raw_template_selectors_match_core` が core の実装と突き合わせる）。`select` の value は option の value 属性値と両方式で同一文言にする。golden・比較の意図は含めない。
- 暫定（REPAIR-3）: 指示は日本語（入力データ）。実回答の収集は手動で、完全性検査が通ることは「50 件が揃った」ことしか意味しない。

```bash
# 1. パケット 50 件 + 割付表を生成（出力先はリポジトリ外の絶対パス。リポ内は拒否される）
AGENT_EVAL_COMPARE_OUT=/abs/packets cargo test -p fandhe-browser-ai --test agent_eval_compare -- --nocapture aisnap9_write_packets_if_requested

# 2. 各パケット本文だけを独立したサブエージェントへインラインで渡す（ファイル・リポジトリは参照させない）。
#    出力 JSON を /abs/answers/<id>-reduced.json または <id>-raw.json に保存する。allocation.* は回答者へ渡さない。

# 3. 回収検査（欠落・不正があれば失敗）。結合 JSON は既存の採点入口へそのまま渡せる
AGENT_EVAL_COMPARE_ANSWERS=/abs/answers AGENT_EVAL_COMPARE_MERGED_OUT=/abs/merged cargo test -p fandhe-browser-ai --test agent_eval_compare -- --nocapture aisnap9_collect_answers_if_requested
```

- 回答ファイルは 1 件 32 KiB まで。`id` がファイル名と一致し、`score.rs` の既存検証（件数・文字列長・形式）を通ったものだけを受理する。`unanswered`（null）は正当な回答として数え、形式不備（`invalid`）は採点で `invalid_shape` になるため回収上は受理する。
- 実回答・パケット・結合 JSON はコミットしない。割付表は実施後に結果レポート付録へ転記する。

## 比較レポート（TASK-22.3）

`AISNAP-9`・Issue #126・`MS-2`。回収した 2 方式の回答を採点し、成功率・差（pt）・−5pt 判定・種別別とページ別の内訳・和集合除外後の集計を Markdown と JSON で出す（`compare.rs` の `build_report` / `render_report_markdown` / `render_report_json`）。LLM は呼ばない。

- 差は「簡約 − 生 DOM」。判定式は差 ≥ −5pt で、丸め前の整数比較（`(簡約合格 − 生 DOM 合格) × 100 ≥ −5 × 件数`）。25 件なら 1 件差（−4.0pt）は達成、2 件差（−8.0pt）は未達。表示の差は千分率でゼロから遠い側へ丸める。
- 主判定は `golden_unresolved` を含む集計。2 方式の `golden_unresolved` の和集合を両方式から除いた集計を常に併記する（#123 の決定）。分母が 0 または 2 方式で不一致の行は `n/a`・未達。
- 回収が不完全（欠落・不受理）ならレポートを作らず失敗する（fail-closed）。
- 暫定（REPAIR-3）: 生成器は合成回答で検算済みで、実測値を持たない。50 実行は親 #122、結果判定は人間レビュー（#127）。実測後に出力を `docs/design/aisnap9-comparison-report.md` へ転記する（実測前には作成しない）。

```bash
# 回収済みの回答からレポートを生成して stdout へ出す。REPORT_OUT があればリポ外の絶対パスへ 2 ファイルを書く
AGENT_EVAL_COMPARE_ANSWERS=/abs/answers AGENT_EVAL_COMPARE_REPORT_OUT=/abs/report \
  AGENT_EVAL_REPORT_COMMIT=<sha> AGENT_EVAL_REPORT_DATE=<yyyy-mm-dd> AGENT_EVAL_REPORT_MODEL=<model> AGENT_EVAL_REPORT_ISSUES='#122 #126' \
  cargo test -p fandhe-browser-ai --test agent_eval_compare -- --nocapture aisnap9_report_if_requested
```

- メタ情報の環境変数（`AGENT_EVAL_REPORT_COMMIT` / `_DATE` / `_MODEL` / `_ISSUES`）は各 128 バイト以下で、`|`・`<`・`>`・制御文字を含む値は拒否する。未指定は「未記入」。
- 出力（`comparison-report.md` / `.json`）はコミットしない。
