# 可観測性データの出力方針

**判定日**: 2026-10-10  
**判定者**: プロジェクトオーナー（Issue #218）  
**対象**: `04-behavior/self-repair-design.md` REPAIR-9（TASK-10 の判断事項 10.h1）

## 位置づけ

TASK-10（MS-4）の「出力形式・保持期間・収集基盤」に関するオーナー判断を設計文書化したもの。判断の正は Issue #218 のコメントであり、本書は判断内容と、実装に必要な作業の概要を整理する。実装タスクは TASK-10.3 以降（Issue #221 ほか）で扱う。

REPAIR-9（Must）は、各モジュールの操作結果を操作種別・成功/失敗・レイテンシ・タイムスタンプを含む構造化ログまたはメトリクス形式で出力することを求める。TASK-95（MS-9）は本タスクの出力形式を前提とする。

## 決定事項

| 項目 | 決定 |
| ---- | ---- |
| 出力形式 | 構造化ログ（JSON Lines）のみ。集計はプロセス内の既存の有界レコーダー、または消費側で行う |
| 出力先 | 既定は無効。config または CLI フラグで有効化し、有効時は stderr へ出す。ファイル出力は任意で、プロファイル保管場所の外へは書かない |
| 保持期間 | ホスト側では保持しない。ファイル出力時のみサイズ上限と世代数上限で打ち切る |
| 外部収集基盤 | MVP では採用しない。トレイトで差し替え可能な将来拡張として記録する |

### 理由

- 既存の `crates/fandhe-browser-core/src/observability.rs`（記録型・JSON Lines エンコーダ）をそのまま使える
- 依存の追加が不要（[dependency-policy](../../.claude/rules/dependency-policy.md)）
- 軽量化の目標（CORE-2）と整合する

## 既存実装の現状

`observability.rs` は以下を提供済みで、`fandhe-browser-core` 内の計装まで完了している。

| 要素 | 内容 |
| ---- | ---- |
| `OperationRecord` | 操作種別・結果・レイテンシ・タイムスタンプの共通レコード。`#[non_exhaustive]`・private フィールド |
| `OperationKind` / `FailureKind` / `OperationOutcome` | 固定 snake_case ASCII 文字列（`as_str`）を持つ列挙。`Error` の message・URL・アドレスはレコードへ持ち込まない |
| `OperationRecord::to_json_line` | JSON 1 行へのエンコーダ（手書き。フィールド順固定: `operation` → `outcome` → 失敗時のみ `failure_kind` → `latency_us` → `timestamp_unix_ms`）。自由形式文字列を含まないためエスケープ不要 |
| `OperationRecorder`（トレイト） | レコードの受け口。実装は panic せず、長時間ブロックせず、失敗を操作へ波及させない |
| `RecorderHandle` | recorder の共有ハンドル。既定は無効（何も記録しない） |
| `InMemoryRecorder` | 件数上限付きのメモリ内集計器（`dropped` 計数・種別別の成功/失敗件数）。テスト・簡易集計用 |

出力例（失敗時）:

```json
{"operation":"fetch","outcome":"failure","failure_kind":"timeout","latency_us":1500,"timestamp_unix_ms":1700000000000}
```

### 計装済みの範囲

- `fetch`・`parse`・`dom`・`query`・`js_stub` が計装済み（TASK-10.2.1・TASK-10.2.2）
- recorder は `FetchOptions::with_recorder`・`ParseOptions::with_recorder`・`Document::set_recorder` 等で呼び出し側が明示注入する。未設定なら一切記録しない（process-global は並列テストで記録が混ざるため採らない）

### 未配線の範囲

- `fandhe-browser-core` の外から recorder を設定する箇所は存在しない（調査時点で cli・cdp・ai・profile に参照なし。`fandhe-browser-js` のベンチに言及があるのみ）
- stderr・ファイルへ書き出す `OperationRecorder` 実装は存在しない
- config キー・CLI フラグは存在しない
- ファイル出力のサイズ上限・世代数上限・パス検証は存在しない

## 今後の実装に必要な作業

以下の名前・既定値は判断コメントに無い具体値であり、**すべて案**（要判断。オーナー確認待ち）。

### 1. stderr 出力 recorder

| 項目 | 内容 |
| ---- | ---- |
| 実装 | `OperationRecorder` を実装する stderr 書き出し用 recorder を追加する（案: `StderrRecorder`） |
| 出力 | `to_json_line()` の結果に改行 1 つを付けて 1 レコード 1 行で書く |
| 契約 | 書き込み失敗は握りつぶし、操作の失敗へ波及させない。panic しない |
| 既定 | 無効（`RecorderHandle::disabled()`） |

### 2. 有効化の入口（config / CLI フラグ）

| 入口 | 案 |
| ---- | -- |
| config キー | `[observability] enabled = false`（既定）・`file`・`file_max_bytes`・`file_max_generations` |
| CLI フラグ | `--observability`（有効化・stderr 出力）・`--observability-file <PATH>`（ファイル出力） |
| 優先順位 | 他の config 項目と同じ規約に合わせる（案: CLI フラグ > config） |

- 組み立ては `fandhe-browser-cli` が行い、生成した recorder を各 crate の options へ注入する（core は設定の読み込み元を知らない）
- 設定エラー（不正なパス・上限値）は config 読み込み時に英語メッセージで失敗させる（`Error::Config`）

### 3. ファイル出力（任意）

| 項目 | 内容 |
| ---- | ---- |
| サイズ上限 | 1 ファイルあたりの上限を超えたらローテーションする（案: 既定 10 MiB） |
| 世代数上限 | 保持する世代数の上限を超えた古いファイルを削除する（案: 既定 3 世代） |
| 保持期間 | 時間ベースの保持は持たない。サイズ上限と世代数上限のみで打ち切る |
| 無制限確保の防止 | 上限値の 0・極端に大きい値は設定エラーとする（上限の最大値も案として定める） |

### 4. パス検証（プロファイル保管場所の外へ書かない）

- 出力先パスは検証・正規化してから扱い、プロファイル保管場所の外を指す場合は拒否する（[security](../../.claude/rules/security.md)「プロファイル境界」・PROF 系）
- 親ディレクトリ要素（`..`）・シンボリックリンク経由の脱出を拒否する。検証は `PathBuf` / `Path::join` で行い、区切り文字を文字列連結しない
- 3 OS（Linux・macOS・Windows）で同じ検証結果になることをテストする（XOS 系）
- 「プロファイル保管場所」を具体的にどのパス（プロファイル別の配下か、全体のルートか）として扱うかは**未決定（オーナー判断待ち）**

### 5. トレイトでの差し替え点

- 差し替え点は既存の `OperationRecorder` トレイトとする。外部収集基盤（メトリクス基盤等）への送出は、このトレイトの別実装として将来追加できる
- MVP では外部収集基盤の実装・依存を追加しない
- 動的ライブラリの実行時ロードは行わない（[security](../../.claude/rules/security.md)「プラグイン境界」）。差し替えは in-process の Rust トレイト実装に限る

### 6. 暫定エンコーダの扱い

- `to_json_line` は `serde` 等を使わない暫定エンコーダだが、今回の決定（JSON Lines のみ・追加依存なし）により、そのまま採用する前提とする
- フィールド追加時は固定文字列・整数のみの方針を保つ（自由形式文字列を足す場合はエスケープ処理が必要になる）

### 7. 起動時ログとの関係

- JS エンジンの起動時構造化ログ（`js-engine.md` の起動時エンジン選択の記述）は REPAIR-9 の出力形式に従う、とされている
- 現行の `OperationRecord` は操作計測用の形で、起動時イベントを表すフィールドを持たない。起動時ログを同じ出力先・同じ形式にするか、別種のイベントとして扱うかは**未決定（オーナー判断待ち）**

## 要判断事項

| 事項 | 現状 |
| ---- | ---- |
| config キー名・CLI フラグ名 | 上記の案 |
| ファイル出力のサイズ上限・世代数上限の既定値 | 案（10 MiB・3 世代） |
| 「プロファイル保管場所」の具体的な範囲 | 未決定 |
| 起動時ログ（JS エンジン選択等）の形式 | 未決定 |

## 他タスクへの影響

- TASK-10.3（Issue #221）: 出力先の実装。本書の決定事項が入力
- TASK-95（MS-9）: 本タスクの出力形式（JSON Lines）を前提とする

## 関連ビヘイビア・タスク

- **対応ビヘイビア**: REPAIR-9（関連: CORE-2）
- **対応タスク**: TASK-10（10.h1: 方針判断）
- **マイルストーン**: MS-4

## 参照

SSOT: [fandhe-browser-spec](https://github.com/Fandhe-AI/fandhe-browser-spec) `04-behavior/self-repair-design.md`（本リポでは submodule の `docs/spec/04-behavior/self-repair-design.md`）・`05-tasks.md`（TASK-10）
