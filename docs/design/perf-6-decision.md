# PERF-6 の採否判断記録

**判定日**: 2026-09-23  
**判定者**: プロジェクトオーナー  
**対象**: `04-behavior/perf-targets.md` PERF-6

## 決定事項

PERF-6（アイドル RSS の Chromium 比 85% 以上削減）を採択し、旧 PERF-2（Chromium 比 70% 以上削減）を置き換えることを決定した。PERF-2 は欠番とする。

## 根拠

### 実測値

- **PoC-9**: fandhe-browser のアイドル RSS 削減率は 92.3%（既達）
- **PoC-13**: 競合最有力（Lightpanda）のアイドル RSS 削減率は 78.2% 〜 88.4%

### 差別化戦略

旧目標の 70% は、競合との差別化の指標としては弱い。目標を 85% 以上に引き上げ、軽量性を差別化の軸として明確にする。PoC-9 の実測値（92.3%）はこの目標を満たしている。

## spec への反映状況

本決定は `04-behavior/perf-targets.md` に反映済み。PERF-2 は欠番とされ、PERF-6 が REQ-19 の受け入れ基準 2 を引き継ぐ。

## 関連ビヘイビア・タスク

- **対応ビヘイビア**: PERF-6
- **対応タスク**: TASK-83（本判定記録の作成）
- **マイルストーン**: MS-1
- **参考**: `04-behavior/perf-targets.md`「旧要件との対応」表の「REQ-19 の受け入れ基準 2」

## 他タスクへの影響

本決定により、以下のタスクの目標値が確定する。

- TASK-27（バイナリサイズ・アイドル RSS・cold start の Chromium 比測定）: 目標値 85% 以上で測定
- TASK-31（V8 統合後バイナリサイズ増分の実測）: PERF-1・PERF-6 の目標と照合
- TASK-81（50 インスタンス同時起動時の集約メモリ実測）: PERF-4 測定基盤として連携
- TASK-82（レンダリング有効時のメモリ増分・レンダリング時間の数値化）: PERF-5・PERF-6 と並行実測

PERF-6 は単独で測定条件を完結させ、他の ID に依存しない。

## 参照

SSOT: [fandhe-browser-spec](https://github.com/Fandhe-AI/fandhe-browser-spec) `04-behavior/perf-targets.md`（本リポでは submodule の `docs/spec/04-behavior/perf-targets.md`）
