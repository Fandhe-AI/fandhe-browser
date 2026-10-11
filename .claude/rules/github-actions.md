---
paths:
  - ".github/workflows/**"
  - ".github/actions/**"
description: >
  GitHub Actions のワークフローを追加・変更する際の方針。runner（public リポジトリ）、必須チェックと
  ジョブ名、発火条件・concurrency・paths の扱い、新規ジョブ追加チェックリスト、キャッシュ方針（上限・key 設計・保存条件）、
  所要時間の計測方法と棚卸し結果を規定する。
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
6. Rust ジョブは `actions/cache/restore` + `actions/cache/save`（保存は main のみ。`target` は `scripts/ci-prune-target.sh` で workspace 成果物を除いてから保存。Windows を含む 3 OS 共通。prune は save と同じ条件〔main かつ exact hit でない〕のときだけ走らせ、PR と exact hit の main では省く）と `CARGO_INCREMENTAL=0`・`CARGO_PROFILE_DEV_DEBUG=line-tables-only`（3 OS 共通）。key・保存条件は後述「キャッシュ方針」に従う
7. action はコミット SHA で固定し（`# vX.Y.Z` コメント付き）、新規 action は安易に追加しない（cache は `actions/cache`〔`/restore`・`/save` を含む〕、toolchain は `dtolnay/rust-toolchain` のみ）。`permissions` は最小権限で明示し、`timeout-minutes` を必ず設定する
8. `run:` に `${{ }}` を直接埋め込まず `env:` 経由で渡す。secrets を PR ログへ出さない
9. lefthook と同じ検査を CI に足さない／CI にある検査をフックに足さない（[git-hooks](./git-hooks.md)）
10. 所要時間の目安: PR の `ci.yml` 全体は約 11 分以内、`lint-docs`・`changes`・`rust-lint` は 30 秒未満。超える見込みなら PR 本文に理由を書く

## キャッシュ方針

GitHub Actions の cache はリポジトリ単位で上限があり、超えると最終アクセスが古いものから追い出される（7 日間アクセスが無いものは削除）。
本リポジトリの上限は **10GB**（Org 設定も 10GB）。Org の Actions 予算は **$0（超過時停止）** のため、10GB を超える上限は使えない（上限を上げると 10GB を超えた時点で cache が読み取り専用になる）。上限・予算の変更はユーザー判断事項。
目標は「1 世代（全 cache 種別の合計。現状約 6〜7GB）を常時 10GB 以内に収め、追い出されても main の最新 cache に hit し続ける」こと。key 切り替え（`Cargo.lock`・rustc・`-vN` の変更）で新旧 2 世代が並ぶと古い世代が LRU で追い出されるが、新世代が保存された後なのでミスにはならない（2026-10-11 の実測で追い出しによる miss は 0 件。下記「実測」）。毎回読まれる main の最新 cache は LRU で最後まで残るため、この状態を保てていれば上限を上げる必要はない。新しい cache 種別の追加で常時 10GB を超える構成は避ける。ただし Windows の cache 追加（test・features・harness の 3 種）で 1 世代が 10GB に近づく、または超える場合に古い cache が追い出されることは、オーナー判断（2026-10-11）として許容する（追い出されても main の最新 cache が残ればよい）。Windows 3 種のサイズは `workflow_dispatch` の計測で features 912MB・test 500MB・harness 376MB（圧縮後。合計約 1.8GB）で、Linux・macOS の 1 世代（約 7.1GB）と合わせても 10GB 以内に収まる見込み（`windows-latest` の `Windows` が key に入るため Linux・macOS とは別の cache）。

### 入れる前に効果を測る

- cache を足す・残す前に、ログで効果を確かめる（hit したジョブで依存 crate の `Compiling` が出ないこと、restore にかかった秒数とコールドビルドとの差）。効果が無いものは入れない（例: `cross-target-check` は target を cache しても効果が薄いため registry のみ。後述「サイズ削減」）
- 本リポジトリの cache を使うジョブは GitHub-hosted runner（毎回空の環境）で動く。self-hosted runner ではツールチェーン・`CARGO_HOME`・`CARGO_TARGET_DIR` 等がジョブをまたいで残り、GitHub の cache が復元時間だけかかって無意味になることがある（Org の実例: mirror-ui は pnpm cache の復元に 1 ジョブ平均 75 秒かけて `downloaded 0` だったため撤去。team-hub は `Swatinem/rust-cache` が runner の `CARGO_TARGET_DIR` を保存できず registry しか保存しておらず、後処理が共有 `CARGO_HOME` の crate を消していたため撤去）

### key 設計

- 形式は `rust-<runner.os>-<種別>-vN-<rustc cachekey>-<hashFiles('Cargo.lock')>`（`cross` は registry のみのため rustc を含めない）。restore-keys は `rust-<runner.os>-<種別>-vN-<rustc cachekey>-` までにする
- lockfile は **ルートの `Cargo.lock` だけ** を hash する。`hashFiles('**/Cargo.lock')` は `.agents/skills/**/samples` 等の無関係な lockfile まで拾い、依存が変わらないのに全 cache を作り直す（2026-10-11 に #859 のスキル追加で約 7.1GB の新世代が作られ、旧世代が LRU で追い出された）
- rustc 版（`dtolnay/rust-toolchain` の出力 `cachekey`。step に `id: toolchain` を付ける）を key に含め、restore-keys も同じ rustc に限る。`rust-toolchain.toml` は `stable` 追従のため、含めないと stable 更新後に古い rustc の target を exact hit で復元し続け、再保存もされず依存を毎回ビルドし直す
- key・restore-keys に SHA・run_id・日付などの一意値を入れない（毎回 miss して保存し、cache を食い潰す）
- cache 内容（path・ビルド条件・prune の仕方・`Cargo.toml` の `[profile]`）を変えたら `-vN` を上げる。restore-keys に旧 `-vN` を残さない。key に `Cargo.toml` のハッシュは入らないため、`[profile]` の変更は `-vN` で切り替えないと既存 key に exact hit したまま依存を毎回再ビルドし、再保存もされない（2026-10-11 に `[profile.dev.package."*"]` の追加で test `v5` → `v6`・features / harness `v2` → `v3`。依存のビルドスクリプト・proc-macro が行番号付きに変わり、それらに依存する crate の `-C metadata` も連鎖して変わるため。registry のみの cross は上げない）

### 保存は main のみ

- 復元は `actions/cache/restore`（全イベント）、保存は `actions/cache/save` を `if: ${{ success() && github.ref == 'refs/heads/main' && steps.cache.outputs.cache-hit != 'true' }}` で行う。`actions/cache`（post で自動保存）を Rust の大きな cache に使わない
- PR が保存した cache はその PR（`refs/pull/<n>/merge`）からしか読めず、main の cache と同内容の重複になって main の cache を追い出す。PR は main の cache を読むだけにする
- トレードオフ: `Cargo.lock` を変える PR は、2 回目以降の push でも main の旧 lock の cache から部分復元し、変わった依存だけ毎回ビルドし直す（2026-10-11 の実測では依存が同じ PR の部分復元と exact hit で依存の再コンパイルは 0 件・所要時間差は誤差の範囲）
- `ci.yml` の concurrency は main でも `cancel-in-progress: true` のため、取り消された main の run は保存しない。新しい key の cache は、その後最初に完走した main の run が保存する
- 例外: `rust-lint` の cargo-deny 導入 cache（約 5MB・版のみの key で作り直しが起きない）は `actions/cache` のままでよい

### サイズ削減

- `target` は保存前に `scripts/ci-prune-target.sh` で workspace メンバーの成果物を除く（依存 crate の成果物だけ残す）。同スクリプトは最後に cache 対象の展開後サイズ（`du -sh`）を出す
- Rust ジョブ（Windows を含む 3 OS）は `CARGO_INCREMENTAL=0` と `CARGO_PROFILE_DEV_DEBUG=line-tables-only`（workspace メンバーを含む全体の debuginfo を行番号のみにする。release・bench には影響しない）。依存 crate は `Cargo.toml` の `[profile.dev.package."*"]` でも同じ値に固定済み（ローカル開発と共用）で衝突しないが、メンバーまで行番号のみにするのは env だけのため env は維持する。Windows ではこれに加えて PDB 生成・リンク時間の短縮にもなる（効果は `workflow_dispatch` で計測する）
- target を cache しても効果が無いジョブは registry のみにする（`cross-target-check`：v8 の prebuilt がターゲットごとに約 150MB あり、check のみで再ビルドが速いため）
- 新しい cache を足す前に、既存の cache 種別（test・features・harness・cross）と共有できないかを確認し、足す場合は 1 世代の合計が上記の目標に収まるかを見積もる（Windows の 3 種は追い出し許容のオーナー判断のうえ導入した。サイズは上記「cache 予算」の通り）

### 計測方法

```bash
gh api repos/Fandhe-AI/fandhe-browser/actions/cache/usage                       # 合計サイズ・件数
gh cache list -R Fandhe-AI/fandhe-browser -L 100                                # key・サイズ・ref の一覧
gh api --paginate "repos/Fandhe-AI/fandhe-browser/actions/caches?per_page=100" \
  --jq '.actions_caches[]|[.key,.size_in_bytes,.ref,.created_at,.last_accessed_at]|@tsv'
gh run view <run-id> --log | grep -E "Cache restored from key|Cache not found|Cache saved with key|Cache Size|cache size \(uncompressed\)" -A4
gh cache delete <id> -R Fandhe-AI/fandhe-browser                                # 不要になった cache の削除
```

- exact hit は `Cache restored from key: <primary key>`（save step は skipped）、部分復元は restore-keys 側の key、miss は `Cache not found`
- 追い出しの有無は cache 一覧で「7 日以内に作られた main の key が消えているか」と、miss の key が以前存在したかで判定する
- 効いているかは、hit したジョブのログで依存 crate の `Compiling` が出ていないこと（workspace メンバーのみ）で確認する
- cache は全 PR・main で共有する状態のため、`gh cache delete` は一覧で対象（key・ref・最終アクセス）を確認してから行う

### PR での確認

ワークフローの cache を追加・変更する PR では、次を確認したことを PR 本文に書く。

- 効果を測った（上記「入れる前に効果を測る」。restore 秒数・依存の再ビルド有無）
- 保存は main への push のみ（PR は restore だけ）
- key はルートの `Cargo.lock` と rustc 版から作り、SHA・run_id 等の一意値を含まない。restore-keys で部分復元できる
- 1 世代の合計サイズ（1 件のサイズ × 種別 × OS）を見積もり、10GB に余裕をもって収まる

### 実測（2026-10-11）

| 項目 | 変更前 |
| ---- | ------ |
| 1 世代の内訳（圧縮後） | Linux: test 1.63GB・features 1.29GB・harness 0.49GB・cross 0.07GB／macOS: test 1.52GB・features 1.32GB・harness 0.49GB（計約 7.1GB）＋ cargo-deny 5MB |
| 使用量 | 最大 12.3GB（2 世代）→ LRU 追い出し後 9.3GB・9 件 |
| ヒット率（CI 52 run・restore 301 件） | exact 約 279 件・部分復元 約 20 件（世代交代直後）・miss 2 件（key 種別の新設時のみ）。追い出しによる miss は 0 件（旧世代は追い出されたが、新世代の作成後だった） |
| 無駄な保存 | `**/Cargo.lock` による無関係な世代交代 1 回（約 7.1GB）、PR scope の保存 4 件（約 3.5GB。main の保存前に始まった PR の重複を含む） |

| 項目 | 変更後（PR #865 の CI。いずれも cache なしのコールドビルド） |
| ---- | ------ |
| key | `rust-Linux-test-v5-20260928b940-1e44cdd8…`（rustc 1.99.0 + ルート `Cargo.lock`。`.agents` のサンプルを拾っていた旧 key の hash `68f04b0b…` から、依存不変の本来の hash に戻った） |
| target の展開後サイズ（debuginfo 既定 → `line-tables-only`） | test 2.3G → 2.0G・features 5.7G → 4.5G・harness 1.8G → 1.6G（registry 約 90MB は共通） |
| コールドビルドの所要時間（既定 → `line-tables-only`） | rust-test-ubuntu 550 → 506 秒・rust-features-ubuntu 1201 → 901 秒・harness-ubuntu 113 → 118 秒（cache hit 時は従来それぞれ約 430・700・85 秒） |
| PR の保存 | なし（save step は skipped） |

圧縮後の世代サイズと main での exact hit は main マージ後に cache 一覧で確認する（#865 マージ後の 2026-10-11 時点の使用量は 2.67GB・8 件）。

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
| `ci.yml`（main push） | push | 中央値 22.6 分・最大 30.9 分（6 件） | 3 OS 実行。Windows の `rust-features-windows` が最長（約 31 分。Windows の cache 導入後は約 17 分で、`workflow_dispatch` 1 回の計測。main push の値は未計測） |
| `ai-review.yml` | pull_request | 約 1〜2 分 | |
| `update-external.yml` | schedule・dispatch | 約 2 分 | 日次 |
| `bench-competitor.yml` | schedule・dispatch | 約 2〜3 分 | 週次・3 OS。対象バイナリ未配線のため skipped が多い |
| `release.yml` | workflow_dispatch | 実績少 | 手動のみ |

短縮余地は残るが、`rust-features-ubuntu` を複数ジョブに分割すると required checks の追加（ruleset 更新）が要るため、実施する場合はユーザー判断とする。
