//! runner: WPT サブセットのテストファイルを core 経由で実行し、ファイル単位の結果を
//! 分類するランナー本体（`PLUG-10`・TASK-101.2.2・MS-8・Issue #554）。
//!
//! 位置づけ: `fetch-wpt.sh` が固定リビジョンの WPT を取得して `subset.tsv` を書き出し、
//! 本モジュールが [`parse_subset_tsv`] でそれを読み、[`run_entry`] / [`run_subset`] が
//! 1 ファイルずつ次の順で実行する（呼び出し順の契約は [`crate::environment`] 参照）。
//!
//! 1. 種別が testharness 以外（reftest・other）なら読まずに [`FileOutcome::Skipped`]
//! 2. HTML を core の `parse_document` でパースし、`<script>` を文書順に取り出す
//! 3. ファイルごとに新しい `JsRuntime` を作り、`install_testharness_globals` →
//!    testharness.js の評価 → `attach_result_reporter` → テスト側スクリプトの評価
//! 4. `ResultCollector::take` の結果から [`Verdict`] を決める
//!
//! 合格率の集計・レポート（#276）、プロファイル別オプション（#275）、実行不能項目の
//! 記録（#277）は担当外で、本モジュールは [`FileOutcome`] を返すだけで集計しない。
//!
//! # 外部入力の扱い（fail-closed）
//!
//! `subset.tsv` の `file` とテスト HTML 中の `src` は untrusted として扱う。文字種・`..`・
//! URL 形式を検証して字句的に正規化したうえで `Path::join` し、さらに canonicalize 後に
//! WPT ルート配下であることを確認する（symlink 経由の脱出を断つ。open 後の差し替えへの
//! 対策と残存リスクは [`open_under_root`] 参照）。ネットワーク取得は
//! 一切行わず、URL 形式の `src` は [`FileOutcome::ScriptRejected`] にする。
//!
//! # 制限（簡易実装。実装済みを装わない。REPAIR-3）
//!
//! - DOM・タイマーが無いため、DOM を要するテストや `async_test` 系は testharness.js 自身の
//!   判定で失敗するか結果なし（[`Verdict::NoResults`]）になる。偽の DOM で通さない
//! - `type="module"` のスクリプトは評価できないため、含むファイルは実行せず
//!   [`FileOutcome::UnsupportedScript`] にする（classic だけの結果で Pass を装わない）。
//!   JSON などのデータブロックは実行対象ではないため読み飛ばす
//! - `src` 付きの `defer` / `async` classic script も、実行順（defer は文書末・async は
//!   到着順）を再現できないため同様に [`FileOutcome::UnsupportedScript`] にする
//! - スクリプトが 1 つでも評価に失敗したらそのファイルは [`FileOutcome::ScriptFailed`]
//!   として打ち切る（WPT 本来の「失敗しても続行」とは異なる）

use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fandhe_browser_core::js_stub::JsRuntime;
use fandhe_browser_core::{Config, EngineKind, ParseOptions, parse_document};

use crate::environment::{EnvironmentError, attach_result_reporter, install_testharness_globals};
use crate::results::{
    CollectedResults, HarnessCompletion, HarnessStatus, SubtestResult, SubtestStatus,
};

/// `subset.tsv` の最大行数（`wpt-subset.json` のエントリ上限と同じ）。
pub const MAX_SUBSET_ENTRIES: usize = 10_000;
/// `subset.tsv` の 1 行の最大バイト数。
pub const MAX_SUBSET_LINE_BYTES: usize = 4 * 1024;
/// `subset.tsv` 全体の最大バイト数。
pub const MAX_SUBSET_BYTES: usize = 1024 * 1024;
/// 読み込むテスト HTML・外部スクリプト 1 ファイルの最大バイト数。
pub const MAX_SOURCE_BYTES: u64 = 1024 * 1024;

/// 1 ファイルの実行手順（testharness.js・外部サポート・インライン）の最大件数。
///
/// 根拠: 実 WPT のテストは testharness.js・report・数個のヘルパ・テスト本体で通常 10 件未満。
/// 64 件はそれを十分に上回りつつ、細工した HTML によるスクリプト大量参照の DoS を断つ
/// （`PLUG-10`・security.md「不安全な設計」）。
pub const MAX_PLAN_STEPS: usize = 64;
/// 1 ファイルで評価するスクリプト本体の合計最大バイト数。
///
/// 根拠: 1 本の上限（[`MAX_SOURCE_BYTES`] = 1 MiB）の 8 倍。実 WPT では testharness.js
/// （約 0.2 MiB）とヘルパの合計が 1 MiB 前後で、8 MiB なら正当なテストを落とさず、
/// 1 MiB 級のスクリプトを [`MAX_PLAN_STEPS`] 件並べる 64 MiB 超の評価を拒否できる。
/// js crate / core 側の入力上限は 1 回の評価単位で、ファイル全体の総量は本値で制限する。
pub const MAX_TOTAL_SCRIPT_BYTES: u64 = 8 * MAX_SOURCE_BYTES;
/// 1 ファイルの実行にかけられる総時間の上限。
///
/// 根拠: js crate が 1 回の評価を 2 秒で打ち切るため、手順の境界で確認すれば超過は
/// 最大 2 秒。30 秒は正当なテスト（通常 1 秒未満）に十分で、[`MAX_PLAN_STEPS`] 件の全てが
/// 2 秒を使い切る最悪ケース（128 秒）を抑える。
pub const MAX_FILE_DURATION: Duration = Duration::from_secs(30);

/// testharness.js の WPT ルート相対パス（正規化後）。
const TESTHARNESS_PATH: &str = "resources/testharness.js";
/// testharnessreport.js の WPT ルート相対パス（正規化後。`attach_result_reporter` が代替）。
const TESTHARNESSREPORT_PATH: &str = "resources/testharnessreport.js";

/// スクリプト末尾に付ける固定文字列。完了値を `undefined` にして、`test()` の戻り値
/// （Test オブジェクト）が完了値になってもエンジン間で扱いが揺れないようにする。
/// 付けるのは定数だけで、外部文字列は連結しない。
const SCRIPT_SUFFIX: &str = "\n;undefined;\n";

/// 接尾辞込みで [`MAX_SOURCE_BYTES`]（JsRuntime の入力上限）に収まるスクリプト本体の上限。
const MAX_SCRIPT_BYTES: u64 = MAX_SOURCE_BYTES - SCRIPT_SUFFIX.len() as u64;

/// WPT のテスト種別（`wpt-subset.json` の `harness`）。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessKind {
    /// testharness.js を使うテスト。本ランナーの実行対象。
    Testharness,
    /// reftest。実行せずスキップする。
    Reftest,
    /// その他（crashtest 等）。実行せずスキップする。
    Other,
}

impl HarnessKind {
    /// `subset.tsv` / JSON の列挙文字列から変換する。未知の文字列は `None`。
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "testharness" => Some(Self::Testharness),
            "reftest" => Some(Self::Reftest),
            "other" => Some(Self::Other),
            _ => None,
        }
    }
}

/// サブセットの 1 エントリ（WPT ルートからの相対パスと種別）。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubsetEntry {
    /// WPT ルートからの相対パス（`/` 区切り。検証済み）。
    pub file: String,
    /// テスト種別。
    pub harness: HarnessKind,
}

impl SubsetEntry {
    /// 検証済みの値からエントリを作る。`file` は [`validate_relative_path`] を通す。
    pub fn new(file: &str, harness: HarnessKind) -> Result<Self, SubsetError> {
        validate_relative_path(file).map_err(|reason| SubsetError::InvalidFile {
            file: file.to_string(),
            reason,
        })?;
        Ok(Self {
            file: file.to_string(),
            harness,
        })
    }
}

/// [`parse_subset_tsv`] の失敗。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubsetError {
    /// 入力・行数・行長が上限を超えた。
    TooLarge {
        /// 超過した上限の説明。
        what: &'static str,
    },
    /// 行が `<harness>\t<file>` の形式でない。
    MalformedLine {
        /// 1 始まりの行番号。
        line: usize,
    },
    /// `harness` が列挙値でない。
    UnknownHarness {
        /// 1 始まりの行番号。
        line: usize,
    },
    /// `file` がパス規則に違反した。
    InvalidFile {
        /// 違反した値。
        file: String,
        /// 違反の理由。
        reason: &'static str,
    },
    /// 同じ `file` が重複した。
    Duplicate {
        /// 重複した値。
        file: String,
    },
}

impl std::fmt::Display for SubsetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { what } => write!(f, "subset input too large: {what}"),
            Self::MalformedLine { line } => write!(f, "malformed subset line {line}"),
            Self::UnknownHarness { line } => write!(f, "unknown harness kind at line {line}"),
            Self::InvalidFile { file, reason } => write!(f, "invalid file {file:?}: {reason}"),
            Self::Duplicate { file } => write!(f, "duplicate file {file:?}"),
        }
    }
}

impl std::error::Error for SubsetError {}

/// `file` のパス規則（README のスキーマ契約と同じ）。許可文字 `[A-Za-z0-9._/-]` のみ、
/// 先頭 `/` 禁止、`..` セグメント禁止、空セグメント禁止。
pub fn validate_relative_path(file: &str) -> Result<(), &'static str> {
    if file.is_empty() {
        return Err("empty path");
    }
    if file.starts_with('/') {
        return Err("absolute path");
    }
    if !file
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b'-'))
    {
        return Err("invalid character");
    }
    for seg in file.split('/') {
        if seg.is_empty() {
            return Err("empty segment");
        }
        if seg == ".." {
            return Err("parent segment");
        }
    }
    Ok(())
}

/// `fetch-wpt.sh` が書き出す `subset.tsv`（1 行 `<harness>\t<file>`）を読む。
///
/// 件数・行長・全体サイズを上限検証してから確保し、`file` はパス規則と重複を検証する。
/// 1 件でも違反したら全体を `Err` にする（fail-closed）。`\r\n` 改行は許容する。
pub fn parse_subset_tsv(input: &str) -> Result<Vec<SubsetEntry>, SubsetError> {
    if input.len() > MAX_SUBSET_BYTES {
        return Err(SubsetError::TooLarge {
            what: "input bytes",
        });
    }
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    for (idx, raw) in input.lines().enumerate() {
        let line_no = idx.saturating_add(1);
        if raw.is_empty() {
            continue;
        }
        if raw.len() > MAX_SUBSET_LINE_BYTES {
            return Err(SubsetError::TooLarge { what: "line bytes" });
        }
        if entries.len() >= MAX_SUBSET_ENTRIES {
            return Err(SubsetError::TooLarge {
                what: "entry count",
            });
        }
        let mut cols = raw.split('\t');
        let (Some(harness), Some(file), None) = (cols.next(), cols.next(), cols.next()) else {
            return Err(SubsetError::MalformedLine { line: line_no });
        };
        let harness =
            HarnessKind::parse(harness).ok_or(SubsetError::UnknownHarness { line: line_no })?;
        let entry = SubsetEntry::new(file, harness)?;
        if !seen.insert(entry.file.clone()) {
            return Err(SubsetError::Duplicate { file: entry.file });
        }
        entries.push(entry);
    }
    Ok(entries)
}

/// 実行オプション。将来 #275 のプロファイル別指定を足せるよう `non_exhaustive`（REPAIR-4）。
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// 固定リビジョンの WPT ルート（`fetch-wpt.sh` の `wpt-work/wpt`）。
    pub wpt_root: PathBuf,
    /// 使う JS エンジン。`None` は JS 無効（[`FileOutcome::EngineUnavailable`] になる）。
    pub engine: Option<EngineKind>,
    /// 1 ファイルあたりの総量上限（`PLUG-10`）。既定は [`RunLimits::default`]。
    pub limits: RunLimits,
}

impl RunOptions {
    /// WPT ルートとエンジンを指定して作る。上限は既定値。
    pub fn new(wpt_root: impl Into<PathBuf>, engine: Option<EngineKind>) -> Self {
        Self {
            wpt_root: wpt_root.into(),
            engine,
            limits: RunLimits::default(),
        }
    }

    /// 総量上限を差し替える（テスト用に小さな値を渡す用途を想定）。
    pub fn with_limits(mut self, limits: RunLimits) -> Self {
        self.limits = limits;
        self
    }
}

/// 1 ファイルの実行に課す総量上限（件数・合計バイト数・総実行時間）。
///
/// 既定値は [`MAX_PLAN_STEPS`]・[`MAX_TOTAL_SCRIPT_BYTES`]・[`MAX_FILE_DURATION`]。
/// 超過は [`FileOutcome::LimitExceeded`] になり、部分実行の結果で Pass を装わない。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunLimits {
    /// 実行手順の最大件数。
    pub max_steps: usize,
    /// スクリプト本体の合計最大バイト数。
    pub max_total_bytes: u64,
    /// ファイル全体の最大実行時間。
    pub max_duration: Duration,
}

impl Default for RunLimits {
    fn default() -> Self {
        Self {
            max_steps: MAX_PLAN_STEPS,
            max_total_bytes: MAX_TOTAL_SCRIPT_BYTES,
            max_duration: MAX_FILE_DURATION,
        }
    }
}

/// [`FileOutcome::LimitExceeded`] が示す超過した上限の種類。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitKind {
    /// 実行手順の件数。
    Steps,
    /// スクリプト本体の合計バイト数。
    TotalBytes,
    /// ファイル全体の実行時間。
    Duration,
}

/// ファイル単位の合否分類。
///
/// `Pass` はサブテストが 1 件以上あり全て PASS で、かつ completion が `OK` で届いた場合だけ。
/// 0 件は成功を装わず [`Verdict::NoResults`] とする。completion が届いていて `OK` 以外
/// （ERROR・TIMEOUT・PRECONDITION_FAILED）なら、サブテストが全件 PASS でも `Fail` にする。
/// completion が無いと未完了の async_test / promise_test の有無を判別できないため、
/// 全件 PASS でも `Pass` にせず [`Verdict::Incomplete`] とする（REPAIR-3）。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// サブテストが 1 件以上あり、全て PASS。
    Pass,
    /// PASS 以外のサブテストが 1 件以上ある。
    Fail,
    /// サブテストの結果が 1 件も通知されなかった。
    NoResults,
    /// 通知済みサブテストは全て PASS だが completion が届かず、未完了テストが残っていない
    /// と確認できない（合格として数えない）。
    Incomplete,
}

/// サブテスト結果と完了通知から [`Verdict`] を決める。
///
/// completion が届いて `OK` 以外ならハーネス自体が失敗しているため `Fail`（結果 0 件でも
/// `NoResults` にせず `Fail`）。completion が無く全件 PASS なら `Incomplete`（`Pass` にしない）。
pub fn verdict_of(subtests: &[SubtestResult], completion: Option<&HarnessCompletion>) -> Verdict {
    if completion.is_some_and(|c| c.status != HarnessStatus::Ok) {
        Verdict::Fail
    } else if subtests.is_empty() {
        Verdict::NoResults
    } else if !subtests.iter().all(|s| s.status == SubtestStatus::Pass) {
        Verdict::Fail
    } else if completion.is_some() {
        Verdict::Pass
    } else {
        Verdict::Incomplete
    }
}

/// 1 ファイルの実行結果の分類（REPAIR-4: 将来拡張できる列挙）。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileOutcome {
    /// testharness 以外の種別のため実行しなかった。
    Skipped {
        /// エントリの種別。
        harness: HarnessKind,
    },
    /// 固定リビジョンにファイルが無い（黙って選び直さない）。
    Missing,
    /// ファイルの読み込みに失敗した（UTF-8 不正を含む）。
    ReadFailed {
        /// 失敗の説明。
        error: String,
    },
    /// ファイルが [`MAX_SOURCE_BYTES`] を超えた。
    TooLarge,
    /// 1 ファイルの総量上限（[`RunLimits`]）を超えたため実行を打ち切った（`PLUG-10`）。
    LimitExceeded {
        /// 超過した上限の種類。
        kind: LimitKind,
    },
    /// HTML のパースに失敗した。
    HtmlParseFailed {
        /// 失敗の説明。
        error: String,
    },
    /// testharness 種別なのに `/resources/testharness.js` を読み込んでいない。
    HarnessNotReferenced,
    /// testharness.js の評価、または結果アダプタの登録に失敗した。
    HarnessLoadFailed {
        /// 失敗の説明。
        error: String,
    },
    /// `type="module"`、または `src` 付きの `defer` / `async` スクリプトを含む
    /// （評価順を再現できないため実行しない）。
    UnsupportedScript {
        /// `type="module"` なら `type` 属性の値、`defer` / `async` ならその属性名。
        script_type: String,
    },
    /// 参照された外部スクリプトが無い（黙って飛ばさない）。
    SupportScriptMissing {
        /// HTML 中の `src`。
        src: String,
    },
    /// 外部スクリプトの参照がルール違反（URL 形式・ルート外など）で拒否された。
    ScriptRejected {
        /// HTML 中の `src`。
        src: String,
        /// 拒否の理由。
        reason: String,
    },
    /// テスト側スクリプトの評価に失敗した（エンジン再起動・タイムアウトを含む）。
    ScriptFailed {
        /// 文書順（0 始まり）のスクリプト番号。
        index: usize,
        /// 失敗の説明。
        error: String,
    },
    /// JS ランタイムを作れない・環境を構築できない（エンジンなしビルド等）。
    EngineUnavailable {
        /// 失敗の説明。
        error: String,
    },
    /// 結果の回収に失敗した（JS 側から不正な通知があった）。
    CollectFailed {
        /// 失敗の説明。
        error: String,
    },
    /// 実行を完了した。
    Completed {
        /// 通知順のサブテスト結果。
        subtests: Vec<SubtestResult>,
        /// 完了通知（届かないこともある。参考情報）。
        completion: Option<HarnessCompletion>,
        /// ファイル単位の合否。
        verdict: Verdict,
    },
}

/// サブセット全体を順に実行する。集計はしない（#276 の担当）。
pub fn run_subset(
    options: &RunOptions,
    entries: &[SubsetEntry],
) -> Vec<(SubsetEntry, FileOutcome)> {
    entries
        .iter()
        .map(|e| (e.clone(), run_entry(options, e)))
        .collect()
}

/// 1 エントリを実行して分類する。ファイルごとに新しい `JsRuntime` を作る。
pub fn run_entry(options: &RunOptions, entry: &SubsetEntry) -> FileOutcome {
    if entry.harness != HarnessKind::Testharness {
        return FileOutcome::Skipped {
            harness: entry.harness,
        };
    }
    let root = match fs::canonicalize(&options.wpt_root) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return FileOutcome::Missing,
        Err(e) => {
            return FileOutcome::ReadFailed {
                error: e.to_string(),
            };
        }
    };
    let html = match read_under_root(&root, &entry.file) {
        Ok(h) => h,
        Err(outcome) => return outcome,
    };
    let scripts = match collect_scripts(&html, &entry.file, &options.limits) {
        Ok(s) => s,
        Err(CollectError::Parse(error)) => return FileOutcome::HtmlParseFailed { error },
        Err(CollectError::Limit(kind)) => return FileOutcome::LimitExceeded { kind },
    };
    // 未対応スクリプトは JS を実行する前に確定させる（部分実行の結果で Pass にしない）。
    if let Some(ScriptSource::Unsupported(script_type)) = scripts
        .iter()
        .find(|s| matches!(s, ScriptSource::Unsupported(_)))
    {
        return FileOutcome::UnsupportedScript {
            script_type: script_type.clone(),
        };
    }

    // 事前に参照を解決する。拒否・不在は評価前に確定させ、JS を一切実行しない。
    let mut plan: Vec<Step> = Vec::new();
    let mut harness_referenced = false;
    for script in &scripts {
        match script {
            ScriptSource::Unsupported(_) => {}
            ScriptSource::Inline(code) => plan.push(Step::Eval(code.clone())),
            ScriptSource::External(src) => {
                let rel = match resolve_src(&entry.file, src) {
                    Ok(r) => r,
                    Err(reason) => {
                        return FileOutcome::ScriptRejected {
                            src: src.clone(),
                            reason,
                        };
                    }
                };
                if rel == TESTHARNESS_PATH {
                    harness_referenced = true;
                    plan.push(Step::Harness(src.clone(), rel));
                } else {
                    plan.push(Step::Support(src.clone(), rel));
                }
            }
        }
    }
    if !harness_referenced {
        return FileOutcome::HarnessNotReferenced;
    }

    // 実行前に件数と合計サイズを検証する（JS を 1 つも評価しないうちに fail-closed）。
    let limits = options.limits;
    if plan.len() > limits.max_steps {
        return FileOutcome::LimitExceeded {
            kind: LimitKind::Steps,
        };
    }
    let mut planned_bytes: u64 = 0;
    for step in &plan {
        let size = match step {
            Step::Eval(code) => code.len() as u64,
            Step::Harness(src, rel) | Step::Support(src, rel) => {
                match file_size_under_root(&root, rel) {
                    Ok(n) => n,
                    Err(FileOutcome::Missing) => {
                        return FileOutcome::SupportScriptMissing { src: src.clone() };
                    }
                    Err(other) => return other,
                }
            }
        };
        planned_bytes = planned_bytes.saturating_add(size);
        if planned_bytes > limits.max_total_bytes {
            return FileOutcome::LimitExceeded {
                kind: LimitKind::TotalBytes,
            };
        }
    }

    let started = Instant::now();
    let mut runtime = match new_runtime(options.engine) {
        Ok(r) => r,
        Err(error) => return FileOutcome::EngineUnavailable { error },
    };
    let collector = match install_testharness_globals(&mut runtime) {
        Ok(c) => c,
        Err(e) => {
            return FileOutcome::EngineUnavailable {
                error: e.to_string(),
            };
        }
    };

    let mut consumed_bytes: u64 = 0;
    for (index, step) in plan.iter().enumerate() {
        // 1 評価の超過は js crate の評価タイムアウト（2 秒）で抑えられるため、手順の
        // 境界でファイル全体の時間を確認すれば超過は上限 + 評価 1 回分に収まる。
        if started.elapsed() > limits.max_duration {
            return FileOutcome::LimitExceeded {
                kind: LimitKind::Duration,
            };
        }
        match step {
            Step::Harness(src, rel) => {
                let code = match read_under_root(&root, rel) {
                    Ok(c) => c,
                    Err(FileOutcome::Missing) => {
                        return FileOutcome::SupportScriptMissing { src: src.clone() };
                    }
                    Err(other) => return other,
                };
                // 事前検証後にファイルが増えた場合（TOCTOU）に備え、読み込んだ実バイト数でも検証する。
                consumed_bytes = consumed_bytes.saturating_add(code.len() as u64);
                if consumed_bytes > limits.max_total_bytes {
                    return FileOutcome::LimitExceeded {
                        kind: LimitKind::TotalBytes,
                    };
                }
                if let Err(error) = eval(&mut runtime, &code) {
                    return FileOutcome::HarnessLoadFailed { error };
                }
                if let Err(e) = attach_result_reporter(&mut runtime) {
                    return FileOutcome::HarnessLoadFailed {
                        error: reporter_error(&e),
                    };
                }
            }
            Step::Support(src, rel) => {
                let code = match read_under_root(&root, rel) {
                    Ok(c) => c,
                    Err(FileOutcome::Missing) => {
                        return FileOutcome::SupportScriptMissing { src: src.clone() };
                    }
                    Err(other) => return other,
                };
                // 事前検証後にファイルが増えた場合（TOCTOU）に備え、読み込んだ実バイト数でも検証する。
                consumed_bytes = consumed_bytes.saturating_add(code.len() as u64);
                if consumed_bytes > limits.max_total_bytes {
                    return FileOutcome::LimitExceeded {
                        kind: LimitKind::TotalBytes,
                    };
                }
                if let Err(error) = eval(&mut runtime, &code) {
                    return FileOutcome::ScriptFailed { index, error };
                }
            }
            Step::Eval(code) => {
                if code.len() as u64 > MAX_SCRIPT_BYTES {
                    return FileOutcome::TooLarge;
                }
                consumed_bytes = consumed_bytes.saturating_add(code.len() as u64);
                if consumed_bytes > limits.max_total_bytes {
                    return FileOutcome::LimitExceeded {
                        kind: LimitKind::TotalBytes,
                    };
                }
                if let Err(error) = eval(&mut runtime, code) {
                    return FileOutcome::ScriptFailed { index, error };
                }
            }
        }
    }

    if started.elapsed() > limits.max_duration {
        return FileOutcome::LimitExceeded {
            kind: LimitKind::Duration,
        };
    }

    match collector.take() {
        Ok(CollectedResults {
            subtests,
            completion,
            ..
        }) => {
            let verdict = verdict_of(&subtests, completion.as_ref());
            FileOutcome::Completed {
                subtests,
                completion,
                verdict,
            }
        }
        Err(e) => FileOutcome::CollectFailed {
            error: e.to_string(),
        },
    }
}

/// 事前に解決した実行手順。
enum Step {
    /// testharness.js 本体（`src`・ルート相対パス）。評価直後に結果アダプタを登録する。
    Harness(String, String),
    /// 外部のサポートスクリプト（`src`・ルート相対パス）。
    Support(String, String),
    /// インラインスクリプト。
    Eval(String),
}

#[derive(Debug)]
enum ScriptSource {
    Inline(String),
    External(String),
    /// 評価できない種別（`type="module"` ならその `type` 値、`src` 付きの `defer` /
    /// `async` ならその属性名）。
    Unsupported(String),
}

fn reporter_error(e: &EnvironmentError) -> String {
    e.to_string()
}

fn new_runtime(engine: Option<EngineKind>) -> Result<JsRuntime, String> {
    // エンジン名は固定文字列だけを TOML に埋め込む（外部入力は連結しない）。
    let toml = match engine {
        Some(EngineKind::V8) => "[js]\nengine = \"v8\"\n",
        Some(EngineKind::Boa) => "[js]\nengine = \"boa\"\n",
        // JS 無効。既定エンジンへ黙って落とさず、実行不能として返す。
        None => return Err("JS engine is disabled (no engine selected)".to_string()),
    };
    let cfg = Config::from_toml_str(toml).map_err(|e| e.to_string())?;
    JsRuntime::from_config(cfg.js()).map_err(|e| e.to_string())
}

fn eval(runtime: &mut JsRuntime, code: &str) -> Result<(), String> {
    let mut script = String::with_capacity(code.len().saturating_add(SCRIPT_SUFFIX.len()));
    script.push_str(code);
    script.push_str(SCRIPT_SUFFIX);
    runtime
        .execute(&script)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// JS として評価する `type` 属性か（無指定・空・JS の MIME）。
fn is_classic_script_type(ty: Option<&str>) -> bool {
    match ty {
        None => true,
        Some(t) => {
            // MIME パラメータ（`text/javascript; charset=utf-8` 等）は種別判定から除く
            let t = t.split(';').next().unwrap_or_default().trim();
            t.is_empty()
                || [
                    "text/javascript",
                    "application/javascript",
                    "application/x-javascript",
                    "text/ecmascript",
                    "application/ecmascript",
                    "text/x-javascript",
                ]
                .iter()
                .any(|m| t.eq_ignore_ascii_case(m))
        }
    }
}

/// [`collect_scripts`] の失敗。
#[derive(Debug)]
enum CollectError {
    /// HTML のパースに失敗した。
    Parse(String),
    /// 収集中に総量上限（件数・インライン本文の合計バイト数）を超えた。
    Limit(LimitKind),
}

/// HTML をパースして JS の `<script>` を文書順に集める（`src` があれば外部扱い）。
///
/// 集めながら件数（`limits.max_steps`）とインライン本文の合計バイト数
/// （`limits.max_total_bytes`）を逐次判定し、超えた時点で打ち切って `Limit` を返す。
/// 上限を超えるスクリプトを `Vec` に積まない（`PLUG-10`・Issue #554 のレビュー指摘）。
/// `testharnessreport.js` は `attach_result_reporter` が代替し手順にならないため、
/// 件数にも `Vec` にも含めない。評価できない種別（`type="module"`、`src` 付きの
/// `defer` / `async`）を見つけたら、その場で `Unsupported` 1 件だけを返す。
///
/// `defer` / `async` は `src` を持つ classic script にだけ効く（インラインでは無視される）
/// ため、`src` 付きに限って未対応にする。実行順（defer は文書末・async は到着順）は
/// 再現できないので、文書順に実行して Pass を装わない（REPAIR-3）。
///
/// `<template>` の template contents は core の `descendants` が辿らない（別フラグメント）
/// ため、template 内の `<script>` は計画に含まれない（`tests/runner_subset.rs` で検証）。
fn collect_scripts(
    html: &str,
    test_file: &str,
    limits: &RunLimits,
) -> Result<Vec<ScriptSource>, CollectError> {
    let options = ParseOptions::default()
        .with_scripting_enabled(true)
        .with_max_input_bytes(MAX_SOURCE_BYTES as usize);
    let parsed = parse_document(html, &options).map_err(|e| CollectError::Parse(e.to_string()))?;
    let doc = &parsed.document;
    let mut out = Vec::new();
    let mut inline_bytes: u64 = 0;
    for id in doc.descendants(doc.root()) {
        if doc.local_name(id) != Some("script") {
            continue;
        }
        let ty = doc.attribute(id, "type");
        if !is_classic_script_type(ty) {
            if ty.is_some_and(|t| t.trim().eq_ignore_ascii_case("module")) {
                return Ok(vec![ScriptSource::Unsupported(
                    ty.unwrap_or_default().to_string(),
                )]);
            }
            continue;
        }
        match doc.attribute(id, "src") {
            Some(src) => {
                for attr in ["defer", "async"] {
                    if doc.attribute(id, attr).is_some() {
                        return Ok(vec![ScriptSource::Unsupported(attr.to_string())]);
                    }
                }
                if resolve_src(test_file, src).is_ok_and(|rel| rel == TESTHARNESSREPORT_PATH) {
                    continue;
                }
                if out.len() >= limits.max_steps {
                    return Err(CollectError::Limit(LimitKind::Steps));
                }
                out.push(ScriptSource::External(src.to_string()));
            }
            None => {
                if out.len() >= limits.max_steps {
                    return Err(CollectError::Limit(LimitKind::Steps));
                }
                let code = doc.text_content(id).unwrap_or_default();
                inline_bytes = inline_bytes.saturating_add(code.len() as u64);
                if inline_bytes > limits.max_total_bytes {
                    return Err(CollectError::Limit(LimitKind::TotalBytes));
                }
                out.push(ScriptSource::Inline(code));
            }
        }
    }
    Ok(out)
}

/// `<script src>` の値を、テストファイル `test_file`（ルート相対）基準でルート相対パスへ
/// 字句的に解決する。URL 形式・不正文字・ルート外への脱出は `Err`（理由は英語）。
///
/// 純関数。symlink による脱出は呼び出し後の [`open_under_root`] が断つ。
pub fn resolve_src(test_file: &str, src: &str) -> Result<String, String> {
    if src.is_empty() {
        return Err("empty src".to_string());
    }
    if src.starts_with("//") {
        return Err("protocol-relative URL is not allowed".to_string());
    }
    if src.contains(':') {
        return Err("URL with scheme is not allowed".to_string());
    }
    if src.contains('?') || src.contains('#') {
        return Err("query or fragment is not allowed".to_string());
    }
    if src.contains('\\') {
        return Err("backslash is not allowed".to_string());
    }
    let mut segs: Vec<&str> = Vec::new();
    if !src.starts_with('/') {
        // テストファイルのディレクトリ（ファイル名を除く）から始める。
        let mut dir: Vec<&str> = test_file.split('/').collect();
        dir.pop();
        segs = dir;
    }
    for seg in src.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if segs.pop().is_none() {
                    return Err("path escapes the WPT root".to_string());
                }
            }
            s => segs.push(s),
        }
    }
    let rel = segs.join("/");
    validate_relative_path(&rel).map_err(|r| r.to_string())?;
    Ok(rel)
}

/// Windows の `FILE_ATTRIBUTE_REPARSE_POINT`（symlink・junction 等の再解析ポイント）。
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// ルート配下の通常ファイルを開き、ハンドルとサイズを返す（上限超過は TooLarge）。
///
/// # TOCTOU 対策（`PLUG-10`・Issue #554 のレビュー指摘）
///
/// 脅威モデル: 取得ディレクトリは本ツール（`fetch-wpt.sh`）が毎回新しく作り、書き込めるのは
/// ローカル利用者だけである。そのうえで、canonicalize から open までの間に symlink を
/// 差し替えられてもルート外のファイルを読まないよう、検証の順序を「open → ハンドル基準の
/// 種別・サイズ判定 → canonicalize → ルート配下確認 → ハンドルと canonical パスの同一性照合」
/// にする。サイズ判定・読み取り上限も開いたハンドル基準で行う（パスを開き直さない）。
///
/// - Unix: ハンドルの `(dev, ino)` と canonical パスの `(dev, ino)` が一致しなければ拒否する
/// - Windows: ハンドルが再解析ポイントなら拒否する。`(dev, ino)` 相当を std だけでは取れない
///   ため、open 後に途中のディレクトリが差し替えられる残存リスクが Windows に残る
///   （canonicalize 後のルート配下確認で緩和するのみ）
/// - その他の OS: 同一性を確認できないため fail-closed（`ReadFailed`）
fn open_under_root(root: &Path, rel: &str) -> Result<(fs::File, u64), FileOutcome> {
    let mut path = root.to_path_buf();
    for seg in rel.split('/') {
        path.push(seg);
    }
    let file = match fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(FileOutcome::Missing),
        // Windows ではディレクトリを開くと PermissionDenied になる。通常ファイルでないものは
        // Unix 側（`is_file` 判定）と揃えて存在しない扱いにする。
        Err(_) if fs::metadata(&path).is_ok_and(|m| !m.is_file()) => {
            return Err(FileOutcome::Missing);
        }
        Err(e) => {
            return Err(FileOutcome::ReadFailed {
                error: e.to_string(),
            });
        }
    };
    let meta = file.metadata().map_err(|e| FileOutcome::ReadFailed {
        error: e.to_string(),
    })?;
    if !meta.is_file() {
        return Err(FileOutcome::Missing);
    }
    if meta.len() > MAX_SCRIPT_BYTES {
        return Err(FileOutcome::TooLarge);
    }
    let canon = match fs::canonicalize(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(FileOutcome::Missing),
        Err(e) => {
            return Err(FileOutcome::ReadFailed {
                error: e.to_string(),
            });
        }
    };
    if !canon.starts_with(root) {
        // symlink 等でルート外へ出る経路は存在しないものとして扱う。
        return Err(FileOutcome::Missing);
    }
    confirm_same_file(&meta, &canon)?;
    Ok((file, meta.len()))
}

/// 開いたハンドルの metadata と canonical パスが同じファイルであることを確認する（Unix）。
///
/// `(dev, ino)` が一致しなければ、open と canonicalize の間にパスが差し替えられたとみなして
/// 拒否する。
#[cfg(unix)]
fn confirm_same_file(handle: &fs::Metadata, canon: &Path) -> Result<(), FileOutcome> {
    use std::os::unix::fs::MetadataExt;

    let by_path = match fs::metadata(canon) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(FileOutcome::Missing),
        Err(e) => {
            return Err(FileOutcome::ReadFailed {
                error: e.to_string(),
            });
        }
    };
    if handle.dev() == by_path.dev() && handle.ino() == by_path.ino() {
        Ok(())
    } else {
        Err(FileOutcome::ReadFailed {
            error: "file identity changed while opening".to_string(),
        })
    }
}

/// 開いたハンドルが再解析ポイントでないことを確認する（Windows）。
///
/// 残存リスク: 途中のディレクトリの差し替えは検出できない（[`open_under_root`] 参照）。
#[cfg(windows)]
fn confirm_same_file(handle: &fs::Metadata, _canon: &Path) -> Result<(), FileOutcome> {
    use std::os::windows::fs::MetadataExt;

    if handle.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        Err(FileOutcome::ReadFailed {
            error: "reparse point is not allowed".to_string(),
        })
    } else {
        Ok(())
    }
}

/// 同一性を確認できない OS では fail-closed にする。
#[cfg(not(any(unix, windows)))]
fn confirm_same_file(_handle: &fs::Metadata, _canon: &Path) -> Result<(), FileOutcome> {
    Err(FileOutcome::ReadFailed {
        error: "file identity check is not supported on this platform".to_string(),
    })
}

/// ルート配下のファイルのサイズだけを返す（読まない）。総量上限の事前検証用。
fn file_size_under_root(root: &Path, rel: &str) -> Result<u64, FileOutcome> {
    open_under_root(root, rel).map(|(_, len)| len)
}

/// ルート相対パス `rel` のファイルを、ルート配下であることを確認して読む。
///
/// 戻り値の `Err` は、そのまま [`FileOutcome`] として返せる分類
/// （`Missing` / `TooLarge` / `ReadFailed`）。検証は [`open_under_root`] が開いたハンドルで行う。
fn read_under_root(root: &Path, rel: &str) -> Result<String, FileOutcome> {
    let (file, _) = open_under_root(root, rel)?;
    // サイズ確認後にファイルが増えても上限を超えて確保しないよう、読み取り自体を
    // MAX_SCRIPT_BYTES + 1 バイトに制限し、超過は TooLarge に分類する。
    let mut bytes = Vec::new();
    file.take(MAX_SCRIPT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| FileOutcome::ReadFailed {
            error: e.to_string(),
        })?;
    if bytes.len() as u64 > MAX_SCRIPT_BYTES {
        return Err(FileOutcome::TooLarge);
    }
    String::from_utf8(bytes).map_err(|e| FileOutcome::ReadFailed {
        error: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(status: SubtestStatus) -> SubtestResult {
        // non_exhaustive のため JSON 相当の経路ではなく、判定表の検証用に
        // コレクタ経由でなく直接組み立てられる範囲で Default を使う。
        let mut r = fake_result();
        r.status = status;
        r
    }

    fn fake_result() -> SubtestResult {
        use crate::results::ResultCollector;
        // 注入関数と同じ経路（検証付き）で 1 件作る。
        let c = ResultCollector::new();
        let mut f = c.report_result_fn();
        f(&[
            fandhe_browser_core::JsValue::String("t".to_string()),
            fandhe_browser_core::JsValue::Number(0.0),
            fandhe_browser_core::JsValue::Null,
        ])
        .expect("report");
        c.take().expect("take").subtests.remove(0)
    }

    /// PLUG-10: TSV の正常系（`\r\n` 含む）。
    #[test]
    fn parse_subset_tsv_accepts_valid_lines() {
        let got =
            parse_subset_tsv("testharness\tdom/a.html\r\nreftest\tcss/b.html\n\nother\tc/d.html\n")
                .expect("parse");
        assert_eq!(
            got,
            vec![
                SubsetEntry {
                    file: "dom/a.html".to_string(),
                    harness: HarnessKind::Testharness
                },
                SubsetEntry {
                    file: "css/b.html".to_string(),
                    harness: HarnessKind::Reftest
                },
                SubsetEntry {
                    file: "c/d.html".to_string(),
                    harness: HarnessKind::Other
                },
            ]
        );
    }

    /// PLUG-10: 不正な行は全体を Err にする（fail-closed）。
    #[test]
    fn parse_subset_tsv_rejects_invalid_lines() {
        assert_eq!(
            parse_subset_tsv("bogus\ta.html\n"),
            Err(SubsetError::UnknownHarness { line: 1 })
        );
        assert_eq!(
            parse_subset_tsv("testharness\n"),
            Err(SubsetError::MalformedLine { line: 1 })
        );
        assert_eq!(
            parse_subset_tsv("testharness\ta\tb\n"),
            Err(SubsetError::MalformedLine { line: 1 })
        );
        for bad in [
            "../x.html",
            "/abs.html",
            "a/../b.html",
            "a b.html",
            "a//b.html",
        ] {
            let r = parse_subset_tsv(&format!("testharness\t{bad}\n"));
            assert!(
                matches!(r, Err(SubsetError::InvalidFile { .. })),
                "{bad}: {r:?}"
            );
        }
        assert_eq!(
            parse_subset_tsv("testharness\ta.html\nreftest\ta.html\n"),
            Err(SubsetError::Duplicate {
                file: "a.html".to_string()
            })
        );
    }

    /// PLUG-10: 行長・件数・全体サイズの上限。
    #[test]
    fn parse_subset_tsv_enforces_limits() {
        let long = format!("testharness\t{}\n", "a".repeat(MAX_SUBSET_LINE_BYTES));
        assert_eq!(
            parse_subset_tsv(&long),
            Err(SubsetError::TooLarge { what: "line bytes" })
        );
        let many: String = (0..=MAX_SUBSET_ENTRIES)
            .map(|i| format!("other\tf{i}.html\n"))
            .collect();
        assert_eq!(
            parse_subset_tsv(&many),
            Err(SubsetError::TooLarge {
                what: "entry count"
            })
        );
        let huge = "x".repeat(MAX_SUBSET_BYTES + 1);
        assert_eq!(
            parse_subset_tsv(&huge),
            Err(SubsetError::TooLarge {
                what: "input bytes"
            })
        );
    }

    /// PLUG-10: `src` の解決規則（ルート基準・相対・拒否パターン）。
    #[test]
    fn resolve_src_rules() {
        assert_eq!(
            resolve_src("dom/nodes/t.html", "/resources/testharness.js").as_deref(),
            Ok("resources/testharness.js")
        );
        assert_eq!(
            resolve_src("dom/nodes/t.html", "support/h.js").as_deref(),
            Ok("dom/nodes/support/h.js")
        );
        assert_eq!(
            resolve_src("dom/nodes/t.html", "../common.js").as_deref(),
            Ok("dom/common.js")
        );
        assert_eq!(
            resolve_src("dom/nodes/t.html", "./x.js").as_deref(),
            Ok("dom/nodes/x.js")
        );
        for bad in [
            "",
            "https://example.com/a.js",
            "//example.com/a.js",
            "data:text/javascript,1",
            "a.js?x=1",
            "a.js#h",
            "../../../etc/passwd",
            "/../x.js",
            "a\\b.js",
            "/res ources/a.js",
        ] {
            assert!(resolve_src("dom/nodes/t.html", bad).is_err(), "{bad}");
        }
    }

    /// PLUG-10: verdict の判定表（0 件は Pass にしない）。
    #[test]
    fn verdict_table() {
        assert_eq!(verdict_of(&[], None), Verdict::NoResults);
        assert_eq!(
            verdict_of(&[sub(SubtestStatus::Pass)], None),
            Verdict::Incomplete
        );
        assert_eq!(
            verdict_of(&[sub(SubtestStatus::Pass), sub(SubtestStatus::Fail)], None),
            Verdict::Fail
        );
        assert_eq!(
            verdict_of(&[sub(SubtestStatus::NotRun)], None),
            Verdict::Fail
        );
        let done = |status| HarnessCompletion {
            status,
            message: None,
            message_truncated: false,
        };
        let pass = [sub(SubtestStatus::Pass)];
        assert_eq!(
            verdict_of(&pass, Some(&done(HarnessStatus::Ok))),
            Verdict::Pass
        );
        for st in [
            HarnessStatus::Error,
            HarnessStatus::Timeout,
            HarnessStatus::PreconditionFailed,
        ] {
            assert_eq!(verdict_of(&pass, Some(&done(st))), Verdict::Fail);
            assert_eq!(verdict_of(&[], Some(&done(st))), Verdict::Fail);
        }
    }

    /// PLUG-10: reftest・other はファイルを読まずにスキップされる。
    #[test]
    fn non_testharness_entries_are_skipped_without_reading() {
        let opts = RunOptions::new("/nonexistent-wpt-root", None);
        for kind in [HarnessKind::Reftest, HarnessKind::Other] {
            let entry = SubsetEntry::new("css/a.html", kind).expect("entry");
            assert_eq!(
                run_entry(&opts, &entry),
                FileOutcome::Skipped { harness: kind }
            );
        }
    }

    /// PLUG-10: script の MIME 判定。
    #[test]
    fn classic_script_types() {
        assert!(is_classic_script_type(None));
        assert!(is_classic_script_type(Some("")));
        assert!(is_classic_script_type(Some("Text/JavaScript")));
        assert!(is_classic_script_type(Some(
            "text/javascript; charset=utf-8"
        )));
        assert!(is_classic_script_type(Some(
            " Application/JavaScript ;charset=UTF-8"
        )));
        assert!(!is_classic_script_type(Some("module")));
        assert!(!is_classic_script_type(Some("application/json")));
        assert!(!is_classic_script_type(Some(
            "application/json; text/javascript"
        )));
    }

    /// PLUG-10: 件数上限は収集中に逐次判定し、超えた時点で打ち切る（Vec に積まない）。
    #[test]
    fn collect_scripts_stops_at_step_limit() {
        let html = "<script>1</script>".repeat(10);
        let limits = RunLimits {
            max_steps: 3,
            ..RunLimits::default()
        };
        assert!(matches!(
            collect_scripts(&html, "a/t.html", &limits),
            Err(CollectError::Limit(LimitKind::Steps))
        ));
        // 上限ちょうどは通る。testharnessreport.js は件数に含めない。
        let ok = "<script>1</script><script src=\"/resources/testharnessreport.js\"></script>\
                  <script>2</script><script>3</script>";
        let got = collect_scripts(ok, "a/t.html", &limits).expect("ok");
        assert_eq!(got.len(), 3);
    }

    /// PLUG-10: インライン本文の合計バイト数も収集中に判定する。
    #[test]
    fn collect_scripts_stops_at_total_bytes_limit() {
        let html = format!("<script>{0}</script><script>{0}</script>", "x".repeat(600));
        let mut limits = RunLimits {
            max_total_bytes: 1000,
            ..RunLimits::default()
        };
        assert!(matches!(
            collect_scripts(&html, "a/t.html", &limits),
            Err(CollectError::Limit(LimitKind::TotalBytes))
        ));
        limits.max_total_bytes = 1200;
        assert!(collect_scripts(&html, "a/t.html", &limits).is_ok());
    }

    /// PLUG-10: `src` 付きの defer / async は実行順を再現できないため Unsupported にする。
    #[test]
    fn collect_scripts_marks_defer_and_async_unsupported() {
        for (attrs, expected) in [
            ("defer", "defer"),
            ("async", "async"),
            ("async defer", "defer"),
        ] {
            let html = format!("<script src=\"a.js\" {attrs}></script>");
            match collect_scripts(&html, "a/t.html", &RunLimits::default()) {
                Ok(v) => match v.as_slice() {
                    [ScriptSource::Unsupported(name)] => assert_eq!(name, expected),
                    _ => panic!("{attrs}: unexpected scripts"),
                },
                Err(_) => panic!("{attrs}: collect failed"),
            }
        }
        // インラインでは defer / async は無視されるため、通常の実行対象のまま。
        let inline = "<script defer>1</script>";
        match collect_scripts(inline, "a/t.html", &RunLimits::default()) {
            Ok(v) => assert!(matches!(v.as_slice(), [ScriptSource::Inline(_)])),
            Err(_) => panic!("inline collect failed"),
        }
    }

    /// PLUG-10: defer 付きファイルは JS を評価せず UnsupportedScript で fail-closed になる。
    #[test]
    fn run_entry_reports_unsupported_for_deferred_script() {
        let root = scratch_dir("defer");
        write_file(&root, "resources/testharness.js", "// stub");
        write_file(
            &root,
            "t.html",
            "<script src=\"/resources/testharness.js\"></script>\
             <script src=\"x.js\" defer></script>",
        );
        let entry = SubsetEntry::new("t.html", HarnessKind::Testharness).expect("entry");
        assert_eq!(
            run_entry(&RunOptions::new(&root, None), &entry),
            FileOutcome::UnsupportedScript {
                script_type: "defer".to_string()
            }
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// PLUG-10: 開いたハンドルで種別・サイズを判定し、通常ファイルは (ハンドル, サイズ) で返る。
    #[test]
    fn open_under_root_returns_handle_and_size() {
        let root = fs::canonicalize(scratch_dir("open")).expect("canon");
        write_file(&root, "a/b.js", "12345");
        let (mut f, len) = open_under_root(&root, "a/b.js").expect("open");
        assert_eq!(len, 5);
        let mut s = String::new();
        f.read_to_string(&mut s).expect("read");
        assert_eq!(s, "12345");
        assert!(matches!(
            open_under_root(&root, "a"),
            Err(FileOutcome::Missing)
        ));
        assert!(matches!(
            open_under_root(&root, "nope.js"),
            Err(FileOutcome::Missing)
        ));
        let _ = fs::remove_dir_all(&root);
    }

    /// PLUG-10: ルート外を指す symlink は存在しないものとして拒否する（Unix）。
    #[cfg(unix)]
    #[test]
    fn open_under_root_rejects_symlink_escaping_root() {
        let base = fs::canonicalize(scratch_dir("symlink")).expect("canon");
        let root = base.join("wpt");
        write_file(&root, "keep.js", "ok");
        write_file(&base, "outside.js", "secret");
        std::os::unix::fs::symlink(base.join("outside.js"), root.join("leak.js")).expect("link");
        assert!(matches!(
            open_under_root(&root, "leak.js"),
            Err(FileOutcome::Missing)
        ));
        assert!(matches!(
            read_under_root(&root, "leak.js"),
            Err(FileOutcome::Missing)
        ));
        // ルート内を指す symlink は同一性が一致するため許可される。
        std::os::unix::fs::symlink(root.join("keep.js"), root.join("alias.js")).expect("link");
        assert_eq!(
            read_under_root(&root, "alias.js").ok().as_deref(),
            Some("ok")
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// PLUG-10: 開いたハンドルと canonical パスが別ファイルなら拒否する（open 後の差し替え。Unix）。
    #[cfg(unix)]
    #[test]
    fn confirm_same_file_rejects_replaced_path() {
        let root = fs::canonicalize(scratch_dir("identity")).expect("canon");
        write_file(&root, "a.js", "a");
        write_file(&root, "b.js", "b");
        let handle = fs::File::open(root.join("a.js")).expect("open");
        let meta = handle.metadata().expect("meta");
        assert!(confirm_same_file(&meta, &root.join("a.js")).is_ok());
        match confirm_same_file(&meta, &root.join("b.js")) {
            Err(FileOutcome::ReadFailed { error }) => {
                assert_eq!(error, "file identity changed while opening");
            }
            other => panic!("unexpected: {other:?}"),
        }
        let _ = fs::remove_dir_all(&root);
    }

    fn scratch_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "wpt-runner-unit-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn write_file(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, body).expect("write");
    }
}
