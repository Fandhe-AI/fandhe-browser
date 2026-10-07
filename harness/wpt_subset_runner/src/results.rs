//! results: testharness.js のサブテスト結果を Rust 側で受け取る型とコレクタ
//! （`PLUG-10`・TASK-101.2.1・Issue #553）。
//!
//! [`crate::environment`] が JS へ注入するネイティブ関数（`NativeFn`）の実体を作る。
//! JS 側（WPT のテストコード由来）から渡される引数は untrusted なので、型・範囲・
//! 件数・長さを検証してから記録する。違反は黙って捨てず、コレクタに記録して
//! [`ResultCollector::take`] を `Err` にする（fail-closed。REPAIR-3）。
//!
//! # 後続作業の入口
//!
//! 合否の分類は #554（`runner`）、合格率の集計・レポート化は #276、実行不能項目の記録は #277（いずれも
//! `report`）が担う。

use std::sync::{Arc, Mutex, MutexGuard};

use fandhe_browser_core::{JsEngineError, JsValue, NativeFn};

/// 記録するサブテスト数の上限。WPT 1 ファイルのサブテスト数として十分大きい値で、
/// 無制限確保による DoS を防ぐ（coding-rust.md「長さ・件数を上限検証」）。
pub const MAX_SUBTESTS: usize = 10_000;
/// サブテスト名の最大バイト数。名前は識別子なので切り詰めず、超えたら違反にする。
pub const MAX_NAME_BYTES: usize = 8 * 1024;
/// メッセージの最大バイト数。診断用なので UTF-8 文字境界で切り詰め、
/// `message_truncated` で明示する。
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024;

/// サブテストの状態（testharness.js の `Test.statuses` に対応）。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtestStatus {
    /// PASS（0）。
    Pass,
    /// FAIL（1）。
    Fail,
    /// TIMEOUT（2）。
    Timeout,
    /// NOTRUN（3）。
    NotRun,
    /// PRECONDITION_FAILED（4）。
    PreconditionFailed,
}

impl SubtestStatus {
    /// JS の Number から変換する。有限の整数 0..=4 以外は `None`。
    pub fn from_js_number(n: f64) -> Option<Self> {
        match integer_code(n)? {
            0 => Some(Self::Pass),
            1 => Some(Self::Fail),
            2 => Some(Self::Timeout),
            3 => Some(Self::NotRun),
            4 => Some(Self::PreconditionFailed),
            _ => None,
        }
    }
}

/// ハーネス全体の状態（testharness.js の `TestsStatus.statuses` に対応）。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessStatus {
    /// OK（0）。
    Ok,
    /// ERROR（1）。
    Error,
    /// TIMEOUT（2）。
    Timeout,
    /// PRECONDITION_FAILED（3）。
    PreconditionFailed,
}

impl HarnessStatus {
    /// JS の Number から変換する。有限の整数 0..=3 以外は `None`。
    pub fn from_js_number(n: f64) -> Option<Self> {
        match integer_code(n)? {
            0 => Some(Self::Ok),
            1 => Some(Self::Error),
            2 => Some(Self::Timeout),
            3 => Some(Self::PreconditionFailed),
            _ => None,
        }
    }
}

/// 有限の小さな非負整数なら `u8` へ変換する（NaN・無限大・小数・範囲外は `None`）。
fn integer_code(n: f64) -> Option<u8> {
    if n.is_finite() && n.fract() == 0.0 && (0.0..=255.0).contains(&n) {
        // 範囲・整数性を検証済みなので切り捨てが起きない。
        Some(n as u8)
    } else {
        None
    }
}

/// 1 件のサブテスト結果（REPAIR-4: 将来フィールドを足せるよう `non_exhaustive`）。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtestResult {
    /// サブテスト名。
    pub name: String,
    /// 合否。
    pub status: SubtestStatus,
    /// testharness.js が付けたメッセージ。
    pub message: Option<String>,
    /// `message` を [`MAX_MESSAGE_BYTES`] で切り詰めたか。
    pub message_truncated: bool,
}

/// ハーネス完了通知の内容。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessCompletion {
    /// ハーネス全体の状態。
    pub status: HarnessStatus,
    /// メッセージ。
    pub message: Option<String>,
    /// `message` を切り詰めたか。
    pub message_truncated: bool,
}

/// [`ResultCollector::take`] が返す収集結果。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectedResults {
    /// 通知順のサブテスト結果。
    pub subtests: Vec<SubtestResult>,
    /// 完了通知。届いていなければ `None`（microtask の実行はエンジン依存で、
    /// 通知の有無は保証しない。[`crate::environment`] の制限を参照）。
    pub completion: Option<HarnessCompletion>,
}

/// [`ResultCollector::take`] の失敗。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollectError {
    /// JS 側から不正な通知があった。`first` は最初の違反内容。
    Violations {
        /// 違反の件数。
        count: usize,
        /// 最初の違反の説明。
        first: String,
    },
    /// 内部ロックが poison された。
    Poisoned,
}

impl std::fmt::Display for CollectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Violations { count, first } => {
                write!(f, "{count} invalid report(s) from JS; first: {first}")
            }
            Self::Poisoned => write!(f, "result collector lock was poisoned"),
        }
    }
}

impl std::error::Error for CollectError {}

#[derive(Debug)]
struct State {
    subtests: Vec<SubtestResult>,
    completion: Option<HarnessCompletion>,
    violations: usize,
    first_violation: Option<String>,
    max_subtests: usize,
}

impl State {
    fn violate(&mut self, message: &str) -> JsEngineError {
        self.violations = self.violations.saturating_add(1);
        if self.first_violation.is_none() {
            self.first_violation = Some(message.to_string());
        }
        JsEngineError::EvaluationFailed(message.to_string())
    }
}

/// JS から届いた結果を溜めるコレクタ。`Clone` は同じ状態を共有する
/// （注入するネイティブ関数と呼び出し側が同じ状態を見る）。
#[derive(Debug, Clone)]
pub struct ResultCollector {
    state: Arc<Mutex<State>>,
}

impl Default for ResultCollector {
    fn default() -> Self {
        Self::with_max_subtests(MAX_SUBTESTS)
    }
}

impl ResultCollector {
    /// 既定の上限（[`MAX_SUBTESTS`]）でコレクタを作る。
    pub fn new() -> Self {
        Self::default()
    }

    /// サブテスト数の上限を指定して作る（テストで上限到達を速く確認するため）。
    pub(crate) fn with_max_subtests(max_subtests: usize) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                subtests: Vec::new(),
                completion: None,
                violations: 0,
                first_violation: None,
                max_subtests,
            })),
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, State>, JsEngineError> {
        self.state.lock().map_err(|_| {
            JsEngineError::EvaluationFailed("result collector lock was poisoned".to_string())
        })
    }

    /// 溜めた結果を取り出す。違反が 1 件でもあれば `Err`（黙って捨てない）。
    /// 成功時は内部の結果を空にする。
    pub fn take(&self) -> Result<CollectedResults, CollectError> {
        let mut state = self.state.lock().map_err(|_| CollectError::Poisoned)?;
        if state.violations > 0 {
            return Err(CollectError::Violations {
                count: state.violations,
                first: state.first_violation.clone().unwrap_or_default(),
            });
        }
        Ok(CollectedResults {
            subtests: std::mem::take(&mut state.subtests),
            completion: state.completion.take(),
        })
    }

    /// `__fandheWptReportResult(name, status, message)` の実体。
    pub(crate) fn report_result_fn(&self) -> NativeFn {
        let collector = self.clone();
        Box::new(move |args| collector.handle_result(args))
    }

    /// `__fandheWptReportCompletion(status, message)` の実体。
    pub(crate) fn report_completion_fn(&self) -> NativeFn {
        let collector = self.clone();
        Box::new(move |args| collector.handle_completion(args))
    }

    fn handle_result(&self, args: &[JsValue]) -> Result<JsValue, JsEngineError> {
        let mut state = self.lock()?;
        let Some(JsValue::String(name)) = args.first() else {
            return Err(state.violate("report_result: name must be a string"));
        };
        if name.len() > MAX_NAME_BYTES {
            return Err(state.violate("report_result: name is too long"));
        }
        let Some(JsValue::Number(code)) = args.get(1) else {
            return Err(state.violate("report_result: status must be a number"));
        };
        let Some(status) = SubtestStatus::from_js_number(*code) else {
            return Err(state.violate("report_result: status is out of range"));
        };
        let Some((message, message_truncated)) = parse_message(args.get(2)) else {
            return Err(state.violate("report_result: message must be a string or null"));
        };
        if state.subtests.len() >= state.max_subtests {
            return Err(state.violate("report_result: too many subtests"));
        }
        state.subtests.push(SubtestResult {
            name: name.clone(),
            status,
            message,
            message_truncated,
        });
        Ok(JsValue::Undefined)
    }

    fn handle_completion(&self, args: &[JsValue]) -> Result<JsValue, JsEngineError> {
        let mut state = self.lock()?;
        let Some(JsValue::Number(code)) = args.first() else {
            return Err(state.violate("report_completion: status must be a number"));
        };
        let Some(status) = HarnessStatus::from_js_number(*code) else {
            return Err(state.violate("report_completion: status is out of range"));
        };
        let Some((message, message_truncated)) = parse_message(args.get(1)) else {
            return Err(state.violate("report_completion: message must be a string or null"));
        };
        if state.completion.is_some() {
            return Err(state.violate("report_completion: completion was reported twice"));
        }
        state.completion = Some(HarnessCompletion {
            status,
            message,
            message_truncated,
        });
        Ok(JsValue::Undefined)
    }
}

/// メッセージ引数（文字列・null・undefined・省略）を解釈する。型違いは `None`。
fn parse_message(value: Option<&JsValue>) -> Option<(Option<String>, bool)> {
    match value {
        None | Some(JsValue::Null) | Some(JsValue::Undefined) => Some((None, false)),
        Some(JsValue::String(s)) => {
            let (text, truncated) = truncate_utf8(s, MAX_MESSAGE_BYTES);
            Some((Some(text.to_string()), truncated))
        }
        Some(_) => None,
    }
}

/// UTF-8 の文字境界を壊さずに `max_bytes` 以下へ切り詰める。
fn truncate_utf8(s: &str, max_bytes: usize) -> (&str, bool) {
    if s.len() <= max_bytes {
        return (s, false);
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (s.get(..end).unwrap_or(""), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> JsValue {
        JsValue::String(v.to_string())
    }

    /// PLUG-10: ステータス変換は 0..=4 / 0..=3 だけ受理する。
    #[test]
    fn plug_10_status_conversion_table() {
        assert_eq!(
            SubtestStatus::from_js_number(0.0),
            Some(SubtestStatus::Pass)
        );
        assert_eq!(
            SubtestStatus::from_js_number(1.0),
            Some(SubtestStatus::Fail)
        );
        assert_eq!(
            SubtestStatus::from_js_number(2.0),
            Some(SubtestStatus::Timeout)
        );
        assert_eq!(
            SubtestStatus::from_js_number(3.0),
            Some(SubtestStatus::NotRun)
        );
        assert_eq!(
            SubtestStatus::from_js_number(4.0),
            Some(SubtestStatus::PreconditionFailed)
        );
        assert_eq!(HarnessStatus::from_js_number(0.0), Some(HarnessStatus::Ok));
        assert_eq!(
            HarnessStatus::from_js_number(1.0),
            Some(HarnessStatus::Error)
        );
        assert_eq!(
            HarnessStatus::from_js_number(2.0),
            Some(HarnessStatus::Timeout)
        );
        assert_eq!(
            HarnessStatus::from_js_number(3.0),
            Some(HarnessStatus::PreconditionFailed)
        );
        for bad in [-1.0, 5.0, 1.5, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(SubtestStatus::from_js_number(bad), None, "{bad}");
        }
        assert_eq!(HarnessStatus::from_js_number(4.0), None);
    }

    /// PLUG-10: 正常な結果が 1 件記録され、`take` 後は空になる。
    #[test]
    fn plug_10_result_is_recorded() {
        let c = ResultCollector::new();
        let mut f = c.report_result_fn();
        let r = f(&[s("t1"), JsValue::Number(1.0), s("boom")]);
        assert!(matches!(r, Ok(JsValue::Undefined)));
        let out = c.take().expect("違反なし");
        assert_eq!(
            out.subtests,
            vec![SubtestResult {
                name: "t1".to_string(),
                status: SubtestStatus::Fail,
                message: Some("boom".to_string()),
                message_truncated: false,
            }]
        );
        assert_eq!(out.completion, None);
        assert!(c.take().expect("空").subtests.is_empty());
    }

    /// PLUG-10: 名前の型不一致・長さ超過は Err になり、take も Err になる。
    #[test]
    fn plug_10_invalid_name_is_a_violation() {
        let c = ResultCollector::new();
        let mut f = c.report_result_fn();
        assert!(f(&[JsValue::Number(1.0), JsValue::Number(0.0), JsValue::Null]).is_err());
        let long = "a".repeat(MAX_NAME_BYTES + 1);
        assert!(f(&[s(&long), JsValue::Number(0.0), JsValue::Null]).is_err());
        match c.take() {
            Err(CollectError::Violations { count, first }) => {
                assert_eq!(count, 2);
                assert_eq!(first, "report_result: name must be a string");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// PLUG-10: 範囲外ステータスも違反として残る。
    #[test]
    fn plug_10_out_of_range_status_is_a_violation() {
        let c = ResultCollector::new();
        let mut f = c.report_result_fn();
        assert!(f(&[s("x"), JsValue::Number(9.0), JsValue::Null]).is_err());
        assert!(matches!(
            c.take(),
            Err(CollectError::Violations { count: 1, .. })
        ));
    }

    /// PLUG-10: メッセージ超過は文字境界で切り詰められ、フラグが立つ。
    #[test]
    fn plug_10_long_message_is_truncated_at_char_boundary() {
        let c = ResultCollector::new();
        let mut f = c.report_result_fn();
        // 3 バイト文字が境界（MAX_MESSAGE_BYTES）をまたぐ位置に来るようにする。
        let msg = format!("a{}", "あ".repeat(MAX_MESSAGE_BYTES));
        assert!(f(&[s("t"), JsValue::Number(1.0), s(&msg)]).is_ok());
        let out = c.take().expect("違反なし");
        let r = out.subtests.first().expect("1 件");
        assert!(r.message_truncated);
        let m = r.message.as_deref().expect("message");
        assert!(m.len() <= MAX_MESSAGE_BYTES);
        assert_eq!(m.len(), 1 + 3 * ((MAX_MESSAGE_BYTES - 1) / 3));
        assert!(msg.starts_with(m));
    }

    /// PLUG-10: サブテスト数の上限超過は Err になる。
    #[test]
    fn plug_10_too_many_subtests_is_a_violation() {
        let c = ResultCollector::with_max_subtests(2);
        let mut f = c.report_result_fn();
        assert!(f(&[s("a"), JsValue::Number(0.0), JsValue::Null]).is_ok());
        assert!(f(&[s("b"), JsValue::Number(0.0), JsValue::Null]).is_ok());
        assert!(f(&[s("c"), JsValue::Number(0.0), JsValue::Null]).is_err());
        assert!(c.take().is_err());
    }

    /// PLUG-10: completion の 2 回目は Err になる。
    #[test]
    fn plug_10_second_completion_is_a_violation() {
        let c = ResultCollector::new();
        let mut f = c.report_completion_fn();
        assert!(f(&[JsValue::Number(0.0), JsValue::Null]).is_ok());
        assert!(f(&[JsValue::Number(0.0), JsValue::Null]).is_err());
        assert!(c.take().is_err());
    }

    /// PLUG-10: completion が正常に記録される。
    #[test]
    fn plug_10_completion_is_recorded() {
        let c = ResultCollector::new();
        let mut f = c.report_completion_fn();
        assert!(f(&[JsValue::Number(1.0), s("err")]).is_ok());
        let out = c.take().expect("違反なし");
        assert_eq!(
            out.completion,
            Some(HarnessCompletion {
                status: HarnessStatus::Error,
                message: Some("err".to_string()),
                message_truncated: false,
            })
        );
    }

    /// PLUG-10: 引数が足りなくても panic せず Err になる。
    #[test]
    fn plug_10_missing_arguments_do_not_panic() {
        let c = ResultCollector::new();
        assert!(c.report_result_fn()(&[]).is_err());
        assert!(c.report_completion_fn()(&[]).is_err());
        assert!(c.report_result_fn()(&[s("x")]).is_err());
        assert!(c.take().is_err());
    }
}
