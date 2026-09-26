# AGENTS.md

## 本書の用途

本書は `.github/workflows/ai-review.yml`（Fandhe-AI/actions の reusable workflow を呼ぶ wrapper）が Codex による PR 自動レビューの基準として読む、リポジトリ固有のレビュー観点集である。

- Codex の既定 prompt は**PR の base コミットの本書**を読む。そのため本書への変更は、当該 PR のレビューには反映されず、**マージ後の次の PR から実効**になる
- 本書は日本語で記述する。プログラムの出力文字列（エラーメッセージ・ログ・CLI 出力）や識別子・コマンドは原語（英語）のままでよい
- `docs/spec`（`fandhe-browser-spec` submodule）は private であり、レビュー実行環境からは読めない前提とする。レビューでは spec 本文との一致を判定材料にせず、**ビヘイビア ID（`<PREFIX>-<N>`）・TASK-n・MS-n の併記があるか**という、diff だけで確認できる観点に限定する。ID が欠けた spec 由来の変更は「spec 参照規約」観点（P1）として指摘する

## 優先度定義

| 優先度 | 意味 | 判断基準 | 扱い |
| ------ | ---- | -------- | ---- |
| P0 | マージブロック | セキュリティ・ライセンス・アーキテクチャ根幹の違反、または回帰検出の後退（テストの skip・ignore 等）・実装済みを装う偽装など品質ゲートの破壊 | 修正するまでマージしない |
| P1 | 強く推奨 | 規約違反・保守性の重大な低下 | 未対応のままマージ不可（ai-review の codex ジョブが fail する）。対応不要の場合はスレッドで理由を明示して resolve する |
| P2 | 提案 | 改善提案 | CI を fail させない。次回以降の改善提案として記録すればよい |

## ビルド・テスト・回帰確認コマンド（REPAIR-7・TASK-8）

workspace（`Cargo.toml`）が未作成の間は下記コマンドは対象がなく実行できない。**workspace 作成後の PR からは、PR 本文に実行結果が記載されているか（同じ PR で CI 設定を変更する場合はその diff にこれらのコマンドが含まれているか）を確認する。** 本節の未達は、個別に優先度を明記した項目を除き既定で P1 とする。

### ローカルゲート（コミット・PR 前に必ず通す。REPAIR-5・REPAIR-6）

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- clippy 警告は 0 件を維持する。理由コメントなしで `#[allow(...)]` により警告を握りつぶす差分は P1。crate・モジュール全体に及ぶ広範な `#[allow(...)]`（`#![allow(...)]` 等）や、`unsafe` 関連 lint（`unsafe_code`・`clippy::undocumented_unsafe_blocks` 等）・外部入力の検証を隠す lint（`clippy::unwrap_used`・`clippy::expect_used`・`clippy::indexing_slicing` 等。「外部入力の検証」観点）の外部入力経路での抑止は、理由コメントの有無を問わず P0
- テストの skip・ignore・アサーション弱体化で CI を通す差分は P0（回帰検出の後退を招くため）

### feature 分離の検証（RENDER-1）

`fandhe-browser-render`（Servo・MPL-2.0）の feature gate `rendering` に触れる変更では、既定ビルドと `--features rendering` の両方で確認する。PR 本文に次のコマンドの実行結果が記載されているか（同じ PR で CI 設定を変更する場合はその diff に含まれているか）を確認する。

```bash
make lint-rendering   # cargo clippy --workspace --all-targets --features rendering -- -D warnings
make test-rendering   # cargo test --workspace --features rendering
make check-render-isolation   # cargo tree --workspace -e normal,build,dev --exclude fandhe-browser-render で
                               # 既定ビルド（feature なし）の依存グラフに Servo 系クレートが含まれないことを確認
```

### ライセンス検査

```bash
cargo deny check licenses
make check-deny-license-reject   # 許可外ライセンス（GPL/AGPL/LGPL/MPL-2.0・ライセンス欄なし）が
                                   # reject されることの negative test（TASK-9.1・REPAIR-8）
```

### 3 OS 一級対応

- CI は Linux・macOS・Windows の 3 OS ネイティブランナーでビルド・テストする（クロスコンパイル前提にしない）
- OS 固有のパス・大文字小文字非区別ファイルシステム・改行コードに関わる変更は、PR 本文に 3 OS での実行結果が記載されているか（同じ PR で CI 設定を変更する場合はその diff に 3 OS matrix でのテスト実行が含まれているか）を確認する

## レビュー観点

### セキュリティ（P0 中心。`.claude/rules/security.md`・`.claude/rules/dependency-policy.md`・`.claude/rules/licensing.md`）

| 観点 | 確認内容 | 優先度 |
| ---- | -------- | ------ |
| 秘密情報 | 実トークン・API キー・Cookie・接続資格情報・実プロファイルデータがコード・テスト・fixture・ドキュメントに含まれていないか | P0 |
| 偽装・回避機能 | UA 文字列・フィンガープリントの偽装、anti-bot 回避を目的とする機能が追加されていないか（SEC 系ビヘイビア） | P0 |
| フォールバックの検出回避性 | 未実装の CDP メソッド等で「成功を一律に返す」フォールバックが、検出回避として作用しないか | P0 |
| プロファイル境界 | プロファイル保管場所の外へ書き込める経路（パストラバーサル・シンボリックリンク経由）がないか、パス要素を検証・正規化してからルート配下であることを確認しているか（PROF 系ビヘイビア） | P0 |
| プロファイル分離 | プロファイル間で Cookie・ストレージ・キャッシュを共有・漏えいさせていないか | P0 |
| プラグイン境界 | 動的ライブラリの実行時ロードを行っていないか（実行時拡張はプロセス分離に限る）。プラグインからの入力を untrusted として検証しているか（PLUG 系ビヘイビア） | P0 |
| 外部入力の検証 | ネットワーク取得した HTML/JS・CDP/API リクエスト・プラグイン入出力の経路で `unwrap`・`expect`・添字アクセス（`[]`）を使わず、`get()`・`try_into()`・checked 演算で処理しているか | P0 |
| リソース上限 | 長さ・件数を上限検証してからアロケーションに使っているか（巨大 HTML・無限リダイレクト・JS 無限ループのタイムアウト欠如による DoS を防ぐ） | P0 |
| `unsafe`/FFI | `unsafe` の新規追加はユーザー承認済みか（PR 本文に承認の記録（承認した Issue・コメントへのリンクと承認内容の転記等）があるかで確認）。`unsafe` ブロックに `// SAFETY:` コメント（理由・維持すべき不変条件）があるか | P0 |
| SSRF | 取得先 URL の scheme（`file:` 等）・内部アドレスへの無検証アクセスがないか | P0 |
| CDP/API サーバーの公開範囲 | CDP 互換サーバー・独自 AI API サーバーを既定で外部インターフェースへ認証なし公開する変更になっていないか | P0 |
| インジェクション | CDP・AI API の引数をシェル・パス・JS 評価文字列へ未検証で連結していないか | P0 |
| 依存の追加・更新 | `Cargo.toml` の依存が `=x.y.z` の完全固定（exact pin）か。ユーザー承認（クレート名・バージョン・目的・ライセンス・メンテ状況・推移的依存・バイナリサイズ影響の提示）を経ているか（PR 本文に承認の記録（承認した Issue・コメントへのリンクと承認内容の転記等）があるかで確認） | P0 |
| 依存ライセンス | GPL/LGPL/AGPL 系の依存を導入していないか。MPL-2.0（Servo）は `fandhe-browser-render` 配下に限定されているか。既存ライセンスに新規許可を追加する判断はユーザーへ委ねられているか | P0 |

### アーキテクチャ・設計整合（`.claude/rules/coding-rust.md`・`CLAUDE.md`）

| 観点 | 確認内容 | 優先度 |
| ---- | -------- | ------ |
| crate 境界 | 変更が `crates/fandhe-browser-{core,js,ai,cdp,render,profile,cli,mcp}` の想定責務（core: fetch・HTML パース・DOM・query・CSSOM・config・可観測性／js: エンジン抽象・V8・boa／ai: AI 最適化 API・プラグイン API／cdp: CDP 互換／render: Servo／profile: プロファイル分離／cli: 組み立て・CLI／mcp: 参照プラグイン）に収まっているか（REPAIR-1） | P1 |
| 依存方向 | 下記「crate 間の許可依存」節に記載した依存方向に沿っているか・循環がないか・共有型（`AppState` 等）が下位 crate（core）にあるか | P0 |
| レンダリング層の隔離 | `fandhe-browser-render`（Servo）が feature gate `rendering` 配下に隔離され、既定ビルドの依存グラフへ混入していないか（RENDER-1） | P0 |
| JS エンジン抽象 | V8（`rusty_v8`）・`boa` の具象型が `fandhe-browser-js` の外（上位 crate）へ漏れていないか。トレイト抽象越しに利用しているか（JS-1） | P1 |
| 3 OS 対応 | パスを `PathBuf`/`Path::join` で組み立てているか（文字列連結・区切り文字のハードコードがないか）。Windows の長パス・大文字小文字非区別ファイルシステムを考慮しているか。内部データファイルの改行が LF 固定か。OS 固有処理が `cfg(target_os = ...)` で局所化されているか | P1 |
| `docs/spec` 非依存ビルド | コード・`build.rs`・テストが `docs/spec` 配下を読み込んでいないか（`docs/spec` 抜きでビルド・テストが成立するか） | P0 |
| spec 参照規約 | spec の内容を引用・要約する箇所にビヘイビア ID・TASK-n・MS-n が併記されているか（spec ファイルの丸ごとコピーになっていないか）。`docs/spec` 配下自体を本リポ側で編集していないか | P1 |
| エラーハンドリング | ライブラリコードが `Result` を返し panic させていないか（release は `panic = "abort"` 前提） | P0 |

#### crate 間の許可依存

spec `docs/spec/04-behavior/self-repair-design.md`「crate 間の依存方向と `AppState` の配置」節の転記（REPAIR-1 の crate 分割を実装する際の設計制約。2026-09-24 提案・オーナーが PR レビューで最終判断。TASK-1・TASK-19・TASK-33・TASK-38・TASK-41・TASK-47・TASK-92・TASK-94 が参照）。

| crate | 依存してよい workspace 内 crate |
| ----- | ------------------------------ |
| `fandhe-browser-js` | なし |
| `fandhe-browser-profile` | なし |
| `fandhe-browser-core` | js・profile |
| `fandhe-browser-render` | core |
| `fandhe-browser-ai` | core・profile |
| `fandhe-browser-cdp` | core・profile |
| `fandhe-browser-cli` | core・profile・ai・cdp・render（feature `rendering` 有効時のみ） |
| `fandhe-browser-mcp` | なし（HTTP・stdio・JSON Schema の契約のみ） |

- 同じ層の crate 間（ai ⇔ cdp）の依存は禁止する
- `fandhe-browser-js` は core からのみ依存される。cli・ai・cdp・render・mcp は直接依存しない（エンジン feature `js-v8`/`js-boa` は cli → core → js の順に転送し、選択は core の設定で行う）
- `fandhe-browser-render` は cli からのみ、feature `rendering` 有効時に依存される。ai・cdp は core が定義する描画トレイト経由で呼び出し、render に直接依存しない
- 共有型（`AppState` 等）は下位 crate（core）に置く
- 上表に記載のない依存はすべて禁止する（依存は上位から下位への一方向に限る）。追加する場合は下位方向への追加に限り、先に spec 側「crate 間の依存方向と `AppState` の配置」節の更新を PR 説明で示していることを確認する

### 再利用・アセット化（AI 自己補修性。REPAIR 系ビヘイビア）

| 観点 | 確認内容 | 優先度 |
| ---- | -------- | ------ |
| 単一責務・疎結合 | 1 回の改修が波及する crate・モジュールが最小に保たれているか（REPAIR-1・REPAIR-2） | P1 |
| 構造化された戻り値 | 公開 API の戻り値が将来拡張できる構造を持つ型か（真偽値・フラットな文字列で済ませていないか。REPAIR-4） | P1 |
| スタブの明示 | 未実装・簡易実装箇所が「実装済みを装って」いないか。ドキュメントコメントに将来仕様と対応するビヘイビア ID が明記されているか（REPAIR-3） | P0 |
| コメント規約 | crate・モジュールの入口に `//!`、公開 API に `///` で役割要約があるか。呼び出し元・呼び出し先の文脈、他 crate との契約（公開トレイト・エラー型・前提条件・スレッド安全性）が書かれているか。逐語説明や spec 本文の長い引用になっていないか | P2 |
| テストとビヘイビア ID の対応 | 挙動がビヘイビア ID（例: `CORE-1`）に対応づけてテストされ、テスト名またはドキュメントコメントに ID が記されているか。ユニットテストと結合テストが併置され、期待値が具体値で書かれているか | P1 |
| スコープ外事項の追跡 | 実装・レビュー中に見つかったスコープ外の事項が、当該 PR に混入せず Issue 追跡へ切り出されているか（`.claude/rules/out-of-scope-tracking.md`。スコープ外混入は `.claude/rules/conventional-commits.md` の禁止事項） | P1 |

### 規約（表記・コミット）

| 観点 | 確認内容 | 優先度 |
| ---- | -------- | ------ |
| 日本語規約 | コード内コメント・ドキュメントが日本語で書かれているか。エラーメッセージ・ログ・CLI 出力・API レスポンス等プログラムの出力文字列が英語になっているか（`.claude/rules/japanese-style.md`） | P2 |
| Conventional Commits | PR タイトル（および PR 本文に見えるコミットメッセージ）が Conventional Commits 形式か。type/scope が英語、説明文が日本語になっているか（`.claude/rules/conventional-commits.md`） | P2 |

### CI・ワークフロー

| 観点 | 確認内容 | 優先度 |
| ---- | -------- | ------ |
| 第三者 action の固定 | サードパーティ action はコミット SHA で固定されているか | P0 |
| Fandhe-AI/actions の例外 | `Fandhe-AI/actions` は組織内（first-party）の上流リポジトリであり、上記「第三者 action の固定」の対象ではない。reusable workflow への参照は組織方針（2026-08-18 オーナー判断）により可変タグ `@latest` の使用が認められている。`@latest` への統一・SHA pin の除去を指摘しない | 指摘しない（例外） |
| runner 方針 | public リポジトリのため既定は GitHub ホステッドランナー。self-hosted の使用が許可されるのは `ai-review.yml` の `codex`/`review` ジョブのみ（組織承認済み例外）。`preflight`/`post_feedback` を含む他ジョブ・他 workflow は GitHub ホステッドランナーになっているか（補足: 許可範囲の詳細は Fandhe-AI/actions `ai-review/docs/runner-exception.md` 参照。Codex から読めない場合がある） | P0 |
| permissions | ワークフロー・ジョブの `permissions` が最小権限で明示されているか | P0 |
| secrets の扱い | secrets が `pull_request` イベントのログへ出力されていないか | P0 |
| CI パイプライン本体（`ci.yml`・`release.yml`） | `ci.yml` は TASK-55.2（Issue #59）で `push`（main）/`pull_request` トリガーを有効化済み（`workflow_dispatch` は手動再実行用に併存）。`release.yml` は crates.io 公開用の意図的設計として引き続き `workflow_dispatch` 限定。`ci.yml` への変更では、3 OS matrix（Linux・macOS・Windows）を備えているか、本リポに存在しない `make` ターゲット・`scripts/` を前提にした（vector-db 由来のような）ジョブが混入していないかを確認する | P1 |
| 依存監査パイプライン | `cargo-deny`（advisories/bans/licenses/sources）は 3 OS の `rust-ci` ジョブへ導入済み（TASK-77・#64）。許可外ライセンスを reject する fail-closed 性の negative test（`make check-deny-license-reject`・`deny-license-reject` ジョブ）も導入済み（TASK-9.1・#338・REPAIR-8） | P2 |
| 対象サイト群の回帰チェック | TASK-9.2（Issue #339・REPAIR-8）で `compat-regression` ジョブ導入済み（`harness/compat-regression/check-matrix.sh`。閾値 70%・COMPAT-1/COMPAT-4）。実マトリクス `harness/compat-practical/results/matrix.json`（TASK-71.3・Issue #312 が生成予定）が未導入のため `--allow-missing` 運用中（ファイル不在時は `::warning::` を出して exit 0）。`ci.yml`・`Makefile` の変更では `--allow-missing` の削除条件（#312 完了）が守られているか確認する | P2 |
