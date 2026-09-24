# fandhe-browser

Rust 製の軽量・ミニマムなブラウザの実装リポジトリです。AI エージェント（Claude・Playwright MCP 等）によるブラウザ操作・クラウド上のスクレイピングを、Chromium のサイズ・バージョン組み合わせ問題・トークン過剰消費なしに実現することを目指します。

## 位置づけ

- **本リポジトリは public** です（vector-db・rust-ai-library と同一方針）
- **仕様・ビヘイビア定義**: [fandhe-browser-spec](https://github.com/Fandhe-AI/fandhe-browser-spec)（`docs/spec` に submodule 参照。**private リポジトリとして意図的に非公開を維持**する方針であり、アクセス権のない環境からは submodule を解決できません）
- 旧称は `rust-browser` です。spec リポの文書本文には旧称のまま残っている箇所があります

## ステータス

実装は未着手です（ロードマップの着手判定は Go 済み。実装開始は別途の指示を経て行います）。タスク定義は spec リポの [`05-tasks.md`](https://github.com/Fandhe-AI/fandhe-browser-spec/blob/main/05-tasks.md)（TASK-1〜105・約 175 人日）、マイルストーンは [`06-roadmap.md`](https://github.com/Fandhe-AI/fandhe-browser-spec/blob/main/06-roadmap.md)（MS-1〜9）を参照してください。

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

## 開発環境構築

```bash
git clone git@github.com:Fandhe-AI/fandhe-browser.git
cd fandhe-browser
git submodule update --init   # docs/spec（private・要アクセス権）
```

`docs/spec`（`fandhe-browser-spec`）は private リポジトリのため、アクセス権のない環境では submodule 取得が失敗します。実装コードのビルド・テストは `docs/spec` 抜きでも成立するよう維持します。

## ライセンス

MIT OR Apache-2.0 のデュアルライセンスです（[LICENSE-MIT](./LICENSE-MIT) / [LICENSE-APACHE](./LICENSE-APACHE)）。レンダリング層で組み込む `Servo` は MPL-2.0 です。
