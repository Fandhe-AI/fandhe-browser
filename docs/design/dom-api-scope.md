# 未実装 DOM/Web API 一覧（MVP スコープ切り分け材料）

対応 `TASK-25`（25.1） / `MS-3` / ビヘイビア `CORE-5`（[spec-reference](../../.claude/rules/spec-reference.md)）。

## 目的

`CORE-5` は、MVP スコープを Must / Should / Could / Won't に切り分ける判断材料として、
現時点で未実装の DOM/Web API 8 項目を一覧化することを求めている。本ドキュメントはその一覧化のみを行う。
**MVP スコープの Must/Won't 等の判定自体は本ドキュメントの範囲外**であり、担当は人間（親 Issue #132
の受入条件）である。本書は各項目の現状（未実装・簡易実装）と本リポジトリの根拠、関連ビヘイビア ID・
TASK・MS を記録するに留め、実装済みであるかのような記述はしない（REPAIR-3）。

## 一覧表

| # | 項目 | 現状 | 本リポの根拠 | 関連ビヘイビア ID | 関連 TASK・MS | MVP 判断 |
| - | ---- | ---- | ------------ | ------------------ | -------------- | -------- |
| 1 | JS 実行全般（`<script>`・イベントハンドラ・fetch/XHR 起因の DOM 更新） | 未実装（スタブ境界のみ） | `crates/fandhe-browser-core/src/js_stub.rs` の `execute_js_stub`・`crates/fandhe-browser-js` の `create_engine` | `CORE-5` (1)・`JS-1`・`JS-2`・`COMPAT-4` | `TASK-28`・`TASK-29`・`TASK-30`・`TASK-32`・`TASK-71`・`TASK-72`・`TASK-74`・`MS-3`・`MS-6` | 未判定（人間担当・TASK-25 親 #132） |
| 2 | CSSOM・computed style・レイアウト | 未実装 | `crates/fandhe-browser-core/src/render.rs` の `DisabledRenderer`（`fandhe-browser-render` は雛形のみ） | `CORE-5` (2)・`RENDER-1`・`RENDER-3`・`PLUG-7`・`PLUG-8` | `TASK-33`・`TASK-38`・`TASK-105`・`TASK-100`・`MS-4`・`MS-8` | 未判定（人間担当・TASK-25 親 #132） |
| 3 | DOM イベントディスパッチ（`click`/`input` の発火） | 未実装 | イベントモデル・ディスパッチのコードなし（JS 実行〔1〕が前提） | `CORE-5` (3)・`JS-2`・`CDP-2`・`CLI-1` | spec 側で対応 TASK が未割当（関連: `TASK-30`・`TASK-42`・`TASK-47`） | 未判定（人間担当・TASK-25 親 #132） |
| 4 | `<select>` の入れ子 `option` の選択値解決 | 未実装 | フォーム値を解決する API 自体がない（`parse.rs` モジュール doc に範囲外の記載あり） | `CORE-5` (4)・`AISNAP-12`・`COMPAT-1` | spec 側で対応 TASK が未割当（関連: `TASK-72`・`TASK-11`） | 未判定（人間担当・TASK-25 親 #132） |
| 5 | Shadow DOM・`<iframe>` を跨ぐクロスドキュメント操作 | 未実装（Shadow DOM は明示的に無効化） | `parse.rs` の `TreeSink::allow_declarative_shadow_roots` は常に `false` を返す。`<iframe>` は通常要素としてのみパースし子文書は取得・構築しない | `CORE-5` (5) | spec 側で対応 TASK が未割当 | 未判定（人間担当・TASK-25 親 #132） |
| 6 | `MutationObserver` 等の DOM 変化監視 | 未実装 | 該当コードなし（DOM を動的に変更する経路〔JS 実行〕自体がまだない） | `CORE-5` (6)・`AISNAP-10`〜`13`・`PLUG-9` | spec 側で対応 TASK が未割当（関連: `TASK-30`） | 未判定（人間担当・TASK-25 親 #132） |
| 7 | 文字コード検出（UTF-8 以外の明示宣言への対応） | 簡易実装（UTF-8 固定） | `parse.rs` の `parse_document_bytes` は UTF-8 を厳密検証し非 UTF-8 は `ParseError::InvalidUtf8`。`fetch.rs` の `body_text_lossy` は `String::from_utf8_lossy` による簡易変換のみ | `CORE-5` (7) | spec 側で対応 TASK が未割当 | 未判定（人間担当・TASK-25 親 #132） |
| 8 | Cookie による複数リクエスト間のセッション維持の実地検証 | 未実装 | `reqwest` は `cookies` feature なしで導入（ルート `Cargo.toml`）。`Fetcher` に cookie jar なし。`fandhe-browser-profile` は `DataKind::Cookies` の保管領域のみ持つ | `CORE-5` (8)・`PROF-1`・`PROF-2`・`PROF-3`・`PROF-5` | spec 側で対応 TASK が未割当（関連: `TASK-50`・`TASK-51`・`TASK-52`・`TASK-53`） | 未判定（人間担当・TASK-25 親 #132） |

## 項目別詳細

### (1) JS 実行全般（`<script>`・イベントハンドラ・fetch/XHR 起因の DOM 更新）

- **現状**: 未実装（スタブ境界のみ）。
- **根拠**: `crates/fandhe-browser-core/src/js_stub.rs` の `execute_js_stub` は常に `Error::JsExecutionUnavailable` を返す。
  `crates/fandhe-browser-js` の `create_engine` は同梱済みエンジン種別には `NotYetImplemented`、
  未同梱の種別には `NotBundled` を返す（`lib.rs`・`engine_trait.rs`）。`fandhe-browser-core` は
  現時点で `fandhe-browser-js` にまだ依存していない。
- **関連 ID**: `CORE-5` (1)・`JS-1`・`JS-2`・`COMPAT-4`。
- **関連 TASK・依存関係**: `TASK-28`（済・トレイト定義）→ `TASK-29`（V8）・`TASK-32`（boa）。
  `execute_js_stub` の V8 実呼び出しへの置換は `TASK-30`（`MS-3`）が担う。`TASK-71`・`72`・`74`（`MS-6`）は
  JS 実行を前提とした再測定タスク。
- **判断時の論点**: 本項目は他の未実装項目（3・6）の前提条件になっている。

### (2) CSSOM・computed style・レイアウト

- **現状**: 未実装。
- **根拠**: `fandhe-browser-core` に CSSOM・スタイル解析のコードはない。`render.rs` の
  `DisabledRenderer` は `capture_screenshot`・`element_visibility`・`bounding_box` のすべてで
  `RenderError::RenderingDisabled` を返す。`fandhe-browser-render` は雛形のみ。
- **関連 ID**: `CORE-5` (2)・`RENDER-1`・`RENDER-3`・`PLUG-7`（段階 1）・`PLUG-8`。
- **関連 TASK・依存関係**: `TASK-33`（feature gate・一部済み）→ `TASK-38`（Servo 本実装・`MS-4`）。
  CSSOM の最小構築は `TASK-105`（`MS-8`）が担い、`TASK-100`（`PLUG-8`・`MS-8`）はこれを前提とする。
- **判断時の論点**: レイアウト自体は `TASK-105` の範囲にも含まれない。

### (3) DOM イベントディスパッチ（`click`/`input` の発火）

- **現状**: 未実装。
- **根拠**: workspace 全体にイベントモデル・ディスパッチを行うコードはない。(1) の JS 実行が前提。
- **関連 ID**: `CORE-5` (3)。間接的に関係: `JS-2`・`CDP-2`（Playwright 高レベル API 到達）・`CLI-1`
  （フォーム操作を含む基本コマンド）。
- **関連 TASK・依存関係**: ディスパッチそのものを担う TASK は spec 側で未割当。関連: `TASK-30`・`TASK-42`・`TASK-47`。
- **判断時の論点**: (1) の解消後に着手可能になる。

### (4) `<select>` の入れ子 `option` の選択値解決

- **現状**: 未実装。
- **根拠**: 本リポジトリにはフォーム値を解決する API 自体がない（`fandhe-browser-core`・`fandhe-browser-ai`
  ともに該当コードなし）。`parse.rs` のモジュール doc に `CORE-5` (4) として範囲外の記載がある。
- **関連 ID**: `CORE-5` (4)。間接的に関係: `AISNAP-12`（`<select>` の先頭 8 件圧縮）・`COMPAT-1`
  （フォーム類型の動作率）。
- **関連 TASK・依存関係**: spec 側で未割当。関連: `TASK-72`（フォーム類型の測定）・`TASK-11`（簡約 DOM）。
- **判断時の論点**: spec には「`select` 自身の `value` 属性を暫定採用する」という記述があるが、これは
  PoC-2（`core-proto`）時点の状態であり、本リポジトリには移植されていない。現状として転記しない。

### (5) Shadow DOM・`<iframe>` を跨ぐクロスドキュメント操作

- **現状**: 未実装（Shadow DOM は明示的に無効化）。
- **根拠**: `parse.rs` の `TreeSink::allow_declarative_shadow_roots` は常に `false` を返し、宣言的
  shadow root を拒否する（モジュール doc に `CORE-5` (5) の記載あり）。`<iframe>` は通常の要素として
  パースするのみで、子文書の取得・構築は行わない。`<template>` の contents は `template_contents` として
  別に保持されるが、これはクロスドキュメント操作ではなく実装済みの別機能である。
- **関連 ID**: `CORE-5` (5)。
- **関連 TASK・依存関係**: spec 側で未割当。

### (6) `MutationObserver` 等の DOM 変化監視

- **現状**: 未実装。
- **根拠**: 該当コードはない。DOM を動的に変更する経路（JS 実行〔1〕）自体もまだない。
- **関連 ID**: `CORE-5` (6)。間接的に関係: `AISNAP-10`〜`13`（要素参照の安定性・DOM の軽微な変化）・
  `PLUG-9`（JS/Web API 層・Could）。
- **関連 TASK・依存関係**: spec 側で未割当。関連: `TASK-30`。

### (7) 文字コード検出（UTF-8 以外の明示宣言への対応）

- **現状**: 簡易実装（UTF-8 固定）。
- **根拠**: `parse.rs` の `parse_document_bytes` は UTF-8 を厳密に検証し、UTF-8 以外は
  `ParseError::InvalidUtf8` を返す（検出は行わない）。`fetch.rs` の `FetchResponse::body_text_lossy`
  は `String::from_utf8_lossy` による簡易変換であり、`charset` ヘッダや meta タグは参照しない。
  `body()` は生のバイト列を返す。
- **関連 ID**: `CORE-5` (7)。
- **関連 TASK・依存関係**: spec 側で未割当。

### (8) Cookie による複数リクエスト間のセッション維持の実地検証

- **現状**: 未実装。
- **根拠**: `reqwest` は `cookies` feature なしで導入されている（ルート `Cargo.toml`）。
  `Fetcher` には cookie jar がない（`fetch.rs` のモジュール doc「範囲外」に `CORE-5` (8) の記載あり）。
  `fandhe-browser-profile` は `DataKind::Cookies`（`cookies/` ディレクトリ）の保管領域のみを持ち
  （`PROF-1`・#437）、Cookie の読み書き自体は未実装。`self-repair-design.md` は「core → profile」の
  依存方向を想定している。
- **関連 ID**: `CORE-5` (8)・`PROF-1`・`PROF-2`・`PROF-3`・`PROF-5`。
- **関連 TASK・依存関係**: セッション維持そのものは spec 側で未割当。関連: `TASK-50`（済・骨格）／
  `TASK-51`・`52`（分離テスト）／`TASK-53`（削除）。

## スコープ判断の扱い

上記表の「MVP 判断」列はすべて `未判定（人間担当・TASK-25 親 #132）` とする。本ドキュメントは
一覧化のみを目的とし、Must/Should/Could/Won't の判定は行わない（`delegation-impl.md` の着手条件）。

## 後続タスクとの関係

- `TASK-105`（CSSOM/computed style の最小構築・`MS-8`）は項目 (2) を前提とし、`TASK-100`
  （`PLUG-8` の CSSOM feature gating・`MS-8`）はさらに `TASK-105` を前提とする（`06-roadmap.md`）。
- `TASK-30`（`JS-2`・`MS-3`）は項目 (1) を解消する中核タスクであり、項目 (3)・(6) の前提でもある。
- `MS-8` は `MS-3` の `TASK-25` に依存する（`06-roadmap.md`）。

## 出典

- `docs/spec/04-behavior/core-dom.md`（`CORE-5`）
- `docs/spec/05-tasks.md`（`TASK-25`・`TASK-25.1`・`TASK-105`・`TASK-100`・`TASK-30` ほか本文中に記載の TASK）
- `docs/spec/06-roadmap.md`（`MS-3`・`MS-4`・`MS-6`・`MS-8`）
