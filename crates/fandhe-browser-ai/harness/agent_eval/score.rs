//! エージェント回答の採点と種別別集計（TASK-21.3・`AISNAP-8`・Issue #121・MS-2）。
//!
//! 別途取得したエージェント回答（簡約表現だけを見て答えたもの）を golden と照合し、
//! タスクごとの正誤・4 種別（click / extract / form / nav）の成功率・全体成功率・
//! 全体 70% 以上の判定を算出する純ロジック。PoC の `score.mjs` 相当だが、ref が
//! `AISNAP-10` のダイジェスト形式で、プログラム出力は英語規約のため移植ではなく再定義している。
//!
//! 呼び出し元は `score_tests.rs`（`[[test]] agent_eval_score`）で、取り込み側ルートが
//! `tasks`（golden 定義）と `generate_reduced`（`ResolvedTask`。golden ロケータの ref 解決結果）を
//! 同名の `mod` で `#[path]` 宣言している前提で `crate::` 参照する。I/O は持たない。
//!
//! 暫定事項（`REPAIR-3`）: 本モジュールは採点器であり測定結果を持たない。実エージェントの回答取得と
//! 「全体 70% 以上」の実測は親 #118 の工程で、ここでは判定ロジックを提供するのみ。
//!
//! 回答 JSON は LLM 出力由来の外部入力。サイズ・件数・手順数・文字列長を上限検証し、
//! `get()` 系のみで辿る（`unwrap`・添字アクセスを使わない）。
//!
//! TASK-22.1b（`AISNAP-9`・Issue #805・`MS-2`）: 生 DOM 直渡し方式の `{selector, index}` ロケータ回答も
//! 採点する（`parse_raw_answers` / `resolve_raw_dom`）。回答者が見たのは `serialize_raw_dom` の出力なので、
//! 回答と golden のロケータは「生 DOM を再パースした Document」上で解決し、要素の同一性
//! （文書順の位置から作るキー）で比べる。セレクタ文字列の一致では比べない。解決結果は
//! `Answer::Ref` / `ResolvedTask` のキーへ変換して既存の `judge` / `score` へ渡すため、簡約方式の
//! 採点経路は変わらない。部分点は付けず（0/1）、`golden_unresolved` を含む集計と除いた集計の
//! 両方を `ScoreReport` に持たせる（#123 のオーナー判断）。
//!
//! 暫定事項（`REPAIR-3`）: 2 方式の差の計算と −5pt 判定は `compare.rs`（#126・TASK-22.3）が担い、ここでは行わない。
//! セレクタは core が対応する一部の構文に限られ、未対応構文の回答は `invalid_shape` になる
//! （指示テンプレート #125 で使用可能な構文を明示する前提）。

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::generate_reduced::{ResolvedLocator, ResolvedTask, golden_locators};
use crate::tasks::{Action, Category, Golden, Task, json_str};
use fandhe_browser_core::dom::{Document, NodeId};
use fandhe_browser_core::query::query_selector_all_str;

/// 全体成功率の合格しきい値（`AISNAP-8`: 絶対値 70% 以上）。
pub const PASS_THRESHOLD_PERCENT: u32 = 70;
/// 回答 JSON の最大バイト数。
pub const MAX_INPUT_BYTES: usize = 1024 * 1024;
/// 回答エントリの最大件数。
pub const MAX_ENTRIES: usize = 64;
/// form 回答 1 件あたりの最大手順数。
pub const MAX_STEPS: usize = 32;
/// 回答内の文字列 1 つの最大バイト数。
pub const MAX_STRING_BYTES: usize = 4096;

/// 回答の 1 手順（form）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerStep {
    pub action: Action,
    pub target_ref: String,
    pub value: Option<String>,
}

/// 1 タスクへの回答。種別との整合は採点時に検証する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// click・nav の回答（ref）。
    Ref(String),
    /// extract の回答（値）。
    Value(String),
    /// form の回答（操作列）。
    Steps(Vec<AnswerStep>),
    /// 欠落または全フィールドが null（回答不能）。
    Unanswered,
    /// 型・必須項目が不正。
    Invalid,
}

/// 不合格の理由（`results.json` には英語識別子で出す）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// 回答がない。
    Unanswered,
    /// 種別と回答の形が合わない・必須項目欠落。
    InvalidShape,
    /// 回答が golden と一致しない。
    Mismatch,
    /// 照合に必要な golden の ref が解決できていない（fail-closed）。
    GoldenUnresolved,
}

impl FailureClass {
    pub fn as_str(self) -> &'static str {
        match self {
            FailureClass::Unanswered => "unanswered",
            FailureClass::InvalidShape => "invalid_shape",
            FailureClass::Mismatch => "mismatch",
            FailureClass::GoldenUnresolved => "golden_unresolved",
        }
    }
}

/// 1 タスクの採点結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskResult {
    pub id: String,
    pub category: Category,
    pub pass: bool,
    pub failure: Option<FailureClass>,
}

/// 合格数 / 総数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tally {
    pub pass: u32,
    pub total: u32,
}

impl Tally {
    /// 成功率（千分率。四捨五入）。`total == 0` は `None`。
    pub fn rate_permille(&self) -> Option<u32> {
        if self.total == 0 {
            return None;
        }
        // pub フィールドは任意値を取り得るため u64 へ拡張して演算し、桁あふれを避ける
        let (pass, total) = (u64::from(self.pass), u64::from(self.total));
        let permille = (pass * 2000 + total) / (2 * total);
        Some(u32::try_from(permille).unwrap_or(u32::MAX))
    }

    /// `"88.0%"` 形式。`total == 0` は `"n/a"`。
    pub fn rate_text(&self) -> String {
        match self.rate_permille() {
            Some(p) => format!("{}.{}%", p / 10, p % 10),
            None => "n/a".to_owned(),
        }
    }

    /// `threshold_percent` 以上か。丸め前の整数比較で、`total == 0` は未達。
    pub fn meets(&self, threshold_percent: u32) -> bool {
        // u64 へ拡張して比較し、桁あふれによる誤判定を避ける
        self.total > 0
            && u64::from(self.pass) * 100 >= u64::from(threshold_percent) * u64::from(self.total)
    }
}

/// 全体成功率の判定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    pub threshold_percent: u32,
    pub meets: bool,
}

/// 種別別・全体の集計。除外したタスク id を併記する（`AISNAP-9`・#805）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    /// click・extract・form・nav の順。
    pub by_category: [(Category, Tally); 4],
    pub overall: Tally,
    /// 分母から除いたタスク id（昇順）。実在する id だけを持つ。
    pub excluded: Vec<String>,
}

/// 採点レポート。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScoreReport {
    pub results: Vec<TaskResult>,
    /// click・extract・form・nav の順（`golden_unresolved` を含む集計）。
    pub by_category: [(Category, Tally); 4],
    pub overall: Tally,
    pub verdict: Verdict,
    /// `golden_unresolved` のタスクを分母から除いた集計（`AISNAP-9`・#805）。
    pub excluding_golden_unresolved: Summary,
}

/// 採点・回答パースのエラー。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScoreError {
    Json(String),
    TooLarge(String),
    DuplicateId(String),
    UnknownId(String),
    MissingGolden(String),
    MissingResolved(String),
    /// 生 DOM 方式の採点で、タスクが指すページの Document が渡されていない。
    MissingPage(String),
    /// 生 DOM 上で golden のセレクタが評価できない（golden 定義の不備。fail-closed）。
    GoldenSelector {
        task: String,
        message: String,
    },
}

impl fmt::Display for ScoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScoreError::Json(m) => write!(f, "invalid answers json: {m}"),
            ScoreError::TooLarge(m) => write!(f, "answers too large: {m}"),
            ScoreError::DuplicateId(i) => write!(f, "duplicate answer id: {i}"),
            ScoreError::UnknownId(i) => write!(f, "unknown task id: {i}"),
            ScoreError::MissingGolden(i) => write!(f, "missing golden for task: {i}"),
            ScoreError::MissingResolved(i) => write!(f, "missing resolved refs for task: {i}"),
            ScoreError::MissingPage(p) => write!(f, "missing raw dom page: {p}"),
            ScoreError::GoldenSelector { task, message } => {
                write!(f, "{task}: golden selector failed: {message}")
            }
        }
    }
}

impl std::error::Error for ScoreError {}

type JsonObject = serde_json::Map<String, serde_json::Value>;

fn check_len(s: &str, what: &str) -> Result<(), ScoreError> {
    if s.len() > MAX_STRING_BYTES {
        return Err(ScoreError::TooLarge(format!("{what} exceeds string limit")));
    }
    Ok(())
}

/// 文字列フィールド。内側 `Ok(None)` は欠落または null、内側 `Err(())` は型違い
/// （そのタスクを `Invalid` にする）。外側 `Err` は上限超過。
fn str_field(obj: &JsonObject, key: &str) -> Result<Result<Option<String>, ()>, ScoreError> {
    match obj.get(key) {
        None | Some(serde_json::Value::Null) => Ok(Ok(None)),
        Some(serde_json::Value::String(s)) => {
            check_len(s, key)?;
            Ok(Ok(Some(s.clone())))
        }
        Some(_) => Ok(Err(())),
    }
}

fn parse_steps(arr: &[serde_json::Value]) -> Result<Answer, ScoreError> {
    if arr.len() > MAX_STEPS {
        return Err(ScoreError::TooLarge("steps exceed step limit".to_owned()));
    }
    let mut steps = Vec::with_capacity(arr.len());
    for v in arr {
        let Some(o) = v.as_object() else {
            return Ok(Answer::Invalid);
        };
        let action = match o.get("action").and_then(|a| a.as_str()) {
            Some("fill") => Action::Fill,
            Some("select") => Action::Select,
            Some("click") => Action::Click,
            _ => return Ok(Answer::Invalid),
        };
        let Ok(Some(target_ref)) = str_field(o, "ref")? else {
            return Ok(Answer::Invalid);
        };
        let Ok(value) = str_field(o, "value")? else {
            return Ok(Answer::Invalid);
        };
        let shape_ok = match action {
            Action::Click => value.is_none(),
            Action::Fill | Action::Select => value.is_some(),
        };
        if !shape_ok {
            return Ok(Answer::Invalid);
        }
        steps.push(AnswerStep {
            action,
            target_ref,
            value,
        });
    }
    Ok(Answer::Steps(steps))
}

fn parse_entry(o: &JsonObject) -> Result<Answer, ScoreError> {
    let r = str_field(o, "ref")?;
    let v = str_field(o, "value")?;
    let steps = match o.get("steps") {
        None | Some(serde_json::Value::Null) => None,
        Some(x) => Some(x),
    };
    let (Ok(r), Ok(v)) = (r, v) else {
        return Ok(Answer::Invalid);
    };
    match (r, v, steps) {
        (None, None, None) => Ok(Answer::Unanswered),
        (Some(r), None, None) => Ok(Answer::Ref(r)),
        (None, Some(v), None) => Ok(Answer::Value(v)),
        (None, None, Some(s)) => match s.as_array() {
            Some(arr) => parse_steps(arr),
            None => Ok(Answer::Invalid),
        },
        _ => Ok(Answer::Invalid),
    }
}

/// トップレベル検証（サイズ・配列・件数・id・重複・未知 id）を共通化し、エントリ本体だけ `f` に委ねる。
fn parse_entries<T>(
    tasks: &[Task],
    json: &str,
    f: fn(&JsonObject) -> Result<T, ScoreError>,
) -> Result<BTreeMap<String, T>, ScoreError> {
    if json.len() > MAX_INPUT_BYTES {
        return Err(ScoreError::TooLarge("input exceeds size limit".to_owned()));
    }
    let root: serde_json::Value =
        serde_json::from_str(json).map_err(|e| ScoreError::Json(e.to_string()))?;
    let Some(arr) = root.as_array() else {
        return Err(ScoreError::Json("top level must be an array".to_owned()));
    };
    if arr.len() > MAX_ENTRIES {
        return Err(ScoreError::TooLarge(
            "entries exceed entry limit".to_owned(),
        ));
    }
    let mut out = BTreeMap::new();
    for item in arr {
        let Some(o) = item.as_object() else {
            return Err(ScoreError::Json("entry must be an object".to_owned()));
        };
        let Some(id) = o.get("id").and_then(|i| i.as_str()) else {
            return Err(ScoreError::Json("entry id must be a string".to_owned()));
        };
        check_len(id, "id")?;
        if !tasks.iter().any(|t| t.id == id) {
            return Err(ScoreError::UnknownId(id.to_owned()));
        }
        let ans = f(o)?;
        if out.insert(id.to_owned(), ans).is_some() {
            return Err(ScoreError::DuplicateId(id.to_owned()));
        }
    }
    Ok(out)
}

/// 回答 JSON（`[{"id": ..., "ref"|"value"|"steps": ...}, ...]`）を id → 回答へ変換する。
/// 1 件の形式不備は `Answer::Invalid` に留め、全体のエラーにはしない。
pub fn parse_answers(tasks: &[Task], json: &str) -> Result<BTreeMap<String, Answer>, ScoreError> {
    parse_entries(tasks, json, parse_entry)
}

fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn judge(golden: &Golden, resolved: &ResolvedTask, answer: &Answer) -> Option<FailureClass> {
    use FailureClass::*;
    match answer {
        Answer::Unanswered => return Some(Unanswered),
        Answer::Invalid => return Some(InvalidShape),
        _ => {}
    }
    match (golden, answer) {
        (Golden::Ref { .. }, Answer::Ref(r)) => {
            let hit = resolved
                .locators
                .iter()
                .flat_map(|l| l.refs.iter())
                .any(|g| g == r.trim());
            let any = resolved.locators.iter().any(|l| !l.refs.is_empty());
            if hit {
                None
            } else if !any {
                Some(GoldenUnresolved)
            } else {
                Some(Mismatch)
            }
        }
        // extract は ref ではなく値で照合する（golden ref 未解決でも値が合えば合格）。
        (Golden::Value { value, .. }, Answer::Value(v)) => {
            if normalize(value) == normalize(v) {
                None
            } else {
                Some(Mismatch)
            }
        }
        (Golden::Steps(gs), Answer::Steps(steps)) => {
            if resolved.locators.len() != gs.len()
                || resolved.locators.iter().any(|l| l.refs.is_empty())
            {
                return Some(GoldenUnresolved);
            }
            if gs.len() != steps.len() {
                return Some(Mismatch);
            }
            let ok = gs
                .iter()
                .zip(steps)
                .zip(&resolved.locators)
                .all(|((g, a), l)| {
                    g.action == a.action
                        && l.refs.iter().any(|r| r == a.target_ref.trim())
                        && match (g.value, a.value.as_deref()) {
                            (None, None) => true,
                            (Some(gv), Some(av)) => normalize(gv) == normalize(av),
                            _ => false,
                        }
                });
            if ok { None } else { Some(Mismatch) }
        }
        _ => Some(InvalidShape),
    }
}

/// 回答を golden と照合して採点する（純関数）。`tasks`・`golden`・`resolved` は同じ id 集合を持つこと。
pub fn score(
    tasks: &[Task],
    golden: &[(&str, Golden)],
    resolved: &[ResolvedTask],
    answers: &BTreeMap<String, Answer>,
) -> Result<ScoreReport, ScoreError> {
    if let Some(id) = answers
        .keys()
        .find(|k| !tasks.iter().any(|t| t.id == k.as_str()))
    {
        return Err(ScoreError::UnknownId(id.clone()));
    }
    let mut results = Vec::with_capacity(tasks.len());
    for t in tasks {
        let g = golden
            .iter()
            .find(|(id, _)| *id == t.id)
            .map(|(_, g)| g)
            .ok_or_else(|| ScoreError::MissingGolden(t.id.to_owned()))?;
        let r = resolved
            .iter()
            .find(|r| r.id == t.id)
            .ok_or_else(|| ScoreError::MissingResolved(t.id.to_owned()))?;
        let failure = match answers.get(t.id) {
            Some(a) => judge(g, r, a),
            None => Some(FailureClass::Unanswered),
        };
        results.push(TaskResult {
            id: t.id.to_owned(),
            category: t.category,
            pass: failure.is_none(),
            failure,
        });
    }
    let all = summarize(&results, &BTreeSet::new());
    let excluded = golden_unresolved_ids(&results);
    let verdict = Verdict {
        threshold_percent: PASS_THRESHOLD_PERCENT,
        meets: all.overall.meets(PASS_THRESHOLD_PERCENT),
    };
    Ok(ScoreReport {
        excluding_golden_unresolved: summarize(&results, &excluded),
        by_category: all.by_category,
        overall: all.overall,
        results,
        verdict,
    })
}

/// 結果のうち `exclude` に含まれる id を分母・分子から除いて集計する（純関数。`AISNAP-9`・#805）。
///
/// #126（TASK-22.3）が 2 方式の `golden_unresolved` の和集合を渡し直して分母を揃える入口にもなる。
/// `Summary::excluded` には `results` に実在した除外 id だけを昇順で入れる。
pub fn summarize(results: &[TaskResult], exclude: &BTreeSet<String>) -> Summary {
    let mut cats = [
        (Category::Click, Tally::default()),
        (Category::Extract, Tally::default()),
        (Category::Form, Tally::default()),
        (Category::Nav, Tally::default()),
    ];
    let mut overall = Tally::default();
    let mut excluded = BTreeSet::new();
    for r in results {
        if exclude.contains(&r.id) {
            excluded.insert(r.id.clone());
            continue;
        }
        for (c, tally) in cats.iter_mut() {
            if *c == r.category {
                tally.total += 1;
                tally.pass += u32::from(r.pass);
            }
        }
        overall.total += 1;
        overall.pass += u32::from(r.pass);
    }
    Summary {
        by_category: cats,
        overall,
        excluded: excluded.into_iter().collect(),
    }
}

/// 失敗理由が `golden_unresolved` のタスク id 集合。
pub fn golden_unresolved_ids(results: &[TaskResult]) -> BTreeSet<String> {
    results
        .iter()
        .filter(|r| r.failure == Some(FailureClass::GoldenUnresolved))
        .map(|r| r.id.clone())
        .collect()
}

// ---- 生 DOM 直渡し方式（`AISNAP-9`・TASK-22.1b・#805） ----

/// 生 DOM 方式の 1 手順（form）。ロケータは生 DOM を再パースした Document 上で解決する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawAnswerStep {
    pub action: Action,
    pub selector: String,
    pub index: usize,
    pub value: Option<String>,
}

/// 生 DOM 方式の 1 タスクへの回答。`Answer` と対になる、解決前の形。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawAnswer {
    /// click・nav の回答（`{selector, index}`）。
    Locator { selector: String, index: usize },
    /// extract の回答（値）。
    Value(String),
    /// form の回答（操作列）。
    Steps(Vec<RawAnswerStep>),
    /// 欠落または全フィールドが null。
    Unanswered,
    /// 型・必須項目が不正（`ref` の混入など形式の取り違えを含む）。
    Invalid,
}

/// `index` フィールド。内側 `Ok(None)` は欠落または null、内側 `Err(())` は非負整数でない値。
fn index_field(obj: &JsonObject) -> Result<Option<usize>, ()> {
    match obj.get("index") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .map(Some)
            .ok_or(()),
    }
}

fn parse_raw_steps(arr: &[serde_json::Value]) -> Result<RawAnswer, ScoreError> {
    if arr.len() > MAX_STEPS {
        return Err(ScoreError::TooLarge("steps exceed step limit".to_owned()));
    }
    let mut steps = Vec::with_capacity(arr.len());
    for v in arr {
        let Some(o) = v.as_object() else {
            return Ok(RawAnswer::Invalid);
        };
        let action = match o.get("action").and_then(|a| a.as_str()) {
            Some("fill") => Action::Fill,
            Some("select") => Action::Select,
            Some("click") => Action::Click,
            _ => return Ok(RawAnswer::Invalid),
        };
        let sel = str_field(o, "selector")?;
        let rf = str_field(o, "ref")?;
        let val = str_field(o, "value")?;
        let (Ok(Some(selector)), Ok(None), Ok(value), Ok(Some(index))) =
            (sel, rf, val, index_field(o))
        else {
            return Ok(RawAnswer::Invalid);
        };
        let shape_ok = match action {
            Action::Click => value.is_none(),
            Action::Fill | Action::Select => value.is_some(),
        };
        if !shape_ok {
            return Ok(RawAnswer::Invalid);
        }
        steps.push(RawAnswerStep {
            action,
            selector,
            index,
            value,
        });
    }
    Ok(RawAnswer::Steps(steps))
}

fn parse_raw_entry(o: &JsonObject) -> Result<RawAnswer, ScoreError> {
    // 文字列長の上限超過は全体エラー。型違いなどはこのタスクだけ Invalid にする
    let sel = str_field(o, "selector")?;
    let rf = str_field(o, "ref")?;
    let val = str_field(o, "value")?;
    let idx = index_field(o);
    let steps = match o.get("steps") {
        None | Some(serde_json::Value::Null) => None,
        Some(x) => Some(x),
    };
    // 簡約方式の `ref` が混ざった回答は形式の取り違えとして Invalid（安全側）
    let (Ok(sel), Ok(None), Ok(val), Ok(idx)) = (sel, rf, val, idx) else {
        return Ok(RawAnswer::Invalid);
    };
    match (sel, idx, val, steps) {
        (None, None, None, None) => Ok(RawAnswer::Unanswered),
        (Some(selector), Some(index), None, None) => Ok(RawAnswer::Locator { selector, index }),
        (None, None, Some(v), None) => Ok(RawAnswer::Value(v)),
        (None, None, None, Some(s)) => match s.as_array() {
            Some(arr) => parse_raw_steps(arr),
            None => Ok(RawAnswer::Invalid),
        },
        _ => Ok(RawAnswer::Invalid),
    }
}

/// 生 DOM 方式の回答 JSON（`[{"id", "selector"+"index" | "value" | "steps"}, ...]`）をパースする。
/// 上限・重複 id・未知 id の扱いは `parse_answers` と同じ。1 件の形式不備は `RawAnswer::Invalid` に留める。
///
/// 文字列の上限 `MAX_STRING_BYTES`（4096）は core のセレクタ入力上限と同じ値で、セレクタ用の別上限は設けない。
pub fn parse_raw_answers(
    tasks: &[Task],
    json: &str,
) -> Result<BTreeMap<String, RawAnswer>, ScoreError> {
    parse_entries(tasks, json, parse_raw_entry)
}

/// 回答側が範囲外を指したときのキー。要素同一性キー（`node:<N>`）とは決して一致しない。
const OUT_OF_RANGE_KEY: &str = "out-of-range";

/// 要素同一性キー。`doc` の文書順（`descendants`）での位置から作る。
fn element_key(doc: &Document, node: NodeId) -> Option<String> {
    doc.descendants(doc.root())
        .position(|n| n == node)
        .map(|pos| format!("node:{pos}"))
}

/// 回答ロケータの解決結果。
enum Located {
    /// 要素を指した（範囲外は番兵キー）。
    Key(String),
    /// セレクタが構文エラー・未対応構文など（`invalid_shape`）。
    Invalid,
}

fn locate(doc: &Document, selector: &str, index: usize) -> Located {
    match query_selector_all_str(doc, doc.root(), selector) {
        Err(_) => Located::Invalid,
        Ok(found) => Located::Key(
            found
                .get(index)
                .and_then(|n| element_key(doc, *n))
                .unwrap_or_else(|| OUT_OF_RANGE_KEY.to_owned()),
        ),
    }
}

fn resolve_raw_answer(doc: &Document, a: &RawAnswer) -> Answer {
    match a {
        RawAnswer::Unanswered => Answer::Unanswered,
        RawAnswer::Invalid => Answer::Invalid,
        RawAnswer::Value(v) => Answer::Value(v.clone()),
        RawAnswer::Locator { selector, index } => match locate(doc, selector, *index) {
            Located::Key(k) => Answer::Ref(k),
            Located::Invalid => Answer::Invalid,
        },
        RawAnswer::Steps(steps) => {
            let mut out = Vec::with_capacity(steps.len());
            for s in steps {
                let Located::Key(k) = locate(doc, &s.selector, s.index) else {
                    return Answer::Invalid;
                };
                out.push(AnswerStep {
                    action: s.action,
                    target_ref: k,
                    value: s.value.clone(),
                });
            }
            Answer::Steps(out)
        }
    }
}

/// 生 DOM 方式の golden と回答を、同じ Document 上の要素同一性キーへ解決する（`AISNAP-9`・#805）。
///
/// `pages` は各ページの生 DOM（`serialize_raw_dom` の出力）を再パースした Document。
/// 戻り値は既存の `score` にそのまま渡せる。golden のロケータが範囲外のときは `refs` を空にして
/// `golden_unresolved` になる（エラーにしない）。golden のセレクタ自体が評価できない場合は
/// golden 定義の不備として `ScoreError::GoldenSelector`。
pub fn resolve_raw_dom(
    tasks: &[Task],
    golden: &[(&str, Golden)],
    pages: &BTreeMap<String, Document>,
    answers: &BTreeMap<String, RawAnswer>,
) -> Result<(Vec<ResolvedTask>, BTreeMap<String, Answer>), ScoreError> {
    if let Some(id) = answers
        .keys()
        .find(|k| !tasks.iter().any(|t| t.id == k.as_str()))
    {
        return Err(ScoreError::UnknownId(id.clone()));
    }
    let mut resolved = Vec::with_capacity(tasks.len());
    let mut out = BTreeMap::new();
    for t in tasks {
        let doc = pages
            .get(t.page)
            .ok_or_else(|| ScoreError::MissingPage(t.page.to_owned()))?;
        let g = golden
            .iter()
            .find(|(id, _)| *id == t.id)
            .map(|(_, g)| g)
            .ok_or_else(|| ScoreError::MissingGolden(t.id.to_owned()))?;
        let mut locators = Vec::new();
        for l in golden_locators(g) {
            let found = query_selector_all_str(doc, doc.root(), l.selector).map_err(|e| {
                ScoreError::GoldenSelector {
                    task: t.id.to_owned(),
                    message: e.to_string(),
                }
            })?;
            let refs = found
                .get(l.index)
                .and_then(|n| element_key(doc, *n))
                .into_iter()
                .collect();
            locators.push(ResolvedLocator {
                selector: l.selector.to_owned(),
                index: l.index,
                refs,
            });
        }
        resolved.push(ResolvedTask {
            id: t.id.to_owned(),
            locators,
        });
        if let Some(a) = answers.get(t.id) {
            out.insert(t.id.to_owned(), resolve_raw_answer(doc, a));
        }
    }
    Ok((resolved, out))
}

fn tally_json(t: &Tally) -> String {
    format!(
        "\"pass\": {}, \"total\": {}, \"rate\": {}",
        t.pass,
        t.total,
        json_str(&t.rate_text())
    )
}

/// `results.json` の内容。2 スペースインデント・LF・末尾改行。回答本文は出力しない。
pub fn render_results_json(r: &ScoreReport) -> String {
    let cats: Vec<String> = r
        .by_category
        .iter()
        .map(|(c, t)| {
            format!(
                "    {{\"category\": {}, {}}}",
                json_str(c.as_str()),
                tally_json(t)
            )
        })
        .collect();
    let items: Vec<String> = r
        .results
        .iter()
        .map(|t| {
            let failure = match t.failure {
                Some(f) => json_str(f.as_str()),
                None => "null".to_owned(),
            };
            format!(
                "    {{\"id\": {}, \"category\": {}, \"pass\": {}, \"failure\": {}}}",
                json_str(&t.id),
                json_str(t.category.as_str()),
                t.pass,
                failure
            )
        })
        .collect();
    let ex = &r.excluding_golden_unresolved;
    let ex_cats: Vec<String> = ex
        .by_category
        .iter()
        .map(|(c, t)| {
            format!(
                "      {{\"category\": {}, {}}}",
                json_str(c.as_str()),
                tally_json(t)
            )
        })
        .collect();
    let ex_ids: Vec<String> = ex.excluded.iter().map(|i| json_str(i)).collect();
    let exclusion = format!(
        "  \"excluding_golden_unresolved\": {{\n    \"excluded\": [{}],\n    \"overall\": {{{}}},\n    \"by_category\": [\n{}\n    ]\n  }},\n",
        ex_ids.join(", "),
        tally_json(&ex.overall),
        ex_cats.join(",\n")
    );
    format!(
        "{{\n  \"threshold_percent\": {},\n  \"meets\": {},\n  \"overall\": {{{}}},\n  \"by_category\": [\n{}\n  ],\n{}  \"results\": [\n{}\n  ]\n}}\n",
        r.verdict.threshold_percent,
        r.verdict.meets,
        tally_json(&r.overall),
        cats.join(",\n"),
        exclusion,
        items.join(",\n")
    )
}
