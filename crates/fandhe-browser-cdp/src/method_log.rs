//! 受信した CDP メソッド名の集計ログ（TASK-42.6・#244、ビヘイビア `CDP-6`・`SEC-2`・MS-4）。
//!
//! `crate::protocol::Dispatcher::dispatch_on` が、パースと名前検証を通ったリクエストごとに
//! [`ReceivedMethodLog::record`] を呼ぶ。PoC-5 で行っていた「受信メソッドを記録して必須
//! メソッドを洗い出す」用途を、成功フォールバックなしで引き継ぐ部品で、[`crate::CdpState`]
//! が 1 つ保持する。外部（cli・将来の TASK-10.3 / #221 の出力先）へは読み取り専用の
//! [`ReceivedMethodsSnapshot`] だけを公開する。
//!
//! 記録するのはメソッド名と件数のみ（`id`・`sessionId`・`params` は記録しない）。メソッド名は
//! クライアントが自由に決められるため、未実装メソッドとして区別して保持する名前数に
//! [`MAX_TRACKED_METHODS`] の上限を設け、超過分は件数だけ数える（無制限メモリ確保による DoS の
//! 防止）。ハンドラ登録済みメソッド（`Handled`）は名前集合がハンドラ登録数で有界なため上限の
//! 対象外とし、未実装名の枠を食わない。上限到達後に初めて届いた未実装名は保持しないが、
//! [`MAX_OVERFLOW_REPORTS`] 件までは呼び出し側がログ出力できるよう
//! [`RecordResult::DroppedReport`] を返し、必須メソッドの洗い出し（`CDP-6`）を妨げない。

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

/// 区別して保持するメソッド名数の上限。
pub(crate) const MAX_TRACKED_METHODS: usize = 256;

/// 保持上限到達後に、名前を保持せず呼び出し側へ出力を促す回数の上限（ログ洪水の防止）。
pub(crate) const MAX_OVERFLOW_REPORTS: u64 = 1024;

/// メソッドの処理結果の区分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MethodDisposition {
    /// ハンドラが登録されていた（ハンドラ自身の成否は問わない）。
    Handled,
    /// ハンドラが無く `-32601` を返した。
    Unimplemented,
}

/// [`ReceivedMethodLog::record`] の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordResult {
    /// 初めて見た名前として保持した。
    FirstSeen,
    /// 既に保持している名前のカウントを増やした。
    Counted,
    /// 保持上限に達していたため名前を保持せず、破棄件数だけ増やした（出力枠も使い切り済み）。
    Dropped,
    /// 保持上限に達していたため名前は保持しないが、出力枠が残っているので呼び出し側は
    /// 名前を 1 回ログへ出してよい（同名の重複は排除しない。枠は [`MAX_OVERFLOW_REPORTS`]）。
    DroppedReport,
}

/// メソッド 1 つ分の受信件数。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct MethodCounts {
    /// ハンドラが登録されていたリクエスト数。
    pub handled: u64,
    /// 未実装として `-32601` を返したリクエスト数。
    pub unimplemented: u64,
}

/// 受信メソッド集計の読み取り専用スナップショット（`CDP-6`）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReceivedMethodsSnapshot {
    /// メソッド名順に整列した件数表。
    pub methods: Vec<(String, MethodCounts)>,
    /// 保持上限を超えたため名前を保持できなかったリクエスト数。
    pub dropped: u64,
}

#[derive(Default)]
struct Inner {
    methods: HashMap<String, MethodCounts>,
    dropped: u64,
    overflow_reports: u64,
}

/// 上限付きの受信メソッド集計。
#[derive(Default)]
pub(crate) struct ReceivedMethodLog {
    inner: Mutex<Inner>,
}

impl ReceivedMethodLog {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// メソッド名 1 件を記録する。`method` は呼び出し側で名前検証済みであること。
    pub(crate) fn record(&self, method: &str, kind: MethodDisposition) -> RecordResult {
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let first = !g.methods.contains_key(method);
        // 上限は未実装名だけに適用する（Handled はハンドラ登録数で有界）。
        let tracked_unimpl = g.methods.values().filter(|c| c.unimplemented > 0).count();
        if first
            && kind == MethodDisposition::Unimplemented
            && tracked_unimpl >= MAX_TRACKED_METHODS
        {
            g.dropped = g.dropped.saturating_add(1);
            if g.overflow_reports < MAX_OVERFLOW_REPORTS {
                g.overflow_reports += 1;
                return RecordResult::DroppedReport;
            }
            return RecordResult::Dropped;
        }
        let c = g.methods.entry(method.to_owned()).or_default();
        match kind {
            MethodDisposition::Handled => c.handled = c.handled.saturating_add(1),
            MethodDisposition::Unimplemented => c.unimplemented = c.unimplemented.saturating_add(1),
        }
        if first {
            RecordResult::FirstSeen
        } else {
            RecordResult::Counted
        }
    }

    /// 現在の集計を名前順で複製して返す。
    pub(crate) fn snapshot(&self) -> ReceivedMethodsSnapshot {
        let g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let mut methods: Vec<(String, MethodCounts)> =
            g.methods.iter().map(|(k, v)| (k.clone(), *v)).collect();
        methods.sort_by(|a, b| a.0.cmp(&b.0));
        ReceivedMethodsSnapshot {
            methods,
            dropped: g.dropped,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cdp6_method_log_counts_handled_and_unimplemented_separately() {
        let log = ReceivedMethodLog::new();
        log.record("A.b", MethodDisposition::Handled);
        log.record("A.b", MethodDisposition::Unimplemented);
        log.record("A.b", MethodDisposition::Unimplemented);
        let s = log.snapshot();
        assert_eq!(s.methods.len(), 1);
        assert_eq!(s.methods[0].0, "A.b");
        assert_eq!(s.methods[0].1.handled, 1);
        assert_eq!(s.methods[0].1.unimplemented, 2);
        assert_eq!(s.dropped, 0);
    }

    #[test]
    fn cdp6_method_log_reports_first_occurrence_only_once() {
        let log = ReceivedMethodLog::new();
        assert_eq!(
            log.record("X.y", MethodDisposition::Unimplemented),
            RecordResult::FirstSeen
        );
        assert_eq!(
            log.record("X.y", MethodDisposition::Unimplemented),
            RecordResult::Counted
        );
    }

    #[test]
    fn cdp6_method_log_caps_distinct_methods_and_counts_dropped() {
        let log = ReceivedMethodLog::new();
        for i in 0..MAX_TRACKED_METHODS + 3 {
            let r = log.record(&format!("M.m{i}"), MethodDisposition::Unimplemented);
            if i < MAX_TRACKED_METHODS {
                assert_eq!(r, RecordResult::FirstSeen);
            } else {
                assert_eq!(r, RecordResult::DroppedReport);
            }
        }
        assert_eq!(
            log.record("M.m0", MethodDisposition::Unimplemented),
            RecordResult::Counted
        );
        let s = log.snapshot();
        assert_eq!(s.methods.len(), MAX_TRACKED_METHODS);
        assert_eq!(s.dropped, 3);
        let m0 = s.methods.iter().find(|(k, _)| k == "M.m0").unwrap().1;
        assert_eq!(m0.unimplemented, 2);
    }

    #[test]
    fn cdp6_method_log_handled_names_do_not_consume_unimplemented_slots() {
        let log = ReceivedMethodLog::new();
        for i in 0..MAX_TRACKED_METHODS {
            log.record(&format!("H.h{i}"), MethodDisposition::Handled);
        }
        assert_eq!(
            log.record("New.method", MethodDisposition::Unimplemented),
            RecordResult::FirstSeen
        );
    }

    #[test]
    fn cdp6_method_log_overflow_reports_are_bounded() {
        let log = ReceivedMethodLog::new();
        for i in 0..MAX_TRACKED_METHODS {
            log.record(&format!("M.m{i}"), MethodDisposition::Unimplemented);
        }
        let mut reports = 0u64;
        for i in 0..MAX_OVERFLOW_REPORTS + 5 {
            if log.record(&format!("O.o{i}"), MethodDisposition::Unimplemented)
                == RecordResult::DroppedReport
            {
                reports += 1;
            }
        }
        assert_eq!(reports, MAX_OVERFLOW_REPORTS);
        assert_eq!(log.snapshot().dropped, MAX_OVERFLOW_REPORTS + 5);
    }

    #[test]
    fn cdp6_method_log_snapshot_is_sorted_and_deterministic() {
        let log = ReceivedMethodLog::new();
        for n in ["Z.z", "A.a", "M.m"] {
            log.record(n, MethodDisposition::Handled);
        }
        let names: Vec<String> = log.snapshot().methods.into_iter().map(|(k, _)| k).collect();
        assert_eq!(names, vec!["A.a", "M.m", "Z.z"]);
    }
}
