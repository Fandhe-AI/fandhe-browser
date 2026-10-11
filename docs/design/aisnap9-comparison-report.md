# AISNAP-9 比較測定レポート（簡約方式 vs 生 DOM 直渡し方式）

- **対応ビヘイビア**: `AISNAP-9`（関連: `AISNAP-8`・`AISNAP-15`）
- **対応タスク**: TASK-22（TASK-22.4・#860。生成器は TASK-22.3・#126、実行支援は TASK-22.2・#125）
- **マイルストーン**: MS-2
- **基準コミット**: 0c63820
- **実施日**: 2026-10-11
- **使用モデル（回答者）**: claude-sonnet-5-5
- **関連 Issue**: #122（親）・#860（本実測）・#127（結果判定。owner:human）・#123（実験設計のオーナー判断）
- **状態**: 実測済み。判定（Conditional Go 条件 2(c) の充足可否）は #127 の人間レビューで確定する

## 目的・前提

同一の 25 タスク（TASK-21 の代表タスクセット。click 7・extract 6・form 6・nav 6）を、簡約方式（アクセシビリティツリー形式の簡約表現と ref で回答）と生 DOM 直渡し方式（`serialize_raw_dom` の HTML と `{selector, index}` で回答）で解かせ、成功率の差「簡約 − 生 DOM」が −5pt 以上かを確かめる（`AISNAP-9`）。手順と判定基準は `docs/design/aisnap-comparison-experiment.md` に従う。

## 測定方法（設計書との差分）

設計書・`crates/fandhe-browser-ai/harness/agent_eval/README.md` の「比較実行」「比較レポート」節の手順どおりに実施した。設計書で決めていなかった実施上の詳細と、手順から外れた処理を次に記す。

- **回答者の実行形態**: 1 実行ごとに独立した `claude -p` セッションを起動し、パケット本文だけを標準入力で渡した。ツールは無効（`--tools ""`）、設定ソース・MCP・スキルは読み込まず（`--setting-sources ""`・`--strict-mcp-config`・`--disable-slash-commands`）、空の作業ディレクトリで実行したため、回答者はリポジトリ・golden・割付表・他の実行の会話を参照できない。設計書の「サブエージェントによるブラインドの手動実行。1 タスクにつき方式ごとに独立」（#123）に対応する。各セッションの既定のシステムプロンプト（Claude Code のもの）は残る
- **実行順**: 割付表の実行番号順に 50 実行を逐次実施した（付録の割付表）。応答の再試行は 0 件
- **回答の正規化（1 件）**: 生 DOM 方式の `extract-04` の回答だけが、指示に反してコードフェンス（3 連バッククォートの json ブロック）で囲まれており、回収検査で `invalid JSON` として拒否された。回答の中身は書き換えず、前後のフェンス行だけを除いて回収した（正規化後: `{"id": "extract-04", "value": "r0c14-14"}`）。この扱いは設計書に定めがないため #127 で確認を求める。フェンス付きを形式違反として不合格に数えた場合でも、生 DOM 方式は 23/25（92.0%）、差は 0.0pt となり判定（達成）は変わらない
- **実回答・パケット・結合 JSON**: 実施者のローカル（リポジトリ外）に保管し、コミットしない（README の方針）。割付表は付録へ転記した

## 結果

### 全体・種別別・ページ別（golden_unresolved を含む。主判定）

| 区分 | 件数 | 簡約 合格 | 簡約 率 | 生 DOM 合格 | 生 DOM 率 | 差 (pt) |
| ---- | ---- | --------- | ------- | ----------- | --------- | ------- |
| 全体 | 25 | 23 | 92.0% | 24 | 96.0% | -4.0 |
| click | 7 | 6 | 85.7% | 6 | 85.7% | +0.0 |
| extract | 6 | 5 | 83.3% | 6 | 100.0% | -16.7 |
| form | 6 | 6 | 100.0% | 6 | 100.0% | +0.0 |
| nav | 6 | 6 | 100.0% | 6 | 100.0% | +0.0 |

| ページ | 件数 | 簡約 合格 | 生 DOM 合格 | 差 (pt) |
| ------ | ---- | --------- | ----------- | ------- |
| checkboxes-form | 2 | 2 | 2 | +0.0 |
| dashboard-table | 2 | 2 | 2 | +0.0 |
| dropdown-form | 2 | 1 | 1 | +0.0 |
| ec-product-list | 2 | 2 | 2 | +0.0 |
| hn-list | 3 | 3 | 3 | +0.0 |
| inputs-form | 1 | 1 | 1 | +0.0 |
| large-table | 1 | 1 | 1 | +0.0 |
| login-form | 3 | 3 | 3 | +0.0 |
| mdn-docs | 1 | 1 | 1 | +0.0 |
| python-portal | 3 | 2 | 3 | -33.3 |
| quotes-list | 3 | 3 | 3 | +0.0 |
| reddit-list | 1 | 1 | 1 | +0.0 |
| wiki-portal-nav | 1 | 1 | 1 | +0.0 |

### 2 方式の golden_unresolved の和集合を除いた集計（併記）

除外タスク: なし

| 区分 | 件数 | 簡約 合格 | 簡約 率 | 生 DOM 合格 | 生 DOM 率 | 差 (pt) |
| ---- | ---- | --------- | ------- | ----------- | --------- | ------- |
| 全体 | 25 | 23 | 92.0% | 24 | 96.0% | -4.0 |
| click | 7 | 6 | 85.7% | 6 | 85.7% | +0.0 |
| extract | 6 | 5 | 83.3% | 6 | 100.0% | -16.7 |
| form | 6 | 6 | 100.0% | 6 | 100.0% | +0.0 |
| nav | 6 | 6 | 100.0% | 6 | 100.0% | +0.0 |

| ページ | 件数 | 簡約 合格 | 生 DOM 合格 | 差 (pt) |
| ------ | ---- | --------- | ----------- | ------- |
| checkboxes-form | 2 | 2 | 2 | +0.0 |
| dashboard-table | 2 | 2 | 2 | +0.0 |
| dropdown-form | 2 | 1 | 1 | +0.0 |
| ec-product-list | 2 | 2 | 2 | +0.0 |
| hn-list | 3 | 3 | 3 | +0.0 |
| inputs-form | 1 | 1 | 1 | +0.0 |
| large-table | 1 | 1 | 1 | +0.0 |
| login-form | 3 | 3 | 3 | +0.0 |
| mdn-docs | 1 | 1 | 1 | +0.0 |
| python-portal | 3 | 2 | 3 | -33.3 |
| quotes-list | 3 | 3 | 3 | +0.0 |
| reddit-list | 1 | 1 | 1 | +0.0 |
| wiki-portal-nav | 1 | 1 | 1 | +0.0 |

## 判定

- 主判定（除外なし）: 差 -4.0 pt → 達成（しきい値 -5pt）
- 除外後: 差 -4.0 pt → 達成

### 感度（回答の正規化の影響）

| 扱い | 簡約 | 生 DOM | 差 (pt) | 判定 |
| ---- | ---- | ------ | ------- | ---- |
| フェンスを除いて回収（主判定） | 23/25（92.0%） | 24/25（96.0%） | −4.0 | 達成 |
| フェンス付きを不合格に数える | 23/25（92.0%） | 23/25（92.0%） | 0.0 | 達成 |

## 限界

- 粒度が粗い: 25 件のため 1 件 = 4pt で、しきい値 −5pt は 1 件差までを許容し、2 件差（−8pt）で未達になる。
- 簡約方式に甘い: 簡約方式の ref は生成側が付与したもので、回答者に有利な形で提示される。生 DOM 方式はセレクタ構文の制約で不利になりうる。結果は人間レビュー（#127）で解釈する。
- 回答者は独立したサブエージェントによる手動実行で、試行のばらつきは考慮していない。
- 回答の正規化（コードフェンスの除去）を 1 件行った。上記「感度」のとおり判定は変わらない。
- 回答者は単一モデル（claude-sonnet-5-5）・各タスク各方式 1 試行で、モデル・試行間のばらつきは測っていない。

## 考察（不合格タスクの内訳）

- **click-02（両方式とも不合格・mismatch）**: タスク文は「ドロップダウンから Option 2 を選ぶ対象の要素を特定」で、両方式の回答者とも `select` 要素そのもの（生 DOM 方式は `#dropdown`）を答えた。方式による差ではなく、タスク文と golden の対応（選択肢の要素を求めるか、操作対象の `select` を求めるか）の解釈の問題であり、差の計算には影響しない
- **extract-06（簡約方式のみ不合格・unanswered）**: タスクは「サイト内検索ボックスの placeholder 文字列」。簡約表現には `searchbox "Search"` と accessible name が出ているが、その name が placeholder 由来か（簡約表現の name 計算は label → `title` → `placeholder` の順。`crates/fandhe-browser-ai/src/snapshot/name.rs`）を表現から判別できないため、回答者は推測を避けて null を返した。生 DOM 方式は `placeholder="Search"` 属性を直接読めた。全体の差 −4.0pt はこの 1 件による。簡約表現に name の出所（placeholder 等）を示すかどうかは、改善の要否として #127 で判断する
- **入力トークン（参考）**: 1 実行あたりの入力トークン（キャッシュ読取・作成を含む。各セッション共通の既定システムプロンプト分を含む）は、簡約方式が平均 8,709（最大 25,923）、生 DOM 方式が平均 12,727（最大 50,091）。合計は簡約 217,730、生 DOM 318,191 で、簡約方式は約 32% 少ない。トークン削減率の正式な指標は `AISNAP-15`（別レポート）であり、本値は参考に留める

## 付録

### タスク別結果

| タスク id | 種別 | ページ | 先に提示した方式 | 簡約 | 生 DOM |
| --------- | ---- | ------ | ---------------- | ---- | ------ |
| click-01 | click | login-form | 簡約 | 合格 | 合格 |
| click-02 | click | dropdown-form | 生 DOM | 不合格 (mismatch) | 不合格 (mismatch) |
| click-03 | click | checkboxes-form | 簡約 | 合格 | 合格 |
| click-04 | click | dashboard-table | 生 DOM | 合格 | 合格 |
| click-05 | click | hn-list | 簡約 | 合格 | 合格 |
| click-06 | click | ec-product-list | 生 DOM | 合格 | 合格 |
| click-07 | click | quotes-list | 簡約 | 合格 | 合格 |
| extract-01 | extract | dashboard-table | 生 DOM | 合格 | 合格 |
| extract-02 | extract | ec-product-list | 簡約 | 合格 | 合格 |
| extract-03 | extract | hn-list | 生 DOM | 合格 | 合格 |
| extract-04 | extract | large-table | 簡約 | 合格 | 合格 |
| extract-05 | extract | quotes-list | 生 DOM | 合格 | 合格 |
| extract-06 | extract | python-portal | 簡約 | 不合格 (unanswered) | 合格 |
| form-01 | form | login-form | 生 DOM | 合格 | 合格 |
| form-02 | form | dropdown-form | 簡約 | 合格 | 合格 |
| form-03 | form | inputs-form | 生 DOM | 合格 | 合格 |
| form-04 | form | checkboxes-form | 簡約 | 合格 | 合格 |
| form-05 | form | python-portal | 生 DOM | 合格 | 合格 |
| form-06 | form | reddit-list | 簡約 | 合格 | 合格 |
| nav-01 | nav | wiki-portal-nav | 生 DOM | 合格 | 合格 |
| nav-02 | nav | mdn-docs | 簡約 | 合格 | 合格 |
| nav-03 | nav | python-portal | 生 DOM | 合格 | 合格 |
| nav-04 | nav | hn-list | 簡約 | 合格 | 合格 |
| nav-05 | nav | quotes-list | 生 DOM | 合格 | 合格 |
| nav-06 | nav | login-form | 簡約 | 合格 | 合格 |

### 割付表

| タスク id | 種別 | ページ | 先に提示する方式 | 実行番号（先, 後） |
| --------- | ---- | ------ | ---------------- | ------------------ |
| click-01 | click | login-form | 簡約 | 1, 2 |
| click-02 | click | dropdown-form | 生 DOM | 3, 4 |
| click-03 | click | checkboxes-form | 簡約 | 5, 6 |
| click-04 | click | dashboard-table | 生 DOM | 7, 8 |
| click-05 | click | hn-list | 簡約 | 9, 10 |
| click-06 | click | ec-product-list | 生 DOM | 11, 12 |
| click-07 | click | quotes-list | 簡約 | 13, 14 |
| extract-01 | extract | dashboard-table | 生 DOM | 15, 16 |
| extract-02 | extract | ec-product-list | 簡約 | 17, 18 |
| extract-03 | extract | hn-list | 生 DOM | 19, 20 |
| extract-04 | extract | large-table | 簡約 | 21, 22 |
| extract-05 | extract | quotes-list | 生 DOM | 23, 24 |
| extract-06 | extract | python-portal | 簡約 | 25, 26 |
| form-01 | form | login-form | 生 DOM | 27, 28 |
| form-02 | form | dropdown-form | 簡約 | 29, 30 |
| form-03 | form | inputs-form | 生 DOM | 31, 32 |
| form-04 | form | checkboxes-form | 簡約 | 33, 34 |
| form-05 | form | python-portal | 生 DOM | 35, 36 |
| form-06 | form | reddit-list | 簡約 | 37, 38 |
| nav-01 | nav | wiki-portal-nav | 生 DOM | 39, 40 |
| nav-02 | nav | mdn-docs | 簡約 | 41, 42 |
| nav-03 | nav | python-portal | 生 DOM | 43, 44 |
| nav-04 | nav | hn-list | 簡約 | 45, 46 |
| nav-05 | nav | quotes-list | 生 DOM | 47, 48 |
| nav-06 | nav | login-form | 簡約 | 49, 50 |
