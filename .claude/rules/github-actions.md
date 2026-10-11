---
paths:
  - ".github/workflows/**"
  - ".github/actions/**"
description: >
  GitHub Actions のワークフローを追加・変更する際の方針。runner（public リポジトリ）、必須チェックと
  ジョブ名、発火条件・concurrency・paths の扱い、新規ジョブ追加チェックリスト、所要時間の計測方法と棚卸し結果を規定する。
---

# GitHub Actions 編集ルール

ジョブ構成・3 OS の実行時機・docs-only 判定の詳細は [ci](./ci.md)、ローカルのコマンド定義は [makefile](./makefile.md)、
lefthook との重複回避は [git-hooks](./git-hooks.md) を参照する。本ファイルは「追加・変更時に守ること」を扱う。

## runner 方針

- 本リポジトリは **public**。runner は GitHub-hosted（`ubuntu-latest`・`macos-latest`・`windows-latest`）を使う
- public リポジトリで self-hosted runner を使うと、fork PR 経由で任意コードが実行されるリスクがある。新規に使わない
- 現在 self-hosted を使う箇所は `ai-review.yml` の `codex` ジョブ（reusable workflow の review ジョブ。provider 名と同名の用途ラベル `codex` の専用 runner）のみ。資格情報を扱うため例外とし、fork PR は reusable workflow 側が fail-closed で拒否する。`preflight`・`post_feedback` は `ubuntu-latest`（組織 runner 方針: Fandhe-AI/actions の `docs/runner-policy.md`）
- macOS・Windows は OS 固有の挙動を検証するための例外（`ci.yml` の `*-macos`・`*-windows`・`bench-competitor.yml`）

## 必須チェックとジョブ名

- ruleset `main-protection` の required status checks（2026-10-11 時点）: `changes`・`lint-docs`・`rust-lint`・`rust-test-{ubuntu,macos,windows}`・`rust-features-{ubuntu,macos,windows}`・`harness-{ubuntu,macos,windows}`・`windows-servo-prereqs`・`cross-target-check`・`codex / preflight`・`codex / review`・`codex / post_feedback`・`Cursor Bugbot`。branch protection（旧方式）は未設定
- 集約ジョブ（`ci-complete` 等）は無い。出る check-run をすべて required にする構成のため、**ジョブ名・`name:` を変える／ジョブを足す／消すときは ruleset の required checks を同時に更新する**（ユーザー判断事項。Claude は ruleset を変更しない）
- required にするジョブでは matrix・reusable workflow 呼び出し（`caller / callee` 形式の名前）を使わない（skipped の matrix は展開されず check 名が変わる）
- 非 required の補助ジョブを `ci.yml` に足さない

## 発火条件

- `ci.yml` は `pull_request` と `push`（`main` のみ）と `workflow_dispatch`。push を default ブランチに限り、同一コミットの二重実行を避ける
- `concurrency` は PR 単位で `cancel-in-progress: true`（`ci.yml`・`ai-review.yml`）。release・定期実行・update-external のように途中キャンセルが危険なものは `false`
- **required check を含む workflow に workflow 単位の `paths` / `paths-ignore` を付けない**（フィルタで起動しないと check が pending のままマージ不能になる）。変更検知はジョブ内（`changes` ジョブ + `scripts/ci-changes.sh`）で行い、下流ジョブの skipped は required を満たす
- draft PR のスキップは現状行っていない。導入する場合は `ready_for_review` も `pull_request.types` に含め、required の `changes` が skipped にならないかを確認する
- schedule: `update-external.yml` は毎日 1 回（約 2 分）、`bench-competitor.yml` は毎週月曜 1 回（3 OS）。頻度を上げない。`ci.yml` に `schedule` を足さない（全ジョブが cron で走る）

## 新規ジョブ・ワークフロー追加チェックリスト

1. 既存ワークフローへのジョブ追加を優先する（ワークフロー数を増やさない）。ただし required 化が要るため ruleset 更新が前提
2. 不要な `needs` を作らない。`changes` への依存のみとし、直列チェーンにしない。クリティカルパスは `rust-features-ubuntu`（PR で約 12 分）
3. 各ステップは `if: ${{ !cancelled() }}` で独立に走らせ、1 回の実行で全指摘を得る（既存方針）
4. 検査コマンドは `make <target>` か `scripts/*.sh` を呼ぶ。CI 独自にロジックを書かない（ツール版は `scripts/versions.sh`）
5. checkout は `persist-credentials: false`。`fetch-depth: 0` は差分・commit 範囲が要る `changes`・`lint-docs` のみ
6. Rust ジョブは `actions/cache`（`target` は `scripts/ci-prune-target.sh` で workspace 成果物を除いてから保存。Windows は除外）と `CARGO_INCREMENTAL=0`
7. action はコミット SHA で固定し（`# vX.Y.Z` コメント付き）、新規 action は安易に追加しない（cache は `actions/cache`、toolchain は `dtolnay/rust-toolchain` のみ）。`permissions` は最小権限で明示し、`timeout-minutes` を必ず設定する
8. `run:` に `${{ }}` を直接埋め込まず `env:` 経由で渡す。secrets を PR ログへ出さない
9. lefthook と同じ検査を CI に足さない／CI にある検査をフックに足さない（[git-hooks](./git-hooks.md)）
10. 所要時間の目安: PR の `ci.yml` 全体は約 11 分以内、`lint-docs`・`changes`・`rust-lint` は 30 秒未満。超える見込みなら PR 本文に理由を書く

## 計測方法

```bash
gh run list --workflow ci.yml --limit 30 --json databaseId,conclusion,createdAt,updatedAt,event
gh run view <run-id> --json jobs --jq '.jobs[]|[.name,.conclusion,.startedAt,.completedAt]|@tsv'
gh api repos/Fandhe-AI/fandhe-browser/actions/runs/<run-id>/jobs --paginate   # ステップ単位
```

## 棚卸し結果（2026-10-11 計測）

| ワークフロー | トリガー | 実測 | 備考 |
| ------------ | -------- | ---- | ---- |
| `ci.yml`（PR） | pull_request | 中央値 11.0 分・最大 12.5 分（20 件） | 最長は `rust-features-ubuntu` 約 12 分（clippy と test を 14 ステップで直列実行）、次いで `rust-test-ubuntu` 約 7 分。docs-only は `changes` + `lint-docs` のみで約 30 秒 |
| `ci.yml`（main push） | push | 中央値 22.6 分・最大 30.9 分（6 件） | 3 OS 実行。Windows の `rust-features-windows` が最長（約 31 分） |
| `ai-review.yml` | pull_request | 約 1〜2 分 | |
| `update-external.yml` | schedule・dispatch | 約 2 分 | 日次 |
| `bench-competitor.yml` | schedule・dispatch | 約 2〜3 分 | 週次・3 OS。対象バイナリ未配線のため skipped が多い |
| `release.yml` | workflow_dispatch | 実績少 | 手動のみ |

短縮余地は残るが、`rust-features-ubuntu` を複数ジョブに分割すると required checks の追加（ruleset 更新）が要るため、実施する場合はユーザー判断とする。
