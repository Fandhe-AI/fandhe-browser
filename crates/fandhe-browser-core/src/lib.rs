//! `fandhe-browser-core`: fandhe-browser のコア crate。
//!
//! fetch（ネットワーク取得）・HTML パース・DOM・query（DOM 探索）・CSSOM・
//! config（設定）・可観測性（ログ・トレーシング）・描画機能への境界（[`render`]）を
//! 担う crate。`fandhe-browser-profile`（`state` モジュールが `Profile` を保持する。
//! TASK-41.1・#169。workspace 内 path 依存）と `fandhe-browser-js`（`js_stub` が
//! 評価を委譲する。workspace 内 path 依存）に依存する（AGENTS.md「crate 間の許可依存」:
//! js・profile）。`Cargo.toml` の `[dependencies]` には
//! `html5ever = "=0.40.1"`（Issue #35 承認済み・TASK-24.4・#38）と reqwest・
//! rustls（同じく Issue #35 承認済み・TASK-24.2・#36）を持つ。他の依存追加は
//! 該当タスクで dependency-policy.md のユーザー承認制に従って行う。
//! `fandhe-browser-ai`・`fandhe-browser-cdp` 等の
//! 上位 crate からは一方向に依存される（AGENTS.md「crate 間の許可依存」・
//! coding-rust.md「循環依存を作らない」）。
//!
//! REPAIR-1（TASK-1（旧サブ番号 1.2）・MS-1）: workspace 分割の一環として追加した
//! 空 skeleton から出発している。REPAIR-1（TASK-24（24.1）・MS-1）: `fetch`/
//! `parse`/`dom`/`query` の各モジュール構成と、それらが共通で使うエラー型
//! （[`error::Error`] / [`error::Result`]）を追加した。`fetch`（TASK-24.2・#36）は
//! reqwest・rustls を使う本実装を持つ。`parse`（TASK-24.4・#38）は html5ever の
//! `TreeSink` を自作実装し、HTML 文字列・バイト列から `dom::Document`（arena）を
//! 構築する本実装を持つ。`dom`（TASK-24.5・#39）は arena の型定義
//! （[`dom::Document`]・[`dom::Node`] 等）に加え、走査 API（親子・兄弟・祖先・
//! 子孫を辿るイテレータ）・要素/属性アクセサ・`text_content` の本実装を持つ
//! （セレクタ照合はスコープ外。REPAIR-3: 実装済みを装わない）。
//! `selector`（TASK-24.7・#41）は CSS セレクタの
//! サブセットをパースする本実装を持つ。`query`（TASK-24.10・#418）は
//! `selector` の AST を `dom::Document` に照合し、`querySelector`/
//! `querySelectorAll`/`Element.matches()` 相当の API を提供する本実装を持つ。
//! JS 実行の境界は TASK-24（24.9・Issue #43）で `js_stub` モジュールとして追加し、
//! TASK-30（30.3・Issue #161・ビヘイビア `JS-2`）で設定が選んだ `fandhe-browser-js` の
//! エンジン（[`js_stub::JsRuntime`]）への評価委譲へ置換した（boa は TASK-32 まで未実装）。
//! 子プロセス版 JS エンジンのワーカー入口 `run_js_worker_if_requested` は、cli が js へ
//! 直接依存せずに呼べるよう crate ルートから再エクスポートしている（TASK-30・`JS-2`・
//! #513）。cli の `main` 先頭で `fandhe_browser_core::run_js_worker_if_requested()` として
//! 呼ぶ（呼び出しの配線は TASK-41・#514）。
//! [`render`] モジュールは TASK-33（サブタスク 33.2・ビヘイビア `RENDER-1`）で追加した
//! 描画トレイトの定義に加え、feature `rendering` 無効時に用いる既定実装
//! `DisabledRenderer`（TASK-33（33.3）・issue #47）を含む。`fandhe-browser-render`
//! （Servo）側の本実装・cli による具象実装の注入は含まない（別 issue の担当。render
//! モジュールの doc コメントを参照）。
//! [`state`] モジュール（TASK-41.1・#169・`CDP-1`・`AISNAP-6`）は cdp と ai が共有する
//! 共通状態 [`state::AppState`]（`Profile`・描画ハンドル・ナビゲート状態）を定義する。
//! [`observability`] モジュールは TASK-10（10.1・Issue #219・ビヘイビア
//! `REPAIR-9`）で、`fetch`/`parse`/`dom`/`query`/`js_stub` 各モジュールの
//! 操作計測が共通で使うレコード型（[`observability::OperationRecord`] 等）を
//! 追加した。`fetch`・`parse`・`dom` へは TASK-10.2.1（#549）、`query`・`js_stub` へは
//! TASK-10.2.2（#550）で recorder（[`observability::OperationRecorder`]）経由の計装を
//! 組み込み済み。本番の出力先の確定・実装は #221・#218 の担当で含まない
//! （[`observability::InMemoryRecorder`] はテスト・簡易集計用で本番の出力先ではない）。
//! [`config`] モジュールは TASK-91（91.1・Issue #214・対象ビヘイビアなし・
//! 基盤タスク）で、`fandhe-browser.toml` の `[profile]` セクション
//! （保存先・分離強度）を読み込む本実装を追加した（`toml`・`serde` は
//! Issue #213 で承認済み）。`[js] engine`（TASK-91（91.2）・Issue #215）も
//! 読める。`[rendering] enabled`（TASK-91（91.3）・Issue #216）も読める
//! （未同梱ビルドでの有効化は暫定でエラー。`config` モジュール doc コメント参照）。
//! [`cssom`] モジュールは TASK-105（`CORE-5`）で公開型（105.1・#255）と
//! 宣言パーサー（105.2・#256）・詳細度計算（105.3・#257）・ルールブロック分割
//! （105.4.1・#551）・スタイル源収集と Stylesheet 構築（105.4.2・#552）・セレクタマッチング（105.5・#259）・カスケード解決と computed style API（105.6・#260）を追加した。
//! 外部入力の上限検証（105.7・#261）も含む。結合テスト（105.8・#262・`tests/cssom.rs`）も含む
//! （`!important` 未反映等は [`cssom`] の doc を参照）。
//! [`cssom_profile`] モジュール（TASK-100.2・#266・`PLUG-8`・MS-8）は `profiles/*.json` を
//! 埋め込み、CSS プロパティ単位の対応可否を照会する。TASK-100.3（#267）で公開入口
//! [`profile_gate`] を、TASK-100.4（#268）で gating 本体（宣言の除去）を追加した。
//! [`page_script`] モジュール（TASK-109・#774・`JS-4`・`JS-6`）は、パース済み DOM から
//! 実行対象の `<script>` を文書順に収集する（評価・`src` 取得は #780・#781 の担当）。
//!
//! # スタブについて
//!
//! 未実装・簡易実装の詳細は各モジュールの `//!` を参照（`REPAIR-3`: 実装済みを
//! 装わない）。crate 直下では対象モジュールの名前のみを挙げる。
//!
//! - [`cssom`]（`CORE-5`・`TASK-105`・`MS-8`。型定義・宣言パーサー・詳細度計算・ルール分割・スタイル源収集・セレクタマッチング・カスケード解決のみ。`!important`・継承等は未実装）
//! - [`page_script`]（`JS-4`・`JS-6`・`TASK-109`・`MS-6`。収集のみ。SVG の script・`language`・module 実行等は未実装）
//! - [`js_stub`]（`JS-2`・`TASK-30`・`MS-3`）
//! - [`render::DisabledRenderer`]（`RENDER-1`・`TASK-33`/`TASK-38`・`MS-1`/`MS-4`）
//! - [`observability::OperationRecord::to_json_line`]（`REPAIR-9`・`TASK-10（10.1）`・
//!   `MS-4`。出力形式は Issue #218 で未確定な暫定エンコーダ）
//! - [`observability::InMemoryRecorder`]（`REPAIR-9`・`TASK-10.3`・#221。
//!   テスト・簡易集計用で本番の出力先ではない）

pub mod config;
pub mod cssom;
pub mod cssom_profile;
pub mod dom;
pub mod error;
pub mod fetch;
pub mod host;
pub mod js_stub;
pub mod observability;
pub mod page_script;
pub mod parse;
pub mod query;
pub mod render;
pub mod selector;
pub mod state;

pub use config::{
    Config, ConfigError, EngineKind, IsolationStrength, JsConfig, ProfileConfig, RenderingConfig,
    bundled_engines,
};
pub use cssom::{
    ComputedDeclaration, ComputedStyle, Declaration, DeclarationOrigin, Importance, Specificity,
    StyleRule, Stylesheet,
};
pub use cssom_profile::{
    BrowserProfile, GatedStyle, ProfileGate, profile_gate, profile_gate_from_name,
};
pub use dom::{
    Ancestors, Attribute, Children, DEFAULT_MAX_SERIALIZED_BYTES, Descendants, Document, DomLimits,
    Node, NodeData, NodeId, QuirksMode, SerializeResult, SerializeScope,
};
pub use error::{DomError, Error, NameKind, ParseError, Result};
pub use fetch::{FetchOptions, FetchResponse, Fetcher};
// js crate のワーカー入口をそのまま再エクスポートする（ラッパーを挟まない）。ラッパーで
// マーカー設定時にも `None` を返すと fail-closed が崩れるため（security.md・`JS-2`・#513）。
pub use fandhe_browser_js::run_js_worker_if_requested;
// `JsRuntime::inject_global_function` の引数・エラー型。利用側（WPT ランナー等）が js crate へ
// 直接依存せず core 経由で扱えるようにする（PLUG-10・TASK-101.2.1・#553）。
pub use fandhe_browser_js::{JsEngineError, JsValue, NativeFn};
pub use observability::{
    FailureKind, InMemoryRecorder, OperationCounts, OperationKind, OperationOutcome,
    OperationRecord, OperationRecorder, RecorderHandle,
};
pub use page_script::{
    CollectedScripts, DEFAULT_MAX_INLINE_SCRIPT_BYTES, DEFAULT_MAX_PAGE_SCRIPTS,
    ScriptCollectionOptions, ScriptDiagnostic, ScriptDiagnosticKind, ScriptEntry, ScriptSource,
    collect_page_scripts,
};
pub use parse::{
    ParseDiagnostics, ParseErrorPolicy, ParseOptions, ParsedDocument, parse_document,
    parse_document_bytes,
};
pub use query::{
    element_matches, query_selector, query_selector_all, query_selector_all_str, query_selector_str,
};
pub use state::{AppState, NavigationGeneration, NavigationResult, NavigationState, StateError};
