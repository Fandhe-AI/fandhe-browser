---
paths:
  - "Makefile"
  - "scripts/**"
description: >
  Makefile と scripts/ を編集する際の方針。Makefile は薄い入口に徹し、処理の定義は scripts/*.sh に置く。
  ターゲットの追加・改名・削除手順、スクリプトの書き方、各ターゲットの契約を規定する。
  scripts/hooks/** は存在せず、lefthook の編集方針は git-hooks.md が担当する。
---

# Makefile・scripts 編集ルール

## 基本方針

- Makefile は **薄い入口**。各レシピは `@scripts/<name>.sh [args]` か、言語ネイティブ・Docker の 1 行呼び出し（`docker compose build` 等）に限る
- 条件分岐・ループ・ツール導入・複数行シェル・環境依存の処理は `scripts/` の bash スクリプトに置く。Makefile に `ifneq`・`for`・`if`・`curl` を書かない
- CI（`.github/workflows/ci.yml`）・lefthook・README が呼ぶコマンドを共通化するため、CI 独自にロジックを書かず `make <target>` か `scripts/*.sh` を呼ぶ
- 詳細な設計判断は make skill（`.claude/skills/make`）を参照する。診断は `node .claude/skills/make/scripts/bin/inspect-repo.mjs --root . --json`（読み取りのみ）

## ファイル構成

| ファイル | 役割 |
| -------- | ---- |
| `scripts/lib.sh` | 共有ヘルパー（`require_cmd`・`reject_args`・`has_cargo`・`has_members`・`skip`）。source して使い、リポジトリルートへ `cd` する |
| `scripts/versions.sh` | ツール版・サイズ上限値の正本（`: "${VAR:=default}"` 形式。環境変数または `make <target> VAR=value` で上書き可） |
| `scripts/cargo-gate.sh <gate>` | Cargo.toml・member crate が無い間は skip する cargo 系ゲート（fmt・lint・test・rendering・分離検証・publish 検査） |
| `scripts/harness.sh <name>` | 実測ハーネス・ベンチ・計測系の実体 |
| `scripts/lint-docs.sh <kind>` | markdownlint・yamllint・editorconfig-checker・commitlint |
| `scripts/help.sh` | Make 非依存のターゲット一覧（手動保守） |
| その他 | `setup.sh`・`rustup-check.sh`・`submodule.sh`・`install-hooks.sh`・`deny.sh`・`about.sh`・`check-deny-license-reject.sh`、CI も直接呼ぶ `check-render-isolation.sh`・`check-js-engine-isolation.sh`・`check-static-link.sh`・`check-dynamic-link-windows.sh`・`check-dynamic-link-macos.sh`・`ci-changes.sh`・`ci-prune-target.sh` |

`scripts/hooks/**` は存在しない。lefthook の編集方針は [git-hooks](./git-hooks.md) が担当し、`lefthook.yml` の `run:` はインラインで書く。

## ターゲットの追加・改名・削除手順

1. `.PHONY` に追加（改名・削除なら更新）する
2. レシピの前に `## 説明` を付ける（`make help` はこのコメントから生成される。無いターゲットは表示されない）
3. `scripts/help.sh` を同じ変更で更新する（自動同期の仕組みは無い）
4. 呼び出し元を洗い出して更新する

    ```bash
    grep -rnE "make [a-z-]+" .github .claude lefthook.yml scripts harness benches docs README.md CLAUDE.md AGENTS.md CONTRIBUTING.md Dockerfile compose.yaml
    ```

5. `make help` と `make -n <target>` で確認し、`shellcheck -x -P SCRIPTDIR scripts/*.sh` を通す
6. `ci:` の依存に入れるかを決める。時間がかかるもの（release ビルド・計測）・ネットワークを使うものは入れない

既存のターゲット名・引数・環境変数・終了コードは CI・lefthook・docs が参照しているため、変えるときは呼び出し元も同じ PR で直す。

## 禁止事項と互換性

- 中身の無いターゲット（`@true` 等）を置かない。呼び出し元が終了コードを信用してしまう
- GNU Make 3.81（macOS 標準）で動くこと。`.ONESHELL`・`.RECIPEPREFIX`・`.WAIT` 等の 3.82+ 専用機能を使わない
- `.DEFAULT_GOAL := help` を維持し、全ターゲットを `.PHONY` にする（実ファイルを生成するターゲットは無い）

## スクリプトの規約

- 先頭に `#!/usr/bin/env bash` と `set -euo pipefail`、続けて `source "${DIR}/lib.sh"`（`# shellcheck source=./lib.sh` を付ける）
- 実行権限を付ける（`chmod +x`）。引数を取らないスクリプトは `reject_args` で誤用を終了コード 2 にする
- shellcheck を通す（CI でもローカルでも `shellcheck -x -P SCRIPTDIR scripts/*.sh`）
- コメントは日本語、エラーメッセージ・ログなどプログラムの出力文字列は英語（[japanese-style](./japanese-style.md)）
- 前提が未整備（Cargo.toml・member crate が無い等）の場合は `skip` で 0 終了、必須ツール（jq・node 等）の欠落は `require_cmd` で fail-closed（silent skip にしない）
- ツール版・上限値は `scripts/versions.sh` にだけ書く。`ci.yml` は `BINARY_SIZE_LIMIT_BYTES`・`BINARY_SIZE_PACKAGE` を同ファイルから正規表現で抽出するため、`: "${NAME:=value}"` の書式を変えない。CARGO_DENY_VERSION は `ci.yml`・`Dockerfile`、lint ツール版は `ci.yml` の `lint-docs` と同期する

## ターゲットの契約

| 種別 | 契約 |
| ---- | ---- |
| `fmt` | ソースを書き換える唯一の整形ターゲット |
| `fmt-check`・`lint-*`・`lint`・`check-*`・`deny` | ソースを変更しない。失敗は非 0 で返す |
| `test`・`test-*`・`check-*` の自己テスト | 一時ファイルは `mktemp` に作り `trap` で掃除する |
| `measure-*`・`bench-record`・`trace-playwright` | 計測・記録用。`make ci` に含めない（ネットワーク・release ビルドを伴う） |
| `setup`・`hooks` | 再実行しても安全。`rustup` は確認のみで自動導入しない（curl \| sh を作らない） |
| `docker-*` | `compose.yaml` の `dev` サービスを呼ぶだけ |

このリポジトリには `clean` ターゲットは無い。追加する場合は共有 `CARGO_TARGET_DIR` を消さない（`--target-dir target` に限定し、`target/` が symlink なら拒否する）。
