# fandhe-browser

Rust 製の軽量・ミニマムなブラウザの実装リポジトリです。AI エージェント（Claude・Playwright MCP 等）によるブラウザ操作・クラウド上のスクレイピングを、Chromium のサイズ・バージョン組み合わせ問題・トークン過剰消費なしに実現することを目指します。

## 位置づけ

- **本リポジトリは public** です（fandhe-db・rust-ai-library と同一方針）
- **仕様・ビヘイビア定義**: [fandhe-browser-spec](https://github.com/Fandhe-AI/fandhe-browser-spec)（`docs/spec` に submodule 参照。**private リポジトリとして意図的に非公開を維持**する方針であり、アクセス権のない環境からは submodule を解決できません）
- 旧称は `rust-browser` です。spec リポの文書本文には旧称のまま残っている箇所があります

## プロジェクト概要

- **想定用途**: AI エージェントによるブラウザ操作、およびクラウド上での大規模スクレイピング。実用上の互換性（主要サイトでの動作）を重視します
- **主な価値**
  - Chromium 比でのバイナリサイズ・アイドルメモリの大幅削減（コンテナ集約時のコストを抑える）
  - AI エージェントへ渡す DOM 情報のトークン量削減（アクセシビリティツリー・簡約 DOM を第一級 API として提供）
  - プロファイル分離による安全なマルチテナント運用
  - Linux・macOS・Windows 3 OS への一級対応
- **非目標（コア v1）**: フル Chromium 互換・GPU レンダリングパイプラインは v1 のコアでは対象外です。段階的な互換拡張はコアでなくプラグインとして扱う方針です（`04-behavior/README.md` の判定節、`plugin-extension.md` PLUG-7〔TASK-99・MS-9〕、段階 0 は `compat-level.md` COMPAT-1〔TASK-72・MS-6〕・COMPAT-4〔TASK-71/74・MS-6〕）
- **対象外サイト（コア v1）**: anti-bot 機構により機械的にブロックされるサイト（Reddit 等）と、robots.txt で対象パスが Disallow のサイトは対象用途・実測対象から除外します。認証突破・anti-bot 回避は実装しません（`security-policy.md` SEC-1・SEC-3、TASK-85・TASK-70・MS-7。除外サイトの一覧は `docs/design/anti-bot-exclusion.md`）。robots.txt は選定基準であり、実行時には解釈しません（利用者側の責任）

## 到達目標

以下はロードマップ上の**目標値**であり、現時点でこれらを達成していることを意味しません（実装状況は「ステータス」節を参照）。詳細・全体は spec リポの [`06-roadmap.md`](https://github.com/Fandhe-AI/fandhe-browser-spec/blob/main/06-roadmap.md)「成功指標」を参照してください。

- アイドル RSS を Chromium 比 85% 以上削減（`PERF-6`、TASK-27・MS-3）
- AI 向けスナップショットのトークン削減率 70% 以上（`AISNAP-1`、TASK-11・MS-2。2026-10-29 に 85% から見直し・#97）
- 要素参照破損率 10% 以下（`AISNAP-10`、TASK-17・MS-2）
- 全体動作率 70% 以上（`COMPAT-4`、TASK-71/74・MS-6）
- 3 OS 間の動作率差 10 ポイント以内（`XOS-2`、TASK-59・MS-5）
- コンテナイメージサイズ削減率 95% 以上・100 コンテナ集約メモリ 90% 以上削減（`CTR-5`・`CTR-6`、TASK-68/69・MS-5）
- AI 改修タスクの単独完遂実証（`REPAIR-2`、TASK-2・MS-7）・CI ゲートによる意図的破壊的変更の検出（`REPAIR-5`、TASK-6・MS-7）
- Rust 製 MCP 参照プラグインの軽量性（`PLUG-3`）・トークン削減維持（`PLUG-4`、いずれも TASK-94・MS-9）

最終的な目標は、これらの指標を満たした実装を OSS として公開・維持することです。

## ステータス

workspace と crate 骨格（`crates/fandhe-browser-*`）の実装段階です。エンドユーザー向け CLI はまだ提供していません（TASK-41 で追加予定）。個別タスクの進捗は GitHub Issues で管理しています。タスク定義は spec リポの [`05-tasks.md`](https://github.com/Fandhe-AI/fandhe-browser-spec/blob/main/05-tasks.md)（TASK-1〜105）、マイルストーンは [`06-roadmap.md`](https://github.com/Fandhe-AI/fandhe-browser-spec/blob/main/06-roadmap.md)（MS-1〜9）を参照してください。

## 実装方針（要点）

- **二層アーキテクチャ**: 非レンダリング基盤層（HTML パース・DOM・要素操作）を既定とし、レンダリング層は `Servo` を別 workspace・別 `Cargo.lock` の子プロセス（Servo ホスト、`servo-host/`）に隔離し、設定でホストのパスを与えたときだけ起動するオプトインとします。本体バイナリに Servo は含まれません
- **Servo ホストの egress（既知の制約）**: Linux（bwrap）と macOS（sandbox-exec）では、`[rendering]` の `egress_isolation` で Servo ホストを OS 隔離し、通信を本体の許可リスト付きプロキシに限ります。既定の `best-effort` は隔離できなければ警告を出して隔離なしで起動し、`required`（例: `[rendering]` の下に `egress_isolation = "required"`）は隔離できなければ起動しません。Windows は隔離できません（既知の制約）。隔離中は WebSocket が使えず、v1 ではファイルシステムを隔離しません。隔離していないとき（`best-effort` で隔離できなかった場合・Windows）は、ページ内のリダイレクト・サブリソース・DNS リバインディングを URL の検証では防げません（`RENDER-3`・TASK-38）。詳細は [servo-host-egress-isolation.md](./docs/design/servo-host-egress-isolation.md) を参照してください
- **JS エンジン**: V8（`rusty_v8`）を既定とし、トレイト抽象化で `boa` へ切り替え可能にします
- **AI 最適化 API**: アクセシビリティツリー・簡約 DOM を第一級 API として提供し、AI へ渡すトークン量を削減します
- **自動化プロトコル**: CDP 互換レイヤー（既存 Playwright / Puppeteer 資産の流用）と独自 AI 最適化 API を併設します
- **プラグイン境界**: コアを最小に保ち、MCP 連携や互換拡張はプラグインとして段階的に追加します
- **プロファイル分離**: 保管場所の指定によりプロファイルを分離し、データの相互影響を防ぎます
- **対象 OS**: Linux・macOS・Windows の 3 OS に一級対応します（各 OS 上のネイティブ CI ランナーでビルド）
- **AI 自己補修**: AI 自身が保守・改善・機能追加できる設計を初期から制約として織り込みます

詳細なビヘイビア（107 件・16 領域）は spec リポの [`04-behavior/`](https://github.com/Fandhe-AI/fandhe-browser-spec/tree/main/04-behavior) を唯一の正（SSOT）とします。

## crate 構成

| crate | 役割 |
| ----- | ---- |
| `fandhe-browser-core` | fetch（ネットワーク取得）・HTML パース・DOM・query（DOM 探索）・CSSOM・config・可観測性（ログ・トレーシング） |
| `fandhe-browser-js` | JS エンジン抽象トレイト（V8／`rusty_v8` を既定とし `boa` へ切替可能） |
| `fandhe-browser-ai` | AI 最適化 API（アクセシビリティツリー・簡約 DOM）・プラグイン API |
| `fandhe-browser-cdp` | CDP（Chrome DevTools Protocol）互換サーバー。Playwright / Puppeteer 互換 |
| `fandhe-browser-render` | Servo ホストへの IPC クライアント（core の `Renderer` を実装する `HostRenderer`・子プロセス管理・プロトコルコーデック）。Servo 系には依存しない。cli が無条件に依存し、ホストパスが設定されたときだけ有効（`RENDER-1`、TASK-33/34/38・MS-1/MS-4） |
| `fandhe-browser-profile` | プロファイル（ユーザーデータディレクトリ）分離 |
| `fandhe-browser-cli`（予定） | CLI 本体 |
| `fandhe-browser-mcp` | MCP 参照プラグイン（別バイナリ） |

`servo-host/` は上の crate 一覧（本体 workspace）の外にある別 workspace で、バイナリ `fandhe-browser-servo-host` を作ります（ルート `Cargo.toml` の `exclude`）。

## インストール（crates.io）

crates.io への公開後は、次のコマンドで CLI（`fandhe-browser`）を導入できます（#1531。公開は `.github/workflows/release.yml` で、オーナーが承認して実行します）。

```bash
cargo install fandhe-browser-cli --locked
```

- Rust（stable）と C ツールチェーンが要ります（macOS は Xcode Command Line Tools、Linux は `build-essential` 相当）。既定ビルドは V8 の prebuilt 静的ライブラリをダウンロードするため、crates.io と GitHub Releases への接続が要ります
- 公開対象は 8 crate（`fandhe-browser-profile`・`-js`・`-core`・`-ai`・`-cdp`・`-render`・`-cli`・`-mcp`）で、版は共通です。別 workspace の `fandhe-browser-servo-host`（描画ホスト。macOS・Linux 向け）も同じ版で公開します（初回は 0.1.1。公開パイプライン `release.yml` は 9 件を扱う。#1543）

### 描画ホスト（screenshot）

`query`・`navigate`・`launch` は描画ホストなしで動きます。`screenshot` には描画ホスト `fandhe-browser-servo-host` の導入が必要で、無いと `rendering is not available` で失敗します。

- 公開状況: ホストは 0.1.1 から公開する予定で、公開されるまで `cargo install` できません。公開済みの 0.1.0 の CLI に対応するホストはありません
- 対象 OS: macOS・Linux。Windows は対象外です（`cargo install` は ANGLE の `libEGL.dll`・`libGLESv2.dll` を置かず、egress 隔離も未実装で GA の対象外のため。#1524）
- ライセンス: ホストの crate は Servo（MPL-2.0）のソースを含まず `MIT OR Apache-2.0` ですが、ビルドしたバイナリには MPL-2.0 が適用されます（`NOTICE` 参照）

macOS の手順（`<版>` には CLI とホストで同じ値を指定します。版が違うと描画が起動に失敗することがあります）:

```bash
xcode-select --install          # 未導入のとき（clang・make・python3）
brew install cmake pkg-config   # 未導入のとき
export LLVM_OBJDUMP="$(xcrun --find llvm-objdump 2>/dev/null || xcrun --find objdump)"

cargo install fandhe-browser-cli --version <版> --locked
cargo install fandhe-browser-servo-host --version <版> --locked

fandhe-browser-servo-host --selftest   # stdout が selftest ok: で始まれば成功
fandhe-browser screenshot https://example.com example.png
```

- 導入済みの版は `fandhe-browser --version` で確認します（この機能を持たない古い版は `cargo install --list` で確認します）。ずれたときは `cargo install fandhe-browser-servo-host --version <CLI の版> --locked --force` で揃えます
- 両方を `cargo install` すると同じディレクトリに入り、本体がホストを隣から見つけるため環境変数は不要です。`--root` などで別の場所に入れたときは `FANDHE_BROWSER_RENDER_HOST` にホストの絶対パスを渡します
- 出力先に既存ファイルがあると上書きせず失敗します
- Homebrew の llvm を PATH に足さないでください（Apple clang を覆います）
- Servo のフルビルドのため時間とディスクを多く使います（CI の macOS release ビルドで約 24 分）
- Linux の前提（apt・clang・`LIBCLANG_PATH`）と詳細は `servo-host/README.md` を参照してください
- 外部サイトを描画するなら、設定ファイルに `[rendering]` の `egress_isolation = "required"` を書き、絶対パスを `FANDHE_BROWSER_CONFIG` で渡すことを勧めます。既定の `best-effort` は隔離できないと警告だけ出して動きます
- 同梱の Servo は LTS ではない月次版 `=0.7.0` で、セキュリティ修正が戻りません（#1300）。信頼できないページの描画には注意してください

## クイックスタート

以下はソースからビルド・テストする手順です。

### 前提ツール

- [git](https://git-scm.com/)
- [GNU Make](https://www.gnu.org/software/make/)（`make setup`/`make ci` 等のタスク実行に必須です。Linux・macOS には標準で入っています。Windows では [Chocolatey](https://community.chocolatey.org/packages/make)・[Scoop](https://scoop.sh/) 等で導入し、`make` は Git Bash など Windows 側の bash から実行します。WSL 上の rustup・Make は Linux ホスト向けビルドになるため別方式として扱います。Windows の詳細な導入手順は [docs/setup/windows.md](docs/setup/windows.md) を参照してください）
- [rustup](https://rustup.rs/)（`rust-toolchain.toml` が指定する stable ツールチェーン・rustfmt・clippy を自動選択します。導入方法は rustup 公式サイトの手順を参照してください）
- 任意: [Docker](https://www.docker.com/)（`make docker-ci` で環境非依存の検証）。[lefthook](https://github.com/evilmartians/lefthook) 本体は任意（未導入でも `make setup` が実行する `make hooks` が brew または npx（`lefthook@<固定バージョン>`）で自動導入します。brew・npx のいずれも無い環境では `make hooks`／`make setup` がエラーで停止するため、その場合は先に brew か Node.js（npx）を導入してください）

### 手順

```bash
git clone git@github.com:Fandhe-AI/fandhe-browser.git
cd fandhe-browser

# submodule 初期化・rustup 導入確認・lefthook 導入を一括実行
# （docs/spec へのアクセス権が無い環境では警告を出して続行します）
make setup

cargo build --workspace
cargo test --workspace

# fmt・clippy・test・Servo 隔離検査・cargo deny 等の
# ローカルゲート一式（.claude/rules/ci.md と同等）
make ci
```

環境に依存させたくない場合は `make docker-ci` でコンテナ内から同じゲートを実行できます。`make help` でターゲット一覧を確認できます。Makefile は薄い入口で、処理の定義は `scripts/` にあります（Make 非依存の一覧は `scripts/help.sh`。編集方針は `.claude/rules/makefile.md`）。

`docs/spec`（`fandhe-browser-spec`）は private リポジトリのため、アクセス権のない環境では submodule 取得が失敗しますが、実装コードのビルド・テストは `docs/spec` 抜きでも成立するよう維持します。

ビルド成果物（`target/`）は数十 GB に膨らみやすいため、ルート `Cargo.toml` の `[profile.dev.package."*"]` で依存クレート（workspace 外）のデバッグ情報を行番号のみにしています（`dev`・`test` に効き、本リポのクレートは full debuginfo のままです。panic 位置・バックトレースの行番号は残り、デバッガで依存の変数・型を見られなくなるだけです）。溜まった古い成果物は `cargo install cargo-sweep` のうえ `cargo sweep --time 14`（14 日以上使われていない成果物を削除）や `cargo sweep --installed`（rustup に現存しないツールチェーンでビルドした成果物を削除）で消せます。

## ライセンス

MIT OR Apache-2.0 のデュアルライセンスです（[LICENSE-MIT](./LICENSE-MIT) / [LICENSE-APACHE](./LICENSE-APACHE)）。`Servo`（MPL-2.0）は Servo ホスト（`servo-host/`）の配布物にだけ含まれ、本体バイナリと既定・軽量コンテナには含まれません。crates.io のホスト crate は Servo のソースを含まず `MIT OR Apache-2.0` ですが、ビルドしたバイナリには MPL-2.0 が適用されます。適用範囲と帰属表示は `NOTICE`・`LICENSE-THIRD-PARTY` を参照してください。
