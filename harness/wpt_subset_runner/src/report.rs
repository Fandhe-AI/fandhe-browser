//! report: 合格率の集計・レポート出力（TASK-101.4・Issue #276）と実行不能項目の記録
//! （TASK-101.5・Issue #277）。ともに `PLUG-10`・MS-8。
//!
//! # 合格率レポート（TASK-101.4）
//!
//! [`WptReport::from_runs`] が [`run_subset_for_profiles`](crate::runner::run_subset_for_profiles)
//! の戻り値（[`ProfileRun`]）をプロファイル別に集計し、[`WptReport::to_json`] がスキーマ固定の
//! 機械可読 JSON（`schemaVersion: 1`）を書き出す。分母は「実行を試みた件数」
//! （testharness 種別。実行に失敗したファイルも含める）、分子は [`Verdict::Pass`] のみで、
//! 実行失敗を分母から外して合格率を良く見せない（REPAIR-3）。
//! 現時点では TASK-100（`PLUG-8`）の gating が未提供のため、chrome と safari の数値は同一になる。
//!
//! # 実行不能項目の記録（TASK-101.5）
//!
//! WPT サブセット（PoC-16 が選んだ 257 件）のうち、本ハーネスが実行できない
//! reftest・other を「なぜ実行しないか」と「その確度」付きで機械可読に書き出す。
//! [`WptReport`] の `"unrunnable"` キーへ [`UnrunnableReport::to_json`] の出力をそのまま
//! 埋め込む。このセクション単体のスキーマは [`UnrunnableReport`] が決める。
//!
//! # 確度の区別
//!
//! - reftest: `match`/`mismatch` は同一ブラウザーで描画して比較すれば原理上は実行できる。
//!   実行できないのは本ハーネスが描画比較（ピクセル比較等）を実装していないという実装上の制約のため
//!   （ハーネスの機能範囲から確認できる事実。[`Basis::Confirmed`]。第 2 エンジンが要るとは主張しない）
//! - other: ファイル内容を確かめておらず、実行できない可能性が高いという推測
//!   （[`Basis::Speculative`]）。推測を確認済みに見せかけない（REPAIR-3）
//!
//! # 方針との関係
//!
//! ここで記録するのは理由だけ。対象外にする方針の承認は人間担当の #279（TASK-101.h1）が
//! 決めるため、「excluded」「approved」など方針が決まったように読める語は使わない。
//!
//! 入力は `wpt-subset.json` の `harness` 列挙文字列と `file` の組。JSON パーサーを
//! 持たない（依存最小）ので、ライブラリは JSON を読まず手書きで書き出すだけにする。

use std::collections::HashSet;
use std::fmt::{self, Write as _};

use fandhe_browser_core::EngineKind;

use crate::runner::{FileOutcome, ProfileRun, Verdict, WptProfile};

/// 記録する件数の上限（`wpt-subset.json` の上限と同じ。無制限確保による DoS を防ぐ）。
pub const MAX_REPORT_ENTRIES: usize = 10_000;
/// `file` の最大バイト数。
pub const MAX_FILE_BYTES: usize = 4 * 1024;

/// 実行不能の確度。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// 確認済み。
    Confirmed,
    /// 推測（内容未検証）。
    Speculative,
}

impl Basis {
    /// 出力に使う固定コード。
    pub fn code(&self) -> &'static str {
        match self {
            Basis::Confirmed => "confirmed",
            Basis::Speculative => "speculative",
        }
    }
}

/// 実行不能の理由。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnrunnableReason {
    /// reftest: 本ハーネスが描画比較を実装していない（実装上の制約。確認済み）。
    ReftestComparisonNotImplemented,
    /// other: 内容未検証で、実行できない可能性が高い（推測）。
    UnverifiedLikelyUnrunnable,
}

impl UnrunnableReason {
    /// 出力順（宣言順）で固定した全理由。
    const ALL: [UnrunnableReason; 2] = [
        UnrunnableReason::ReftestComparisonNotImplemented,
        UnrunnableReason::UnverifiedLikelyUnrunnable,
    ];

    fn index(&self) -> usize {
        match self {
            UnrunnableReason::ReftestComparisonNotImplemented => 0,
            UnrunnableReason::UnverifiedLikelyUnrunnable => 1,
        }
    }

    /// 出力に使う固定の機械可読コード。
    pub fn code(&self) -> &'static str {
        match self {
            UnrunnableReason::ReftestComparisonNotImplemented => {
                "reftest-comparison-not-implemented"
            }
            UnrunnableReason::UnverifiedLikelyUnrunnable => "unverified-likely-unrunnable",
        }
    }

    /// 理由の確度。
    pub fn basis(&self) -> Basis {
        match self {
            UnrunnableReason::ReftestComparisonNotImplemented => Basis::Confirmed,
            UnrunnableReason::UnverifiedLikelyUnrunnable => Basis::Speculative,
        }
    }

    /// 対応する `wpt-subset.json` の harness ラベル。
    pub fn harness_label(&self) -> &'static str {
        match self {
            UnrunnableReason::ReftestComparisonNotImplemented => "reftest",
            UnrunnableReason::UnverifiedLikelyUnrunnable => "other",
        }
    }

    /// 英語の説明文。
    pub fn description(&self) -> &'static str {
        match self {
            UnrunnableReason::ReftestComparisonNotImplemented => {
                "reftest needs a rendering comparison against a reference page; this harness does not implement it"
            }
            UnrunnableReason::UnverifiedLikelyUnrunnable => {
                "content not verified; likely unrunnable (speculative)"
            }
        }
    }
}

/// 記録処理のエラー（fail-closed）。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportError {
    /// 未知の harness ラベル。
    UnknownHarness {
        /// 受け取ったラベル。
        label: String,
    },
    /// 件数が上限超過。
    TooManyEntries,
    /// `file` が不正（空・長さ超過・許可文字外・絶対パス・`..` セグメント）。
    InvalidFile {
        /// 理由。
        reason: &'static str,
    },
    /// 同じ `file` が重複。
    Duplicate {
        /// 重複したファイル。
        file: String,
    },
    /// 集計対象のプロファイルが 1 件も無い（TASK-101.4）。
    NoProfiles,
    /// 同じプロファイルの実行結果が複数ある（TASK-101.4）。
    DuplicateProfile {
        /// 重複したプロファイル。
        profile: WptProfile,
    },
    /// プロファイル間でエントリ列（`file`・種別の並び）が一致しない（TASK-101.4）。
    EntryMismatch {
        /// 先頭と食い違ったプロファイル。
        profile: WptProfile,
    },
    /// 実行不能件数と `Skipped` の件数が一致しない内部不整合（TASK-101.4）。
    InconsistentSkipCount {
        /// 食い違ったプロファイル。
        profile: WptProfile,
    },
}

impl fmt::Display for ReportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReportError::UnknownHarness { label } => write!(f, "unknown harness label: {label:?}"),
            ReportError::TooManyEntries => write!(f, "too many report entries"),
            ReportError::InvalidFile { reason } => write!(f, "invalid file entry: {reason}"),
            ReportError::Duplicate { file } => write!(f, "duplicate file entry: {file:?}"),
            ReportError::NoProfiles => write!(f, "no profile runs to aggregate"),
            ReportError::DuplicateProfile { profile } => {
                write!(f, "duplicate profile run: {profile}")
            }
            ReportError::EntryMismatch { profile } => {
                write!(
                    f,
                    "entries of profile '{profile}' differ from the first profile"
                )
            }
            ReportError::InconsistentSkipCount { profile } => {
                write!(
                    f,
                    "skipped count of profile '{profile}' disagrees with unrunnable total"
                )
            }
        }
    }
}

impl std::error::Error for ReportError {}

/// harness ラベルを実行不能理由へ分類する。testharness は実行対象なので `None`。
/// 未知のラベルは `Err`（fail-closed）。
pub fn classify_unrunnable(harness_label: &str) -> Result<Option<UnrunnableReason>, ReportError> {
    match harness_label {
        "testharness" => Ok(None),
        "reftest" => Ok(Some(UnrunnableReason::ReftestComparisonNotImplemented)),
        "other" => Ok(Some(UnrunnableReason::UnverifiedLikelyUnrunnable)),
        other => Err(ReportError::UnknownHarness {
            label: other.to_string(),
        }),
    }
}

/// 理由ごとにまとめた実行不能項目。#276 のレポートが埋め込む。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnrunnableReport {
    files: [Vec<String>; 2],
}

impl UnrunnableReport {
    /// `(harness ラベル, file)` の列から作る。
    pub fn from_entries<'a, I>(entries: I) -> Result<Self, ReportError>
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        Self::from_entries_with_limit(entries, MAX_REPORT_ENTRIES)
    }

    pub(crate) fn from_entries_with_limit<'a, I>(
        entries: I,
        max_entries: usize,
    ) -> Result<Self, ReportError>
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        let mut files: [Vec<String>; 2] = [Vec::new(), Vec::new()];
        let mut seen: HashSet<&str> = HashSet::new();
        let mut count = 0usize;
        for (label, file) in entries {
            count += 1;
            if count > max_entries {
                return Err(ReportError::TooManyEntries);
            }
            if file.is_empty() {
                return Err(ReportError::InvalidFile { reason: "empty" });
            }
            if file.len() > MAX_FILE_BYTES {
                return Err(ReportError::InvalidFile { reason: "too long" });
            }
            if file.starts_with('/') {
                return Err(ReportError::InvalidFile {
                    reason: "absolute path",
                });
            }
            if !file
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b'-'))
            {
                return Err(ReportError::InvalidFile {
                    reason: "disallowed character",
                });
            }
            if file.split('/').any(|seg| seg == "..") {
                return Err(ReportError::InvalidFile {
                    reason: "parent segment",
                });
            }
            if !seen.insert(file) {
                return Err(ReportError::Duplicate {
                    file: file.to_string(),
                });
            }
            if let Some(reason) = classify_unrunnable(label)?
                && let Some(list) = files.get_mut(reason.index())
            {
                list.push(file.to_string());
            }
        }
        for list in files.iter_mut() {
            list.sort();
        }
        Ok(Self { files })
    }

    /// 理由に対応するファイル一覧（昇順）。
    pub fn records(&self, reason: UnrunnableReason) -> &[String] {
        self.files.get(reason.index()).map_or(&[], Vec::as_slice)
    }

    /// 理由ごとの件数。
    pub fn count(&self, reason: UnrunnableReason) -> usize {
        self.records(reason).len()
    }

    /// 全理由の合計件数。
    pub fn total(&self) -> usize {
        self.files.iter().map(Vec::len).sum()
    }

    /// セクション 1 個分のコンパクトな JSON を返す（キー構成は 0 件でも固定）。
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        let _ = write!(
            out,
            "{{\"schemaVersion\":1,\"total\":{},\"byReason\":[",
            self.total()
        );
        for (i, reason) in UnrunnableReason::ALL.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("{\"reason\":");
            write_json_string(&mut out, reason.code());
            out.push_str(",\"basis\":");
            write_json_string(&mut out, reason.basis().code());
            out.push_str(",\"harness\":");
            write_json_string(&mut out, reason.harness_label());
            out.push_str(",\"description\":");
            write_json_string(&mut out, reason.description());
            let _ = write!(out, ",\"count\":{},\"files\":[", self.count(*reason));
            for (j, file) in self.records(*reason).iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                write_json_string(&mut out, file);
            }
            out.push_str("]}");
        }
        out.push_str("]}");
        out
    }
}

/// `byOutcome` に出す全キー（宣言順で固定。0 件でも省略しない）。
const OUTCOME_KEYS: [&str; 15] = [
    "skipped",
    "missing",
    "readFailed",
    "tooLarge",
    "limitExceeded",
    "htmlParseFailed",
    "harnessNotReferenced",
    "harnessLoadFailed",
    "unsupportedScript",
    "supportScriptMissing",
    "scriptRejected",
    "scriptFailed",
    "engineUnavailable",
    "collectFailed",
    "completed",
];
/// `byOutcome` 内の `skipped` の位置。
const SKIPPED_INDEX: usize = 0;
/// `byVerdict` に出す全キー（宣言順で固定）。
const VERDICT_KEYS: [&str; 4] = ["pass", "fail", "noResults", "incomplete"];

/// [`FileOutcome`] を [`OUTCOME_KEYS`] の位置へ対応させる。
/// ワイルドカードを使わず、バリアント追加時にコンパイルエラーで集計漏れを検出する。
fn outcome_index(o: &FileOutcome) -> usize {
    match o {
        FileOutcome::Skipped { .. } => 0,
        FileOutcome::Missing => 1,
        FileOutcome::ReadFailed { .. } => 2,
        FileOutcome::TooLarge => 3,
        FileOutcome::LimitExceeded { .. } => 4,
        FileOutcome::HtmlParseFailed { .. } => 5,
        FileOutcome::HarnessNotReferenced => 6,
        FileOutcome::HarnessLoadFailed { .. } => 7,
        FileOutcome::UnsupportedScript { .. } => 8,
        FileOutcome::SupportScriptMissing { .. } => 9,
        FileOutcome::ScriptRejected { .. } => 10,
        FileOutcome::ScriptFailed { .. } => 11,
        FileOutcome::EngineUnavailable { .. } => 12,
        FileOutcome::CollectFailed { .. } => 13,
        FileOutcome::Completed { .. } => 14,
    }
}

/// [`Verdict`] を [`VERDICT_KEYS`] の位置へ対応させる（網羅 match）。
fn verdict_index(v: &Verdict) -> usize {
    match v {
        Verdict::Pass => 0,
        Verdict::Fail => 1,
        Verdict::NoResults => 2,
        Verdict::Incomplete => 3,
    }
}

/// 1 プロファイル分の集計（`PLUG-10`・TASK-101.4）。
///
/// 分母 `executed` は `Skipped` 以外の全件（実行失敗を含む）、分子 `passed` は
/// `Completed` かつ [`Verdict::Pass`] の件数。[`WptReport::from_runs`] が作る。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileSummary {
    /// 集計したプロファイル。
    pub profile: WptProfile,
    /// 入力エントリ数。
    pub total: usize,
    /// 実行を試みた件数（`total` − `skipped`。分母）。
    pub executed: usize,
    /// 合格件数（分子）。
    pub passed: usize,
    by_verdict: [usize; 4],
    by_outcome: [usize; 15],
}

impl ProfileSummary {
    fn from_run(run: &ProfileRun) -> Self {
        let mut by_verdict = [0usize; 4];
        let mut by_outcome = [0usize; 15];
        let mut passed = 0usize;
        for (_, outcome) in &run.results {
            if let Some(c) = by_outcome.get_mut(outcome_index(outcome)) {
                *c += 1;
            }
            if let FileOutcome::Completed { verdict, .. } = outcome {
                if let Some(c) = by_verdict.get_mut(verdict_index(verdict)) {
                    *c += 1;
                }
                if matches!(verdict, Verdict::Pass) {
                    passed += 1;
                }
            }
        }
        let total = run.results.len();
        let skipped = by_outcome.get(SKIPPED_INDEX).copied().unwrap_or(0);
        Self {
            profile: run.profile,
            total,
            executed: total.saturating_sub(skipped),
            passed,
            by_verdict,
            by_outcome,
        }
    }

    /// `Skipped`（reftest・other）の件数。
    pub fn skipped(&self) -> usize {
        self.by_outcome.get(SKIPPED_INDEX).copied().unwrap_or(0)
    }

    /// `byOutcome` のキー名（`skipped`・`missing`・…・`completed`）で件数を引く。未知のキーは 0。
    pub fn outcome_count(&self, key: &str) -> usize {
        OUTCOME_KEYS
            .iter()
            .position(|k| *k == key)
            .and_then(|i| self.by_outcome.get(i))
            .copied()
            .unwrap_or(0)
    }

    /// `Completed` の内訳（`pass`・`fail`・`noResults`・`incomplete`）。未知のキーは 0。
    pub fn verdict_count(&self, key: &str) -> usize {
        VERDICT_KEYS
            .iter()
            .position(|k| *k == key)
            .and_then(|i| self.by_verdict.get(i))
            .copied()
            .unwrap_or(0)
    }

    /// 合格率（0.0〜1.0）。`executed == 0` は `None`（0% と区別する）。
    pub fn pass_rate(&self) -> Option<f64> {
        if self.executed == 0 {
            None
        } else {
            Some(self.passed as f64 / self.executed as f64)
        }
    }
}

/// プロファイル別合格率レポート全体（`PLUG-10`・TASK-101.4・MS-8・Issue #276）。
///
/// 入力は [`run_subset_for_profiles`](crate::runner::run_subset_for_profiles) の戻り値。
/// [`WptReport::to_json`] はキー名・型・順序が実行結果に依存しない JSON を返す。
/// `passRate` は小数 4 桁の数値で、`executed == 0` のときは `0.0000`
/// （数値型を保つため。0 件かどうかは `executed` で判別する）。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WptReport {
    /// 実行に使ったエンジン。`None` は JS 無効（JSON では `"none"`）。
    pub engine: Option<EngineKind>,
    /// プロファイル別集計（[`WptProfile::ALL`] の順）。
    pub profiles: Vec<ProfileSummary>,
    /// 実行不能項目（reftest・other）。
    pub unrunnable: UnrunnableReport,
}

impl WptReport {
    /// プロファイル別の実行結果から集計する。入力順に依存せず出力は [`WptProfile::ALL`] 順。
    ///
    /// fail-closed: プロファイル 0 件・重複・プロファイル間のエントリ列不一致・件数上限超過・
    /// 内部不整合は `Err`。
    pub fn from_runs(runs: &[ProfileRun], engine: Option<EngineKind>) -> Result<Self, ReportError> {
        let first = runs.first().ok_or(ReportError::NoProfiles)?;
        if first.results.len() > MAX_REPORT_ENTRIES {
            return Err(ReportError::TooManyEntries);
        }
        let mut seen: Vec<WptProfile> = Vec::new();
        for run in runs {
            if seen.contains(&run.profile) {
                return Err(ReportError::DuplicateProfile {
                    profile: run.profile,
                });
            }
            seen.push(run.profile);
            let same = run.results.len() == first.results.len()
                && run
                    .results
                    .iter()
                    .zip(&first.results)
                    .all(|((a, _), (b, _))| a.file == b.file && a.harness == b.harness);
            if !same {
                return Err(ReportError::EntryMismatch {
                    profile: run.profile,
                });
            }
        }
        let unrunnable = UnrunnableReport::from_entries(
            first
                .results
                .iter()
                .map(|(e, _)| (e.harness.as_str(), e.file.as_str())),
        )?;
        let mut profiles = Vec::new();
        for p in WptProfile::ALL {
            if let Some(run) = runs.iter().find(|r| r.profile == p) {
                let summary = ProfileSummary::from_run(run);
                if summary.skipped() != unrunnable.total() {
                    return Err(ReportError::InconsistentSkipCount { profile: p });
                }
                profiles.push(summary);
            }
        }
        Ok(Self {
            engine,
            profiles,
            unrunnable,
        })
    }

    /// コンパクトな JSON（`schemaVersion: 1`）を返す。キー構成は 0 件でも固定。
    pub fn to_json(&self) -> String {
        let mut out = String::from("{\"schemaVersion\":1,\"behavior\":\"PLUG-10\",\"engine\":");
        write_json_string(&mut out, self.engine.map_or("none", EngineKind::as_str));
        out.push_str(",\"profiles\":[");
        for (i, p) in self.profiles.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("{\"profile\":");
            write_json_string(&mut out, p.profile.as_str());
            let _ = write!(
                out,
                ",\"total\":{},\"executed\":{},\"passed\":{},\"passRate\":{:.4},\"byVerdict\":{{",
                p.total,
                p.executed,
                p.passed,
                p.pass_rate().unwrap_or(0.0)
            );
            for (j, k) in VERDICT_KEYS.iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                let _ = write!(out, "\"{k}\":{}", p.verdict_count(k));
            }
            out.push_str("},\"byOutcome\":{");
            for (j, k) in OUTCOME_KEYS.iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                let _ = write!(out, "\"{k}\":{}", p.outcome_count(k));
            }
            out.push_str("}}");
        }
        out.push_str("],\"unrunnable\":");
        out.push_str(&self.unrunnable.to_json());
        out.push('}');
        out
    }
}
/// JSON 文字列リテラルとして書き出す（`"`・`\`・制御文字をエスケープ）。
fn write_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: UnrunnableReason = UnrunnableReason::ReftestComparisonNotImplemented;
    const O: UnrunnableReason = UnrunnableReason::UnverifiedLikelyUnrunnable;

    #[test]
    fn plug_10_classify_labels() {
        assert_eq!(classify_unrunnable("testharness"), Ok(None));
        assert_eq!(classify_unrunnable("reftest"), Ok(Some(R)));
        assert_eq!(classify_unrunnable("other"), Ok(Some(O)));
        for bad in ["", "Reftest", "crashtest"] {
            assert_eq!(
                classify_unrunnable(bad),
                Err(ReportError::UnknownHarness {
                    label: bad.to_string()
                })
            );
        }
    }

    #[test]
    fn plug_10_reason_codes_are_stable() {
        assert_eq!(R.code(), "reftest-comparison-not-implemented");
        assert_eq!(O.code(), "unverified-likely-unrunnable");
        assert_eq!(R.basis().code(), "confirmed");
        assert_eq!(O.basis().code(), "speculative");
    }

    #[test]
    fn plug_10_report_from_small_fixture() {
        let r = UnrunnableReport::from_entries([
            ("reftest", "b.html"),
            ("testharness", "t.html"),
            ("reftest", "a.html"),
            ("other", "o.html"),
        ])
        .unwrap();
        assert_eq!(r.records(R), ["a.html", "b.html"]);
        assert_eq!(r.records(O), ["o.html"]);
        assert_eq!(r.count(R), 2);
        assert_eq!(r.total(), 3);
    }

    #[test]
    fn plug_10_json_is_exact() {
        let r = UnrunnableReport::from_entries([("reftest", "a.html")]).unwrap();
        let expected = concat!(
            "{\"schemaVersion\":1,\"total\":1,\"byReason\":[",
            "{\"reason\":\"reftest-comparison-not-implemented\",\"basis\":\"confirmed\",",
            "\"harness\":\"reftest\",\"description\":\"reftest needs a rendering comparison against a ",
            "reference page; this harness does not implement it\",\"count\":1,\"files\":[\"a.html\"]},",
            "{\"reason\":\"unverified-likely-unrunnable\",\"basis\":\"speculative\",",
            "\"harness\":\"other\",\"description\":\"content not verified; likely unrunnable ",
            "(speculative)\",\"count\":0,\"files\":[]}]}"
        );
        assert_eq!(r.to_json(), expected);
    }

    #[test]
    fn plug_10_json_escapes_strings() {
        let mut s = String::new();
        write_json_string(&mut s, "a\"b\\c\nd\u{1}é");
        assert_eq!(s, "\"a\\\"b\\\\c\\nd\\u0001é\"");
    }

    #[test]
    fn plug_10_rejects_duplicates_and_empty() {
        assert_eq!(
            UnrunnableReport::from_entries([("other", "x"), ("reftest", "x")]),
            Err(ReportError::Duplicate {
                file: "x".to_string()
            })
        );
        assert_eq!(
            UnrunnableReport::from_entries([("other", "")]),
            Err(ReportError::InvalidFile { reason: "empty" })
        );
    }

    #[test]
    fn plug_10_rejects_unsafe_paths() {
        for (bad, reason) in [
            ("/x", "absolute path"),
            ("../x", "parent segment"),
            ("a/../x", "parent segment"),
            ("a b.html", "disallowed character"),
            ("a\\b.html", "disallowed character"),
            ("é.html", "disallowed character"),
        ] {
            assert_eq!(
                UnrunnableReport::from_entries([("reftest", bad)]),
                Err(ReportError::InvalidFile { reason }),
                "{bad}"
            );
        }
        assert!(UnrunnableReport::from_entries([("reftest", "a/b-c_d.1.html")]).is_ok());
    }

    #[test]
    fn plug_10_entry_limit() {
        let names: Vec<String> = (0..4).map(|i| format!("f{i}.html")).collect();
        let it = || names.iter().map(|n| ("reftest", n.as_str()));
        assert!(UnrunnableReport::from_entries_with_limit(it(), 4).is_ok());
        assert_eq!(
            UnrunnableReport::from_entries_with_limit(it(), 3),
            Err(ReportError::TooManyEntries)
        );
    }

    /// 整形済み `wpt-subset.json` から `(harness, file)` を取り出す（テスト専用）。
    fn scan_subset(json: &str) -> Vec<(String, String)> {
        let val = |line: &str, key: &str| -> Option<String> {
            let rest = line.trim().strip_prefix(&format!("\"{key}\": \""))?;
            Some(
                rest.strip_suffix("\",")
                    .or_else(|| rest.strip_suffix('"'))?
                    .to_string(),
            )
        };
        let mut out = Vec::new();
        let mut file: Option<String> = None;
        for line in json.lines() {
            if let Some(f) = val(line, "file") {
                file = Some(f);
            } else if let Some(h) = val(line, "harness")
                && let Some(f) = file.take()
            {
                out.push((h, f));
            }
        }
        out
    }

    #[test]
    fn plug_10_real_subset_counts_88_and_17() {
        let json = include_str!("../wpt-subset.json");
        let pairs = scan_subset(json);
        assert_eq!(pairs.len(), 257);
        assert!(json.contains("\"totalSelected\": 257,"));
        let r = UnrunnableReport::from_entries(pairs.iter().map(|(h, f)| (h.as_str(), f.as_str())))
            .unwrap();
        assert_eq!(r.count(R), 88);
        assert_eq!(r.count(O), 17);
        assert_eq!(r.total(), 105);
        assert!(
            r.records(O)
                .iter()
                .any(|f| f == "html/webappapis/animation-frames/cancel-handle-manual.html")
        );
    }

    use crate::runner::{HarnessKind, SubsetEntry};

    fn entry(file: &str, h: HarnessKind) -> SubsetEntry {
        SubsetEntry::new(file, h).unwrap()
    }

    fn completed(v: Verdict) -> FileOutcome {
        FileOutcome::Completed {
            subtests: Vec::new(),
            completion: None,
            verdict: v,
        }
    }

    fn skipped(h: HarnessKind) -> FileOutcome {
        FileOutcome::Skipped { harness: h }
    }

    fn sample_run(profile: WptProfile) -> ProfileRun {
        let t = HarnessKind::Testharness;
        ProfileRun {
            profile,
            results: vec![
                (entry("a.html", t), completed(Verdict::Pass)),
                (entry("b.html", t), completed(Verdict::Fail)),
                (entry("c.html", t), completed(Verdict::Incomplete)),
                (entry("d.html", t), completed(Verdict::NoResults)),
                (entry("e.html", t), FileOutcome::Missing),
                (
                    entry("r1.html", HarnessKind::Reftest),
                    skipped(HarnessKind::Reftest),
                ),
                (
                    entry("o1.html", HarnessKind::Other),
                    skipped(HarnessKind::Other),
                ),
            ],
        }
    }

    #[test]
    fn plug_10_report_counts_and_rate() {
        let r = WptReport::from_runs(&[sample_run(WptProfile::Chrome)], None).unwrap();
        let p = r.profiles.first().unwrap();
        assert_eq!((p.total, p.executed, p.passed), (7, 5, 1));
        assert_eq!(p.pass_rate(), Some(0.2));
        assert_eq!(p.skipped(), 2);
        assert_eq!(p.outcome_count("missing"), 1);
        assert_eq!(p.outcome_count("completed"), 4);
        assert_eq!(p.verdict_count("incomplete"), 1);
        assert_eq!(r.unrunnable.total(), 2);
    }

    #[test]
    fn plug_10_report_json_is_exact() {
        let t = HarnessKind::Testharness;
        let run = ProfileRun {
            profile: WptProfile::Safari,
            results: vec![
                (entry("a.html", t), completed(Verdict::Pass)),
                (entry("b.html", t), FileOutcome::Missing),
                (
                    entry("r.html", HarnessKind::Reftest),
                    skipped(HarnessKind::Reftest),
                ),
            ],
        };
        let json = WptReport::from_runs(&[run], Some(EngineKind::Boa))
            .unwrap()
            .to_json();
        let head = concat!(
            "{\"schemaVersion\":1,\"behavior\":\"PLUG-10\",\"engine\":\"boa\",\"profiles\":[",
            "{\"profile\":\"safari\",\"total\":3,\"executed\":2,\"passed\":1,\"passRate\":0.5000,",
            "\"byVerdict\":{\"pass\":1,\"fail\":0,\"noResults\":0,\"incomplete\":0},",
            "\"byOutcome\":{\"skipped\":1,\"missing\":1,\"readFailed\":0,\"tooLarge\":0,",
            "\"limitExceeded\":0,\"htmlParseFailed\":0,\"harnessNotReferenced\":0,",
            "\"harnessLoadFailed\":0,\"unsupportedScript\":0,\"supportScriptMissing\":0,",
            "\"scriptRejected\":0,\"scriptFailed\":0,\"engineUnavailable\":0,",
            "\"collectFailed\":0,\"completed\":1}}],\"unrunnable\":"
        );
        let unrunnable = UnrunnableReport::from_entries([
            ("testharness", "a.html"),
            ("testharness", "b.html"),
            ("reftest", "r.html"),
        ])
        .unwrap()
        .to_json();
        assert_eq!(json, format!("{head}{unrunnable}}}"));
    }

    #[test]
    fn plug_10_report_zero_executed_keeps_schema() {
        let run = ProfileRun {
            profile: WptProfile::Chrome,
            results: vec![(
                entry("r.html", HarnessKind::Reftest),
                skipped(HarnessKind::Reftest),
            )],
        };
        let r = WptReport::from_runs(&[run], None).unwrap();
        assert_eq!(r.profiles.first().unwrap().pass_rate(), None);
        let json = r.to_json();
        assert!(json.contains("\"engine\":\"none\""));
        assert!(json.contains("\"executed\":0,\"passed\":0,\"passRate\":0.0000"));
        // プロファイル節のキー構成は件数があるレポートと同一。
        let full = WptReport::from_runs(&[sample_run(WptProfile::Chrome)], None)
            .unwrap()
            .to_json();
        let section = |s: &str| -> String {
            let start = s.find("\"profiles\"").unwrap();
            let end = s.find("\"unrunnable\"").unwrap();
            let mut keys = String::new();
            let mut in_str = false;
            let mut cur = String::new();
            for c in s[start..end].chars() {
                if c == '"' {
                    if in_str && s[start..end].contains(&format!("\"{cur}\":")) {
                        keys.push_str(&cur);
                        keys.push(',');
                    }
                    cur.clear();
                    in_str = !in_str;
                } else if in_str {
                    cur.push(c);
                }
            }
            keys
        };
        assert_eq!(section(&json), section(&full));
    }

    #[test]
    fn plug_10_report_is_deterministic_and_order_independent() {
        let a = [
            sample_run(WptProfile::Chrome),
            sample_run(WptProfile::Safari),
        ];
        let b = [
            sample_run(WptProfile::Safari),
            sample_run(WptProfile::Chrome),
        ];
        let ja = WptReport::from_runs(&a, None).unwrap().to_json();
        assert_eq!(ja, WptReport::from_runs(&a, None).unwrap().to_json());
        assert_eq!(ja, WptReport::from_runs(&b, None).unwrap().to_json());
        assert_eq!(ja.matches("\"profile\":\"chrome\"").count(), 1);
        assert_eq!(ja.matches("\"profile\":\"safari\"").count(), 1);
    }

    #[test]
    fn plug_10_report_rejects_bad_input() {
        assert_eq!(
            WptReport::from_runs(&[], None),
            Err(ReportError::NoProfiles)
        );
        assert_eq!(
            WptReport::from_runs(
                &[
                    sample_run(WptProfile::Chrome),
                    sample_run(WptProfile::Chrome)
                ],
                None
            ),
            Err(ReportError::DuplicateProfile {
                profile: WptProfile::Chrome
            })
        );
        let mut other = sample_run(WptProfile::Safari);
        other.results.pop();
        assert_eq!(
            WptReport::from_runs(&[sample_run(WptProfile::Chrome), other], None),
            Err(ReportError::EntryMismatch {
                profile: WptProfile::Safari
            })
        );
        // testharness 種別なのに Skipped という、実行不能件数と食い違う入力。
        let mut bad = sample_run(WptProfile::Chrome);
        if let Some(slot) = bad.results.first_mut() {
            slot.1 = skipped(HarnessKind::Testharness);
        }
        assert_eq!(
            WptReport::from_runs(&[bad], None),
            Err(ReportError::InconsistentSkipCount {
                profile: WptProfile::Chrome
            })
        );
    }
}
