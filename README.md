# fandhe-browser

Rust 製の軽量・ミニマムなブラウザの実装リポジトリです。AI エージェント（Claude・Playwright MCP 等）によるブラウザ操作・クラウド上のスクレイピングを、Chromium のサイズ・バージョン組み合わせ問題・トークン過剰消費なしに実現することを目指します。

## 位置づけ

- **本リポジトリは public** です（vector-db・rust-ai-library と同一方針）
- **仕様・ビヘイビア定義**: [fandhe-browser-spec](https://github.com/Fandhe-AI/fandhe-browser-spec)（`docs/spec` に submodule 参照。**private リポジトリとして意図的に非公開を維持**する方針であり、アクセス権のない環境からは submodule を解決できません）
- 旧称は `rust-browser` です。spec リポの文書本文には旧称のまま残っている箇所があります

## プロジェクト概要

- **想定用途**: AI エージェントによるブラウザ操作、およびクラウド上での大規模スクレイピング。実用上の互換性（主要サイトでの動作）を重視します
- **主な価値**
  - Chromium 比でのバイナリサイズ・アイドルメモリの大幅削減（コンテナ集約時のコストを抑える）
  - AI エージェントへ渡す DOM 情報のトークン量削減（アクセシビリティツリー・簡約 DOM を第一級 API として提供）
  - プロファイル分離による安全なマルチテナント運用
  - Linux・macOS・Windows 3 OS への一級対応
- **非目標（コア v1）**: フル Chromium 互換・GPU レンダリングパイプラインは v1 のコアでは対象外です。段階的な互換拡張はコアでなくプラグインとして扱う方針です（`04-behavior/README.md` の判定節、`plugin-extension.md` PLUG-7、段階 0 は `compat-level.md` COMPAT-1・COMPAT-4）

## 到達目標

以下はロードマップ上の**目標値**であり、現時点でこれらを達成していることを意味しません（実装状況は「ステータス」節を参照）。詳細・全体は spec リポの [`06-roadmap.md`](https://github.com/Fandhe-AI/fandhe-browser-spec/blob/main/06-roadmap.md)「成功指標」を参照してください。

- アイドル RSS を Chromium 比 85% 以上削減（`PERF-6`）
- AI 向けスナップショットのトークン削減率 85% 以上（`AISNAP-1`）
- 要素参照破損率 10% 以下（`AISNAP-10`）
- 全体動作率 70% 以上（`COMPAT-4`）
- 3 OS 間の動作率差 10 ポイント以内（`XOS-2`）
- コンテナイメージサイズ削減率 95% 以上・100 コンテナ集約メモリ 90% 以上削減（`CTR-5`・`CTR-6`）
- AI 改修タスクの単独完遂実証（`REPAIR-2`）・CI ゲートによる意図的破壊的変更の検出（`REPAIR-5`）
- Rust 製 MCP 参照プラグインの軽量性（`PLUG-3`）・トークン削減維持（`PLUG-4`）

最終的な目標は、これらの指標を満たした実装を OSS として公開・維持することです。

## ステータス

workspace と crate 骨格（`crates/fandhe-browser-*`）の実装段階です。エンドユーザー向け CLI はまだ提供していません（TASK-41 で追加予定）。個別タスクの進捗は GitHub Issues で管理しています。タスク定義は spec リポの [`05-tasks.md`](https://github.com/Fandhe-AI/fandhe-browser-spec/blob/main/05-tasks.md)（TASK-1〜105）、マイルストーンは [`06-roadmap.md`](https://github.com/Fandhe-AI/fandhe-browser-spec/blob/main/06-roadmap.md)（MS-1〜9）を参照してください。

## 実装方針（要点）

- **二層アーキテクチャ**: 非レンダリング基盤層（HTML パース・DOM・要素操作）を既定とし、レンダリング層（`Servo` 組込）は feature gate 配下のオプトインとして隔離します
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
| `fandhe-browser-render` | Servo 組込。feature gate `rendering` 配下のオプトインとし、MPL-2.0 を本 crate 内に隔離（`RENDER-1`） |
| `fandhe-browser-profile` | プロファイル（ユーザーデータディレクトリ）分離 |
| `fandhe-browser-cli`（予定） | CLI 本体 |
| `fandhe-browser-mcp`（予定） | MCP 参照プラグイン（別バイナリ） |

## クイックスタート

現時点ではエンドユーザー向け CLI は未提供です（`fandhe-browser-cli` は TASK-41 で追加予定）。以下はソースからビルド・テストする手順です。

### 前提ツール

- [git](https://git-scm.com/)
- [rustup](https://rustup.rs/)（`rust-toolchain.toml` が指定する stable ツールチェーン・rustfmt・clippy を自動選択します。導入方法は rustup 公式サイトの手順を参照してください）
- 任意: [lefthook](https://github.com/evilmartians/lefthook)（`make hooks` で git hooks を導入）、[Docker](https://www.docker.com/)（`make docker-ci` で環境非依存の検証）

### 手順

```bash
git clone git@github.com:Fandhe-AI/fandhe-browser.git
cd fandhe-browser

# submodule 初期化・rustup 導入確認・lefthook 導入を一括実行
# （docs/spec へのアクセス権が無い環境では警告を出して続行します）
make setup

cargo build --workspace
cargo test --workspace

# fmt・clippy・test・--features rendering・Servo 隔離検査・cargo deny 等の
# ローカルゲート一式（.claude/rules/ci.md と同等）
make ci
```

環境に依存させたくない場合は `make docker-ci` でコンテナ内から同じゲートを実行できます。`make help` でターゲット一覧を確認できます。

`docs/spec`（`fandhe-browser-spec`）は private リポジトリのため、アクセス権のない環境では submodule 取得が失敗しますが、実装コードのビルド・テストは `docs/spec` 抜きでも成立するよう維持します。

## ライセンス

MIT OR Apache-2.0 のデュアルライセンスです（[LICENSE-MIT](./LICENSE-MIT) / [LICENSE-APACHE](./LICENSE-APACHE)）。レンダリング層で組み込む `Servo` は MPL-2.0 です。
