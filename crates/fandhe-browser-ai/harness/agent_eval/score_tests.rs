//! `score.rs` のテスト（TASK-21.3・`AISNAP-8`・Issue #121）。
//!
//! 採点器の検証は合成回答だけで行う。ここで組む「22/25」等は採点器の検算であり、
//! 実エージェントの測定結果ではない（実測は親 #118。`REPAIR-3`）。
//! 実回答を採点する入口は `aisnap8_score_real_answers_if_requested`。

// 未使用の公開項目は本 target では使わない。
#[allow(dead_code)]
#[path = "generate_reduced.rs"]
mod generate_reduced;
#[allow(dead_code)]
#[path = "../../benches/token_reduction/raw_dom.rs"]
mod raw_dom;
#[allow(dead_code)]
#[path = "../../benches/token_reduction/retention_check.rs"]
mod retention_check;
#[allow(dead_code)]
#[path = "score.rs"]
mod score;
#[allow(dead_code)]
#[path = "../../benches/token_reduction/snapshot_text.rs"]
mod snapshot_text;
#[allow(dead_code)]
#[path = "tasks.rs"]
mod tasks;

use fandhe_browser_core::dom::Document;
use fandhe_browser_core::parse::{ParseOptions, parse_document};
use generate_reduced::{ResolvedTask, golden_locators, raw_dom_documents, resolve_golden};
use score::{
    Answer, AnswerStep, FailureClass, RawAnswer, RawAnswerStep, ScoreError, ScoreReport, Tally,
    parse_answers, parse_raw_answers, render_results_json, resolve_raw_dom, score, summarize,
};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::PathBuf;
use tasks::{Action, Category, GOLDEN, Golden, TASKS};

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixtures() -> PathBuf {
    manifest().join("benches").join("fixtures")
}

fn resolved() -> Vec<ResolvedTask> {
    resolve_golden(&fixtures()).expect("resolve")
}

/// 各ロケータの先頭 ref・golden の値で正答を組む。
fn perfect_answers(resolved: &[ResolvedTask]) -> BTreeMap<String, Answer> {
    let mut m = BTreeMap::new();
    for ((id, g), r) in GOLDEN.iter().zip(resolved) {
        let first = |i: usize| -> String {
            r.locators
                .get(i)
                .and_then(|l| l.refs.first())
                .cloned()
                .unwrap_or_default()
        };
        let a = match g {
            Golden::Ref { .. } => {
                let rf = r
                    .locators
                    .iter()
                    .find_map(|l| l.refs.first().cloned())
                    .unwrap_or_default();
                Answer::Ref(rf)
            }
            Golden::Value { value, .. } => Answer::Value((*value).to_owned()),
            Golden::Steps(steps) => Answer::Steps(
                steps
                    .iter()
                    .enumerate()
                    .map(|(i, s)| AnswerStep {
                        action: s.action,
                        target_ref: first(i),
                        value: s.value.map(str::to_owned),
                    })
                    .collect(),
            ),
        };
        m.insert((*id).to_owned(), a);
    }
    m
}

fn run(answers: &BTreeMap<String, Answer>) -> ScoreReport {
    score(&TASKS, &GOLDEN, &resolved(), answers).expect("score")
}

fn run_with(resolved: &[ResolvedTask], answers: &BTreeMap<String, Answer>) -> ScoreReport {
    score(&TASKS, &GOLDEN, resolved, answers).expect("score")
}

fn failure_of(rep: &ScoreReport, id: &str) -> Option<FailureClass> {
    rep.results
        .iter()
        .find(|r| r.id == id)
        .unwrap_or_else(|| panic!("{id} missing"))
        .failure
}

fn tally(rep: &ScoreReport, c: Category) -> (u32, u32) {
    let t = rep
        .by_category
        .iter()
        .find(|(k, _)| *k == c)
        .map(|(_, t)| *t)
        .expect("category");
    (t.pass, t.total)
}

#[test]
fn aisnap8_perfect_answers_pass_all() {
    let rep = run(&perfect_answers(&resolved()));
    assert_eq!(tally(&rep, Category::Click), (7, 7));
    assert_eq!(tally(&rep, Category::Extract), (6, 6));
    assert_eq!(tally(&rep, Category::Form), (6, 6));
    assert_eq!(tally(&rep, Category::Nav), (6, 6));
    assert_eq!((rep.overall.pass, rep.overall.total), (25, 25));
    assert_eq!(rep.overall.rate_text(), "100.0%");
    assert!(rep.verdict.meets);
    assert_eq!(rep.verdict.threshold_percent, 70);
}

#[test]
fn aisnap8_no_answers_fail_all_as_unanswered() {
    let rep = run(&BTreeMap::new());
    assert_eq!((rep.overall.pass, rep.overall.total), (0, 25));
    assert_eq!(rep.overall.rate_text(), "0.0%");
    assert!(!rep.verdict.meets);
    assert!(
        rep.results
            .iter()
            .all(|r| r.failure == Some(FailureClass::Unanswered))
    );
}

/// `AISNAP-8` 記載の内訳（nav 1 件・extract 2 件が誤答）を再現する合成。採点器の検算で測定結果ではない。
#[test]
fn aisnap8_synthetic_breakdown_22_of_25() {
    let mut a = perfect_answers(&resolved());
    a.insert("nav-01".into(), Answer::Ref("e-wrong".into()));
    a.insert("extract-02".into(), Answer::Value("wrong".into()));
    a.insert("extract-03".into(), Answer::Value("wrong".into()));
    let rep = run(&a);
    assert_eq!(tally(&rep, Category::Click), (7, 7));
    assert_eq!(tally(&rep, Category::Form), (6, 6));
    assert_eq!(tally(&rep, Category::Nav), (5, 6));
    assert_eq!(tally(&rep, Category::Extract), (4, 6));
    let text = |c| {
        rep.by_category
            .iter()
            .find(|(k, _)| *k == c)
            .map(|(_, t)| t.rate_text())
            .expect("cat")
    };
    assert_eq!(text(Category::Click), "100.0%");
    assert_eq!(text(Category::Form), "100.0%");
    assert_eq!(text(Category::Nav), "83.3%");
    assert_eq!(text(Category::Extract), "66.7%");
    assert_eq!((rep.overall.pass, rep.overall.total), (22, 25));
    assert_eq!(rep.overall.rate_text(), "88.0%");
    assert!(rep.verdict.meets);
    assert_eq!(failure_of(&rep, "nav-01"), Some(FailureClass::Mismatch));
}

fn with_failures(n: usize) -> ScoreReport {
    let mut a = perfect_answers(&resolved());
    for t in TASKS.iter().take(n) {
        a.remove(t.id);
    }
    run(&a)
}

#[test]
fn aisnap8_threshold_boundary_on_25_tasks() {
    let r17 = with_failures(8);
    assert_eq!(r17.overall.pass, 17);
    assert_eq!(r17.overall.rate_text(), "68.0%");
    assert!(!r17.verdict.meets);
    let r18 = with_failures(7);
    assert_eq!(r18.overall.pass, 18);
    assert_eq!(r18.overall.rate_text(), "72.0%");
    assert!(r18.verdict.meets);
}

#[test]
fn aisnap8_tally_threshold_edges() {
    assert!(Tally { pass: 7, total: 10 }.meets(70));
    assert!(
        !Tally {
            pass: 69,
            total: 100
        }
        .meets(70)
    );
    let zero = Tally { pass: 0, total: 0 };
    assert!(!zero.meets(70));
    assert_eq!(zero.rate_permille(), None);
    assert_eq!(zero.rate_text(), "n/a");
    // 丸め後は 70.0% でも丸め前が未達なら未達（699/1000 → 69.9%）。
    assert!(
        !Tally {
            pass: 699,
            total: 1000
        }
        .meets(70)
    );
}

#[test]
fn aisnap8_extract_matches_by_value_even_if_golden_ref_unresolved() {
    let r = resolved();
    let e1 = r.iter().find(|t| t.id == "extract-01").expect("e1");
    assert!(e1.locators.iter().all(|l| l.refs.is_empty()));
    let mut a = perfect_answers(&r);
    a.insert(
        "extract-01".into(),
        Answer::Value("  user3@example.com \n".into()),
    );
    assert_eq!(failure_of(&run(&a), "extract-01"), None);
    a.insert(
        "extract-01".into(),
        Answer::Value("user4@example.com".into()),
    );
    assert_eq!(
        failure_of(&run(&a), "extract-01"),
        Some(FailureClass::Mismatch)
    );
    a.insert(
        "extract-01".into(),
        Answer::Value("USER3@example.com".into()),
    );
    assert_eq!(
        failure_of(&run(&a), "extract-01"),
        Some(FailureClass::Mismatch)
    );
}

#[test]
fn aisnap8_extract_collapses_inner_whitespace() {
    let mut a = perfect_answers(&resolved());
    a.insert("extract-03".into(), Answer::Value("20   points".into()));
    assert_eq!(failure_of(&run(&a), "extract-03"), None);
}

#[test]
fn aisnap8_ref_accepts_any_of_locators() {
    let r = resolved();
    let nav = r.iter().find(|t| t.id == "nav-01").expect("nav-01");
    let mut accepted = 0;
    for l in &nav.locators {
        for rf in &l.refs {
            let mut a = perfect_answers(&r);
            a.insert("nav-01".into(), Answer::Ref(format!(" {rf} ")));
            assert_eq!(failure_of(&run(&a), "nav-01"), None, "{rf}");
            accepted += 1;
        }
    }
    assert!(accepted >= 2, "nav-01 must have several accepted refs");
}

fn form_steps(a: &BTreeMap<String, Answer>, id: &str) -> Vec<AnswerStep> {
    match a.get(id) {
        Some(Answer::Steps(s)) => s.clone(),
        other => panic!("{id}: {other:?}"),
    }
}

#[test]
fn aisnap8_form_mismatch_cases() {
    let r = resolved();
    let base = perfect_answers(&r);
    let steps = form_steps(&base, "form-01");
    assert!(steps.len() >= 3);

    let mut swapped = steps.clone();
    swapped.swap(0, 1);
    let mut short = steps.clone();
    short.pop();
    let mut wrong_value = steps.clone();
    wrong_value[0].value = Some("other".into());
    let mut wrong_ref = steps.clone();
    wrong_ref[0].target_ref = "e-wrong".into();
    let mut wrong_action = steps.clone();
    wrong_action[0].action = Action::Select;

    for (name, s) in [
        ("swapped", swapped),
        ("short", short),
        ("value", wrong_value),
        ("ref", wrong_ref),
        ("action", wrong_action),
    ] {
        let mut a = base.clone();
        a.insert("form-01".into(), Answer::Steps(s));
        assert_eq!(
            failure_of(&run(&a), "form-01"),
            Some(FailureClass::Mismatch),
            "{name}"
        );
    }
}

#[test]
fn aisnap8_form_04_double_click_is_mismatch() {
    let base = perfect_answers(&resolved());
    let steps = form_steps(&base, "form-04");
    assert_eq!(steps.len(), 1);
    let mut a = base.clone();
    a.insert(
        "form-04".into(),
        Answer::Steps(vec![steps[0].clone(), steps[0].clone()]),
    );
    assert_eq!(
        failure_of(&run(&a), "form-04"),
        Some(FailureClass::Mismatch)
    );
}

#[test]
fn aisnap8_shape_and_unanswered_classification() {
    let mut a = perfect_answers(&resolved());
    a.insert("click-01".into(), Answer::Value("x".into()));
    a.insert("extract-02".into(), Answer::Ref("x".into()));
    a.insert("form-02".into(), Answer::Ref("x".into()));
    a.insert("nav-03".into(), Answer::Unanswered);
    a.insert("nav-02".into(), Answer::Invalid);
    let rep = run(&a);
    assert_eq!(
        failure_of(&rep, "click-01"),
        Some(FailureClass::InvalidShape)
    );
    assert_eq!(
        failure_of(&rep, "extract-02"),
        Some(FailureClass::InvalidShape)
    );
    assert_eq!(
        failure_of(&rep, "form-02"),
        Some(FailureClass::InvalidShape)
    );
    assert_eq!(failure_of(&rep, "nav-03"), Some(FailureClass::Unanswered));
    assert_eq!(failure_of(&rep, "nav-02"), Some(FailureClass::InvalidShape));
}

#[test]
fn aisnap8_unresolved_golden_ref_fails_closed() {
    let mut r = resolved();
    for t in r
        .iter_mut()
        .filter(|t| t.id == "click-01" || t.id == "form-01")
    {
        for l in &mut t.locators {
            l.refs.clear();
        }
    }
    let mut a = perfect_answers(&resolved());
    a.insert("click-01".into(), Answer::Ref("e-any".into()));
    let rep = run_with(&r, &a);
    assert_eq!(
        failure_of(&rep, "click-01"),
        Some(FailureClass::GoldenUnresolved)
    );
    assert_eq!(
        failure_of(&rep, "form-01"),
        Some(FailureClass::GoldenUnresolved)
    );
}

#[test]
fn aisnap8_parse_answers_shapes() {
    let json = r#"[
      {"id": "click-01", "ref": "e1", "reasoning": "ignored"},
      {"id": "extract-02", "value": "x"},
      {"id": "form-01", "steps": [
        {"action": "fill", "ref": "e2", "value": "v"},
        {"action": "click", "ref": "e3", "value": null}]},
      {"id": "nav-03", "ref": null},
      {"id": "click-02", "ref": 5},
      {"id": "form-02", "steps": [{"action": "fill", "ref": "e2"}]},
      {"id": "form-03", "steps": [{"action": "click", "ref": "e2", "value": "v"}]},
      {"id": "nav-01", "ref": "e1", "value": "x"}
    ]"#;
    let m = parse_answers(&TASKS, json).expect("parse");
    assert_eq!(m.get("click-01"), Some(&Answer::Ref("e1".into())));
    assert_eq!(m.get("extract-02"), Some(&Answer::Value("x".into())));
    assert_eq!(
        m.get("form-01"),
        Some(&Answer::Steps(vec![
            AnswerStep {
                action: Action::Fill,
                target_ref: "e2".into(),
                value: Some("v".into())
            },
            AnswerStep {
                action: Action::Click,
                target_ref: "e3".into(),
                value: None
            },
        ]))
    );
    assert_eq!(m.get("nav-03"), Some(&Answer::Unanswered));
    assert_eq!(m.get("click-02"), Some(&Answer::Invalid));
    assert_eq!(m.get("form-02"), Some(&Answer::Invalid));
    assert_eq!(m.get("form-03"), Some(&Answer::Invalid));
    assert_eq!(m.get("nav-01"), Some(&Answer::Invalid));
    assert_eq!(m.len(), 8);
}

#[test]
fn aisnap8_parse_answers_errors() {
    assert!(matches!(
        parse_answers(&TASKS, "not json"),
        Err(ScoreError::Json(_))
    ));
    assert!(matches!(
        parse_answers(&TASKS, "{}"),
        Err(ScoreError::Json(_))
    ));
    assert!(matches!(
        parse_answers(&TASKS, "[1]"),
        Err(ScoreError::Json(_))
    ));
    assert_eq!(
        parse_answers(
            &TASKS,
            r#"[{"id":"click-01","ref":"a"},{"id":"click-01","ref":"b"}]"#
        ),
        Err(ScoreError::DuplicateId("click-01".into()))
    );
    assert_eq!(
        parse_answers(&TASKS, r#"[{"id":"nope","ref":"a"}]"#),
        Err(ScoreError::UnknownId("nope".into()))
    );
}

#[test]
fn aisnap8_parse_answers_limits() {
    let long = "a".repeat(score::MAX_STRING_BYTES + 1);
    let s = format!(r#"[{{"id":"click-01","ref":"{long}"}}]"#);
    assert!(matches!(
        parse_answers(&TASKS, &s),
        Err(ScoreError::TooLarge(_))
    ));

    let many = vec![r#"{"id":"click-01"}"#; score::MAX_ENTRIES + 1].join(",");
    assert!(matches!(
        parse_answers(&TASKS, &format!("[{many}]")),
        Err(ScoreError::TooLarge(_))
    ));

    let steps = vec![r#"{"action":"click","ref":"e1"}"#; score::MAX_STEPS + 1].join(",");
    let s = format!(r#"[{{"id":"form-01","steps":[{steps}]}}]"#);
    assert!(matches!(
        parse_answers(&TASKS, &s),
        Err(ScoreError::TooLarge(_))
    ));

    let pad = " ".repeat(score::MAX_INPUT_BYTES + 1);
    assert!(matches!(
        parse_answers(&TASKS, &format!("[]{pad}")),
        Err(ScoreError::TooLarge(_))
    ));
}

#[test]
fn aisnap8_score_rejects_unknown_answer_id() {
    let mut a = BTreeMap::new();
    a.insert("zzz".to_owned(), Answer::Unanswered);
    assert_eq!(
        score(&TASKS, &GOLDEN, &resolved(), &a),
        Err(ScoreError::UnknownId("zzz".into()))
    );
}

#[test]
fn aisnap8_results_json_is_fixed_format() {
    let rep = run(&BTreeMap::new());
    let out = render_results_json(&rep);
    assert!(out.starts_with(
        "{\n  \"threshold_percent\": 70,\n  \"meets\": false,\n  \"overall\": {\"pass\": 0, \"total\": 25, \"rate\": \"0.0%\"},\n  \"by_category\": [\n    {\"category\": \"click\", \"pass\": 0, \"total\": 7, \"rate\": \"0.0%\"},\n"
    ));
    assert!(out.contains(
        "    {\"id\": \"click-01\", \"category\": \"click\", \"pass\": false, \"failure\": \"unanswered\"},\n"
    ));
    assert!(out.ends_with("  ]\n}\n"));
    assert!(!out.contains('\r'));
    assert!(!out.ends_with("\n\n"));

    let ok = render_results_json(&run(&perfect_answers(&resolved())));
    assert!(ok.contains("\"meets\": true"));
    assert!(ok.contains(
        "    {\"id\": \"nav-06\", \"category\": \"nav\", \"pass\": true, \"failure\": null}\n"
    ));
}

/// 親 #118 の測定工程向けの入口。`AGENT_EVAL_ANSWERS=<回答ファイルの絶対パス>` が
/// 設定されたときだけ実回答を採点して結果を stdout へ出す（未設定時は何もしない）。
/// `AGENT_EVAL_WRITE` を併用したときのみ `harness/agent_eval/results.json` へ書く。
#[test]
fn aisnap8_score_real_answers_if_requested() {
    let Some(path) = std::env::var_os("AGENT_EVAL_ANSWERS") else {
        return;
    };
    // 上限 +1 バイトまでしか読まない。超過分は parse_answers が上限超過として拒否する
    let mut buf = Vec::new();
    std::fs::File::open(PathBuf::from(path))
        .expect("open answers")
        .take(score::MAX_INPUT_BYTES as u64 + 1)
        .read_to_end(&mut buf)
        .expect("read answers");
    let json = String::from_utf8(buf).expect("answers must be UTF-8");
    let answers = parse_answers(&TASKS, &json).expect("parse answers");
    let rep = run(&answers);
    let out = render_results_json(&rep);
    println!("{out}");
    if std::env::var_os("AGENT_EVAL_WRITE").is_some() {
        let p = manifest()
            .join("harness")
            .join("agent_eval")
            .join("results.json");
        std::fs::write(p, out).expect("write results");
    }
}

/// 巨大な値でも `Tally` の演算が桁あふれしない（AISNAP-8・TASK-21.3）。
#[test]
fn aisnap8_tally_large_values_do_not_overflow() {
    let t = Tally {
        pass: u32::MAX,
        total: u32::MAX,
    };
    assert_eq!(t.rate_permille(), Some(1000));
    assert!(t.meets(100));
    let half = Tally {
        pass: u32::MAX / 2,
        total: u32::MAX,
    };
    assert_eq!(half.rate_permille(), Some(500));
    assert!(!half.meets(70));
}
// ---- 生 DOM 直渡し方式の採点（TASK-22.1b・AISNAP-9・Issue #805） ----

/// login-form を指す golden ロケータを持つタスク（click-01・form-01・nav-06）。
const UNRESOLVED_LOGIN: [&str; 3] = ["click-01", "form-01", "nav-06"];

fn raw_pages() -> BTreeMap<String, Document> {
    raw_dom_documents(&fixtures()).expect("raw pages")
}

fn raw_run_with(
    pages: &BTreeMap<String, Document>,
    answers: &BTreeMap<String, RawAnswer>,
) -> ScoreReport {
    let (res, ans) = resolve_raw_dom(&TASKS, &GOLDEN, pages, answers).expect("resolve raw");
    score(&TASKS, &GOLDEN, &res, &ans).expect("score raw")
}

fn raw_run(answers: &BTreeMap<String, RawAnswer>) -> ScoreReport {
    raw_run_with(&raw_pages(), answers)
}

fn locator_answer(selector: &str, index: usize) -> RawAnswer {
    RawAnswer::Locator {
        selector: selector.to_owned(),
        index,
    }
}

fn raw_step(action: Action, selector: &str, index: usize, value: Option<&str>) -> RawAnswerStep {
    RawAnswerStep {
        action,
        selector: selector.to_owned(),
        index,
        value: value.map(str::to_owned),
    }
}

/// golden ロケータそのものを回答にした正答集。
fn raw_perfect_answers() -> BTreeMap<String, RawAnswer> {
    let mut m = BTreeMap::new();
    for (id, g) in GOLDEN.iter() {
        let a = match g {
            Golden::Ref { any_of } => {
                let l = any_of.first().expect("locator");
                locator_answer(l.selector, l.index)
            }
            Golden::Value { value, .. } => RawAnswer::Value((*value).to_owned()),
            Golden::Steps(steps) => RawAnswer::Steps(
                steps
                    .iter()
                    .map(|s| raw_step(s.action, s.target.selector, s.target.index, s.value))
                    .collect(),
            ),
        };
        m.insert((*id).to_owned(), a);
    }
    m
}

fn clear_refs(r: &mut [ResolvedTask], ids: &[&str]) {
    for t in r.iter_mut().filter(|t| ids.contains(&t.id.as_str())) {
        for l in t.locators.iter_mut() {
            l.refs.clear();
        }
    }
}

#[test]
fn aisnap9_reduced_scoring_unchanged() {
    let rep = run(&perfect_answers(&resolved()));
    assert_eq!((rep.overall.pass, rep.overall.total), (25, 25));
    assert!(rep.excluding_golden_unresolved.excluded.is_empty());
    assert_eq!(rep.excluding_golden_unresolved.overall, rep.overall);
    assert_eq!(rep.excluding_golden_unresolved.by_category, rep.by_category);
    // parse_answers の共通化後も既存形式の入力が同じ回答になる
    let a = parse_answers(
        &TASKS,
        r#"[{"id":"click-01","ref":" e1 "},{"id":"extract-01","value":"v"},{"id":"form-02","steps":[{"action":"select","ref":"e2","value":"1"}]}]"#,
    )
    .expect("parse");
    assert_eq!(a.get("click-01"), Some(&Answer::Ref(" e1 ".to_owned())));
    assert_eq!(a.get("extract-01"), Some(&Answer::Value("v".to_owned())));
    assert_eq!(
        a.get("form-02"),
        Some(&Answer::Steps(vec![AnswerStep {
            action: Action::Select,
            target_ref: "e2".to_owned(),
            value: Some("1".to_owned()),
        }]))
    );
}

#[test]
fn aisnap9_raw_perfect_answers_pass_all() {
    let rep = raw_run(&raw_perfect_answers());
    assert_eq!(tally(&rep, Category::Click), (7, 7));
    assert_eq!(tally(&rep, Category::Extract), (6, 6));
    assert_eq!(tally(&rep, Category::Form), (6, 6));
    assert_eq!(tally(&rep, Category::Nav), (6, 6));
    assert_eq!((rep.overall.pass, rep.overall.total), (25, 25));
    assert_eq!(rep.overall.rate_text(), "100.0%");
    assert!(rep.excluding_golden_unresolved.excluded.is_empty());
}

#[test]
fn aisnap9_raw_alternative_selector_same_element_passes() {
    let mut a = raw_perfect_answers();
    // click-01 の golden は `form#login button[type=submit]`（0）。同じ要素を別の書き方で指す
    a.insert("click-01".into(), locator_answer("button.radius", 0));
    // nav-01 の any_of のうち 2 つ目（div.lang3 > a）でも合格する
    a.insert("nav-01".into(), locator_answer("div.lang3 > a", 0));
    // form-01 のフィールドを id でなく name / type で指す
    a.insert(
        "form-01".into(),
        RawAnswer::Steps(vec![
            raw_step(Action::Fill, "input[name=username]", 0, Some("dummy-user")),
            raw_step(Action::Fill, "input[type=password]", 0, Some("dummy-pass")),
            raw_step(Action::Click, "button", 0, None),
        ]),
    );
    let rep = raw_run(&a);
    assert_eq!(failure_of(&rep, "click-01"), None);
    assert_eq!(failure_of(&rep, "nav-01"), None);
    assert_eq!(failure_of(&rep, "form-01"), None);
    assert_eq!(rep.overall.pass, 25);
}

#[test]
fn aisnap9_raw_wrong_element_and_out_of_range_are_mismatch() {
    let mut a = raw_perfect_answers();
    // 2 つ目のチェックボックス（別要素）
    a.insert(
        "click-03".into(),
        locator_answer("form#checkboxes input[type=checkbox]", 1),
    );
    // 有効なセレクタだが index が一致件数以上
    a.insert("click-01".into(), locator_answer("button.radius", 5));
    let rep = raw_run(&a);
    assert_eq!(failure_of(&rep, "click-03"), Some(FailureClass::Mismatch));
    assert_eq!(failure_of(&rep, "click-01"), Some(FailureClass::Mismatch));
    assert_eq!(rep.overall.pass, 23);
}

#[test]
fn aisnap9_raw_unsupported_or_malformed_selector_is_invalid_shape() {
    let mut a = raw_perfect_answers();
    a.insert("click-01".into(), locator_answer("div:nth-child(1)", 0));
    a.insert("click-02".into(), locator_answer("*", 0));
    a.insert("click-03".into(), locator_answer("a + b", 0));
    a.insert("nav-02".into(), locator_answer("a[href", 0));
    let rep = raw_run(&a);
    for id in ["click-01", "click-02", "click-03", "nav-02"] {
        assert_eq!(
            failure_of(&rep, id),
            Some(FailureClass::InvalidShape),
            "{id}"
        );
    }
    // 1 件のエラーで他のタスクの採点は止まらない
    assert_eq!(rep.overall.pass, 21);
}

#[test]
fn aisnap9_raw_form_steps_mismatch_cases() {
    let good = vec![
        raw_step(Action::Fill, "input#username", 0, Some("dummy-user")),
        raw_step(Action::Fill, "input#password", 0, Some("dummy-pass")),
        raw_step(Action::Click, "form#login button[type=submit]", 0, None),
    ];
    let check = |steps: Vec<RawAnswerStep>| {
        let mut a = raw_perfect_answers();
        a.insert("form-01".into(), RawAnswer::Steps(steps));
        failure_of(&raw_run(&a), "form-01")
    };
    assert_eq!(check(good.clone()), None);
    let mut swapped = good.clone();
    swapped.swap(0, 1);
    assert_eq!(check(swapped), Some(FailureClass::Mismatch));
    assert_eq!(
        check(good.get(..2).expect("two").to_vec()),
        Some(FailureClass::Mismatch)
    );
    let mut bad_value = good.clone();
    if let Some(s) = bad_value.get_mut(0) {
        s.value = Some("other".to_owned());
    }
    assert_eq!(check(bad_value), Some(FailureClass::Mismatch));
    let mut bad_loc = good.clone();
    if let Some(s) = bad_loc.get_mut(1) {
        s.selector = "input#username".to_owned();
    }
    assert_eq!(check(bad_loc), Some(FailureClass::Mismatch));
    let mut bad_action = good.clone();
    if let Some(s) = bad_action.get_mut(2) {
        s.action = Action::Fill;
        s.value = Some("x".to_owned());
    }
    assert_eq!(check(bad_action), Some(FailureClass::Mismatch));
    // 手順内の 1 つが未対応構文なら手順全体が invalid_shape
    let mut unsupported = good;
    if let Some(s) = unsupported.get_mut(0) {
        s.selector = "input:first-child".to_owned();
    }
    assert_eq!(check(unsupported), Some(FailureClass::InvalidShape));

    // form-04 は同じ手順の繰り返しで不合格
    let one = raw_step(
        Action::Click,
        "form#checkboxes input[type=checkbox]",
        0,
        None,
    );
    let mut a = raw_perfect_answers();
    a.insert("form-04".into(), RawAnswer::Steps(vec![one.clone(), one]));
    assert_eq!(
        failure_of(&raw_run(&a), "form-04"),
        Some(FailureClass::Mismatch)
    );
}

#[test]
fn aisnap9_raw_extract_same_criteria_as_reduced() {
    let check = |v: &str| {
        let mut a = raw_perfect_answers();
        a.insert("extract-01".into(), RawAnswer::Value(v.to_owned()));
        failure_of(&raw_run(&a), "extract-01")
    };
    assert_eq!(check("  user3@example.com \n"), None);
    assert_eq!(check("user4@example.com"), Some(FailureClass::Mismatch));
    assert_eq!(check("USER3@example.com"), Some(FailureClass::Mismatch));
    let mut a = raw_perfect_answers();
    a.insert("extract-03".into(), RawAnswer::Value("20   points".into()));
    assert_eq!(failure_of(&raw_run(&a), "extract-03"), None);
}

#[test]
fn aisnap9_raw_parse_answers_shapes() {
    let p = |json: &str| {
        parse_raw_answers(&TASKS, json)
            .expect("parse")
            .remove("click-01")
            .expect("entry")
    };
    assert_eq!(
        p(r#"[{"id":"click-01","selector":"a","index":2,"extra":1}]"#),
        locator_answer("a", 2)
    );
    assert_eq!(p(r#"[{"id":"click-01"}]"#), RawAnswer::Unanswered);
    assert_eq!(
        p(r#"[{"id":"click-01","selector":null,"index":null,"value":null,"steps":null}]"#),
        RawAnswer::Unanswered
    );
    assert_eq!(
        p(r#"[{"id":"click-01","value":" v "}]"#),
        RawAnswer::Value(" v ".to_owned())
    );
    for bad in [
        r#"{"id":"click-01","selector":"a"}"#,
        r#"{"id":"click-01","index":0}"#,
        r#"{"id":"click-01","selector":"a","index":-1}"#,
        r#"{"id":"click-01","selector":"a","index":1.5}"#,
        r#"{"id":"click-01","selector":"a","index":"0"}"#,
        r#"{"id":"click-01","selector":"a","index":0,"ref":"e1"}"#,
        r#"{"id":"click-01","ref":"e1"}"#,
        r#"{"id":"click-01","selector":"a","index":0,"value":"v"}"#,
        r#"{"id":"click-01","selector":5,"index":0}"#,
        r#"{"id":"click-01","steps":"x"}"#,
        r#"{"id":"click-01","steps":[{"action":"click","selector":"a"}]}"#,
        r#"{"id":"click-01","steps":[{"action":"click","ref":"e1"}]}"#,
        r#"{"id":"click-01","steps":[{"action":"click","selector":"a","index":0,"value":"v"}]}"#,
        r#"{"id":"click-01","steps":[{"action":"fill","selector":"a","index":0}]}"#,
        r#"{"id":"click-01","steps":[{"action":"hover","selector":"a","index":0}]}"#,
    ] {
        assert_eq!(p(&format!("[{bad}]")), RawAnswer::Invalid, "{bad}");
    }
    assert_eq!(
        p(
            r#"[{"id":"click-01","steps":[{"action":"fill","selector":"a","index":1,"value":"v"},{"action":"click","selector":"b","index":0}]}]"#
        ),
        RawAnswer::Steps(vec![
            raw_step(Action::Fill, "a", 1, Some("v")),
            raw_step(Action::Click, "b", 0, None),
        ])
    );
}

#[test]
fn aisnap9_raw_parse_answers_limits_and_errors() {
    let long = "a".repeat(score::MAX_STRING_BYTES + 1);
    let too_large = |json: String| {
        assert!(
            matches!(
                parse_raw_answers(&TASKS, &json),
                Err(ScoreError::TooLarge(_))
            ),
            "{}",
            json.len()
        )
    };
    too_large(format!(
        r#"[{{"id":"click-01","selector":"{long}","index":0}}]"#
    ));
    too_large(format!(r#"[{{"id":"click-01","value":"{long}"}}]"#));
    let steps = vec![r#"{"action":"click","selector":"a","index":0}"#; score::MAX_STEPS + 1];
    too_large(format!(
        r#"[{{"id":"form-01","steps":[{}]}}]"#,
        steps.join(",")
    ));
    let entries = vec![r#"{"id":"click-01"}"#; score::MAX_ENTRIES + 1];
    too_large(format!("[{}]", entries.join(",")));
    too_large(" ".repeat(score::MAX_INPUT_BYTES + 1));
    assert_eq!(
        parse_raw_answers(&TASKS, r#"[{"id":"click-01"},{"id":"click-01"}]"#),
        Err(ScoreError::DuplicateId("click-01".into()))
    );
    assert_eq!(
        parse_raw_answers(&TASKS, r#"[{"id":"zzz"}]"#),
        Err(ScoreError::UnknownId("zzz".into()))
    );
}

#[test]
fn aisnap9_raw_golden_unresolved_and_exclusion_tallies() {
    // login-form を空 body に差し替え、そのページを指す ref 型・steps 型を golden 未解決にする
    let mut pages = raw_pages();
    let empty = parse_document("<body></body>", &ParseOptions::default())
        .expect("parse")
        .document;
    pages.insert("login-form".to_owned(), empty);
    let rep = raw_run_with(&pages, &raw_perfect_answers());
    let unresolved: Vec<&str> = rep
        .results
        .iter()
        .filter(|r| r.failure == Some(FailureClass::GoldenUnresolved))
        .map(|r| r.id.as_str())
        .collect();
    assert_eq!(unresolved, UNRESOLVED_LOGIN);
    assert_eq!((rep.overall.pass, rep.overall.total), (22, 25));
    let ex = &rep.excluding_golden_unresolved;
    assert_eq!(ex.excluded, UNRESOLVED_LOGIN);
    assert_eq!((ex.overall.pass, ex.overall.total), (22, 22));
    assert_eq!(ex.overall.rate_text(), "100.0%");

    // 簡約方式でも refs を空にして同じ 2 種類の集計が得られる
    let mut r = resolved();
    clear_refs(&mut r, &["click-01", "form-01"]);
    let rep = run_with(&r, &perfect_answers(&resolved()));
    assert_eq!((rep.overall.pass, rep.overall.total), (23, 25));
    assert_eq!(
        rep.excluding_golden_unresolved.excluded,
        vec!["click-01".to_owned(), "form-01".to_owned()]
    );
    assert_eq!(
        (
            rep.excluding_golden_unresolved.overall.pass,
            rep.excluding_golden_unresolved.overall.total
        ),
        (23, 23)
    );
    // 任意の除外集合（実在しない id は excluded に入らない）
    let custom: BTreeSet<String> = ["nav-01".to_owned(), "nope".to_owned()].into();
    let s = summarize(&rep.results, &custom);
    assert_eq!(s.excluded, vec!["nav-01".to_owned()]);
    assert_eq!((s.overall.pass, s.overall.total), (22, 24));
}

#[test]
fn aisnap9_raw_missing_page_is_error() {
    let mut pages = raw_pages();
    pages.remove("login-form");
    assert_eq!(
        resolve_raw_dom(&TASKS, &GOLDEN, &pages, &BTreeMap::new()).map(|_| ()),
        Err(ScoreError::MissingPage("login-form".into()))
    );
    let mut unknown = BTreeMap::new();
    unknown.insert("zzz".to_owned(), RawAnswer::Unanswered);
    assert_eq!(
        resolve_raw_dom(&TASKS, &GOLDEN, &raw_pages(), &unknown).map(|_| ()),
        Err(ScoreError::UnknownId("zzz".into()))
    );
}

#[test]
fn aisnap9_raw_golden_locators_resolve_in_every_page() {
    // golden のセレクタが生 DOM 上で評価でき、全ロケータが要素を指す（refs が 1 件ずつ）
    let (res, _) =
        resolve_raw_dom(&TASKS, &GOLDEN, &raw_pages(), &BTreeMap::new()).expect("resolve");
    assert_eq!(res.len(), 25);
    for (t, (_, g)) in res.iter().zip(GOLDEN.iter()) {
        assert_eq!(t.locators.len(), golden_locators(g).len(), "{}", t.id);
        assert!(t.locators.iter().all(|l| l.refs.len() == 1), "{}", t.id);
    }
}

#[test]
fn aisnap9_results_json_has_exclusion_block() {
    let rep = run(&BTreeMap::new());
    let out = render_results_json(&rep);
    let block = concat!(
        "  ],\n  \"excluding_golden_unresolved\": {\n    \"excluded\": [],\n",
        "    \"overall\": {\"pass\": 0, \"total\": 25, \"rate\": \"0.0%\"},\n",
        "    \"by_category\": [\n",
        "      {\"category\": \"click\", \"pass\": 0, \"total\": 7, \"rate\": \"0.0%\"},\n",
        "      {\"category\": \"extract\", \"pass\": 0, \"total\": 6, \"rate\": \"0.0%\"},\n",
        "      {\"category\": \"form\", \"pass\": 0, \"total\": 6, \"rate\": \"0.0%\"},\n",
        "      {\"category\": \"nav\", \"pass\": 0, \"total\": 6, \"rate\": \"0.0%\"}\n",
        "    ]\n  },\n  \"results\": [\n"
    );
    assert!(out.contains(block), "{out}");

    let mut r = resolved();
    clear_refs(&mut r, &["click-01"]);
    let rep = run_with(&r, &perfect_answers(&resolved()));
    let out = render_results_json(&rep);
    assert!(out.contains("    \"excluded\": [\"click-01\"],\n"), "{out}");
    assert!(
        out.contains("    \"overall\": {\"pass\": 24, \"total\": 24, \"rate\": \"100.0%\"},\n")
    );
    assert!(!out.contains('\r'));
}

/// 実回答（生 DOM 方式）を採点する入口。`AGENT_EVAL_RAW_ANSWERS=<回答ファイルの絶対パス>` が
/// 設定されたときだけ採点し、結果を stdout へ出す（リポジトリ内へは書かない。未設定時は何もしない）。
#[test]
fn aisnap9_score_raw_answers_if_requested() {
    let Some(path) = std::env::var_os("AGENT_EVAL_RAW_ANSWERS") else {
        return;
    };
    let mut buf = Vec::new();
    std::fs::File::open(PathBuf::from(path))
        .expect("open answers")
        .take(score::MAX_INPUT_BYTES as u64 + 1)
        .read_to_end(&mut buf)
        .expect("read answers");
    let json = String::from_utf8(buf).expect("answers must be UTF-8");
    let answers = parse_raw_answers(&TASKS, &json).expect("parse raw answers");
    println!("{}", render_results_json(&raw_run(&answers)));
}
