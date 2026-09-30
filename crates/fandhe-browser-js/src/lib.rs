//! JS エンジン抽象を担う crate（REPAIR-1・TASK-1（1.3）・MS-1）。
//!
//! `fandhe-browser-core` の DOM 実装から呼ばれ、JS 実行系（既定 V8 /
//! `rusty_v8`、切替先 boa / `boa_engine`）をトレイト越しに抽象化する
//! ことを目指す crate。上位 crate（core・cdp・ai 等）へ V8 / boa の
//! 具象型を漏らさない契約とする（coding-rust.md「JS エンジンはトレイト
//! 抽象越しに使い、V8 / boa の具象型を上位 crate へ漏らさない」）。
//!
//! [`engine_trait`] は TASK-28（28.2・Issue #148）でエンジン種別の列挙型
//! （[`EngineKind`]）と同梱一覧関数（[`bundled_engines`]）を、TASK-28（28.3・
//! Issue #149）でエンジン抽象トレイト本体（[`JsEngine`]）と種別からトレイト
//! オブジェクトを生成する関数（[`create_engine`]）を追加した。両エンジン
//! 共通のコンフォーマンステスト（TASK-28.4・Issue #150）は本 crate の
//! `tests/conformance.rs`（結合テスト）にある。
//!
//! V8（`rusty_v8`）・boa（`boa_engine`）それぞれの実装切替（JS-3）は、
//! 対応する依存追加（TASK-29・TASK-32）を経て別途行う。
//!
//! # スタブについて
//!
//! [`create_engine`] は V8 には子プロセス版エンジンを返し（`TASK-29.6.2`・
//! Issue #548。遅延起動。ホストは `main` の先頭で
//! [`run_js_worker_if_requested`] を呼ぶ義務がある。詳細は [`engine_trait`]）、
//! 同梱済みの boa には `NotYetImplemented`、同梱されていない種別には
//! `NotBundled` を返す。以下は未実装（実装済みを装わない。REPAIR-3）。
//!
//! - V8 の具象実装（`JS-1`、`TASK-29`、`MS-3`）は `create_engine` への配線まで
//!   完了（`TASK-29.6.2`）。トレイト経由の `NativeFn` は `Send` 境界付きで、
//!   inherent API と同じ専用スレッド経路（期限付き待機）で実行するため期限を
//!   強制できる。ただし `NativeCallContext` は関数へ渡らず協調的な中断は
//!   できない（`process_engine` の「既知の制限」）
//! - boa の具象実装（`JS-1`、`TASK-32`、`MS-3`）
//! - core への統合（`js_stub` の置換。`JS-2`、`TASK-30`、`MS-3`）
//!
//! # 公開境界（`JS-1`・TASK-29.6.1・Issue #547）
//!
//! `v8` crate の型は公開 API に現れない。検査は 3 層で行う。
//!
//! 1. `tests/public_api_boundary.rs`: 公開シグネチャを `fandhe_browser_js` と
//!    `std` の型だけの関数ポインタ型注釈へ固定する（`v8` 型が混入すると
//!    コンパイルが失敗する）
//! 2. 下記の `compile_fail` doctest: `v8` の再エクスポートや非公開モジュール
//!    へ到達できないこと
//! 3. crate 属性 `#![deny(private_interfaces, private_bounds)]`: `pub(crate)`
//!    の型が `pub` シグネチャへ漏れるとビルドが失敗する
//!
//! 限界: 外部 crate（`v8`）の型が新規の `pub` 項目へ混入することを網羅的に
//! 自動検出する手段は stable にない（`exported_private_dependencies` は
//! `-Zpublic-dependency` が必要で stable では機能しないことを実測済み）。
//! 新しい `pub` 項目を足したら上記 1 にも追加する。
//!
//! 参照先が存在することの確認（`compile_fail` が別理由で通らないための対）:
//!
//! ```
//! use fandhe_browser_js::{JsEngine, JsEngineError, create_engine};
//! ```
//!
//! `v8` の再エクスポートは無い。
//!
//! ```compile_fail
//! use fandhe_browser_js::v8::V8;
//! ```
//!
//! `v8_engine` は非公開。
//!
//! ```compile_fail
//! use fandhe_browser_js::v8_engine::V8Engine;
//! ```
//!
//! `worker` は非公開。
//!
//! ```compile_fail
//! use fandhe_browser_js::worker;
//! ```
//!
//! `resource_limits` は非公開。
//!
//! ```compile_fail
//! use fandhe_browser_js::resource_limits;
//! ```

#![deny(private_interfaces, private_bounds)]

pub mod engine_trait;
// V8（`rusty_v8`）の Platform/Isolate 初期化（TASK-29.2）と、
// [`worker`] が子プロセスの中で使うスクリプト評価本体（TASK-29.3）を
// 担う非公開モジュール。具象型を上位 crate へ漏らさないため `pub` を
// 付けず、`pub use` もしない（AC-2・coding-rust.md「JS エンジンはトレイト
// 抽象越しに使い、V8 / boa の具象型を上位 crate へ漏らさない」）。
#[cfg(feature = "js-v8")]
mod v8_engine;
// 子プロセスのヒープ外メモリに OS 側の上限を掛ける（TASK-29・Issue #503
// 設計書 §5・codex レビュー指摘 #503 P0 対応）。[`worker`]（子側の起動時
// 強制）と [`process_engine`]（親側の RSS 監視）の両方から使う非公開
// モジュール。
#[cfg(feature = "js-v8")]
mod resource_limits;
// JS 評価用の子プロセスと stdio でやり取りするバイナリプロトコルの
// フレーミング・コーデック（TASK-29・Issue #503「JS プロセス分離」
// 設計書 §3.5・§7 W2）。`v8` crate に依存しない（`js-v8` feature の
// 有無に関わらずコンパイル・テストできる）。[`MARKER_ENV_VAR`] 相当の
// 定数は [`run_js_worker_if_requested`] から feature の有無に関わらず
// 参照するが、フレームのエンコード・デコード関数群は [`worker`]
// （`js-v8` feature 有効時のみ）からしか使われないため、`js-v8` 無効かつ
// 非テストビルドでは引き続き `dead_code` の期待を宣言する（REPAIR-3）。
// `js-v8` feature 無効時は本モジュールの大半（フレームのエンコード・
// デコード関数群）が非テストビルドで未使用になる。項目ごとの
// `dead_code` 抑制は `worker_protocol.rs` 側で個別に宣言する
// （モジュール直下へまとめて属性を付けると、`decode_js_value` 等の
// 個別 `expect(dead_code)` が外側の属性に握りつぶされて
// `unfulfilled_lint_expectations` になるため。REPAIR-3）。
mod worker_protocol;
// JS 評価を行う子プロセスの入口（TASK-29・Issue #503 設計書 §3.1・
// §7 W3）。`js-v8` feature 有効時のみ、実際に V8 を組み込んだ子プロセス
// として動作できる。
#[cfg(feature = "js-v8")]
mod worker;
// 子プロセスへの親側プロキシ（TASK-29・Issue #503 設計書 §3.2〜§3.4・
// §7 W4）。`create_engine` へは TASK-29.6.2（Issue #548）で配線済み。
//
// `pub` にする理由: `tests/v8_worker.rs`（`harness = false`。W6）は結合
// テストであり、別クレートとしてコンパイルされるため `pub(crate)` の
// 項目を参照できない。`V8ProcessEngine`・`new`・`evaluate_script` は
// 本番ビルド（`test-support` feature 無効）でも `#[doc(hidden)] pub` の
// まま残す（`create_engine` から配線した（`TASK-29.6.2`）本番 API の
// 一部であり、テスト専用ではない）。一方、テスト専用の入口
// （`new_for_test`・`send_raw_frame_for_test`・`worker_pid_for_test`・
// `max_script_source_bytes_for_test`・`max_raw_frame_bytes_for_test`・
// `WorkerSpawnConfigForTest`）は feature `test-support` が有効なときだけ
// `pub` になり、無効時（本番ビルドを含む既定の `js-v8` ビルド）は crate
// 外から名前を一切付けられない（Issue #528・TASK-29・`JS-1`。詳細は
// `process_engine.rs` の各項目のドキュメントコメント参照）。
// `V8ProcessEngine` は `Child`・パイプ・チャネルしか保持せず `v8` crate の
// 型を一切参照しないため、coding-rust.md「V8 / boa の具象型を上位 crate
// へ漏らさない」には抵触しない。
#[cfg(feature = "js-v8")]
#[doc(hidden)]
pub mod process_engine;

pub use engine_trait::{
    CreateEngineError, EngineKind, EvaluateOptions, JsEngine, JsEngineError, JsValue,
    NativeCallContext, NativeFn, ObjectHandle, bundled_engines, create_engine,
};

/// この呼び出しが JS 評価用の子プロセスとして起動されたものかどうかを
/// 判定する（`JS-1`・`TASK-29`・Issue #503「JS プロセス分離」設計書
/// §3.1・§7 W3）。
///
/// 呼び出し元（将来）: `fandhe-browser-core` が `pub use` で再エクスポート
/// し（`TASK-30`）、`fandhe-browser-cli` の `main` が tokio ランタイム・
/// ロギング・設定読み込みより**前**に呼ぶ契約（`TASK-41`）。
///
/// 環境変数 `worker_protocol::MARKER_ENV_VAR`
/// （`FANDHE_BROWSER_JS_WORKER`）が設定されていなければ `None` を返し、
/// 呼び出し元は通常どおり処理を続ける（設計書 §3.1「フックを呼ばない
/// ホストへの対策」）。設定されている場合は子プロセスとして動作し、
/// プロセスの終了コードを返す（呼び出し元は `main` からそのまま
/// `return` することを想定する）。
///
/// `js-v8` feature が無効なビルドでこの環境変数が設定されていた場合、
/// このバイナリは子プロセスとして機能できないため
/// `ExitCode::FAILURE` を返す（成功を一律に返すフォールバックはしない。
/// security.md「偽装・回避機能の禁止」）。
///
/// # スレッドに関する不変条件（`JS-1`・`TASK-29`・Issue #520）
///
/// 子プロセスモード（`js-v8` feature 有効時）では、この関数から
/// `worker::worker_main` を経て V8 の Platform 初期化・Isolate 生成まで、
/// **呼び出しスレッドの上で直線的に**進む。子プロセスの中の
/// `worker_main` は V8 の Platform を protected 版（thread-isolated
/// allocation 有効）で要求する（`v8_engine::ensure_v8_initialized_with`
/// 参照）ため、Isolate に入れるのは Platform 初期化を行ったスレッド
/// （＝この関数を呼んだスレッド）とその子孫だけである（PKU を持つ
/// x86-64 Linux での protected Platform の制約）。
///
/// したがって、この関数を呼ぶ側（`TASK-41` の cli `main`）は、
/// **本関数を呼ぶより前に**別スレッドを作ってそこから V8 の Isolate に
/// 入るような構成にしてはならない。tokio ランタイム・ロギング等より
/// 前に本関数を呼ぶという既存の契約（上記「呼び出し元」節）は、この
/// 不変条件を満たすための前提でもある。
pub fn run_js_worker_if_requested() -> Option<std::process::ExitCode> {
    let marker_value = std::env::var(worker_protocol::MARKER_ENV_VAR).ok()?;
    Some(dispatch_worker(&marker_value))
}

#[cfg(feature = "js-v8")]
fn dispatch_worker(marker_value: &str) -> std::process::ExitCode {
    worker::worker_main(marker_value)
}

#[cfg(not(feature = "js-v8"))]
fn dispatch_worker(_marker_value: &str) -> std::process::ExitCode {
    eprintln!(
        "fandhe-browser-js worker: this binary was not built with the js-v8 feature and \
         cannot run as a JS evaluation worker"
    );
    std::process::ExitCode::FAILURE
}
