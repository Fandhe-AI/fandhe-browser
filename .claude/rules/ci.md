# CI・ローカル検証規約（リポ固有。XOS・REPAIR 系ビヘイビア）

## ローカルゲート（コミット・PR 前）

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- テストは変更のたびに全件実行し、失敗・警告を 1 件でも残したまま進めない（fail-closed。REPAIR-5・REPAIR-6）
- feature gate を触った場合は既定ビルドと `--features rendering` の両方で検証する

## 3 OS CI（Linux・macOS・Windows 一級対応）

- 各 OS のネイティブランナーでビルド・テストする（クロスコンパイル前提にしない）
- matrix は ubuntu / macos / windows の 3 OS を必須とし、特定 OS のみの skip で CI を通さない
- OS 依存のファイルシステム挙動（パス・大文字小文字・ロック）のテストは 3 OS すべてで実行する

## 分離・構成の検証

- 既定ビルドに Servo が含まれないことを `cargo tree` で検証する（RENDER-1）
- ライセンス検査は `cargo deny check licenses` で行う（[licensing](./licensing.md)）
- 許可外ライセンス（GPL/AGPL/LGPL/MPL-2.0・ライセンス未記載）を持つ canary を `cargo deny` が reject することを `make check-deny-license-reject` で検証する（TASK-9.1・REPAIR-8）

## ワークフロー変更時の注意

- GitHub Actions のサードパーティ action はコミット SHA で固定する
- `permissions` は最小権限で明示する
- secrets を `pull_request` イベントのログへ出力しない
- CI 設定の変更は infra-builder が担当し、reviewer / security-auditor のレビューを経る
