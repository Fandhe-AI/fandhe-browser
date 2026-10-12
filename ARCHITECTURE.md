# Architecture

fandhe-browser の二層アーキテクチャと crate 分割方針の要約。貢献者と AI エージェントが全体像と依存方向を 1 か所で把握するための文書で、spec 対応は `OSS-5` / TASK-79（79.5） / MS-7。

仕様の SSOT は private リポ [fandhe-browser-spec](https://github.com/Fandhe-AI/fandhe-browser-spec)（`docs/spec` submodule）で、本書はその要約。食い違った場合は spec を正とする（spec 本文に残る旧称 `rust-browser` は `fandhe-browser` に読み替える）。spec リポの閲覧にはアクセス権が要る。

## 二層アーキテクチャ

| 層 | 内容 | ビルド上の扱い |
| -- | ---- | -------------- |
| 非レンダリング基盤層 | fetch・HTML パース・DOM・query・CSSOM・JS 実行 | 既定ビルド |
| レンダリング層 | Servo による描画 | feature gate `rendering` 有効時のみ依存グラフに入る（オプトイン） |

- Servo（MPL-2.0）は `fandhe-browser-render` に隔離し、ライセンス境界と crate 境界を一致させる（`RENDER-1`、`REPAIR-1`）
- 詳細は [docs/design/render-feature-gate.md](docs/design/render-feature-gate.md) と [.claude/rules/licensing.md](.claude/rules/licensing.md) を参照する

## crate 構成

`REPAIR-1` に基づき、Cargo workspace を 8 crate に分割する。

| crate | 層 | 役割 |
| ----- | -- | ---- |
| `fandhe-browser-js` | 最下層 | JS エンジン抽象トレイト（V8 既定・`boa` 切替。`JS-1`） |
| `fandhe-browser-profile` | 最下層 | プロファイル（ユーザーデータディレクトリ）分離 |
| `fandhe-browser-core` | 基盤層 | fetch・HTML パース・DOM・query・CSSOM・config・可観測性。共有状態 `AppState` と描画トレイト `Renderer` を持つ |
| `fandhe-browser-render` | 基盤層の上（オプトイン） | Servo 組込。feature `rendering` 配下（`RENDER-1`） |
| `fandhe-browser-ai` | プロトコル層 | AI 最適化 API（アクセシビリティツリー・簡約 DOM）・プラグイン API |
| `fandhe-browser-cdp` | プロトコル層 | CDP 互換サーバー（Playwright / Puppeteer 互換） |
| `fandhe-browser-cli` | 組み立て | CLI 本体。各 crate を組み立ててサーバーを起動する |
| `fandhe-browser-mcp` | 別バイナリ | MCP 参照プラグイン（別プロセス） |

workspace メンバーには上記 8 crate の他にツール crate `harness/wpt_subset_runner`（package 名 `wpt-subset-runner`。`PLUG-10`・TASK-101.2.1）がある。製品の crate 構成には数えない。

## 依存方向

出典は spec `04-behavior/self-repair-design.md`「crate 間の依存方向と `AppState` の配置」節（[`REPAIR-1`](https://github.com/Fandhe-AI/fandhe-browser-spec/blob/main/04-behavior/self-repair-design.md) の実装時の設計制約）。同節は 2026-09-24 付の提案で、オーナーが PR レビューで最終判断する位置づけである。

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

- 依存は上位から下位への一方向に限り、表にない依存はすべて禁止する
- 同じ層の crate 間（ai ⇔ cdp）の依存は禁止する
- js は core からのみ依存される。V8 / boa の具象型は core より上位へ出さない
- render は cli からのみ、feature `rendering` 有効時に依存される
- mcp は workspace 内 crate に依存しない
- 依存を追加する場合は下位方向に限り、先に spec 側の節を更新する。レビューでは [AGENTS.md](AGENTS.md)「crate 間の許可依存」で確認する（P0）

## 主要な設計判断

- **`AppState` は core に置く**: CDP と AI API が同一インスタンスを共有する（`AISNAP-6`）。ai の依存グラフに cdp が入らないため、AI API は単体で動く（`AISNAP-7`）。実体は `crates/fandhe-browser-core/src/state.rs`
- **プロトコル固有の状態は各 crate に閉じる**: プラグインレジストリは ai（`PLUG-2`）、CDP セッション・ターゲット表は cdp（`CDP-1`）
- **サーバーの組み立ては cli**: ai・cdp はルータを公開するだけでリスナーを持たない。単一の `Arc<AppState>` を渡して同一リスナーに合成する（`crates/fandhe-browser-cli/src/server.rs`）
- **描画は core の `Renderer` トレイト越し**: cdp・ai は render に直接依存しない（`RENDER-1`、`crates/fandhe-browser-core/src/render.rs`）
- **JS エンジンはトレイト抽象**: 同梱は feature `js-v8` / `js-boa`（cli → core → js へ転送）、選択は core の設定（`JS-1`）
- **mcp は別プロセス**: HTTP（`/ai/*`）・stdio・JSON Schema（[docs/design/host-api.schema.json](docs/design/host-api.schema.json)）の契約のみで接続する（`PLUG-1`・`PLUG-3`）

判断理由は、共有型を下位 crate に置いて循環を避けること、1 回の改修が波及する crate を最小に保つこと（`REPAIR-1`・`REPAIR-2`）。代替案の比較は spec を参照する。

## 現在の実装との差分

`REPAIR-3`（実装済みを装わない）に基づく記録。進捗の詳細は GitHub Issues で管理する。

- `fandhe-browser-render` は雛形で、core への依存も `Renderer` 実装も未結線（TASK-38）。cli は feature `rendering` を付けても常に無効レンダラーを使う
- ai・cdp の profile 依存は現状 dev-dependencies のみ（設計上は通常依存も許可）

## 境界の検証

- `make check-render-isolation`（`scripts/check-render-isolation.sh`・`RENDER-1`）: 既定ビルドに Servo が含まれないことを検査する
- `make check-js-engine-isolation`（`scripts/check-js-engine-isolation.sh`・`JS-1`）: エンジン構成別の依存グラフを検査する
- `make check-static-link`（`scripts/check-static-link.sh`・`PLUG-1`・`CTR-4`）: musl 配布バイナリ（既定・軽量）に動的リンクが無いことを検査する（Linux のみ）
- 依存方向の表全体を検査するスクリプトは無く、レビュー観点（AGENTS.md・P0）で確認する
- 現在の依存辺は `cargo tree -p <crate> -e normal` または `cargo metadata --no-deps` で確認できる

## 関連ドキュメント

- [README.md](README.md): 実装方針の要点・crate 構成
- [AGENTS.md](AGENTS.md): ビルド・テスト・レビュー観点
- [CONTRIBUTING.md](CONTRIBUTING.md): 貢献手順
- [.claude/rules/coding-rust.md](.claude/rules/coding-rust.md): crate 境界を含む Rust 規約
- [docs/design/plugin-tiering.md](docs/design/plugin-tiering.md): プラグインの公式サポート区分
