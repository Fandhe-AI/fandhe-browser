//! js_stub: JS 実行呼び出しの境界を定義するモジュール（PoC-2 `core-proto/src/js_stub.rs`
//! 相当。TASK-24（24.9）・ビヘイビア `CORE-1`。Issue #43）。
//!
//! `dom`（TASK-24.5・#39）・`query`（TASK-24.7・#41）や、将来の `fandhe-browser-cdp`
//! の `Runtime.evaluate` ハンドラなど、JS 実行が必要な箇所はこのモジュールの
//! [`execute_js_stub`] を境界として呼び出す想定。`docs/spec/04-behavior/js-engine.md`
//! の JS-2 は本関数名を明示的に名指ししており、`TASK-30`（`MS-3`・Issue #143）で
//! `fandhe-browser-js`（TASK-28 でトレイト・`EngineKind`・生成関数を定義）を
//! 使った V8 実呼び出しへ置換される契約のため、関数名・配置パスは変更しない。
//!
//! 本 PR（TASK-24（24.9））時点では `fandhe-browser-core` から
//! `fandhe-browser-js` への依存は追加しない（依存追加は `TASK-30`（`MS-3`・#143）の
//! スコープであり、dependency-policy.md のユーザー承認制に従う）。
//!
//! # スタブについて
//!
//! スタブである間は常にエラーを返す（REPAIR-3: 実装済みを装わない。
//! security.md「未実装機能で成功を一律に返すフォールバック」の回避）。
//! エンジン非同梱ビルドでも「同梱されていない」ことを表すエラーを返す設計に
//! なる想定（`js-engine.md` 決定 4）。
//!
//! # 可観測性（`REPAIR-9`・`TASK-10.2.2`・Issue #550）
//!
//! [`execute_js_stub_with_options`] に [`JsStubOptions::with_recorder`] で
//! recorder を渡すと、呼び出し 1 回につき [`OperationKind::JsStub`] を 1 件記録する。
//! スタブの記録は常に `Failure { JsExecutionUnavailable }` で、成功を装わない。
//! `script` の内容はレコードに含まれない（固定 enum と数値のみ）。`TASK-30`
//! （`MS-3`・#143）で `fandhe-browser-js` 呼び出しへ置換する際も、本 options 経由の
//! 計装契約を引き継ぐ想定。
//!
//! - `execute_js_stub` の V8 実呼び出しへの置換（`JS-2`・`TASK-30`・`MS-3`）

use std::sync::Arc;
use std::time::Instant;

use crate::observability::{OperationKind, OperationRecorder, RecorderHandle};

/// [`execute_js_stub_with_options`] の設定（`REPAIR-9`・`TASK-10.2.2`）。
///
/// `FetchOptions`・`ParseOptions` と同じ「options + `with_recorder`」方式で
/// recorder を注入する。既定は recorder なし（何も記録しない）。将来
/// `TASK-30`（`MS-3`）でタイムアウト等を非破壊に追加できるよう
/// `#[non_exhaustive]` にしてある（REPAIR-4）。
#[non_exhaustive]
#[derive(Debug, Clone, Default)]
pub struct JsStubOptions {
    recorder: RecorderHandle,
}

impl JsStubOptions {
    /// 既定の options（recorder なし）を作る。
    pub fn new() -> Self {
        Self::default()
    }

    /// 操作レコードの受け口を設定する。呼び出し 1 回につき `JsStub` を
    /// ちょうど 1 件記録する（`REPAIR-9`）。
    #[must_use]
    pub fn with_recorder(mut self, recorder: Arc<dyn OperationRecorder>) -> Self {
        self.recorder = RecorderHandle::new(recorder);
        self
    }
}

/// JS 実行結果を表す型。
///
/// 本スタブでは値を生成しないが、真偽値やフラットな `String` を戻り値に
/// せず、将来拡張できる構造にしてある（REPAIR-4）。`TASK-30`（`MS-3`・#143）で
/// 実際の実行結果を保持するフィールドが追加される想定。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsExecutionOutput {
    /// JS 実行結果の文字列表現（本スタブでは生成されない。TASK-30 で実装）。
    pub value: String,
}

/// JS 実行のスタブ境界（CORE-1・TASK-24（24.9）・Issue #43）。
///
/// 常にエラーを返す設計（`js-engine.md` JS-2 が明示的に名指しする関数）。
/// `fandhe-browser-js`（TASK-28）の統合・`TASK-30`（`MS-3`・Issue #143）での
/// V8 実呼び出しへの置換まで、呼び出し元には「未実装」を明示するエラーを
/// 返し、成功を装わない。
///
/// `script` は現状未使用（本スタブでは評価も文字列連結もしない）。将来の
/// 実装（`TASK-30`・`MS-3`）でスクリプト文字列を渡す契約を維持するためシグネチャに
/// 残してある。エラーメッセージには `script` の内容を埋め込まない（外部
/// 入力の反響・ログインジェクション対策。security.md）。
pub fn execute_js_stub(script: &str) -> crate::Result<JsExecutionOutput> {
    execute_js_stub_with_options(script, &JsStubOptions::default())
}

/// [`execute_js_stub`] の計装付き版（`REPAIR-9`・`TASK-10.2.2`）。
///
/// `options` に recorder があれば `JsStub` を 1 件記録する。スタブである間は
/// 常に `Failure { JsExecutionUnavailable }` で、成功レコードは出さない
/// （REPAIR-3）。`TASK-30`（`MS-3`・#143）の実呼び出しへの置換後も本関数が
/// 計装の入口になる想定。
pub fn execute_js_stub_with_options(
    script: &str,
    options: &JsStubOptions,
) -> crate::Result<JsExecutionOutput> {
    let start = options.recorder.is_enabled().then(Instant::now);
    let result = js_stub_inner(script);
    if let Some(start) = start {
        options
            .recorder
            .record_result(OperationKind::JsStub, &result, start.elapsed());
    }
    result
}

fn js_stub_inner(script: &str) -> crate::Result<JsExecutionOutput> {
    let _ = script;
    Err(crate::Error::JsExecutionUnavailable {
        message: "JS execution is not implemented yet (js_stub boundary; \
                   replaced by fandhe-browser-js per JS-2/TASK-30)"
            .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;

    /// CORE-1（TASK-24（24.9）・#43）: `execute_js_stub` が常に
    /// `Error::JsExecutionUnavailable` を返すことを確認する。
    #[test]
    fn core_1_execute_js_stub_returns_js_execution_unavailable() {
        let err = execute_js_stub("1 + 1").expect_err("js_stub は常に失敗する");
        assert!(matches!(err, Error::JsExecutionUnavailable { .. }));
        assert_eq!(
            err.to_string(),
            "JS execution unavailable: JS execution is not implemented yet \
             (js_stub boundary; replaced by fandhe-browser-js per JS-2/TASK-30)"
        );
    }

    /// CORE-1（TASK-24（24.9）・#43）: 空文字列でも常にエラーになることを
    /// 確認する（`script` の内容に依存しない設計であることの裏付け）。
    #[test]
    fn core_1_execute_js_stub_returns_error_for_empty_script() {
        let err = execute_js_stub("").expect_err("js_stub は常に失敗する");
        assert!(matches!(err, Error::JsExecutionUnavailable { .. }));
    }

    /// CORE-1（TASK-24（24.9）・#43）: 非 ASCII を含むスクリプト文字列でも
    /// 常にエラーになり、その内容がエラーメッセージへ埋め込まれないことを
    /// 確認する（security.md: 外部入力の反響を避ける）。
    #[test]
    fn core_1_execute_js_stub_returns_error_for_non_ascii_script() {
        let script = "console.log('こんにちは')";
        let err = execute_js_stub(script).expect_err("js_stub は常に失敗する");
        assert!(matches!(err, Error::JsExecutionUnavailable { .. }));
        assert!(!err.to_string().contains(script));
    }

    /// REPAIR-9: recorder 付きの呼び出しは `JsStub` / `Failure { JsExecutionUnavailable }` を 1 件記録する。
    #[test]
    fn repair_9_execute_js_stub_with_options_records_failure() {
        use crate::observability::{FailureKind, InMemoryRecorder, OperationOutcome};

        let rec = Arc::new(InMemoryRecorder::with_capacity(8));
        let options = JsStubOptions::new().with_recorder(rec.clone());
        let err = execute_js_stub_with_options("1 + 1", &options).expect_err("常に失敗する");
        assert!(matches!(err, Error::JsExecutionUnavailable { .. }));

        let records = rec.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].operation(), OperationKind::JsStub);
        assert_eq!(
            records[0].outcome(),
            OperationOutcome::Failure {
                kind: FailureKind::JsExecutionUnavailable
            }
        );
    }
}
