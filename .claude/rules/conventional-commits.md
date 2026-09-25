# Conventional Commits 規約

## 形式

```
<type>(<scope>): <日本語の説明>

<本文（任意・日本語）>
```

commit-msg フック（lefthook の正規表現検査）と `make lint-commits`（commitlint・`commitlint.config.mjs`）で検証する。

## type

| type | 用途 |
| ---- | ---- |
| feat | 機能追加 |
| fix | バグ修正 |
| refactor | 挙動を変えないコード整理 |
| perf | 性能改善 |
| test | テストの追加・修正 |
| docs | ドキュメントのみの変更 |
| ci | CI 設定の変更 |
| build | ビルド・依存関係の変更 |
| chore | 上記以外の雑務 |

## scope

| scope | 対象 |
| ----- | ---- |
| core | `crates/fandhe-browser-core` |
| js | `crates/fandhe-browser-js` |
| ai | `crates/fandhe-browser-ai` |
| cdp | `crates/fandhe-browser-cdp` |
| render | `crates/fandhe-browser-render` |
| profile | `crates/fandhe-browser-profile` |
| cli | `crates/fandhe-browser-cli` |
| mcp | `crates/fandhe-browser-mcp` |
| deps | 依存の追加・更新（`Cargo.toml`・`Cargo.lock`） |
| spec | `docs/spec` submodule 参照の更新 |
| claude | `CLAUDE.md`・`.claude/`（agents・rules・settings） |
| skills | `.claude/skills`・`.agents/skills`・`skills-lock.json` |

複数 crate に跨る場合は scope を省略するか、主たる対象を選ぶ。

## breaking change

- 破壊的変更は `!` を付け（例: `feat(ai)!: ...`）、本文に `BREAKING CHANGE:` を記載する

## 禁止事項

- `git commit --no-verify` の使用（pre-commit / commit-msg フックを必ず通す）
- 複数の関心事を 1 コミットに混在させること（type が 2 つ以上必要なら分割する）
- スコープ外の変更の混入（[out-of-scope-tracking](./out-of-scope-tracking.md)）
