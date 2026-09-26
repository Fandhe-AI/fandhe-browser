# Contributing

fandhe-browser への貢献手順（開発環境構築・ローカル検証・コミット / PR 手順・コーディング規約）をまとめる。

spec 対応: `OSS-5`（`04-behavior/oss-licensing.md`）/ `TASK-79`（79.2）/ `MS-7`。

仕様・ビヘイビア定義の SSOT は private リポジトリ [fandhe-browser-spec](https://github.com/Fandhe-AI/fandhe-browser-spec)（`docs/spec` submodule の `04-behavior/`）にある。外部からの貢献者は `docs/spec` を取得できない場合があるが、ビルド・テストは `docs/spec` 抜きで成立する。本ドキュメント中のビヘイビア ID は SSOT を参照するための ID であり、内容の要約はここには含めない。spec 本文に残る旧称 `rust-browser` は `fandhe-browser` に読み替える。

## 開発環境構築

```bash
git clone git@github.com:Fandhe-AI/fandhe-browser.git
cd fandhe-browser
make setup
```

`make setup` はサブモジュール → rustup → lefthook の順で構築する。

- **サブモジュール**: `docs/spec`（private）の取得を試みる。アクセス権がない環境では警告のみで先へ進む
- **rustup**: 導入済みかどうかを確認するだけで、自動導入は行わない。未導入の場合は公式手順（<https://rustup.rs/>）に従って導入してから再実行する（`curl | sh` のような検証なしのインストール経路は使わない）
- **lefthook**: git hooks を導入する

ツールチェーンの単一真実源は `rust-toolchain.toml`（stable + rustfmt/clippy）。

環境に依存せず検証したい場合は Docker（`Dockerfile` / `compose.yaml`）を使う。

```bash
make docker-build  # 開発コンテナイメージをビルド
make docker-shell   # コンテナのシェルに入る
make docker-ci      # コンテナ内で make ci を実行する
```

## ローカル検証

```bash
make ci
```

`make ci` は [ci.md](.claude/rules/ci.md) のローカルゲートと同等のチェックを一括実行する。ターゲット一覧は `make help` で確認できる。

主なローカルゲート:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

加えて `make lint-docs`（markdownlint・yamllint・editorconfig-checker・commitlint）・`make deny`（`cargo deny check licenses` 等）・`make check-render-isolation`（既定ビルドに Servo が含まれないことの検証）がある。

feature gate `rendering` に関わる変更を行った場合は、既定ビルドに加えて次も実行する（`RENDER-1`）。

```bash
make lint-rendering
make test-rendering
```

clippy 警告は 0 件を維持する。テストの skip・ignore やアサーションの弱体化で CI を通すことはしない。

## コミット・PR 手順

1. `main` からブランチを作成する
2. コミットは [Conventional Commits](.claude/rules/conventional-commits.md) 形式で書く: `<type>(<scope>): <日本語の説明>`
   - type は 9 種（feat / fix / refactor / perf / test / docs / ci / build / chore）に限る
   - scope の一覧は [conventional-commits.md](.claude/rules/conventional-commits.md) を参照する
   - 破壊的変更には `!` を付け（例: `feat(ai)!: ...`）、本文に `BREAKING CHANGE:` を記載する
3. lefthook の git hooks がコミット時に自動で走る
   - pre-commit: rustfmt チェック・秘密情報検査
   - commit-msg: コミットメッセージの形式検査
   - pre-push: clippy・test
   - **`--no-verify` の使用は禁止**（フックを必ず通す）
4. 1 つの PR には 1 つの関心事だけを含める。実装中にスコープ外と判断した事項は放置せず [out-of-scope-tracking.md](.claude/rules/out-of-scope-tracking.md) の手順で Issue に記録する
5. PR 本文にはローカルゲートの実行結果を記載する。OS 依存の変更を含む場合は 3 OS（Linux・macOS・Windows）での結果も記載する
6. CI は Linux・macOS・Windows の 3 OS ネイティブランナーで実行される
7. AI レビュー（`.github/workflows/ai-review.yml`）は [AGENTS.md](./AGENTS.md) をレビュー観点として参照する（優先度 P0〜P2）
8. 提出前に OWASP Top 10・偽装機能の禁止・プロファイル境界の観点でセキュリティを確認する（[security.md](.claude/rules/security.md)）

## コーディング規約

以下の各ルールに従う。

- [coding-rust.md](.claude/rules/coding-rust.md): Rust 規約（crate 境界・外部入力の扱い・unsafe / FFI・クロスプラットフォーム対応・テスト方針）
- [code-comment-style.md](.claude/rules/code-comment-style.md): コメント規約（役割・呼び出し文脈・未実装箇所の将来仕様の書き方）
- [security.md](.claude/rules/security.md): セキュリティ規約（秘密情報・偽装機能禁止・プロファイル / プラグイン境界・OWASP Top 10）
- [licensing.md](.claude/rules/licensing.md): ライセンス規約（デュアルライセンス・Servo の MPL-2.0 隔離）
- [dependency-policy.md](.claude/rules/dependency-policy.md): 依存管理規約（依存最小・バージョン完全固定・承認制）
- [japanese-style.md](.claude/rules/japanese-style.md): 日本語出力スタイル（コメント・ドキュメントは日本語、プログラムの出力文字列は英語）
- [spec-reference.md](.claude/rules/spec-reference.md): spec（SSOT）の参照・ビヘイビア ID 併記の規約
- [ci.md](.claude/rules/ci.md): CI・ローカル検証規約
- [conventional-commits.md](.claude/rules/conventional-commits.md): Conventional Commits の type / scope 詳細
- [docs/design/contribution-guidelines.md](docs/design/contribution-guidelines.md): REPAIR-3 に基づく、未実装・簡易実装箇所へのコメント運用のみを扱うドキュメント（本 CONTRIBUTING.md とは対象が異なる）

## 依存の追加

- 事前承認なしに依存（`Cargo.toml` の `dependencies`）を追加・更新する PR は提出しない。まず Issue で相談する
- 採用するバージョンは `=x.y.z` で完全固定する（`^`・`~`・範囲指定は禁止）
- 許可するのは permissive ライセンス（MIT / Apache-2.0 / BSD / ISC / Unlicense / Zlib 等）のみで、GPL / LGPL / AGPL 系は導入しない。MPL-2.0（Servo）は `fandhe-browser-render` 配下に限定する

詳細は [dependency-policy.md](.claude/rules/dependency-policy.md) を参照する。

## ライセンス

本体のライセンスは MIT OR Apache-2.0（[README.md](./README.md) の「ライセンス」節・[LICENSE-MIT](./LICENSE-MIT)・[LICENSE-APACHE](./LICENSE-APACHE) を参照）。

## DCO / CLA

DCO（Developer Certificate of Origin）/ CLA（Contributor License Agreement）の要否は **未確定**。TASK-79（79.h1）で確定し、本ドキュメントへ反映する予定。それまでは要否のどちらも断定しない。

## 脆弱性報告・行動規範

- セキュリティ上の脆弱性は公開 Issue では報告しない
- 脆弱性の報告窓口は今後 `SECURITY.md` で整備する予定（TASK-79.4）
- 行動規範は今後 `CODE_OF_CONDUCT.md` で整備する予定（TASK-79.3）
