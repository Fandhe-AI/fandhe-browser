//! report: 実行不能項目の記録（`PLUG-10`・TASK-101.5・MS-8・Issue #277）。
//!
//! WPT サブセット（PoC-16 が選んだ 257 件）のうち、本ハーネスが実行できない
//! reftest・other を「なぜ実行しないか」と「その確度」付きで機械可読に書き出す。
//! #276（TASK-101.4）が作るレポート全体の `"unrunnable"` キーへ [`UnrunnableReport::to_json`]
//! の出力を埋め込む前提で、本モジュールはこのセクション単体のスキーマだけを決める。
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
    /// `file` が不正（空・長さ超過）。
    InvalidFile {
        /// 理由。
        reason: &'static str,
    },
    /// 同じ `file` が重複。
    Duplicate {
        /// 重複したファイル。
        file: String,
    },
}

impl fmt::Display for ReportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReportError::UnknownHarness { label } => write!(f, "unknown harness label: {label:?}"),
            ReportError::TooManyEntries => write!(f, "too many report entries"),
            ReportError::InvalidFile { reason } => write!(f, "invalid file entry: {reason}"),
            ReportError::Duplicate { file } => write!(f, "duplicate file entry: {file:?}"),
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
}
