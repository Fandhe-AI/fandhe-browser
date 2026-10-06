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

use std::collections::BTreeMap;
use std::fmt;

use crate::generate_reduced::ResolvedTask;
use crate::tasks::{Action, Category, Golden, Task, json_str};

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
        Some((self.pass * 2000 + self.total) / (2 * self.total))
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
        self.total > 0 && self.pass * 100 >= threshold_percent * self.total
    }
}

/// 全体成功率の判定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    pub threshold_percent: u32,
    pub meets: bool,
}

/// 採点レポート。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScoreReport {
    pub results: Vec<TaskResult>,
    /// click・extract・form・nav の順。
    pub by_category: [(Category, Tally); 4],
    pub overall: Tally,
    pub verdict: Verdict,
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

/// 回答 JSON（`[{"id": ..., "ref"|"value"|"steps": ...}, ...]`）を id → 回答へ変換する。
/// 1 件の形式不備は `Answer::Invalid` に留め、全体のエラーにはしない。
pub fn parse_answers(tasks: &[Task], json: &str) -> Result<BTreeMap<String, Answer>, ScoreError> {
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
        let ans = parse_entry(o)?;
        if out.insert(id.to_owned(), ans).is_some() {
            return Err(ScoreError::DuplicateId(id.to_owned()));
        }
    }
    Ok(out)
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
    let mut cats = [
        (Category::Click, Tally::default()),
        (Category::Extract, Tally::default()),
        (Category::Form, Tally::default()),
        (Category::Nav, Tally::default()),
    ];
    let mut overall = Tally::default();
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
        let pass = failure.is_none();
        for (c, tally) in cats.iter_mut() {
            if *c == t.category {
                tally.total += 1;
                tally.pass += u32::from(pass);
            }
        }
        overall.total += 1;
        overall.pass += u32::from(pass);
        results.push(TaskResult {
            id: t.id.to_owned(),
            category: t.category,
            pass,
            failure,
        });
    }
    let verdict = Verdict {
        threshold_percent: PASS_THRESHOLD_PERCENT,
        meets: overall.meets(PASS_THRESHOLD_PERCENT),
    };
    Ok(ScoreReport {
        results,
        by_category: cats,
        overall,
        verdict,
    })
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
    format!(
        "{{\n  \"threshold_percent\": {},\n  \"meets\": {},\n  \"overall\": {{{}}},\n  \"by_category\": [\n{}\n  ],\n  \"results\": [\n{}\n  ]\n}}\n",
        r.verdict.threshold_percent,
        r.verdict.meets,
        tally_json(&r.overall),
        cats.join(",\n"),
        items.join(",\n")
    )
}
