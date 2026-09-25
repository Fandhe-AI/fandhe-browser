# 委譲ルール（作成・編集フェーズ）

## 原則

コードの作成・編集は担当レイヤの builder Agent へ委譲し、main は計画・レビュー・統合に徹する。

## パスベース委譲マッピング（実装）

| 対象パス | 委譲先 Agent | model |
| -------- | ------------ | ----- |
| `crates/fandhe-browser-core/`（fetch・HTML パース・DOM・query・CSSOM・config・可観測性） | core-builder | sonnet |
| `crates/fandhe-browser-js/`（JS エンジン抽象トレイト・V8（rusty_v8）・boa） | js-engine-builder | sonnet |
| `crates/fandhe-browser-ai/`・`crates/fandhe-browser-mcp/`（AI 最適化 API・プラグイン API・MCP 参照プラグイン） | ai-api-builder | sonnet |
| `crates/fandhe-browser-cdp/`（CDP 互換サーバー・Playwright/Puppeteer 互換） | cdp-builder | sonnet |
| `crates/fandhe-browser-render/`（Servo 組込・feature gate `rendering`） | render-builder | sonnet |
| `crates/fandhe-browser-profile/`・OS 差異吸収（パス・ロック・正規化） | profile-platform-builder | sonnet |
| `crates/fandhe-browser-cli/`・`.github/workflows/`・`deny.toml`・`NOTICE`・`Dockerfile`・`benches/`・`harness/` | infra-builder | sonnet |
| テスト実行・失敗解析（`cargo test` / `cargo clippy`） | test-runner | sonnet |
| コードレビュー | reviewer | sonnet |
| セキュリティ監査 | security-auditor | sonnet |
| lint・整形の機械的確認 | linter | haiku |
| README・CLAUDE.md・`AGENTS.md`・`docs/design/`・`.claude/`（agents・rules・settings.json）更新 | docs-writer | haiku |

複数 crate に跨る変更は crate ごとに builder を分けて委譲する（独立していれば並列可）。
crate 境界・公開トレイトの設計変更は builder に任せず main（opus / fable）で設計してから委譲する。

## 実装フローの標準形

1. 計画（main。必要に応じて explorer で事前調査）
2. 実装（builder へ委譲）
3. 検証（test-runner → 失敗があれば builder へ差し戻し）
4. レビュー（reviewer / security-auditor）
5. コミット（create-commit スキル。Conventional Commits・`--no-verify` 禁止）

## 着手条件（本リポ固有）

- **実装の着手はユーザーの明示指示を経てから行う**（ロードマップ上の着手判定とは別に、個別の開始指示を待つ）
- spec のタスク定義で担当が「人間」のタスク（実機実測・技術選定・ライセンス判断・段階判定等）には Agent から着手しない。準備作業（計測スクリプト作成等）に留め、判断事項はユーザーへ報告する
- 依存（Cargo.toml の dependencies）の追加・更新は builder に委譲せず、必ずユーザー承認を経る（[dependency-policy](./dependency-policy.md)）
- スコープ外の発見事項は放置せず [out-of-scope-tracking](./out-of-scope-tracking.md) に従い追跡する
