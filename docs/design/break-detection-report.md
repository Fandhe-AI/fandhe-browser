# 破壊的変更検出の実証レポート（REPAIR-5）

- **対応ビヘイビア**: REPAIR-5（Must。`docs/spec/04-behavior/self-repair-design.md`）
- **対応タスク**: TASK-6（6.1 / 6.2 / 6.3 / 6.4）
- **マイルストーン**: MS-7
- **参考**: PoC-10（`docs/spec/03-poc/ai-self-repair/README.md`「フェイルクローズ検証」。3/3 = 100%）
- **基準コミット**: `3e84cd7`（本レポート作成時点の `origin/main` HEAD）
- **関連 Issue・PR**: #330 ↔ #445（TASK-6.1）・#331 ↔ #448（TASK-6.2）・#332 ↔ #446（TASK-6.3）・#333（本 Issue・TASK-6.4）

## 目的と判定基準

REPAIR-5 は、意図的な破壊的変更 3 種（オフバイワン・checkbox 判定反転・仕様後退）を CI ゲートに入れて `cargo test` を実行すると、3 種すべてが検出・拒否されることを求める。先行の TASK-6.1〜6.3（#445・#448・#446）で `crates/fandhe-browser-core/tests/break_detection.rs` に検出テスト計 11 件（6.1: 4 件、6.2: 4 件、6.3: 3 件）が実装され、origin/main にマージ済みだが、実際に注入して失敗を確認した記録（「注入検証記録」）は #445・#446・#448 の PR 本文・コメント・レビューのいずれにも残っていなかった。本レポートは、その注入を隔離 worktree で再現し、実際に取得した結果を記録したものである。

判定単位は次のとおりとする。

- **種別単位の「検出」**: その種別に属する全注入が red（`cargo test` の失敗）になった場合に検出とする
- **コンパイルエラーは「検出」に数えない**: REPAIR-5 が問うのは「テストによる検出」であり、コンパイルが通らず `cargo test` 自体が実行できないケースは別区分として扱う（本レポートでは 9 件すべてコンパイルが通り該当なし。「注入前の事前確認」参照）
- 1 件でも未検出（green のまま）やコンパイルエラー区分が生じた場合は「100%」と記載しない

### 注入前の事前確認

- I-1a: `dom.rs:216` は `Document::children` が返す `Children` のコンストラクタ呼び出し（`front: 0`）であることを確認済み。`Children` 構造体自体のフィールド定義（`dom.rs:400` 付近）は注入対象ではない
- I-3a・I-3b・I-3c: 分岐・腕を削除した後もコンパイルが通ることを確認済み（`match` の残りの腕で網羅性が保たれる、または未使用になった項目はコンパイラ警告に留まる）。3 件とも `cargo test` 自体は正常に実行でき、コンパイルエラー区分には該当しなかった

## 検出ゲートの構成

検出テストは `crates/fandhe-browser-core/tests/break_detection.rs`（結合テスト）に置かれている。ルート `Cargo.toml` は `[workspace]` のみを持つ仮想マニフェスト（`[package]` を持たない）であり、ルート直下に `tests/` を置いてもワークスペースの結合テストとしてはコンパイル対象にならないため、各 crate 配下の `tests/` に置く必要がある（この配置は #445・#448 で既に報告済みで、spec の成果物パス記述との食い違いは本レポートでは扱わない。「制約・残課題」参照）。`cargo test --workspace` は `.github/workflows/ci.yml` の 3 OS matrix（ubuntu・macos・windows）で常時実行される。

## 検証手順

1. `origin/main`（`3e84cd7`）を基準に隔離 worktree で作業する
2. ベースライン確認: `cargo test -p fandhe-browser-core --test break_detection` を実行し、11 件すべてが `ok` であることを確認する
3. 注入 9 件を 1 件ずつ、次のサイクルで再現する
   1. 対象ファイルの該当箇所 1 か所だけを編集する
   2. `git diff --stat` / `git diff` で変更が 1 ファイル・該当行だけであることを確認する
   3. `cargo test -p fandhe-browser-core --test break_detection --no-fail-fast` を実行し、終了コード・失敗テスト名・`left`/`right` またはパニックメッセージを記録する
   4. `git checkout -- <file>` で復元し、`git status --porcelain` が空であることを確認する
4. 9 件すべての注入・復元が完了した後、手順 2 を再実行し、`crates/fandhe-browser-core/tests/break_detection.rs` の 11 件すべてが `ok` に戻ることを確認する
5. 実行環境: `rustc 1.98.1`（`48a229cea` 2026-09-01）・`cargo 1.98.1`・Linux（x86_64）

## 結果一覧表

| 種別 | 注入 ID | 箇所（ファイル・関数） | 注入内容 | 検出したテスト | 結果 |
| ---- | ------- | ---------------------- | -------- | --------------- | ---- |
| オフバイワン | I-1a | `dom.rs:216`（`Document::children` 内の `Children` 構築） | `front: 0` → `front: 1` | `repair_5_off_by_one_children_keeps_first_and_last`（`left: ["2","3"]` / `right: ["1","2","3"]`）・`repair_5_off_by_one_sibling_navigation_boundaries`（`left: 2` / `right: 3`） | 検出 |
| オフバイワン | I-1b | `parse.rs:212`（`parse_document`） | `input.len() > options.max_input_bytes` → `>=` | `repair_5_off_by_one_max_input_bytes_boundary_str`・`repair_5_off_by_one_max_input_bytes_boundary_bytes`（いずれも「上限ちょうどの入力は Ok であるべき」で panic） | 検出 |
| オフバイワン | I-1c | `parse.rs:257`（`parse_document_bytes`） | `input.len() > options.max_input_bytes` → `>=` | `repair_5_off_by_one_max_input_bytes_boundary_bytes`（「上限ちょうどの入力は Ok であるべき」で panic） | 検出 |
| checkbox 反転 | I-2a | `query.rs:197`（`match_attribute` の `Exists` 腕） | `.is_some()` → `.is_none()` | `repair_5_checkbox_inversion_checked_selector_returns_only_checked`（`left: ["b"]` / `right: ["a","c","d"]`）・`repair_5_checkbox_inversion_element_matches_per_checkbox` | 検出 |
| checkbox 反転 | I-2b | `query.rs:199`（`Equals` 腕） | `==` → `!=` | `repair_5_checkbox_inversion_attribute_presence_accessor`・`repair_5_checkbox_inversion_checked_selector_returns_only_checked`・`repair_5_checkbox_inversion_element_matches_per_checkbox`・`repair_5_checkbox_inversion_type_equals_excludes_radio_and_text`・`repair_5_spec_regression_quoted_attribute_value`（5 件） | 検出 |
| checkbox 反転 | I-2c | `dom.rs:352`（`Document::attribute` の `find` クロージャ） | `(*attr.name.local).eq_ignore_ascii_case(name)` → `!(...)` | `repair_5_checkbox_inversion_attribute_presence_accessor`（「テスト入力の checkbox 数の前提」で panic）・`repair_5_checkbox_inversion_checked_selector_returns_only_checked`・`repair_5_checkbox_inversion_element_matches_per_checkbox`・`repair_5_checkbox_inversion_type_equals_excludes_radio_and_text`・`repair_5_spec_regression_quirks_mode_id_class_case_insensitive`・`repair_5_spec_regression_quoted_attribute_value`（6 件） | 検出 |
| 仕様後退 | I-3a | `selector.rs:407`（`parse_attribute_selector`） | `Some('\'') \| Some('"') => parse_quoted_string(cursor)?,` の腕を削除 | `repair_5_spec_regression_quoted_attribute_value`（`InvalidInput { message: "expected attribute value at byte offset 9" }` で panic） | 検出 |
| 仕様後退 | I-3b | `query.rs:144`（`compound_matches`） | `Some(local_name) if is_html => html_local_name_eq(...)` の腕を削除 | `repair_5_spec_regression_html_type_selector_case_insensitive`（`left: 0` / `right: 1`） | 検出 |
| 仕様後退 | I-3c | `query.rs:180-186`（`id_or_class_eq`） | quirks 分岐を削除し `actual == expected` のみにする | `repair_5_spec_regression_quirks_mode_id_class_case_insensitive`（`left: 0` / `right: 1`） | 検出 |

いずれの注入もコンパイルは通過した（I-3a・I-3b・I-3c はコンパイラ警告（未使用関数・未使用変数・未使用 import）のみ発生。「注入前の事前確認」参照）。

## 集計

| 種別 | 注入数 | 検出数 | 判定 |
| ---- | ------ | ------ | ---- |
| オフバイワン | 3（I-1a・I-1b・I-1c） | 3 | 検出 |
| checkbox 判定反転 | 3（I-2a・I-2b・I-2c） | 3 | 検出 |
| 仕様後退 | 3（I-3a・I-3b・I-3c） | 3 | 検出 |

**検出率 3/3 = 100%（注入単位 9/9 = 100%）**

各注入の復元後は `git status --porcelain` が空であることを都度確認した（復元漏れがないことの確認）。加えて、各注入の `cargo test` 実行結果で「その注入が対象とする失敗テスト以外の残り 10 件（または 9 件）が `ok`」であったことが、直前の注入が正しく復元されていたことを裏付けている。9 件すべての注入・復元が完了した時点で `cargo test -p fandhe-browser-core --test break_detection` を再実行し、11 件すべてが `ok` に戻ることを最終確認した（「検証手順」参照）。

## PoC-10 との対応と差異

PoC-10（`docs/spec/03-poc/ai-self-repair/README.md`「フェイルクローズ検証」）は 3 種の破壊的変更をそれぞれ 1 件ずつ注入して 3/3 = 100% を確認しているが、対象実装が異なるため本レポートでは次のとおり置き換えている。

1. **オフバイワン**: PoC の `.skip(1)` 相当の注入を、core の `Children` イテレータの初期オフセット（`front`）と、`parse_document` / `parse_document_bytes` の入力長境界比較（`>` → `>=`）に置き換えた
2. **checkbox 判定反転**: PoC の `build_form_values` は core にまだ実装されていないため（REPAIR-3: 実装済みを装わない）、query モジュールの checked 判定（`[checked]` 属性の存在判定・`[type=checkbox]` の値一致判定・`Document::attribute` の属性名の大文字小文字比較）に置き換えた
3. **仕様後退**: PoC の `get_meta_tags` の property フォールバック削除を、selector（属性セレクタの引用符付き値解析）と query（HTML 要素名の大文字小文字非依存照合・quirks mode でのクラス名照合）の、spec 対応済み分岐の削除に置き換えた

## 制約・残課題

- 注入の再現はローカル Linux 環境（`rustc 1.98.1`）でのみ行った。検出ゲートそのもの（`break_detection.rs` の 11 件）は 3 OS CI（ubuntu / macos / windows）で常時実行される（[ci.md](../../.claude/rules/ci.md)）が、注入再現を 3 OS すべてで行うことは本レポートの範囲外とした
- `break_detection.rs` の TASK-6.1・6.3 セクションのコメントには「PR 本文の『注入検証記録』参照」という、参照先が存在しないコメントが残っている。本レポート作成の過程で判明したが、コメントをどう修正するか（本レポートへの参照に差し替える等）はユーザー承認を要する事項のため、本 PR では変更しない（別 Issue で扱う）
- フォーム値抽出 API（PoC-10 の `build_form_values` 相当）が core に実装された後、checkbox 判定反転の検出対象にそちらを追加するかどうかは別途検討する
- `max_nodes` 境界のテスト追加は見送り済み（#445 で報告済み）

## 結論

TASK-6.1〜6.3 で実装された `break_detection.rs` の検出テスト 11 件について、対応する 3 種・9 件の破壊的変更注入をすべて隔離 worktree で再現した結果、9 件すべてが `cargo test` の失敗として検出された（検出率 100%）。REPAIR-5 が求める「3 種すべての検出・拒否」を満たしていることを実測で確認した。

## セキュリティ考慮事項（OWASP Top 10 ほか）

- 本 Issue は docs のみの変更で、実行コード・攻撃面は増えない（インジェクション・SSRF・アクセス制御・不安全な設計・脆弱な依存のいずれも該当なし）。依存の追加・更新はない
- 注入した回帰はいずれもコミットに混入していない。各注入の直後に `git checkout` で復元し `git status --porcelain` が空であることを確認し、コミット前にも `git status` で新規 `.md` 1 件だけであることを確認した
- 秘密情報: レポートに環境変数・トークン・ローカルの絶対パス（ホーム配下）は記載していない。記載した環境情報は rustc の版と OS 種別のみ
- 非信頼データ: Issue #333 本文・関連 PR のコメントを逐語で転記せず、事実（PR 番号・テスト名・再現結果）のみを記載した
- 偽装・回避機能: 該当なし（テストの注入検証のみで、UA 偽装や anti-bot 回避には関わらない）

## 関連ビヘイビア・タスク

- **対応ビヘイビア**: REPAIR-5
- **対応タスク**: TASK-6（6.1〜6.4）
- **マイルストーン**: MS-7
- **参考**: PoC-10（`docs/spec/03-poc/ai-self-repair/README.md`）
