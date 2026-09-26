# R-3 注入詳細（改修担当アクセス禁止・TASK-2.2 専用）

- **対応ビヘイビア**: REPAIR-2・REPAIR-3
- **対応タスク**: TASK-2.2（Issue #323）
- **参照元**: [repair-trial-record.md](./repair-trial-record.md) の R-3（本ファイルへのリンクは記載しない）

## 目的・アクセス制限

本ファイルは R-3（`text_content` のオフバイワン注入）の注入担当（別 Agent または人間）専用の作業メモである。
[repair-trial-record.md](./repair-trial-record.md) の実施要領が定義するとおり、**改修担当（R-3 を単独で修復する側）には本ファイルを渡さない**。改修担当には「`cargo test` の一部が失敗する」という症状だけを伝え、本ファイルの存在・内容を開示しない。

`repair-trial-record.md` 側には注入箇所・具体的な修正方法を記載せず、本ファイルにのみ記録する。試行（TASK-2.2）終了後に、本ファイルの内容を「試行結果」章へ追記する形で `repair-trial-record.md` に反映する（注入前に単独修復の実証を汚染しないため）。

## 注入対象・手順

- `text_content` の Element 分岐（`dom.rs`）は次の実装になっている（基準コミット時点）。

  ```rust
  NodeData::Element { .. } | NodeData::DocumentFragment => {
      let mut text = String::new();
      for descendant in self.descendants(id) {
          if let Some(NodeData::Text { contents }) = self.node_data(descendant) {
              text.push_str(contents);
          }
      }
      Some(text)
  }
  ```

- 注入は「最初に見つかった Text 子孫を 1 個だけ読み飛ばす」フラグを for ループに加える形で行う（例: `let mut skipped_first = false;` を用意し、最初の Text 一致時に `skipped_first` を立てて `continue` する）
- 注入前に改修担当以外の担当（別 Agent または人間）が、全ゲート（fmt/clippy/test）が通過していることを確認してから注入する（TASK-2.2 の前提条件）
- 注入すると、既存テスト `core_1_text_content_concatenates_descendant_text`（`<p>a<b>b</b><!--x-->c</p>` に対し期待値 `"abc"`。最初の Text `"a"` が失われるため実際の結果は `"bc"` になり失敗する）と `core_1_children_and_siblings_in_source_order`（`<ul><li>a</li><li>b</li><li>c</li></ul>` の子要素 text_content 期待値 `["a","b","c"]`。各 `li` の唯一の Text 子孫が失われるため `["","",""]` 相当になり失敗する）が失敗するはずであることを、TASK-2.1 ではアサーション内容を読むだけで確認済み（実際の注入編集・実行は TASK-2.2 の範囲）
- 修正後、専用テストを追加しなくても全ゲートを通過する（既存 2 テストの回復で十分）

## 取り扱い

- 注入ブランチは main に取り込まない
- 本ファイルは試行終了後、内容を `repair-trial-record.md` の「試行結果」章へ追記したうえで、そのコミットと同一 PR 内で削除するか、追記済みである旨を明記して残す（重複管理を避けるため、削除を基本方針とする）
