# Rust コーディング規約

## ツールチェーン

- `rust-toolchain.toml`（stable・rustfmt・clippy）を単一真実源とする
- `cargo fmt --all`・`cargo clippy --workspace --all-targets -- -D warnings`・`cargo test --workspace` を通してからコミットする（clippy 警告 0 件を維持。REPAIR-6）
- ビルド・テストは `docs/spec` 抜きで成立させる。コード・`build.rs`・テストから `docs/spec` 配下を参照しない

## crate 構成と境界

- workspace は `crates/fandhe-browser-*` で構成する（core / js / ai / cdp / render / profile / cli / mcp）
- 1 回の改修が波及する crate・モジュールを最小に保つ（単一責務・疎結合。AI 自己補修の前提。REPAIR 系ビヘイビア）
- 循環依存を作らない。複数 crate が共有する型は下位 crate（core 等）へ置き、上位から下位への一方向依存を保つ
- レンダリング層（`fandhe-browser-render`・Servo）は feature gate `rendering` 配下に隔離し、既定ビルドの依存グラフへ混入させない（RENDER-1。[licensing](./licensing.md)）
- JS エンジンはトレイト抽象越しに使い、V8 / boa の具象型を上位 crate へ漏らさない

## 公開 API

- 戻り値は将来拡張できる構造を持つ型にする（真偽値・フラットな文字列で済ませない。REPAIR-4）
- 未実装・簡易実装の箇所は「実装済みを装わない」。ドキュメントコメントに将来仕様と対応するビヘイビア ID を明記する（REPAIR-3・[code-comment-style](./code-comment-style.md)）

## エラーハンドリング

- ライブラリコードでは `Result` を返し、panic させない（release は `panic = "abort"` 前提のため `catch_unwind` に頼らない）
- 外部入力（ネットワーク取得した HTML/JS・CDP / API リクエスト・プラグイン入出力）の経路では `unwrap` / `expect` / 添字アクセス（`[]`）を使わず、`get()`・`try_into()`・checked 演算で明示的に処理する
- 長さ・件数を上限検証してからアロケーションに使う（無制限確保による DoS を防ぐ）

## unsafe・FFI

- `unsafe` は原則禁止。FFI 境界（rusty_v8 等）で必要な場合のみ、`// SAFETY:` コメントで理由と維持すべき不変条件を明記する
- `unsafe` の新規追加はユーザー承認を得る（レビューで P0 として扱う）

## クロスプラットフォーム（Linux・macOS・Windows 一級対応。XOS 系）

- パスは `PathBuf` / `Path::join` で組み立て、文字列連結・区切り文字のハードコードをしない
- Windows の長パス・大文字小文字非区別ファイルシステムを考慮する（大文字小文字だけが異なる名前の衝突はエラーにする）
- 内部データファイルの改行は LF 固定。OS 固有処理は `cfg(target_os = ...)` で局所化する

## テスト

- 挙動は `docs/spec` のビヘイビア ID（例: `CORE-1`）に対応づけてテストし、テスト名またはドキュメントコメントに ID を記す
- ユニットテストと結合テストを併置し、期待値は具体値で書く（真偽値のみの assert に頼らない）
- テストの skip・ignore・アサーション弱体化で CI を通さない

## コメント

- [code-comment-style](./code-comment-style.md) に従う（`//!` / `///` のドキュメンテーションコメント）
