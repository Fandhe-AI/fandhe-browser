# エージェント評価ハーネス: 代表タスク 25 件と golden answer

TASK-21.1（MS-2・`AISNAP-8`・Issue #119）。簡約表現と生 HTML をエージェントに渡したときの成功率を比較するための、代表タスクと正解の定義。
親は #118。TASK-21.2（#120）が簡約表現の生成ハーネス、後続の #121 が採点・成功率算出。

## ファイル

| ファイル | 役割 |
| -------- | ---- |
| `tasks.rs` | 正本。タスク 25 件・golden の定数と JSON 直列化（依存なし）。#120・#121 は `#[path]` で取り込む |
| `tasks_tests.rs` | `[[test]] agent_eval_tasks`。件数・配分・ロケータ解決・JSON 一致を固定する |
| `tasks.json` | エージェントへ渡す側（id・category・page・prompt）。`tasks.rs` から生成 |
| `golden-answers.json` | 採点側。**エージェントへ渡さない**。`tasks.rs` から生成 |
| `generate_reduced.rs` | 簡約表現の生成とロケータの ref 解決（TASK-21.2・#120）。#121 は `#[path]` で取り込む |
| `generate_reduced_tests.rs` | `[[test]] agent_eval_generate_reduced`。25 タスク全件の生成・生成物一致を固定する |
| `reduced/<page>.txt` | エージェントへ渡す簡約表現（`tasks.rs` が指す 13 ページ分）。生成物 |
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
- 生成物は行末の空白だけを落としている（`header: ` 等。`.editorconfig` の `trim_trailing_whitespace` 検査に通すため。内容は変えない）。
- ref 解決は DOM からの再計算（`retention_check::target_refs`）。`build_snapshot` が省略する要素が先行すると不一致側へ倒れ、`refs` は空配列になる（fail-closed）。解決不能はテストの失敗にせず事実として記録する。
- 現時点で `refs` が空のロケータ: click-04（`table#table1 tbody a[href="#edit"]` index 1）・extract-01（`table#table1 tbody td` index 14）・nav-02・nav-05。簡約表現上で ref を持たない正解要素であり、読み取れるかどうかは #121 の測定結果で判定する。
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
