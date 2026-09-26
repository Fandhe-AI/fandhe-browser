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
- 対象サイト群の動作率回帰チェック（`REPAIR-8`・`COMPAT-1`・`COMPAT-4`。TASK-9.2）:
  `make check-compat-regression`（CI では `compat-regression` ジョブ）が
  `harness/compat-regression/check-matrix.sh` で全体・`--categories` 指定の類型
  （static/spa/form。存在必須として明示列挙）に加え `--all-categories` で
  マトリクス内に実在する全ての類型（`cat` はスキーマ上任意の文字列を許すため
  lazy・table 等も含む）の動作率が閾値 70% 以上かを判定する。実マトリクス
  `harness/compat-practical/results/matrix.json`（TASK-71.3・Issue #312 が
  生成予定）が未導入の間は `--allow-missing` を渡し、ファイル不在時は
  `::warning::` を出して通過させる。#312 の完了後は `--allow-missing` を外し
  fail-closed（ファイル不在は exit 2）に戻す。スキーマ契約・終了コードは
  `harness/compat-regression/README.md` を参照

## ワークフロー変更時の注意

- GitHub Actions のサードパーティ action はコミット SHA で固定する
- `permissions` は最小権限で明示する
- secrets を `pull_request` イベントのログへ出力しない
- CI 設定の変更は infra-builder が担当し、reviewer / security-auditor のレビューを経る
