---
paths:
  - "lefthook.yml"
description: >
  lefthook.yml を編集する際の方針。フックは staged 限定で一瞬で終わる検査と、push 後では
  手遅れになる検査（秘密情報・コミットメッセージ）に限り、テスト・clippy・全体 lint など
  CI（.github/workflows/ci.yml）と重複する検査は置かない。時間予算・判断手順・計測方法を規定する。
---

# Git hooks（lefthook）編集ルール

## 目的

フックはコミットのたびに実行されるため、遅いフックはそのまま開発速度の低下になる。
フックはローカルでの早期検知専用であり、CI の代替ではない。**開発速度を落とさないことを最優先**し、
重い検査・網羅的な検査は CI（`.github/workflows/ci.yml`）に任せる。

ローカルゲート（`cargo fmt --all --check`・clippy・test。[ci](./ci.md)）の規約自体は変わらない。
フックで強制しないだけで、PR 前には手動で実行する（後述「手元で CI 相当を確認する」）。

## 時間予算

| フック | 予算 | 判定方法 |
| ------ | ---- | -------- |
| pre-commit | 全ジョブ合計 1 秒未満（`rustfmt-check` は下記の例外） | `lefthook run pre-commit --all-files` の `summary: (done in N seconds)` |
| commit-msg | 即時（外部コマンド・ネットワーク不可） | 実コミット時に表示される commit-msg の summary（メッセージファイルを引数に取るため `--all-files` では計測しない） |
| pre-push・post-checkout・post-merge 等 | 置かない（pre-push は廃止済み） | 追加する場合は理由と実測値を PR 本文に記載し、ユーザー承認を得る |

`--all-files` は全追跡ファイルを対象にする最悪ケースであり、実際のコミット（staged のみ）はこれより速い。

## フックに置かないもの（→ `ci.yml` へ）

| 検査 | 担う CI ジョブ |
| ---- | -------------- |
| clippy・`cargo test`（全 feature） | `rust-test-{ubuntu,macos,windows}` |
| feature 構成別の clippy・test・build | `rust-features-{ubuntu,macos}`・`rust-features-windows-{default,engines,boa}` |
| `cargo fmt --all --check`（workspace 全体）・`cargo deny` | `rust-lint` |
| markdownlint・yamllint・editorconfig-checker・commitlint | `lint-docs` |
| Servo / JS エンジン分離検証・ハーネス自己テスト・サイズ検査 | `harness-{ubuntu,macos,windows}` |
| macOS・Windows 向けの型検査 | `cross-target-check` |

- 上記に加え、ビルド・リポジトリ全体を走査する lint（`make ci`・`make lint-docs` 等の呼び出しを含む）、
  依存のインストール・ネットワークアクセスはフックに置かない
- `ci.yml` のジョブと同じコマンドを同じ範囲で実行するものは置かない

## フックに置いてよいもの

- `{staged_files}` に限定し、`glob` で対象を絞った 1 秒未満の検査
- CI では検知できない、または push 後では遅いもの
  - `secrets-guard`・`secrets-content-guard`: 秘密情報は push 前に止める必要がある。`ci.yml` には
    同等の検査が無い（サーバー側の最終防御線は GitHub の secret scanning + push protection）
  - `conventional-commits`（commit-msg）: 後述の例外

### 明示的な例外

CI と重複するが残している検査（2026-10-11 計測・決定）。

- `rustfmt-check`（pre-commit）: `rust-lint` の `cargo fmt --all --check` と重複するが、staged の `.rs` に
  限定しているため通常のコミットでは軽量（staged の `.rs` 5 件で 0.06 秒）。`--all-files`（`.rs` 233 件）では
  1.4 秒程度になり上記予算を超えるが、全 `.rs` を一度に stage するコミットは稀なため例外として許容する
- `conventional-commits`（commit-msg）: `lint-docs` の commitlint ステップと重複するが、CI 側は
  pull_request の commit 範囲しか検証せず、push 後のメッセージ修正は履歴の書き換えになる。
  シェルの正規表現 1 回で即時に終わるため維持する

## 書き方の規約

- `pre-commit` の `parallel: true` を維持する（直列化しない）
- プロセス起動回数を diff 行数・ファイル数に比例させない。1 行・1 ファイルごとに `grep` 等を起動する
  ループは避け、`awk` 1 本で抽出してからパターンごとに 1 回照合する（`secrets-guard`・
  `secrets-content-guard` の方式）
- 秘密情報系の検査は fail-closed にする
  - `git diff` の失敗・抽出件数の不一致・`grep` の終了状態 2 以上（検査エラー）は「一致なし」と
    同一視せず NG にする
  - `git diff` は `--no-color --no-ext-diff` を付け、利用者の git 設定で出力形式が変わらないようにする
  - ASCII のパターンで照合する `awk` / `grep` は `LC_ALL=C` で実行する（BSD awk / grep が非 UTF-8 の
    内容で異常終了・バイナリ扱いするため）
  - 検出時はファイル名とパターン種別のみを出力し、値そのものは出力しない
- 実体は `lefthook.yml` の `run:` にインラインで書いている。長くなって `scripts/` へ切り出す場合は、
  本ルールの `paths` にもそのパスを追加する
- 条件付き実行が必要な場合は lefthook の `skip` / `only` を使ってよい
  （`.claude/skills/lefthook/references/examples/skip.md`）

## 検査を追加するときの判断手順

1. その検査は `ci.yml` で既に実施されているか
    - 実施済みで、staged 限定にしても 1 秒以上かかる → フックに入れない
2. staged ファイルに限定できるか
    - できない → CI へ置く
3. CI へ置く場合は `ci.yml` にジョブ・ステップを追加する（[ci](./ci.md)「ワークフロー変更時の注意」）
    - ジョブを新設・改名する場合は GitHub の ruleset の required checks を同時に更新する
      （`ci.yml` に集約ジョブは無く、出る check-run をすべて required にする構成）
    - ツールの導入・バージョン固定は既存の `lint-docs` の方式（バージョン + sha256 検証）に合わせる
4. 編集後に以下を実行し、summary の合計時間をユーザーへ報告する（1 秒以上なら構成を見直す）

```bash
time lefthook run pre-commit --all-files
```

## 手元で CI 相当を確認する

フックでは強制しないため、PR 前に必要に応じて手動で実行する。

```bash
make ci          # ローカルゲート一式（lint-docs・fmt-check・clippy・test・deny・分離検証等）
make lint-docs   # markdownlint・yamllint・editorconfig-checker・commitlint のみ
```

## バイパス禁止

遅いからといって `--no-verify` や `LEFTHOOK=0` で回避しない。遅いフックはフック側を直す
（検査を CI へ移す・staged 限定にする・対象 glob を絞る・プロセス起動回数を減らす）。
