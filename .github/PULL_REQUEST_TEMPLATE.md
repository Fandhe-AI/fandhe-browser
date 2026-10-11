<!-- markdownlint-disable-file MD041 -->
## Summary

<!-- 変更内容を箇条書きで書く。HTML コメントは投稿後の本文に残らない -->

- spec 対応: <!-- TASK-n・ビヘイビア ID・MS-n。無ければ「なし」 -->

## 関連 Issue

<!-- Closes #N / Refs #N。人間担当（owner:human）の Issue は Refs を使う -->

## Test plan

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] ドキュメント・設定ファイルを変更した場合: `make lint-docs`
- [ ] feature gate `rendering` に関わる場合: `make lint-rendering`・`make test-rendering`（`RENDER-1`）
- [ ] OS 依存の変更を含む場合: Linux・macOS・Windows での結果を記載
- [ ] 実行していない項目は理由を書いた
- [ ] テストの skip・ignore・アサーション弱体化で通していない

## セキュリティ確認

- [ ] 実トークン・Cookie・資格情報・実プロファイルデータを含まない
- [ ] UA・フィンガープリント偽装、anti-bot 回避として作用する変更を含まない
- [ ] プロファイル境界・プラグイン境界を越える経路を作っていない
- [ ] 外部入力を検証し、上限なしのリソース確保をしていない
- [ ] 依存の追加・更新を含まない（含む場合は事前に相談した Issue 番号を書く）
- [ ] `unsafe` の新規追加を含まない（含む場合は `// SAFETY:` と承認の経緯を書く）
- [ ] `--no-verify` を使っていない

## 対象外（out-of-scope）

<!-- スコープ外と判断した事項と切り出し先 Issue 番号。無ければ「なし」 -->
